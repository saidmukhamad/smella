use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use globset::{GlobBuilder, GlobMatcher};
use oxc_resolver::{
    ResolveError, ResolveOptions, Resolver, TsconfigDiscovery, TsconfigOptions, TsconfigReferences,
};
use serde::Deserialize;
use serde_json::Value;
use walkdir::WalkDir;

use crate::graph::{Graph, Id, ROLES, Role, Root};

const EXTENSIONS: &[&str] = &["ts", "tsx", "js", "jsx", "mts", "mjs", "cts", "cjs"];
const SKIP: &[&str] = &[
    ".git",
    "node_modules",
    "dist",
    "build",
    "target",
    "coverage",
    ".next",
    ".cache",
];

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Entries {
    pub runtime: Option<Vec<String>>,
    pub test: Option<Vec<String>>,
    pub tooling: Option<Vec<String>>,
}
impl Entries {
    fn merge(self, defaults: Self) -> Self {
        Self {
            runtime: self.runtime.or(defaults.runtime),
            test: self.test.or(defaults.test),
            tooling: self.tooling.or(defaults.tooling),
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct Config {
    pub entries: Entries,
    pub public_entries: Option<Vec<String>>,
    pub include: Option<Vec<String>>,
    pub exclude: Option<Vec<String>>,
}

fn matcher(pattern: &str) -> Result<GlobMatcher, String> {
    GlobBuilder::new(pattern.trim_start_matches("./"))
        .literal_separator(true)
        .build()
        .map(|g| g.compile_matcher())
        .map_err(|e| format!("Invalid glob {pattern:?}: {e}"))
}
fn matchers(patterns: &[String]) -> Result<Vec<GlobMatcher>, String> {
    patterns.iter().map(|p| matcher(p)).collect()
}
fn json(path: &Path) -> Result<Value, String> {
    let data = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_slice(&data).map_err(|e| format!("{}: {e}", path.display()))
}
fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

pub struct Project {
    pub root: PathBuf,
    pub roots: Vec<Root>,
    pub graph: Graph,
    pub excluded: Vec<String>,
    resolver: Resolver,
    type_resolver: Resolver,
    workspaces: BTreeMap<String, PathBuf>,
}

impl Project {
    pub fn open(root: &Path, input: Config) -> Result<Self, String> {
        let root = fs::canonicalize(root)
            .map_err(|e| format!("Cannot open repository {}: {e}", root.display()))?;
        if !root.is_dir() {
            return Err(format!("Expected a directory: {}", root.display()));
        }
        let config_path = root.join("smella.config.json");
        let defaults: Config = if config_path.try_exists().map_err(|e| e.to_string())? {
            serde_json::from_value(json(&config_path)?)
                .map_err(|e| format!("smella.config.json: {e}"))?
        } else {
            Config::default()
        };
        let include = input
            .include
            .or(defaults.include)
            .map(|v| matchers(&v))
            .transpose()?;
        let exclude = matchers(&input.exclude.or(defaults.exclude).unwrap_or_default())?;
        let entries = input.entries.merge(defaults.entries);
        let public = input.public_entries.or(defaults.public_entries);
        let mut paths = vec![];
        let mut excluded = vec![];
        let mut manifests = vec![];
        let mut walker = WalkDir::new(&root).sort_by_file_name().into_iter();
        while let Some(entry) = walker.next() {
            let entry = entry.map_err(|e| format!("Cannot enumerate source scope: {e}"))?;
            let file = entry
                .path()
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let name = entry.file_name().to_string_lossy();
            if entry.file_type().is_dir() {
                if entry.depth() > 0
                    && (SKIP.contains(&name.as_ref()) || exclude.iter().any(|p| p.is_match(&file)))
                {
                    excluded.push(format!("{file}/"));
                    walker.skip_current_dir();
                }
                continue;
            }
            if !entry.file_type().is_file() {
                continue;
            }
            if name == "package.json" {
                manifests.push(entry.path().to_owned());
            }
            if !EXTENSIONS.contains(
                &entry
                    .path()
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or(""),
            ) {
                continue;
            }
            if include
                .as_ref()
                .is_none_or(|v| v.iter().any(|p| p.is_match(&file)))
                && !exclude.iter().any(|p| p.is_match(&file))
            {
                paths.push(file);
            } else {
                excluded.push(file);
            }
        }
        paths.sort();
        excluded.sort();
        if paths.is_empty() {
            return Err("No analyzable JS or TS files in the effective scope.".into());
        }
        let tsconfig = root.join("tsconfig.json");
        let options = ResolveOptions {
            extensions: EXTENSIONS.iter().map(|e| format!(".{e}")).collect(),
            extension_alias: vec![
                (".js".into(), strings(&[".js", ".ts", ".tsx"])),
                (".jsx".into(), strings(&[".jsx", ".tsx"])),
                (".mjs".into(), strings(&[".mjs", ".mts"])),
                (".cjs".into(), strings(&[".cjs", ".cts"])),
            ],
            tsconfig: tsconfig.is_file().then_some({
                TsconfigDiscovery::Manual(TsconfigOptions {
                    config_file: tsconfig,
                    references: TsconfigReferences::Auto,
                })
            }),
            builtin_modules: true,
            condition_names: strings(&["node", "import"]),
            main_fields: strings(&["module", "main"]),
            ..Default::default()
        };
        let mut type_options = options.clone();
        type_options
            .extensions
            .extend(strings(&[".d.ts", ".d.mts", ".d.cts"]));
        type_options.extension_alias[0].1.push(".d.ts".into());
        type_options.extension_alias[2].1.push(".d.mts".into());
        type_options.extension_alias[3].1.push(".d.cts".into());
        type_options.condition_names.push("types".into());
        type_options.main_fields.insert(0, "types".into());
        let resolver = Resolver::new(options);
        let type_resolver = resolver.clone_with_options(type_options);

        let root_manifest = root.join("package.json");
        let manifest = if manifests.contains(&root_manifest) {
            json(&root_manifest)?
        } else {
            Value::Null
        };
        let patterns = manifest
            .pointer("/workspaces/packages")
            .or(manifest.get("workspaces"))
            .and_then(Value::as_array);
        let workspace_globs: Vec<_> = patterns
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(matcher)
            .collect::<Result<_, _>>()?;
        let mut packages = vec![(PathBuf::new(), manifest)];
        let mut workspaces = BTreeMap::new();
        for path in manifests.into_iter().filter(|p| p != &root_manifest) {
            let dir = path.parent().unwrap();
            let rel = dir.strip_prefix(&root).unwrap();
            if workspace_globs.iter().any(|g| g.is_match(rel)) {
                let package = json(&path)?;
                if let Some(name) = package["name"].as_str() {
                    workspaces.insert(name.into(), dir.to_owned());
                }
                packages.push((rel.to_owned(), package));
            }
        }
        let mut project = Self {
            root,
            graph: Graph::new(paths),
            excluded,
            workspaces,
            roots: vec![],
            resolver,
            type_resolver,
        };
        for (role, supplied) in
            ROLES
                .into_iter()
                .zip([entries.runtime, entries.test, entries.tooling, public])
        {
            if let Some(patterns) = supplied {
                for p in patterns {
                    project.select(role, &p, "explicit", &p, true)?;
                }
            } else if role == Role::Test {
                for p in [
                    "**/*.{test,spec}.{ts,tsx,js,jsx,mts,mjs,cts,cjs}",
                    "**/__tests__/**",
                ] {
                    project.select(role, p, "discovered", p, false)?;
                }
            } else if role == Role::Tooling {
                let p =
                    "**/{vite,vitest,jest,webpack,rollup,eslint}.config.{ts,js,mjs,cjs,mts,cts}";
                project.select(role, p, "discovered", p, false)?;
            } else {
                for (dir, pkg) in &packages {
                    if role == Role::Runtime
                        && pkg["bin"].is_null()
                        && pkg.pointer("/scripts/start").is_some()
                    {
                        let manifest = dir.join("package.json");
                        project.graph.gap("ambiguous-entry", None, None,
                            format!("{}#scripts.start is a command. Supply entries.runtime to identify its source entry.", manifest.display()), None, None);
                    }
                    if role == Role::Public {
                        let manifest = dir.join("package.json");
                        for key in ["types", "typings"] {
                            if let Some(path) = pkg[key].as_str() {
                                let pattern = dir.join(path.trim_start_matches("./"));
                                project.select(
                                    role,
                                    &pattern.to_string_lossy(),
                                    "discovered",
                                    &format!("{}#{key}", manifest.display()),
                                    true,
                                )?;
                            }
                        }
                    }
                    let field = if role == Role::Runtime {
                        "bin"
                    } else if pkg.get("exports").is_some() {
                        "exports"
                    } else {
                        "main"
                    };
                    let mut targets = vec![];
                    manifest_targets(&pkg[field], field == "bin", &mut targets);
                    if field == "exports" {
                        declared_types(&pkg[field], &mut targets);
                    }
                    let label = format!("{}#{field}", dir.join("package.json").display());
                    let has_targets = !targets.is_empty();
                    for target in targets {
                        let file = dir
                            .join(target.trim_start_matches("./"))
                            .to_string_lossy()
                            .replace('\\', "/");
                        project.select(role, &file, "discovered", &label, true)?;
                    }
                    if role == Role::Public
                        && field == "exports"
                        && !pkg[field].is_null()
                        && !has_targets
                    {
                        project.graph.gap(
                            "ambiguous-entry",
                            None,
                            None,
                            format!("{label} has no supported ESM target. Supply publicEntries."),
                            None,
                            None,
                        );
                    }
                }
            }
        }
        project
            .roots
            .sort_by(|a, b| (&a.file, a.role, &a.input).cmp(&(&b.file, b.role, &b.input)));
        project
            .roots
            .dedup_by(|a, b| a.file == b.file && a.role == b.role && a.input == b.input);
        if project.roots.is_empty() {
            let detail = project
                .graph
                .gaps
                .first()
                .map_or("", |g| g.record.message.as_str());
            return Err(format!(
                "No effective roots. Supply entries.runtime/test/tooling or publicEntries matching included source files. {detail}"
            ));
        }
        Ok(project)
    }
    fn select(
        &mut self,
        role: Role,
        pattern: &str,
        origin: &'static str,
        input: &str,
        required: bool,
    ) -> Result<(), String> {
        let glob = matcher(pattern)?;
        let before = self.roots.len();
        for file in self.graph.paths.iter().filter(|f| glob.is_match(f)) {
            self.roots.push(Root {
                file: file.clone(),
                role,
                origin,
                input: input.into(),
            });
        }
        if required && before == self.roots.len() {
            let message = format!(
                "{role:?} root {pattern:?} selected by {input:?} matched no included file. Supply a source entry."
            );
            self.graph
                .gap("invalid-root", None, None, message, None, None);
        }
        Ok(())
    }

    /// None means a resolved external package or Node builtin.
    pub fn resolve(&self, file: Id, spec: &str, type_only: bool) -> Result<Option<Id>, String> {
        let origin = self.root.join(&self.graph.paths[file]);
        let resolver = if type_only {
            &self.type_resolver
        } else {
            &self.resolver
        };
        let mut result = resolver.resolve(origin.parent().unwrap(), spec);
        if result.is_err() {
            let parts: Vec<_> = spec.split('/').collect();
            let count = if spec.starts_with('@') && parts.len() >= 2 {
                2
            } else {
                1
            };
            if let Some(pkg) = self.workspaces.get(&parts[..count].join("/")) {
                // Resolve as a package self-reference to respect its exports map.
                result = resolver.resolve(pkg, spec);
                if matches!(result, Err(ResolveError::NotFound(_))) {
                    result = resolver.resolve(pkg, &format!("./{}", parts[count..].join("/")));
                }
            }
        }
        match result {
            Ok(resolved) => {
                let path = resolved.path();
                if path.components().any(|p| p.as_os_str() == "node_modules") {
                    return Ok(None);
                }
                let rel = path
                    .strip_prefix(&self.root)
                    .map_err(|_| format!("{spec:?} resolves outside the repository."))?
                    .to_string_lossy()
                    .replace('\\', "/");
                self.graph
                    .paths
                    .binary_search(&rel)
                    .map(Some)
                    .map_err(|_| format!("{spec:?} resolves to {rel}, outside the analyzed files."))
            }
            Err(ResolveError::Builtin { .. }) => Ok(None),
            Err(e) => Err(format!("Cannot resolve {spec:?}: {e}")),
        }
    }
}

fn manifest_targets<'a>(value: &'a Value, bin_map: bool, out: &mut Vec<&'a str>) {
    match value {
        Value::String(s) => out.push(s),
        Value::Object(map) if bin_map || map.keys().any(|k| k.starts_with('.')) => {
            for child in map.values() {
                manifest_targets(child, false, out);
            }
        }
        Value::Object(map) => {
            if let Some((_, target)) = map
                .iter()
                .find(|(k, _)| matches!(k.as_str(), "node" | "import" | "default"))
            {
                manifest_targets(target, false, out);
            }
        }
        Value::Array(a) => {
            if let Some(v) = a.first() {
                manifest_targets(v, false, out);
            }
        }
        _ => {}
    }
}

/// A public package can expose declarations alongside its executable ESM entry.
fn declared_types<'a>(value: &'a Value, out: &mut Vec<&'a str>) {
    if let Value::Object(map) = value {
        if let Some(types) = map.get("types") {
            manifest_targets(types, false, out);
        }
        if map.keys().any(|key| key.starts_with('.')) {
            for child in map.values() {
                declared_types(child, out);
            }
        } else if let Some((_, child)) = map
            .iter()
            .find(|(key, _)| matches!(key.as_str(), "node" | "import" | "default"))
        {
            declared_types(child, out);
        }
    }
}
