//! Map workspace build outputs back to source files via tsconfig outDir->rootDir,
//! project references, declaration/source maps, and package.json source/exports fields.
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Component, Path, PathBuf};

use oxc_allocator::Allocator;
use oxc_ast::{
    AstKind,
    ast::{Expression, ObjectPropertyKind, PropertyKey},
};
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::SourceType;
use serde_json::Value;

use crate::graph::Id;
use crate::project::EXTENSIONS;

const SOURCE_EXTENSIONS: &[&str] = &[
    "ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs", "d.ts", "d.mts", "d.cts",
];
const TYPE_SOURCE_EXTENSIONS: &[&str] = &[
    "ts", "tsx", "mts", "cts", "d.ts", "d.mts", "d.cts", "js", "jsx", "mjs", "cjs",
];
const BUILT_SUFFIXES: &[&str] = &[
    ".d.ts", ".d.mts", ".d.cts", ".js", ".jsx", ".mjs", ".cjs", ".ts", ".tsx", ".mts", ".cts",
];

#[derive(Clone, Debug)]
struct DirMapping {
    out_dir: PathBuf,
    root_dir: PathBuf,
}

#[derive(Clone, Debug)]
struct TsPathRule {
    prefix: String,
    suffix: String,
    wildcard: bool,
    targets: Vec<PathBuf>,
}

#[derive(Clone, Default)]
struct ParsedTsconfig {
    out_dir: Option<PathBuf>,
    declaration_dir: Option<PathBuf>,
    root_dir: Option<PathBuf>,
    inferred_root_dir: Option<PathBuf>,
    base_url: Option<PathBuf>,
    paths: Vec<(String, Vec<String>, PathBuf)>,
}

#[derive(Default)]
pub struct SourceMapper {
    mappings: Vec<DirMapping>,
    tsconfig_paths: HashMap<PathBuf, Vec<TsPathRule>>,
}

pub fn normalize_rel(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::Normal(part) => out.push(part),
            Component::ParentDir if out.pop() => {}
            _ => return None,
        }
    }
    Some(out)
}

pub fn to_slash(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn read_jsonc(path: &Path) -> Option<Value> {
    let mut bytes = fs::read(path).ok()?;
    json_strip_comments::strip_slice(&mut bytes).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn resolve_rel_to(base_dir: &Path, raw: &str) -> Option<PathBuf> {
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| normalize_rel(&base_dir.join(trimmed)))?
}

fn infer_root_from_include(base_dir: &Path, include: &[Value]) -> Option<PathBuf> {
    let mut prefix: Option<PathBuf> = None;
    for entry in include {
        let Some(pattern) = entry.as_str().map(|s| s.trim().trim_start_matches("./")) else {
            continue;
        };
        if pattern.is_empty() || pattern.starts_with('!') {
            continue;
        }
        let mut dir = PathBuf::new();
        for part in pattern.split('/') {
            if part.is_empty() || part == "." {
                continue;
            }
            if part.contains(['*', '?', '[']) || (part.contains('.') && pattern.ends_with(part)) {
                break;
            }
            dir.push(part);
        }
        if dir.as_os_str().is_empty() {
            return None;
        }
        let full = normalize_rel(&base_dir.join(&dir))?;
        match &prefix {
            None => prefix = Some(full),
            Some(existing) if existing == &full => {}
            Some(_) => return None,
        }
    }
    prefix
}

fn load_tsconfig(
    repo_root: &Path,
    rel_file: &Path,
    cache: &mut HashMap<PathBuf, ParsedTsconfig>,
    visiting: &mut BTreeSet<PathBuf>,
    extra_refs: &mut Vec<PathBuf>,
) -> ParsedTsconfig {
    if let Some(cached) = cache.get(rel_file) {
        return cached.clone();
    }
    if !visiting.insert(rel_file.to_owned()) {
        return ParsedTsconfig::default();
    }
    let Some(json) = read_jsonc(&repo_root.join(rel_file)) else {
        visiting.remove(rel_file);
        return ParsedTsconfig::default();
    };
    let dir = rel_file.parent().unwrap_or_else(|| Path::new(""));
    let mut result = ParsedTsconfig::default();

    let parents: Vec<&str> = match &json["extends"] {
        Value::String(s) => vec![s.as_str()],
        Value::Array(arr) => arr.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    };
    for spec in parents.into_iter().filter(|s| s.starts_with('.')) {
        let mut cand = dir.join(spec);
        if cand.extension().is_none() {
            cand.set_extension("json");
        }
        let parent_path = normalize_rel(&cand)
            .filter(|p| repo_root.join(p).is_file())
            .or_else(|| {
                normalize_rel(&dir.join(spec).join("tsconfig.json"))
                    .filter(|p| repo_root.join(p).is_file())
            });
        if let Some(p) = parent_path {
            let inherited = load_tsconfig(repo_root, &p, cache, visiting, extra_refs);
            result.out_dir = inherited.out_dir.or(result.out_dir);
            result.declaration_dir = inherited.declaration_dir.or(result.declaration_dir);
            result.root_dir = inherited.root_dir.or(result.root_dir);
            result.inferred_root_dir = inherited.inferred_root_dir.or(result.inferred_root_dir);
            result.base_url = inherited.base_url.or(result.base_url);
            if !inherited.paths.is_empty() {
                result.paths = inherited.paths;
            }
        }
    }

    if let Some(opts) = json["compilerOptions"].as_object() {
        for (key, slot) in [
            ("outDir", &mut result.out_dir),
            ("declarationDir", &mut result.declaration_dir),
            ("rootDir", &mut result.root_dir),
            ("baseUrl", &mut result.base_url),
        ] {
            if let Some(p) = opts
                .get(key)
                .and_then(Value::as_str)
                .and_then(|s| resolve_rel_to(dir, s))
            {
                *slot = Some(p);
            }
        }
        if let Some(paths_obj) = opts.get("paths").and_then(Value::as_object) {
            let base = result.base_url.clone().unwrap_or_else(|| dir.to_owned());
            result.paths = paths_obj
                .iter()
                .filter_map(|(k, v)| {
                    let targets: Vec<String> = v
                        .as_array()?
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect();
                    (!targets.is_empty()).then(|| (k.clone(), targets, base.clone()))
                })
                .collect();
        }
    }
    if let Some(inferred) = json["include"]
        .as_array()
        .and_then(|inc| infer_root_from_include(dir, inc))
    {
        result.inferred_root_dir = Some(inferred);
    }
    if let Some(refs) = json["references"].as_array() {
        for item in refs {
            if let Some(raw) = item["path"].as_str()
                && let Some(norm) = normalize_rel(&dir.join(raw))
            {
                let abs_ref = repo_root.join(&norm);
                if abs_ref.is_file() {
                    extra_refs.push(norm);
                } else if abs_ref.is_dir() && repo_root.join(norm.join("tsconfig.json")).is_file() {
                    extra_refs.push(norm.join("tsconfig.json"));
                }
            }
        }
    }

    visiting.remove(rel_file);
    cache.insert(rel_file.to_owned(), result.clone());
    result
}

fn static_str<'a>(expr: &'a Expression<'a>) -> Option<&'a str> {
    match expr.get_inner_expression() {
        Expression::StringLiteral(s) => Some(s.value.as_str()),
        Expression::TemplateLiteral(t) if t.expressions.is_empty() && t.quasis.len() == 1 => {
            t.quasis[0].value.cooked.as_ref().map(|c| c.as_str())
        }
        _ => None,
    }
}

fn prop_key<'a>(key: &'a PropertyKey<'a>) -> Option<&'a str> {
    match key {
        PropertyKey::StaticIdentifier(id) => Some(id.name.as_str()),
        PropertyKey::StringLiteral(s) => Some(s.value.as_str()),
        _ => None,
    }
}

fn collect_string_leaves(expr: &Expression<'_>, out: &mut Vec<String>) {
    match expr.get_inner_expression() {
        Expression::ArrayExpression(arr) => {
            for elem in &arr.elements {
                if let Some(e) = elem.as_expression() {
                    collect_string_leaves(e, out);
                }
            }
        }
        Expression::ObjectExpression(obj) => {
            for prop in &obj.properties {
                if let ObjectPropertyKind::ObjectProperty(p) = prop {
                    collect_string_leaves(&p.value, out);
                }
            }
        }
        other => {
            if let Some(s) = static_str(other) {
                out.push(s.to_owned());
            }
        }
    }
}

fn parse_bundler_out_dir(
    repo_root: &Path,
    rel_file: &Path,
    arena: &mut Allocator,
) -> Option<DirMapping> {
    let abs = repo_root.join(rel_file);
    let source = fs::read_to_string(&abs).ok()?;
    arena.reset();
    let parsed = Parser::new(
        arena,
        &source,
        SourceType::from_path(&abs).unwrap_or_default(),
    )
    .parse();
    if parsed.panicked {
        return None;
    }
    let sem = SemanticBuilder::new().build(&parsed.program).semantic;
    let dir = rel_file.parent().unwrap_or_else(|| Path::new(""));
    let mut out_dir: Option<PathBuf> = None;
    let mut entries = Vec::new();
    for node in sem.nodes().iter() {
        let AstKind::ObjectProperty(prop) = node.kind() else {
            continue;
        };
        match prop_key(&prop.key) {
            Some("outDir") => {
                if let Some(s) = static_str(&prop.value) {
                    out_dir = resolve_rel_to(dir, s);
                }
            }
            Some("entry") => collect_string_leaves(&prop.value, &mut entries),
            _ => {}
        }
    }
    let out_dir = out_dir.or_else(|| normalize_rel(&dir.join("dist")))?;
    let mut root_prefix: Option<PathBuf> = None;
    for entry in &entries {
        let trimmed = entry.trim().trim_start_matches("./");
        let parent = Path::new(trimmed).parent().unwrap_or_else(|| Path::new(""));
        if parent.as_os_str().is_empty() {
            root_prefix = Some(dir.to_owned());
            break;
        }
        let cand = normalize_rel(&dir.join(parent))?;
        match &root_prefix {
            None => root_prefix = Some(cand),
            Some(existing) if existing == &cand => {}
            Some(existing) => {
                // Find common ancestor directory across multiple entries.
                let common: PathBuf = existing
                    .components()
                    .zip(cand.components())
                    .take_while(|(a, b)| a == b)
                    .map(|(a, _)| a)
                    .collect();
                root_prefix = Some(common);
            }
        }
    }
    let root_dir = root_prefix.unwrap_or_else(|| {
        let src = dir.join("src");
        if repo_root.join(&src).is_dir() {
            src
        } else {
            dir.to_owned()
        }
    });
    (out_dir != root_dir && !out_dir.as_os_str().is_empty())
        .then_some(DirMapping { out_dir, root_dir })
}

impl SourceMapper {
    pub fn build(
        repo_root: &Path,
        tsconfig_files: &[PathBuf],
        bundler_configs: &[PathBuf],
    ) -> Self {
        let mut cache = HashMap::new();
        let mut visiting = BTreeSet::new();
        let mut queue = tsconfig_files.to_vec();
        let mut seen = BTreeSet::new();
        let mut mappings = Vec::new();
        let mut tsconfig_paths: HashMap<PathBuf, Vec<TsPathRule>> = HashMap::new();

        while let Some(rel_file) = queue.pop() {
            if !seen.insert(rel_file.clone()) {
                continue;
            }
            let mut extra_refs = Vec::new();
            let parsed = load_tsconfig(
                repo_root,
                &rel_file,
                &mut cache,
                &mut visiting,
                &mut extra_refs,
            );
            queue.extend(extra_refs);

            let dir = rel_file
                .parent()
                .unwrap_or_else(|| Path::new(""))
                .to_owned();
            if rel_file
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| {
                    n == "tsconfig.json"
                        || (n == "tsconfig.base.json" && dir.as_os_str().is_empty())
                })
                && !parsed.paths.is_empty()
            {
                let mut rules = Vec::new();
                for (pat, raw_targets, base) in &parsed.paths {
                    let (prefix, suffix, wildcard) = match pat.find('*') {
                        Some(idx) => (pat[..idx].to_owned(), pat[idx + 1..].to_owned(), true),
                        None => (pat.clone(), String::new(), false),
                    };
                    let targets = raw_targets
                        .iter()
                        .map(|t| base.join(t.trim().trim_start_matches("./")))
                        .collect();
                    rules.push(TsPathRule {
                        prefix,
                        suffix,
                        wildcard,
                        targets,
                    });
                }
                rules.sort_by(|a, b| {
                    a.wildcard
                        .cmp(&b.wildcard)
                        .then_with(|| b.prefix.len().cmp(&a.prefix.len()))
                });
                tsconfig_paths.entry(dir.clone()).or_insert(rules);
            }
            let root_dir = parsed
                .root_dir
                .or(parsed.inferred_root_dir)
                .unwrap_or_else(|| dir.clone());
            for out in [parsed.out_dir, parsed.declaration_dir]
                .into_iter()
                .flatten()
            {
                if out != root_dir && !out.as_os_str().is_empty() {
                    // Module Federation type plugins emit compiled declarations under
                    // `<outDir>/compiled-types` or `dist/<outDir>/compiled-types`.
                    if out
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n == "@mf-types")
                    {
                        mappings.push(DirMapping {
                            out_dir: out.join("compiled-types"),
                            root_dir: root_dir.clone(),
                        });
                        if let Some(parent) = out.parent() {
                            mappings.push(DirMapping {
                                out_dir: parent
                                    .join("dist")
                                    .join("@mf-types")
                                    .join("compiled-types"),
                                root_dir: root_dir.clone(),
                            });
                        }
                    }
                    mappings.push(DirMapping {
                        out_dir: out,
                        root_dir: root_dir.clone(),
                    });
                }
            }
        }

        let mut arena = Allocator::default();
        for cfg in bundler_configs {
            if let Some(mapping) = parse_bundler_out_dir(repo_root, cfg, &mut arena) {
                mappings.push(mapping);
            }
        }

        mappings.sort_by(|a, b| {
            b.out_dir
                .components()
                .count()
                .cmp(&a.out_dir.components().count())
                .then_with(|| {
                    b.out_dir
                        .as_os_str()
                        .len()
                        .cmp(&a.out_dir.as_os_str().len())
                })
                .then_with(|| (&a.out_dir, &a.root_dir).cmp(&(&b.out_dir, &b.root_dir)))
        });
        mappings.dedup_by(|a, b| a.out_dir == b.out_dir && a.root_dir == b.root_dir);
        Self {
            mappings,
            tsconfig_paths,
        }
    }

    #[inline]
    pub fn has_out_dirs(&self) -> bool {
        !self.mappings.is_empty()
    }

    pub fn is_in_out_dir(&self, rel: &Path) -> bool {
        !self.mappings.is_empty() && self.mappings.iter().any(|m| rel.starts_with(&m.out_dir))
    }

    pub fn lookup_direct(paths: &[String], rel: &Path) -> Option<Id> {
        Self::lookup_direct_mode(paths, rel, false)
    }

    pub fn lookup_direct_mode(paths: &[String], rel: &Path, type_only: bool) -> Option<Id> {
        let s = to_slash(rel);
        let base = BUILT_SUFFIXES
            .iter()
            .find_map(|ext| s.strip_suffix(ext))
            .unwrap_or(&s);
        if type_only {
            // Prefer companion declaration files when resolving type-only imports to JS modules.
            if let Some(js_base) = [".js", ".mjs", ".cjs"]
                .iter()
                .find_map(|ext| s.strip_suffix(ext))
            {
                for dts in ["d.ts", "d.mts", "d.cts"] {
                    if let Ok(id) = paths.binary_search(&format!("{js_base}.{dts}")) {
                        return Some(id);
                    }
                }
            }
        }
        if let Ok(id) = paths.binary_search(&s) {
            return Some(id);
        }
        let exts = if type_only {
            TYPE_SOURCE_EXTENSIONS
        } else {
            SOURCE_EXTENSIONS
        };
        for suffix in ["", "/index"] {
            for ext in exts {
                if let Ok(id) = paths.binary_search(&format!("{base}{suffix}.{ext}")) {
                    return Some(id);
                }
            }
        }
        None
    }

    fn lookup_sourcemap(repo_root: &Path, paths: &[String], rel: &Path) -> Option<Id> {
        let mut candidates = vec![rel.to_owned()];
        let rel_str = to_slash(rel);
        for (js_ext, dts_ext) in [(".js", ".d.ts"), (".mjs", ".d.mts"), (".cjs", ".d.cts")] {
            if let Some(base) = rel_str.strip_suffix(js_ext) {
                candidates.push(PathBuf::from(format!("{base}{dts_ext}")));
            }
        }
        for cand in candidates {
            let mut map_os = repo_root.join(&cand).into_os_string();
            map_os.push(".map");
            let Ok(data) = fs::read(PathBuf::from(map_os)) else {
                continue;
            };
            let Ok(json) = serde_json::from_slice::<Value>(&data) else {
                continue;
            };
            let Some(sources) = json["sources"].as_array() else {
                continue;
            };
            let source_root = json["sourceRoot"].as_str().unwrap_or("");
            let parent = cand.parent().unwrap_or_else(|| Path::new(""));
            for src in sources.iter().filter_map(Value::as_str) {
                let combined = if source_root.is_empty() {
                    parent.join(src)
                } else {
                    parent.join(source_root).join(src)
                };
                if let Some(norm) = normalize_rel(&combined)
                    && let Some(id) = Self::lookup_direct(paths, &norm)
                {
                    return Some(id);
                }
            }
        }
        None
    }

    pub fn map_to_source(&self, repo_root: &Path, paths: &[String], rel: &Path) -> Option<Id> {
        self.map_to_source_mode(repo_root, paths, rel, false)
    }

    pub fn map_to_source_mode(
        &self,
        repo_root: &Path,
        paths: &[String],
        rel: &Path,
        type_only: bool,
    ) -> Option<Id> {
        let norm = normalize_rel(rel)?;
        if let Some(id) = Self::lookup_sourcemap(repo_root, paths, &norm) {
            return Some(id);
        }
        for m in &self.mappings {
            if let Ok(sub) = norm.strip_prefix(&m.out_dir) {
                let candidate = if sub.as_os_str().is_empty() {
                    m.root_dir.join("index")
                } else {
                    m.root_dir.join(sub)
                };
                if let Some(id) = Self::lookup_direct_mode(paths, &candidate, type_only) {
                    return Some(id);
                }
            }
        }
        Self::lookup_direct_mode(paths, &norm, type_only)
    }

    /// Resolve a specifier through the closest tsconfig `paths` mapping, falling back
    /// through `SourceMapper` when targets point into unbuilt output directories.
    pub fn resolve_tsconfig_path(
        &self,
        file_rel: &Path,
        spec: &str,
        repo_root: &Path,
        paths: &[String],
        type_only: bool,
    ) -> Option<Id> {
        let rules = file_rel
            .ancestors()
            .skip(1)
            .find_map(|anc| self.tsconfig_paths.get(anc))?;
        for rule in rules {
            let matched = if rule.wildcard {
                if spec.starts_with(&rule.prefix)
                    && spec.ends_with(&rule.suffix)
                    && spec.len() >= rule.prefix.len() + rule.suffix.len()
                {
                    Some(&spec[rule.prefix.len()..spec.len() - rule.suffix.len()])
                } else {
                    None
                }
            } else if spec == rule.prefix {
                Some("")
            } else {
                None
            };
            let Some(star) = matched else {
                continue;
            };
            for target_tpl in &rule.targets {
                let raw = to_slash(target_tpl);
                let expanded = if rule.wildcard {
                    raw.replace('*', star)
                } else {
                    raw
                };
                if let Some(norm) = normalize_rel(Path::new(&expanded))
                    && let Some(id) = self.map_to_source_mode(repo_root, paths, &norm, type_only)
                {
                    return Some(id);
                }
            }
        }
        None
    }

    /// Fast zero-allocation check for whether `spec` matches any tsconfig `paths` rule for `file_rel`.
    pub fn is_tsconfig_path_match(&self, file_rel: &Path, spec: &str) -> bool {
        if self.tsconfig_paths.is_empty() {
            return false;
        }
        let Some(rules) = file_rel
            .ancestors()
            .skip(1)
            .find_map(|anc| self.tsconfig_paths.get(anc))
        else {
            return false;
        };
        rules.iter().any(|rule| {
            if rule.wildcard {
                spec.starts_with(&rule.prefix)
                    && spec.ends_with(&rule.suffix)
                    && spec.len() >= rule.prefix.len() + rule.suffix.len()
            } else {
                spec == rule.prefix
            }
        })
    }

    pub fn matches_tsconfig_path(&self, file_rel: &Path, spec: &str) -> Vec<PathBuf> {
        if self.tsconfig_paths.is_empty() {
            return vec![];
        }
        let Some(rules) = file_rel
            .ancestors()
            .skip(1)
            .find_map(|anc| self.tsconfig_paths.get(anc))
        else {
            return vec![];
        };
        for rule in rules {
            let matched = if rule.wildcard {
                if spec.starts_with(&rule.prefix)
                    && spec.ends_with(&rule.suffix)
                    && spec.len() >= rule.prefix.len() + rule.suffix.len()
                {
                    Some(&spec[rule.prefix.len()..spec.len() - rule.suffix.len()])
                } else {
                    None
                }
            } else if spec == rule.prefix {
                Some("")
            } else {
                None
            };
            if let Some(star) = matched {
                return rule
                    .targets
                    .iter()
                    .filter_map(|tpl| {
                        let raw = to_slash(tpl);
                        let expanded = if rule.wildcard {
                            raw.replace('*', star)
                        } else {
                            raw
                        };
                        normalize_rel(Path::new(&expanded))
                    })
                    .collect();
            }
        }
        vec![]
    }

    pub fn map_pattern(&self, repo_root: &Path, paths: &[String], pattern: &str) -> Option<String> {
        let clean = pattern.trim().trim_start_matches("./");
        if clean.is_empty() {
            return None;
        }
        if !clean.contains(['*', '?', '[']) {
            let id = self.map_to_source(repo_root, paths, &normalize_rel(Path::new(clean))?)?;
            return Some(paths[id].clone());
        }
        let star = clean.find('*')?;
        let prefix = to_slash(Path::new(&clean[..star]));
        let suffix = &clean[star + 1..];
        let ext_glob = if BUILT_SUFFIXES.iter().any(|e| suffix.ends_with(e)) {
            format!(".{{{}}}", EXTENSIONS.join(","))
        } else {
            suffix.to_owned()
        };
        for m in &self.mappings {
            let out_prefix = format!("{}/", to_slash(&m.out_dir));
            if let Some(rest) = prefix.strip_prefix(&out_prefix) {
                let root_str = to_slash(&m.root_dir);
                let base = match (root_str.is_empty(), rest.is_empty()) {
                    (true, _) => rest.to_owned(),
                    (false, true) => format!("{root_str}/"),
                    (false, false) => format!("{root_str}/{rest}"),
                };
                return Some(format!("{base}**/*{ext_glob}"));
            }
        }
        None
    }
}

pub fn resolve_exports_targets(
    exports: &Value,
    subpath: &str,
    type_only: bool,
    cjs: bool,
) -> Vec<String> {
    let mut out = Vec::new();
    match exports {
        Value::String(s) if subpath == "." && !s.is_empty() => out.push(s.clone()),
        Value::Object(map) if map.keys().any(|k| k.starts_with('.')) => {
            if let Some(exact) = map.get(subpath) {
                collect_condition_targets(exact, type_only, cjs, &mut out);
            } else {
                let mut best: Option<(usize, &str, &Value)> = None;
                for (key, val) in map {
                    if let Some(star) = key.find('*') {
                        let (prefix, suffix) = (&key[..star], &key[star + 1..]);
                        if subpath.starts_with(prefix)
                            && subpath.ends_with(suffix)
                            && subpath.len() >= prefix.len() + suffix.len()
                            && best.is_none_or(|(len, _, _)| prefix.len() > len)
                        {
                            best = Some((
                                prefix.len(),
                                &subpath[prefix.len()..subpath.len() - suffix.len()],
                                val,
                            ));
                        }
                    }
                }
                if let Some((_, matched, val)) = best {
                    let mut raw = Vec::new();
                    collect_condition_targets(val, type_only, cjs, &mut raw);
                    out.extend(raw.into_iter().map(|t| t.replace('*', matched)));
                }
            }
        }
        Value::Object(_) if subpath == "." => {
            collect_condition_targets(exports, type_only, cjs, &mut out)
        }
        Value::Array(arr) if subpath == "." => {
            for item in arr {
                collect_condition_targets(item, type_only, cjs, &mut out);
            }
        }
        _ => {}
    }
    out
}

fn collect_condition_targets(value: &Value, type_only: bool, cjs: bool, out: &mut Vec<String>) {
    match value {
        Value::String(s) if !s.is_empty() => out.push(s.clone()),
        Value::Array(arr) => {
            for item in arr {
                collect_condition_targets(item, type_only, cjs, out);
            }
        }
        Value::Object(map) => {
            let order: &[&str] = if type_only {
                &["source", "types", "node", "import", "default", "require"]
            } else if cjs {
                &["source", "node", "require", "default", "import", "types"]
            } else {
                &["source", "node", "import", "default"]
            };
            let before = out.len();
            for key in order {
                if let Some(child) = map.get(*key) {
                    collect_condition_targets(child, type_only, cjs, out);
                }
            }
            if !type_only && !cjs && out.len() == before {
                for key in ["require", "types"] {
                    if let Some(child) = map.get(key) {
                        collect_condition_targets(child, type_only, cjs, out);
                    }
                }
            }
        }
        _ => {}
    }
}

pub fn is_non_code_export(target: &str) -> bool {
    matches!(
        Path::new(target)
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("json" | "css" | "scss" | "less" | "md" | "html" | "svg" | "png" | "wasm" | "node")
    )
}
