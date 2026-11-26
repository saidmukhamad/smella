use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use globset::{GlobBuilder, GlobMatcher};
use ignore::{WalkBuilder, WalkState};
use oxc_resolver::{
    ResolveError, ResolveOptions, Resolver, TsconfigDiscovery, TsconfigOptions, TsconfigReferences,
};
use rustc_hash::FxHashMap;
use serde::Deserialize;
use serde_json::Value;

use crate::deps::{
    WorkspaceManifest, manifest_bins, manifest_dep_set, manifest_path_for, manifest_scripts,
};
use crate::graph::{GapScope, Graph, Id, ROLES, Role, Root, RootEntryMode};
use crate::source_map::{
    SourceMapper, is_non_code_export, normalize_rel, resolve_exports_targets, to_slash,
};

pub(crate) const EXTENSIONS: &[&str] = &["ts", "tsx", "js", "jsx", "mts", "mjs", "cts", "cjs"];
const SKIP: &[&str] = &[
    ".git",
    "node_modules",
    "dist",
    "build",
    "target",
    "coverage",
    ".next",
    ".cache",
    ".nx",
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

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SuppressionRule {
    pub file: String,
    pub symbol: String,
    pub reason: String,
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StaleConfigItem {
    pub kind: &'static str,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<crate::graph::Loc>,
    pub message: String,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct Config {
    pub entries: Entries,
    pub public_entries: Option<Vec<String>>,
    pub include: Option<Vec<String>>,
    pub exclude: Option<Vec<String>>,
    pub dynamic_imports: Option<BTreeMap<String, Vec<String>>>,
    pub suppressions: Option<Vec<SuppressionRule>>,
}

pub(crate) fn matcher(pattern: &str) -> Result<GlobMatcher, String> {
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

struct WorkspacePkgInfo {
    dir: PathBuf,
    rel_dir: PathBuf,
    has_exports: bool,
    source: Option<String>,
    exports: Option<Value>,
    main: Option<String>,
    module: Option<String>,
    types: Option<String>,
}

enum WorkspacePackage {
    Unique(Box<WorkspacePkgInfo>),
    Ambiguous,
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceInfo {
    pub dir: String,
    pub name: Option<String>,
}

type ResolveResult = Result<Option<Id>, String>;

/// Per-worker thread-local cache for module resolution plus a reusable path scratch buffer.
#[derive(Default)]
pub struct ResolveCache {
    scratch: String,
    entries: FxHashMap<(u32, u16, u8), FxHashMap<String, ResolveResult>>,
}

pub struct Project {
    pub root: PathBuf,
    pub roots: Vec<Root>,
    pub graph: Graph,
    pub sources: Vec<Result<String, String>>,
    pub excluded: Vec<String>,
    resolvers: Vec<[Resolver; 2]>,
    file_profile: Vec<usize>,
    pub file_dir_id: Vec<u32>,
    has_custom_file_filter: bool,
    workspaces: BTreeMap<String, WorkspacePackage>,
    pub workspace_for_file: Vec<usize>,
    workspace_dirs: Vec<PathBuf>,
    pub workspace_infos: Vec<WorkspaceInfo>,
    pub manifests: Vec<WorkspaceManifest>,
    pub react_entry: Option<Id>,
    pub source_mapper: SourceMapper,
    pub dynamic_imports: BTreeMap<String, Vec<String>>,
    pub suppressions: Vec<SuppressionRule>,
    pub stale_config: Vec<StaleConfigItem>,
}

impl Project {
    pub fn open(root: &Path, input: Config) -> Result<Self, String> {
        let profile = std::env::var_os("SMELLA_PROFILE").is_some();
        let t0 = std::time::Instant::now();
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
        let include_raw = input.include.or(defaults.include);
        let include = include_raw.as_deref().map(matchers).transpose()?;
        let exclude_raw = input.exclude.or(defaults.exclude).unwrap_or_default();
        let exclude = matchers(&exclude_raw)?;
        let mut include_matched = vec![false; include_raw.as_ref().map_or(0, Vec::len)];
        let mut exclude_matched = vec![false; exclude_raw.len()];
        let entries = input.entries.merge(defaults.entries);
        let public = input.public_entries.or(defaults.public_entries);
        let dynamic_imports = input
            .dynamic_imports
            .or(defaults.dynamic_imports)
            .unwrap_or_default();
        let suppressions = input
            .suppressions
            .or(defaults.suppressions)
            .unwrap_or_default();
        for rule in &suppressions {
            let file = rule.file.trim();
            let symbol = rule.symbol.trim();
            let reason = rule.reason.trim();
            if file.is_empty() || file.contains(['*', '?', '[', '{']) || file.ends_with('/') {
                return Err(format!(
                    "Invalid suppression for {:?}: file must be a literal relative file path, not a glob or directory.",
                    rule.file
                ));
            }
            if symbol.is_empty() || symbol == "*" {
                return Err(format!(
                    "Invalid suppression for {file:?}: symbol is required (file-level ignores are not supported)."
                ));
            }
            if reason.is_empty() {
                return Err(format!(
                    "Invalid suppression for {file:?}#{symbol}: a non-empty reason is required."
                ));
            }
        }
        let mut paths = vec![];
        let mut excluded = vec![];
        let nx_enabled = root
            .join("nx.json")
            .try_exists()
            .map_err(|e| e.to_string())?;
        let mut nx_dirs = BTreeSet::new();
        let mut manifests = vec![];
        let mut config_dirs = BTreeMap::new();
        let mut tsconfig_files = vec![];
        let mut bundler_configs = vec![];
        let in_git_repo = root.ancestors().any(|a| a.join(".git").exists());
        let exclude_filter = exclude.clone();
        let exclude_hits: Arc<Vec<AtomicBool>> =
            Arc::new((0..exclude.len()).map(|_| AtomicBool::new(false)).collect());
        let exclude_hits_cb = Arc::clone(&exclude_hits);
        let excluded_dirs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let excluded_dirs_cb = Arc::clone(&excluded_dirs);
        let root_cb = root.clone();
        // `ignore::WalkParallel` spawns fresh OS threads per call (~1.5 ms overhead),
        // so only engage it when a quick 2-level directory probe finds a large tree.
        let use_parallel_walk = fs::read_dir(&root)
            .map(|rd| {
                let mut dir_count = 0usize;
                for entry in rd.filter_map(Result::ok) {
                    if !entry.file_type().is_ok_and(|ft| ft.is_dir()) {
                        continue;
                    }
                    let name = entry.file_name();
                    if SKIP.contains(&name.to_string_lossy().as_ref()) {
                        continue;
                    }
                    dir_count += 1;
                    if dir_count >= 16 {
                        return true;
                    }
                    if let Ok(sub) = fs::read_dir(entry.path()) {
                        for child in sub.filter_map(Result::ok) {
                            if child.file_type().is_ok_and(|ft| ft.is_dir())
                                && !SKIP.contains(&child.file_name().to_string_lossy().as_ref())
                            {
                                dir_count += 1;
                                if dir_count >= 16 {
                                    return true;
                                }
                            }
                        }
                    }
                }
                false
            })
            .unwrap_or(false);
        let mut builder = WalkBuilder::new(&root);
        builder
            .hidden(false)
            .follow_links(false)
            .git_ignore(true)
            .git_exclude(true)
            .git_global(false)
            .ignore(false)
            .require_git(in_git_repo)
            .parents(in_git_repo)
            .filter_entry(move |entry| {
                if entry.depth() == 0 {
                    return true;
                }
                if entry.file_type().is_some_and(|ft| ft.is_dir()) {
                    let file = entry
                        .path()
                        .strip_prefix(&root_cb)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/");
                    let name = entry.file_name().to_string_lossy();
                    let mut excluded_by_rule = false;
                    for (idx, p) in exclude_filter.iter().enumerate() {
                        if p.is_match(&file) {
                            exclude_hits_cb[idx].store(true, Ordering::Relaxed);
                            excluded_by_rule = true;
                        }
                    }
                    if SKIP.contains(&name.as_ref()) || excluded_by_rule {
                        excluded_dirs_cb.lock().unwrap().push(format!("{file}/"));
                        return false;
                    }
                }
                true
            });
        if use_parallel_walk {
            let walk_threads = std::thread::available_parallelism().map_or(4, |n| n.get().min(8));
            builder.threads(walk_threads);
            #[derive(Default)]
            struct WalkBatch {
                paths: Vec<String>,
                excluded: Vec<String>,
                nx_dirs: Vec<PathBuf>,
                tsconfig_files: Vec<PathBuf>,
                bundler_configs: Vec<PathBuf>,
                config_dirs: Vec<PathBuf>,
                manifests: Vec<PathBuf>,
                include_matched: Vec<bool>,
                exclude_matched: Vec<bool>,
                err: Option<String>,
            }
            let merged = Mutex::new(Vec::<WalkBatch>::new());
            let include_ref = &include;
            let exclude_ref = &exclude;
            let root_ref = &root;
            let inc_len = include_matched.len();
            let exc_len = exclude_matched.len();
            builder.build_parallel().run(|| {
                let local = WalkBatch {
                    include_matched: vec![false; inc_len],
                    exclude_matched: vec![false; exc_len],
                    ..Default::default()
                };
                struct Flush<'a> {
                    local: WalkBatch,
                    merged: &'a Mutex<Vec<WalkBatch>>,
                }
                impl Drop for Flush<'_> {
                    fn drop(&mut self) {
                        self.merged
                            .lock()
                            .unwrap()
                            .push(std::mem::take(&mut self.local));
                    }
                }
                let mut guard = Flush {
                    local,
                    merged: &merged,
                };
                Box::new(move |res| {
                    let entry = match res {
                        Ok(e) => e,
                        Err(e) => {
                            guard.local.err = Some(format!("Cannot enumerate source scope: {e}"));
                            return WalkState::Quit;
                        }
                    };
                    if entry.depth() == 0 || !entry.file_type().is_some_and(|ft| ft.is_file()) {
                        return WalkState::Continue;
                    }
                    let file = entry
                        .path()
                        .strip_prefix(root_ref)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/");
                    let name = entry.file_name().to_string_lossy();
                    if nx_enabled && name == "project.json" {
                        guard
                            .local
                            .nx_dirs
                            .push(Path::new(&file).parent().unwrap().to_owned());
                    }
                    if name.starts_with("tsconfig") && name.ends_with(".json") {
                        guard.local.tsconfig_files.push(PathBuf::from(&file));
                    }
                    if name.starts_with("tsdown.config.")
                        && entry
                            .path()
                            .extension()
                            .and_then(|e| e.to_str())
                            .is_some_and(|e| EXTENSIONS.contains(&e))
                    {
                        guard.local.bundler_configs.push(PathBuf::from(&file));
                    }
                    if name == "tsconfig.json" {
                        guard.local.config_dirs.push(
                            entry
                                .path()
                                .parent()
                                .unwrap()
                                .strip_prefix(root_ref)
                                .unwrap()
                                .to_owned(),
                        );
                    }
                    if name == "package.json" {
                        guard.local.manifests.push(entry.path().to_owned());
                    }
                    if !EXTENSIONS.contains(
                        &entry
                            .path()
                            .extension()
                            .and_then(|e| e.to_str())
                            .unwrap_or(""),
                    ) {
                        return WalkState::Continue;
                    }
                    let mut inc_ok = include_ref.is_none();
                    if let Some(inc) = include_ref.as_ref() {
                        for (idx, p) in inc.iter().enumerate() {
                            if p.is_match(&file) {
                                guard.local.include_matched[idx] = true;
                                inc_ok = true;
                            }
                        }
                    }
                    let mut exc_hit = false;
                    for (idx, p) in exclude_ref.iter().enumerate() {
                        if p.is_match(&file) {
                            guard.local.exclude_matched[idx] = true;
                            exc_hit = true;
                        }
                    }
                    if inc_ok && !exc_hit {
                        guard.local.paths.push(file);
                    } else {
                        guard.local.excluded.push(file);
                    }
                    WalkState::Continue
                })
            });
            for batch in merged.into_inner().unwrap() {
                if let Some(err) = batch.err {
                    return Err(err);
                }
                paths.extend(batch.paths);
                excluded.extend(batch.excluded);
                nx_dirs.extend(batch.nx_dirs);
                tsconfig_files.extend(batch.tsconfig_files);
                bundler_configs.extend(batch.bundler_configs);
                for d in batch.config_dirs {
                    config_dirs.insert(d, 0usize);
                }
                manifests.extend(batch.manifests);
                for (i, h) in batch.include_matched.into_iter().enumerate() {
                    include_matched[i] |= h;
                }
                for (i, h) in batch.exclude_matched.into_iter().enumerate() {
                    exclude_matched[i] |= h;
                }
            }
            tsconfig_files.sort();
            bundler_configs.sort();
        } else {
            builder.sort_by_file_name(|a, b| a.cmp(b));
            for entry in builder.build() {
                let entry = entry.map_err(|e| format!("Cannot enumerate source scope: {e}"))?;
                if entry.depth() == 0 || !entry.file_type().is_some_and(|ft| ft.is_file()) {
                    continue;
                }
                let file = entry
                    .path()
                    .strip_prefix(&root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                let name = entry.file_name().to_string_lossy();
                if nx_enabled && name == "project.json" {
                    nx_dirs.insert(Path::new(&file).parent().unwrap().to_owned());
                }
                if name.starts_with("tsconfig") && name.ends_with(".json") {
                    tsconfig_files.push(PathBuf::from(&file));
                }
                if name.starts_with("tsdown.config.")
                    && entry
                        .path()
                        .extension()
                        .and_then(|e| e.to_str())
                        .is_some_and(|e| EXTENSIONS.contains(&e))
                {
                    bundler_configs.push(PathBuf::from(&file));
                }
                if name == "tsconfig.json" {
                    config_dirs.insert(
                        entry
                            .path()
                            .parent()
                            .unwrap()
                            .strip_prefix(&root)
                            .unwrap()
                            .to_owned(),
                        0usize,
                    );
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
                let mut inc_ok = include.is_none();
                if let Some(inc) = include.as_ref() {
                    for (idx, p) in inc.iter().enumerate() {
                        if p.is_match(&file) {
                            include_matched[idx] = true;
                            inc_ok = true;
                        }
                    }
                }
                let mut exc_hit = false;
                for (idx, p) in exclude.iter().enumerate() {
                    if p.is_match(&file) {
                        exclude_matched[idx] = true;
                        exc_hit = true;
                    }
                }
                if inc_ok && !exc_hit {
                    paths.push(file);
                } else {
                    excluded.push(file);
                }
            }
        }
        for (idx, hit) in exclude_hits.iter().enumerate() {
            if hit.load(Ordering::Relaxed) {
                exclude_matched[idx] = true;
            }
        }
        excluded.extend(std::mem::take(&mut *excluded_dirs.lock().unwrap()));
        excluded.sort();
        let t_walk = std::time::Instant::now();
        // Read and parse every package.json once in parallel.
        manifests.sort_by_key(|p| (p.components().count(), p.clone()));
        let raw_manifests = crate::io::read_bytes_batch(&manifests);
        let mut parsed_manifests: FxHashMap<PathBuf, Result<Value, String>> =
            FxHashMap::with_capacity_and_hasher(manifests.len(), Default::default());
        for (path, bytes) in manifests.iter().zip(raw_manifests) {
            let parsed = bytes
                .map_err(|e| format!("{}: {e}", path.display()))
                .and_then(|b| {
                    serde_json::from_slice::<Value>(&b)
                        .map_err(|e| format!("{}: {e}", path.display()))
                });
            parsed_manifests.insert(path.clone(), parsed);
        }
        let mut non_js_bins = BTreeSet::new();
        for manifest_path in &manifests {
            let Some(Ok(pkg)) = parsed_manifests.get(manifest_path) else {
                continue;
            };
            let Some(dir) = manifest_path
                .parent()
                .and_then(|d| d.strip_prefix(&root).ok())
            else {
                continue;
            };
            let mut bin_targets = Vec::new();
            manifest_targets(&pkg["bin"], true, &mut bin_targets);
            for raw in bin_targets {
                let trimmed = raw.trim().trim_start_matches("./");
                if trimmed.is_empty() {
                    continue;
                }
                let Some(norm) = normalize_rel(&dir.join(trimmed)) else {
                    continue;
                };
                if norm
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| EXTENSIONS.contains(&e))
                {
                    continue;
                }
                let abs = root.join(&norm);
                if !abs.is_file() {
                    continue;
                }
                let file = to_slash(&norm);
                match classify_bin_shebang(&abs) {
                    Some(true) => {
                        if include
                            .as_ref()
                            .is_none_or(|v| v.iter().any(|p| p.is_match(&file)))
                            && !exclude.iter().any(|p| p.is_match(&file))
                        {
                            paths.push(file);
                        }
                    }
                    Some(false) => {
                        non_js_bins.insert(file);
                    }
                    None => {}
                }
            }
        }
        paths.sort();
        paths.dedup();
        excluded.sort();
        if paths.is_empty() {
            return Err("No analyzable JS or TS files in the effective scope.".into());
        }
        let options = ResolveOptions {
            extensions: EXTENSIONS.iter().map(|e| format!(".{e}")).collect(),
            extension_alias: vec![
                (".js".into(), strings(&[".js", ".ts", ".tsx"])),
                (".jsx".into(), strings(&[".jsx", ".tsx"])),
                (".mjs".into(), strings(&[".mjs", ".mts"])),
                (".cjs".into(), strings(&[".cjs", ".cts"])),
            ],
            builtin_modules: true,
            condition_names: strings(&["node", "import", "default"]),
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
        let base = Resolver::new(options.clone());
        let mut resolvers = vec![[
            base.clone_with_options(options.clone()),
            base.clone_with_options(type_options.clone()),
        ]];
        let base_tsconfig = nx_enabled
            && !config_dirs.contains_key(Path::new(""))
            && root
                .join("tsconfig.base.json")
                .try_exists()
                .map_err(|e| e.to_string())?;
        if base_tsconfig {
            config_dirs.insert(PathBuf::new(), 0);
        }
        for (dir, index) in &mut config_dirs {
            let tsconfig = Some(TsconfigDiscovery::Manual(TsconfigOptions {
                config_file: root
                    .join(dir)
                    .join(if base_tsconfig && dir.as_os_str().is_empty() {
                        "tsconfig.base.json"
                    } else {
                        "tsconfig.json"
                    }),
                references: TsconfigReferences::Auto,
            }));
            *index = resolvers.len();
            resolvers.push([
                base.clone_with_options(ResolveOptions {
                    tsconfig: tsconfig.clone(),
                    ..options.clone()
                }),
                base.clone_with_options(ResolveOptions {
                    tsconfig,
                    ..type_options.clone()
                }),
            ]);
        }
        let file_profile = closest_directories(&paths, &config_dirs);

        let root_manifest = root.join("package.json");
        let manifest = match parsed_manifests.remove(&root_manifest) {
            Some(Ok(v)) => v,
            Some(Err(e)) => return Err(e),
            None => Value::Null,
        };
        // A workspace member can in turn declare workspaces relative to itself.
        // Register parents first and let the closest member own a source file.
        let pnpm = root.join("pnpm-workspace.yaml");
        let uses_pnpm = pnpm.try_exists().map_err(|e| e.to_string())?;
        let root_patterns = if uses_pnpm {
            #[derive(Deserialize)]
            struct PnpmWorkspace {
                #[serde(default)]
                packages: Vec<String>,
            }
            let bytes = fs::read(&pnpm).map_err(|e| format!("{}: {e}", pnpm.display()))?;
            let config: PnpmWorkspace = serde_yaml_ng::from_slice(&bytes)
                .map_err(|e| format!("{}: {e}", pnpm.display()))?;
            WorkspacePatterns::from_patterns(config.packages.iter().map(String::as_str))?
        } else {
            workspace_patterns(&manifest)?
        };
        let mut patterns = BTreeMap::from([(PathBuf::new(), root_patterns)]);
        let mut packages = vec![(PathBuf::new(), manifest)];
        let mut directories = BTreeMap::from([(PathBuf::new(), 0)]);
        let mut workspaces = BTreeMap::new();
        for path in manifests.into_iter().filter(|p| p != &root_manifest) {
            let dir = path.parent().unwrap();
            let rel = dir.strip_prefix(&root).unwrap();
            let member = rel.ancestors().skip(1).any(|a| {
                patterns
                    .get(a)
                    .is_some_and(|globs| globs.matches(rel.strip_prefix(a).unwrap()))
            });
            if member || nx_dirs.contains(rel) {
                let package = match parsed_manifests.remove(&path) {
                    Some(res) => res?,
                    None => json(&path)?,
                };
                if let Some(name) = package["name"].as_str() {
                    let non_empty = |k: &str| {
                        package[k]
                            .as_str()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned)
                    };
                    workspaces
                        .entry(name.to_owned())
                        .and_modify(|pkg| *pkg = WorkspacePackage::Ambiguous)
                        .or_insert_with(|| {
                            WorkspacePackage::Unique(Box::new(WorkspacePkgInfo {
                                dir: dir.to_owned(),
                                rel_dir: rel.to_owned(),
                                has_exports: package.get("exports").is_some(),
                                source: non_empty("source"),
                                exports: package.get("exports").cloned(),
                                main: non_empty("main"),
                                module: non_empty("module"),
                                types: non_empty("types").or_else(|| non_empty("typings")),
                            }))
                        });
                }
                if !uses_pnpm {
                    patterns.insert(rel.to_owned(), workspace_patterns(&package)?);
                }
                directories.insert(rel.to_owned(), packages.len());
                packages.push((rel.to_owned(), package));
            }
        }
        let source_mapper = SourceMapper::build(&root, &tsconfig_files, &bundler_configs);
        let nx = if nx_enabled {
            let nx = crate::nx::Workspace::read(&root, &nx_dirs, &packages)?;
            for dir in nx.directories() {
                let id = directories.len();
                directories.entry(dir.clone()).or_insert(id);
            }
            Some(nx)
        } else {
            None
        };
        let react_entry = match workspaces.get("react") {
            Some(WorkspacePackage::Unique(pkg)) => {
                let spec = if pkg.has_exports { "react" } else { "./" };
                resolvers[0][0]
                    .resolve(&pkg.dir, spec)
                    .ok()
                    .and_then(|entry| {
                        let file = entry
                            .path()
                            .strip_prefix(&root)
                            .ok()?
                            .to_str()?
                            .replace('\\', "/");
                        paths.binary_search(&file).ok()
                    })
            }
            _ => None,
        };
        let workspace_for_file = closest_directories(&paths, &directories);
        let mut workspace_dirs = vec![PathBuf::new(); directories.len()];
        for (dir, &id) in &directories {
            if id < workspace_dirs.len() {
                workspace_dirs[id] = dir.clone();
            }
        }
        let mut pkg_names_by_dir: BTreeMap<PathBuf, Option<String>> = BTreeMap::new();
        for (dir, pkg) in &packages {
            pkg_names_by_dir.insert(dir.clone(), pkg["name"].as_str().map(str::to_owned));
        }
        let workspace_infos: Vec<WorkspaceInfo> = workspace_dirs
            .iter()
            .map(|dir| {
                let s = to_slash(dir);
                WorkspaceInfo {
                    dir: if s.is_empty() { ".".into() } else { s },
                    name: pkg_names_by_dir.get(dir).cloned().flatten(),
                }
            })
            .collect();
        let mut manifests_data = Vec::new();
        for (dir, pkg) in &packages {
            if !pkg.is_object() {
                continue;
            }
            let ws_id = directories.get(dir).copied().unwrap_or(0);
            let s = to_slash(dir);
            let ws_dir = if s.is_empty() { ".".into() } else { s };
            manifests_data.push(WorkspaceManifest {
                ws_id,
                ws_dir,
                rel_dir: dir.clone(),
                manifest_path: manifest_path_for(dir),
                name: pkg["name"].as_str().map(str::to_owned),
                dependencies: manifest_dep_set(pkg, "dependencies"),
                dev_dependencies: manifest_dep_set(pkg, "devDependencies"),
                peer_dependencies: manifest_dep_set(pkg, "peerDependencies"),
                optional_dependencies: manifest_dep_set(pkg, "optionalDependencies"),
                scripts: manifest_scripts(pkg),
                bins: manifest_bins(pkg),
                config_deps: BTreeSet::new(),
            });
        }
        let mut stale_config = Vec::new();
        if let Some(inc) = &include_raw {
            for (pat, matched) in inc.iter().zip(include_matched) {
                if !matched {
                    stale_config.push(StaleConfigItem {
                        kind: "stale-include",
                        source: "config#include".into(),
                        pattern: Some(pat.clone()),
                        file: None,
                        symbol: None,
                        location: None,
                        message: format!("Include pattern {pat:?} matched no files."),
                    });
                }
            }
        }
        for (pat, matched) in exclude_raw.iter().zip(exclude_matched) {
            if !matched {
                stale_config.push(StaleConfigItem {
                    kind: "stale-exclude",
                    source: "config#exclude".into(),
                    pattern: Some(pat.clone()),
                    file: None,
                    symbol: None,
                    location: None,
                    message: format!("Exclude pattern {pat:?} matched no files or directories."),
                });
            }
        }
        let t_pre_sources = std::time::Instant::now();
        // On Linux, bulk-read all source files up front via batched io_uring rings.
        // On other platforms, workers stream each file into a thread-local reusable buffer
        // right before lexing to keep file bytes L1-cache-resident and overlap I/O with parsing.
        #[cfg(target_os = "linux")]
        let sources = crate::io::read_files(&root, &paths);
        #[cfg(not(target_os = "linux"))]
        let sources = Vec::new();
        let t_sources = std::time::Instant::now();
        let has_custom_file_filter = include_raw.is_some() || !exclude_raw.is_empty();
        let file_dir_id = compute_dir_ids(&paths);
        let mut project = Self {
            root,
            graph: Graph::new(paths),
            sources,
            excluded,
            workspaces,
            roots: vec![],
            resolvers,
            file_profile,
            file_dir_id,
            has_custom_file_filter,
            workspace_for_file,
            workspace_dirs,
            workspace_infos,
            manifests: manifests_data,
            react_entry,
            source_mapper,
            dynamic_imports,
            suppressions,
            stale_config,
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
                crate::discover::discover_jest_config_entries(&mut project);
            } else if role == Role::Tooling {
                if let Some(nx) = &nx {
                    nx.discover(&mut project, role)?;
                }
                let p =
                    "**/{vite,vitest,jest,webpack,rollup,eslint}.config.{ts,js,mjs,cjs,mts,cts}";
                project.select_with_mode(
                    role,
                    p,
                    "discovered",
                    p,
                    false,
                    RootEntryMode::AllExports,
                )?;
                crate::discover::discover_extra_tooling_configs(&mut project);
                crate::discover::discover_tooling_scripts(&mut project, &packages);
                crate::discover::discover_storybook_stories(&mut project)?;
            } else {
                if role == Role::Runtime {
                    crate::discover::discover_bundler_entries(&mut project);
                    crate::discover::discover_public_static_files(&mut project);
                    crate::discover::discover_ambient_declarations(&mut project);
                }
                for (dir, pkg) in &packages {
                    if role == Role::Runtime {
                        let manifest = dir.join("package.json");
                        for (field_label, entry_file) in collect_plugin_entries(pkg) {
                            let file = to_slash(&dir.join(entry_file.trim_start_matches("./")));
                            let label = format!("{}#{field_label}", manifest.display());
                            project.select_with_mode(
                                role,
                                &file,
                                "discovered",
                                &label,
                                true,
                                RootEntryMode::DefaultExport,
                            )?;
                            let html_rel = Path::new(&file).with_extension("html");
                            for script_file in
                                html_script_sources(&project.root, &html_rel, &project.graph.paths)
                            {
                                project.roots.push(Root {
                                    file: script_file,
                                    role,
                                    origin: "discovered",
                                    input: format!("{}#<script>", to_slash(&html_rel)),
                                    mode: RootEntryMode::ModuleOnly,
                                });
                            }
                        }
                    }
                    if role == Role::Runtime
                        && pkg["bin"].is_null()
                        && let Some(script) = pkg.pointer("/scripts/start").and_then(Value::as_str)
                    {
                        let manifest = dir.join("package.json");
                        let label = format!("{}#scripts.start", manifest.display());
                        match crate::discover::parse_start_script(
                            &project, dir, script, &packages, 0,
                        ) {
                            crate::discover::ScriptOutcome::Entries(found) => {
                                for file in found {
                                    project.roots.push(Root {
                                        file,
                                        role,
                                        origin: "discovered",
                                        input: label.clone(),
                                        mode: RootEntryMode::ModuleOnly,
                                    });
                                }
                            }
                            crate::discover::ScriptOutcome::DelegatedOrNoop => {}
                            crate::discover::ScriptOutcome::Ambiguous => {
                                let scope = project.scope_for_discovered_dir(dir);
                                project.graph.gap_scoped(
                                    "ambiguous-entry",
                                    None,
                                    None,
                                    format!(
                                        "{label} is a command. Supply entries.runtime to identify its source entry."
                                    ),
                                    None,
                                    None,
                                    scope,
                                );
                            }
                        }
                    }
                    if role == Role::Public {
                        let manifest = dir.join("package.json");
                        if let Some(path) = pkg["source"]
                            .as_str()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                        {
                            let pattern = to_slash(&dir.join(path.trim_start_matches("./")));
                            project.select(
                                role,
                                &pattern,
                                "discovered",
                                &format!("{}#source", manifest.display()),
                                true,
                            )?;
                        }
                        for key in ["types", "typings"] {
                            if let Some(path) =
                                pkg[key].as_str().map(str::trim).filter(|s| !s.is_empty())
                            {
                                let pattern = to_slash(&dir.join(path.trim_start_matches("./")));
                                project.select(
                                    role,
                                    &pattern,
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
                        let trimmed = target.trim();
                        if trimmed.is_empty()
                            || (role == Role::Public && is_non_code_export(trimmed))
                        {
                            continue;
                        }
                        let file = to_slash(&dir.join(trimmed.trim_start_matches("./")));
                        if role == Role::Runtime && field == "bin" && non_js_bins.contains(&file) {
                            continue;
                        }
                        if role == Role::Public && trimmed.contains('*') {
                            // Non-code asset wildcard directories (e.g. `./skills/*`) have no JS/TS files.
                            let prefix = file.split('*').next().unwrap_or("");
                            let has_code_ext = file
                                .rsplit('.')
                                .next()
                                .is_some_and(|e| EXTENSIONS.contains(&e));
                            if !has_code_ext
                                && !prefix.is_empty()
                                && project.root.join(prefix).is_dir()
                                && !project.graph.paths.iter().any(|p| p.starts_with(prefix))
                                && project
                                    .source_mapper
                                    .map_pattern(&project.root, &project.graph.paths, &file)
                                    .is_none()
                            {
                                continue;
                            }
                        }
                        project.select(role, &file, "discovered", &label, true)?;
                    }
                    if role == Role::Public
                        && field == "exports"
                        && !pkg[field].is_null()
                        && !has_targets
                    {
                        let scope = project.scope_for_discovered_dir(dir);
                        project.graph.gap_scoped(
                            "ambiguous-entry",
                            None,
                            None,
                            format!("{label} has no supported ESM target. Supply publicEntries."),
                            None,
                            None,
                            scope,
                        );
                    }
                }
                if role == Role::Runtime
                    && let Some(nx) = &nx
                {
                    nx.discover(&mut project, role)?;
                }
            }
        }
        project.roots.sort_by(|a, b| {
            (&a.file, a.role, &a.input, std::cmp::Reverse(a.mode)).cmp(&(
                &b.file,
                b.role,
                &b.input,
                std::cmp::Reverse(b.mode),
            ))
        });
        project
            .roots
            .dedup_by(|a, b| a.file == b.file && a.role == b.role && a.input == b.input);
        if profile {
            let t_end = std::time::Instant::now();
            let ms = |a: std::time::Instant, b: std::time::Instant| {
                b.duration_since(a).as_secs_f64() * 1000.0
            };
            eprintln!(
                "[profile:open] total={:.2}ms | walk={:.2}ms meta+nx={:.2}ms read_files={:.2}ms discover={:.2}ms",
                ms(t0, t_end),
                ms(t0, t_walk),
                ms(t_walk, t_pre_sources),
                ms(t_pre_sources, t_sources),
                ms(t_sources, t_end),
            );
        }
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
    pub(crate) fn select(
        &mut self,
        role: Role,
        pattern: &str,
        origin: &'static str,
        input: &str,
        required: bool,
    ) -> Result<(), String> {
        self.select_with_mode(
            role,
            pattern,
            origin,
            input,
            required,
            RootEntryMode::ModuleOnly,
        )
    }

    pub(crate) fn select_with_mode(
        &mut self,
        role: Role,
        pattern: &str,
        origin: &'static str,
        input: &str,
        required: bool,
        mode: RootEntryMode,
    ) -> Result<(), String> {
        // For discovered manifest entries pointing at build outputs (e.g. dist/ or lib/),
        // prefer the mapped source entry over generated output.
        let effective = if origin == "discovered" {
            let clean = pattern.trim().trim_start_matches("./");
            let in_out = normalize_rel(Path::new(clean))
                .is_some_and(|p| self.source_mapper.is_in_out_dir(&p));
            if in_out {
                self.source_mapper
                    .map_pattern(&self.root, &self.graph.paths, pattern)
            } else {
                None
            }
        } else {
            None
        };
        let active_pattern = effective.as_deref().unwrap_or(pattern);
        let before = self.roots.len();
        self.push_matching_roots(active_pattern, role, origin, input, mode)?;
        if before == self.roots.len()
            && origin == "discovered"
            && effective.is_none()
            && let Some(mapped) =
                self.source_mapper
                    .map_pattern(&self.root, &self.graph.paths, pattern)
        {
            self.push_matching_roots(&mapped, role, origin, input, mode)?;
        }
        if required && before == self.roots.len() {
            let message = format!(
                "{role:?} root {pattern:?} selected by {input:?} matched no included file. Supply a source entry."
            );
            let scope = if origin == "discovered" {
                let manifest_dir = input
                    .split('#')
                    .next()
                    .and_then(|p| Path::new(p).parent())
                    .unwrap_or_else(|| Path::new(""));
                self.scope_for_discovered_dir(manifest_dir)
            } else {
                GapScope::Global
            };
            if origin == "explicit" {
                self.stale_config.push(StaleConfigItem {
                    kind: "stale-entry",
                    source: format!("entries.{role:?}").to_lowercase(),
                    pattern: Some(pattern.to_owned()),
                    file: None,
                    symbol: None,
                    location: None,
                    message: format!("Entry pattern {pattern:?} matched no included file."),
                });
            }
            self.graph
                .gap_scoped("invalid-root", None, None, message, None, None, scope);
        }
        Ok(())
    }

    fn split_package_spec(spec: &str) -> Option<(String, Option<&str>)> {
        if spec.starts_with('.') || spec.starts_with('/') || spec.is_empty() {
            return None;
        }
        if let Some(rest) = spec.strip_prefix('@') {
            let mut it = rest.splitn(3, '/');
            let scope = it.next()?;
            let name = it.next()?;
            let sub = it.next().filter(|s| !s.is_empty());
            Some((format!("@{scope}/{name}"), sub))
        } else {
            let mut it = spec.splitn(2, '/');
            let name = it.next()?;
            let sub = it.next().filter(|s| !s.is_empty());
            Some((name.to_owned(), sub))
        }
    }

    fn resolve_workspace_source(
        &self,
        spec: &str,
        type_only: bool,
        cjs: bool,
    ) -> Option<Result<Option<Id>, String>> {
        let (pkg_name, subpath) = Self::split_package_spec(spec)?;
        let package = self.workspaces.get(&pkg_name)?;
        let WorkspacePackage::Unique(pkg) = package else {
            return Some(Err(format!(
                "Multiple workspace packages match {spec:?}. Supply an unambiguous path mapping."
            )));
        };
        let WorkspacePkgInfo {
            rel_dir,
            source,
            exports,
            main,
            module,
            types,
            ..
        } = pkg.as_ref();
        if let Some(exp) = exports {
            let sub = match subpath {
                None => ".".to_owned(),
                Some(s) => format!("./{s}"),
            };
            let targets = resolve_exports_targets(exp, &sub, type_only, cjs);
            for target in &targets {
                if is_non_code_export(target) {
                    return Some(Ok(None));
                }
                let rel = rel_dir.join(target.trim_start_matches("./"));
                if let Some(id) = self.source_mapper.map_to_source_mode(
                    &self.root,
                    &self.graph.paths,
                    &rel,
                    type_only,
                ) {
                    return Some(Ok(Some(id)));
                }
            }
            // Only fall back when the subpath wasn't in `exports` and explicitly points into a configured outDir.
            if targets.is_empty()
                && let Some(s) = subpath
            {
                if is_non_code_export(s) {
                    return Some(Ok(None));
                }
                let rel = rel_dir.join(s);
                if self.source_mapper.is_in_out_dir(&rel)
                    && let Some(id) = self.source_mapper.map_to_source_mode(
                        &self.root,
                        &self.graph.paths,
                        &rel,
                        type_only,
                    )
                {
                    return Some(Ok(Some(id)));
                }
            }
            return None;
        }
        match subpath {
            None => {
                let mut candidates: Vec<&str> = Vec::new();
                if let Some(s) = source.as_deref() {
                    candidates.push(s);
                }
                if type_only {
                    for f in [types, module, main] {
                        if let Some(s) = f.as_deref() {
                            candidates.push(s);
                        }
                    }
                } else {
                    for f in [module, main, types] {
                        if let Some(s) = f.as_deref() {
                            candidates.push(s);
                        }
                    }
                }
                candidates.push("./index");
                for target in candidates {
                    let rel = rel_dir.join(target.trim_start_matches("./"));
                    if let Some(id) = self.source_mapper.map_to_source_mode(
                        &self.root,
                        &self.graph.paths,
                        &rel,
                        type_only,
                    ) {
                        return Some(Ok(Some(id)));
                    }
                }
                None
            }
            Some(s) => {
                if is_non_code_export(s) {
                    return Some(Ok(None));
                }
                let rel = rel_dir.join(s);
                self.source_mapper
                    .map_to_source_mode(&self.root, &self.graph.paths, &rel, type_only)
                    .map(|id| Ok(Some(id)))
            }
        }
    }

    pub fn workspace_dir_for(&self, file: Id) -> &Path {
        let ws_id = self.workspace_for_file.get(file).copied().unwrap_or(0);
        self.workspace_dirs
            .get(ws_id)
            .map(PathBuf::as_path)
            .unwrap_or_else(|| Path::new(""))
    }

    fn push_matching_roots(
        &mut self,
        pattern: &str,
        role: Role,
        origin: &'static str,
        input: &str,
        mode: RootEntryMode,
    ) -> Result<(), String> {
        let clean = pattern.trim_start_matches("./");
        if !clean.contains(['*', '?', '[', '{']) {
            if let Ok(idx) = self.graph.paths.binary_search_by(|p| p.as_str().cmp(clean)) {
                self.roots.push(Root {
                    file: self.graph.paths[idx].clone(),
                    role,
                    origin,
                    input: input.into(),
                    mode,
                });
            }
            return Ok(());
        }
        let glob = matcher(pattern)?;
        for file in self.graph.paths.iter().filter(|f| glob.is_match(f)) {
            self.roots.push(Root {
                file: file.clone(),
                role,
                origin,
                input: input.into(),
                mode,
            });
        }
        Ok(())
    }

    pub fn source_text(&self, file: Id) -> Option<Result<&str, &str>> {
        self.sources
            .get(file)
            .map(|r| r.as_deref().map_err(String::as_str))
    }

    pub fn is_multi_workspace(&self) -> bool {
        self.workspace_dirs.len() > 1
    }

    pub fn files_in_dir_range(&self, dir: &Path) -> (u32, u32) {
        let prefix = to_slash(dir);
        if prefix.is_empty() {
            return (0, self.graph.paths.len() as u32);
        }
        let slash = format!("{prefix}/");
        let start = self
            .graph
            .paths
            .partition_point(|p| p.as_str() < slash.as_str());
        let len = self.graph.paths[start..]
            .iter()
            .take_while(|p| p.starts_with(&slash))
            .count();
        (start as u32, (start + len) as u32)
    }

    pub fn scope_for_discovered_dir(&self, dir: &Path) -> GapScope {
        if dir.as_os_str().is_empty() || !self.is_multi_workspace() {
            GapScope::Global
        } else {
            let (s, e) = self.files_in_dir_range(dir);
            GapScope::Range(s, e)
        }
    }

    pub fn scope_unresolved_import(&self, file: Id, spec: &str) -> GapScope {
        let importer = &self.graph.paths[file];
        let clean_spec = spec.split('?').next().unwrap_or(spec);
        if is_non_code_export(clean_spec) {
            return GapScope::Files(vec![file as u32]);
        }
        if spec.contains('?')
            && let Ok(resolved) = self.resolve_with_mode(file, clean_spec, false, false)
        {
            let mut affected = vec![file as u32];
            if let Some(id) = resolved
                && id != file
            {
                affected.push(id as u32);
                affected.sort_unstable();
            }
            return GapScope::Files(affected);
        }
        if !self.is_multi_workspace() {
            if !clean_spec.starts_with('.')
                && !clean_spec.starts_with('/')
                && Self::split_package_spec(clean_spec)
                    .is_none_or(|(pkg, _)| !self.workspaces.contains_key(&pkg))
                && self
                    .source_mapper
                    .matches_tsconfig_path(Path::new(importer), clean_spec)
                    .is_empty()
            {
                return GapScope::Files(vec![file as u32]);
            }
            return GapScope::Global;
        }
        if clean_spec.starts_with('.') || clean_spec.starts_with('/') {
            let file_dir = Path::new(importer)
                .parent()
                .unwrap_or_else(|| Path::new(""));
            if let Some(rel) = normalize_rel(&file_dir.join(clean_spec)) {
                let target_ws_dir = self
                    .workspace_dirs
                    .iter()
                    .filter(|d| !d.as_os_str().is_empty() && rel.starts_with(d))
                    .max_by_key(|d| d.components().count());
                if self.source_mapper.is_in_out_dir(&rel)
                    || target_ws_dir.is_some_and(|d| d != self.workspace_dir_for(file))
                {
                    if let Some(ws_dir) = target_ws_dir {
                        return GapScope::from_range_and_file(
                            self.files_in_dir_range(ws_dir),
                            file,
                        );
                    }
                } else if let Some(parent) = rel.parent()
                    && !parent.as_os_str().is_empty()
                {
                    return GapScope::from_range_and_file(self.files_in_dir_range(parent), file);
                }
            }
            return GapScope::Files(vec![file as u32]);
        }
        if let Some((pkg_name, _)) = Self::split_package_spec(clean_spec)
            && let Some(WorkspacePackage::Unique(pkg)) = self.workspaces.get(&pkg_name)
        {
            return GapScope::from_range_and_file(self.files_in_dir_range(&pkg.rel_dir), file);
        }
        let ts_targets = self
            .source_mapper
            .matches_tsconfig_path(Path::new(importer), clean_spec);
        if !ts_targets.is_empty() {
            let mut ranges: Vec<(u32, u32)> = Vec::new();
            for t in &ts_targets {
                if let Some(parent) = t.parent()
                    && !parent.as_os_str().is_empty()
                {
                    let r = self.files_in_dir_range(parent);
                    if r.0 < r.1 {
                        ranges.push(r);
                    }
                }
            }
            if ranges.len() == 1 {
                return GapScope::from_range_and_file(ranges[0], file);
            }
            if !ranges.is_empty() {
                let mut ids = vec![file as u32];
                for (s, e) in ranges {
                    ids.extend(s..e);
                }
                ids.sort_unstable();
                ids.dedup();
                return GapScope::Files(ids);
            }
        }
        GapScope::Files(vec![file as u32])
    }

    /// Fast path for relative imports (`./`, `../`) directly against sorted `graph.paths`
    /// using a reusable scratch buffer (zero syscalls, zero locks, zero heap allocations).
    fn resolve_relative_fast(
        &self,
        file: Id,
        spec: &str,
        type_only: bool,
        scratch: &mut String,
    ) -> Option<Result<Option<Id>, String>> {
        if !(spec.starts_with("./") || spec.starts_with("../")) || spec.contains(['?', '#']) {
            return None;
        }
        let dir = dir_of(&self.graph.paths[file]);
        if !normalize_relative_into(dir, spec, scratch) {
            return None;
        }
        if self.source_mapper.has_out_dirs()
            && self
                .source_mapper
                .is_in_out_dir(Path::new(scratch.as_str()))
        {
            return None;
        }
        let paths = &self.graph.paths;
        if let Some(ext) = code_ext_of(scratch) {
            if type_only && matches!(ext, "js" | "mjs" | "cjs") {
                let base_len = scratch.len() - ext.len() - 1;
                for dts in [".d.ts", ".d.mts", ".d.cts"] {
                    scratch.truncate(base_len);
                    scratch.push_str(dts);
                    if let Ok(id) = paths.binary_search(scratch) {
                        return Some(Ok(Some(id)));
                    }
                }
                scratch.truncate(base_len);
                scratch.push('.');
                scratch.push_str(ext);
            }
            if let Ok(id) = paths.binary_search(scratch) {
                return Some(Ok(Some(id)));
            }
            if !self.has_custom_file_filter {
                let aliases: &[&str] = match (ext, type_only) {
                    ("js", false) => &[".ts", ".tsx"],
                    ("js", true) => &[".ts", ".tsx", ".d.ts"],
                    ("jsx", _) => &[".tsx"],
                    ("mjs", false) => &[".mts"],
                    ("mjs", true) => &[".mts", ".d.mts"],
                    ("cjs", false) => &[".cts"],
                    ("cjs", true) => &[".cts", ".d.cts"],
                    _ => &[],
                };
                if !aliases.is_empty() {
                    let base_len = scratch.len() - ext.len() - 1;
                    for alias in aliases {
                        scratch.truncate(base_len);
                        scratch.push_str(alias);
                        if let Ok(id) = paths.binary_search(scratch) {
                            return Some(Ok(Some(id)));
                        }
                    }
                }
            }
            return None;
        }
        let last_seg = scratch.rsplit('/').next().unwrap_or("");
        if !last_seg.is_empty() && !last_seg.contains('.') {
            let base_len = scratch.len();
            scratch.push_str(".ts");
            if let Ok(id) = paths.binary_search(scratch) {
                return Some(Ok(Some(id)));
            }
            if !self.has_custom_file_filter {
                let exts: &[&str] = if type_only {
                    &[
                        ".tsx", ".js", ".jsx", ".mts", ".mjs", ".cts", ".cjs", ".d.ts", ".d.mts",
                        ".d.cts",
                    ]
                } else {
                    &[".tsx", ".js", ".jsx", ".mts", ".mjs", ".cts", ".cjs"]
                };
                for ext in exts {
                    scratch.truncate(base_len);
                    scratch.push_str(ext);
                    if let Ok(id) = paths.binary_search(scratch) {
                        if type_only && matches!(*ext, ".js" | ".mjs" | ".cjs") {
                            for dts in [".d.ts", ".d.mts", ".d.cts"] {
                                scratch.truncate(base_len);
                                scratch.push_str(dts);
                                if let Ok(dts_id) = paths.binary_search(scratch) {
                                    return Some(Ok(Some(dts_id)));
                                }
                            }
                        }
                        return Some(Ok(Some(id)));
                    }
                }
            }
        }
        None
    }

    /// Cached resolution wrapper for per-worker parallel extraction.
    pub fn resolve_with_cache(
        &self,
        file: Id,
        spec: &str,
        type_only: bool,
        cjs: bool,
        cache: &mut ResolveCache,
    ) -> Result<Option<Id>, String> {
        if spec.starts_with("bun:")
            || spec.ends_with(".node")
            || spec.strip_prefix("node:").is_some_and(is_node_builtin)
        {
            return Ok(None);
        }
        if let Some(hit) = self.resolve_relative_fast(file, spec, type_only, &mut cache.scratch) {
            return hit;
        }
        let is_rel = spec.starts_with('.') || spec.starts_with('/');
        let profile = if is_rel { 0 } else { self.file_profile[file] };
        let key = (
            self.file_dir_id[file],
            profile as u16,
            (u8::from(type_only) << 1) | u8::from(cjs),
        );
        // Guard against tsconfig baseUrl self-shadowing (e.g. `knip.ts` importing `"knip"`).
        let may_self_shadow = !is_rel
            && profile != 0
            && Path::new(&self.graph.paths[file])
                .file_stem()
                .and_then(|s| s.to_str())
                == Some(spec);
        if !may_self_shadow
            && let Some(dir_cache) = cache.entries.get(&key)
            && let Some(hit) = dir_cache.get(spec)
        {
            return hit.clone();
        }
        let res = self.resolve_with_mode(file, spec, type_only, cjs);
        if !may_self_shadow && res.as_ref().ok() != Some(&Some(file)) {
            cache
                .entries
                .entry(key)
                .or_default()
                .insert(spec.to_owned(), res.clone());
        }
        res
    }

    /// None means a resolved external package, Node builtin or plain stylesheet.
    pub fn resolve_with_mode(
        &self,
        file: Id,
        spec: &str,
        type_only: bool,
        cjs: bool,
    ) -> Result<Option<Id>, String> {
        if spec.starts_with("bun:")
            || spec.ends_with(".node")
            || spec.strip_prefix("node:").is_some_and(is_node_builtin)
        {
            return Ok(None);
        }
        let origin = self.root.join(&self.graph.paths[file]);
        let origin_dir = origin.parent().unwrap();
        let profile = if spec.starts_with('.') || spec.starts_with('/') {
            0
        } else {
            self.file_profile[file]
        };
        let resolver = &self.resolvers[profile][usize::from(type_only)];
        let mut result = resolver.resolve(origin_dir, spec);
        // Avoid tsconfig `baseUrl` shadowing an installed package with a same-named local file (e.g. `knip.ts` importing `"knip"`).
        if profile != 0
            && !spec.starts_with('.')
            && !spec.starts_with('/')
            && let Ok(ref resolved) = result
            && resolved.path() == origin
            && let Ok(pkg_res) = self.resolvers[0][usize::from(type_only)].resolve(origin_dir, spec)
            && pkg_res
                .path()
                .components()
                .any(|c| c.as_os_str() == "node_modules")
        {
            return Ok(None);
        }
        if result.is_err() {
            if let Some(ws) = self.resolve_workspace_source(spec, type_only, cjs) {
                return ws;
            }
            if !spec.starts_with('.')
                && !spec.starts_with('/')
                && let Some(id) = self.source_mapper.resolve_tsconfig_path(
                    Path::new(&self.graph.paths[file]),
                    spec,
                    &self.root,
                    &self.graph.paths,
                    type_only,
                )
            {
                return Ok(Some(id));
            }
            if spec.starts_with('.') {
                let rel_origin = Path::new(&self.graph.paths[file]);
                if let Some(rel) = normalize_rel(&rel_origin.parent().unwrap().join(spec))
                    && self.source_mapper.is_in_out_dir(&rel)
                    && let Some(id) = self.source_mapper.map_to_source_mode(
                        &self.root,
                        &self.graph.paths,
                        &rel,
                        type_only,
                    )
                    && id != file
                {
                    return Ok(Some(id));
                }
            }
            let parts: Vec<_> = spec.split('/').collect();
            let count = if spec.starts_with('@') && parts.len() >= 2 {
                2
            } else {
                1
            };
            if let Some(package) = self.workspaces.get(&parts[..count].join("/")) {
                let WorkspacePackage::Unique(pkg_info) = package else {
                    return Err(format!(
                        "Multiple workspace packages match {spec:?}. Supply an unambiguous path mapping."
                    ));
                };
                // Resolve as a package self-reference to respect its exports map.
                let package_resolver = &self.resolvers[0][usize::from(type_only)];
                result = package_resolver.resolve(&pkg_info.dir, spec);
                if !pkg_info.has_exports && matches!(result, Err(ResolveError::NotFound(_))) {
                    result = package_resolver
                        .resolve(&pkg_info.dir, &format!("./{}", parts[count..].join("/")));
                }
            } else if let Some(types_spec) = mangle_types_spec(spec)
                && let Ok(typed) = self.resolvers[0][1].resolve(origin_dir, &types_spec)
                && typed
                    .path()
                    .components()
                    .any(|c| c.as_os_str() == "node_modules")
            {
                return Ok(None);
            }
        }
        match result {
            Ok(resolved) => {
                let path = resolved.path();
                if let Some(query) = resolved.query() {
                    let q = query.trim_start_matches('?');
                    if matches!(q, "url" | "raw" | "inline") {
                        return Ok(None);
                    }
                    if !matches!(
                        q,
                        "worker" | "sharedworker" | "worker&inline" | "inline&worker"
                    ) {
                        return Err(format!(
                            "{spec:?} includes a loader query with bundler-specific module semantics."
                        ));
                    }
                }
                if spec.ends_with(".css") && path.extension().is_some_and(|ext| ext == "css") {
                    return Ok(None);
                }
                if path.extension().is_some_and(|ext| ext == "json") {
                    return Ok(None);
                }
                if path.components().any(|p| p.as_os_str() == "node_modules") {
                    // If a workspace package is also installed inside node_modules, prefer its workspace source.
                    if let Some(ws) = self.resolve_workspace_source(spec, type_only, cjs) {
                        return ws;
                    }
                    return Ok(None);
                }
                let rel = match path.strip_prefix(&self.root) {
                    Ok(p) => p.to_string_lossy().replace('\\', "/"),
                    Err(_) => {
                        // A symlinked node_modules tree (e.g. during --base diff analysis) can point
                        // a workspace package symlink at the original checkout; prefer this checkout's workspace.
                        if let Some(ws) = self.resolve_workspace_source(spec, type_only, cjs) {
                            return ws;
                        }
                        return Err(format!("{spec:?} resolves outside the repository."));
                    }
                };
                if self.source_mapper.is_in_out_dir(Path::new(&rel))
                    && let Some(id) = self.source_mapper.map_to_source_mode(
                        &self.root,
                        &self.graph.paths,
                        Path::new(&rel),
                        type_only,
                    )
                    && id != file
                {
                    return Ok(Some(id));
                }
                if type_only
                    && let Some(id) = crate::source_map::SourceMapper::lookup_direct_mode(
                        &self.graph.paths,
                        Path::new(&rel),
                        true,
                    )
                {
                    return Ok(Some(id));
                }
                if let Ok(id) = self.graph.paths.binary_search(&rel) {
                    return Ok(Some(id));
                }
                if let Some(id) = self.source_mapper.map_to_source_mode(
                    &self.root,
                    &self.graph.paths,
                    Path::new(&rel),
                    type_only,
                ) {
                    return Ok(Some(id));
                }
                if is_non_code_export(&rel) {
                    return Ok(None);
                }
                Err(format!(
                    "{spec:?} resolves to {rel}, outside the analyzed files."
                ))
            }
            Err(ResolveError::Builtin { .. }) => Ok(None),
            Err(e) => Err(format!("Cannot resolve {spec:?}: {e}")),
        }
    }
}

fn classify_bin_shebang(path: &Path) -> Option<bool> {
    use std::io::Read;
    let mut file = fs::File::open(path).ok()?;
    let mut buf = [0u8; 160];
    let n = file.read(&mut buf).ok()?;
    let header = std::str::from_utf8(&buf[..n]).ok()?;
    let line = header.lines().next()?.trim();
    let rest = line.strip_prefix("#!")?.trim();
    let mut tokens = rest.split_whitespace();
    let first = Path::new(tokens.next()?)
        .file_name()
        .and_then(|s| s.to_str())?;
    let interp = if first == "env" {
        tokens
            .find(|t| !t.starts_with('-') && !t.contains('='))
            .and_then(|t| Path::new(t).file_name().and_then(|s| s.to_str()))
            .unwrap_or("")
    } else {
        first
    };
    Some(matches!(
        interp,
        "node" | "nodejs" | "bun" | "deno" | "tsx" | "ts-node"
    ))
}

fn mangle_types_spec(spec: &str) -> Option<String> {
    if spec.starts_with('.') || spec.starts_with('/') || spec.starts_with("@types/") {
        return None;
    }
    if let Some(rest) = spec.strip_prefix('@') {
        let mut it = rest.splitn(3, '/');
        let scope = it.next()?;
        let name = it.next()?;
        match it.next() {
            Some(sub) if !sub.is_empty() => Some(format!("@types/{scope}__{name}/{sub}")),
            _ => Some(format!("@types/{scope}__{name}")),
        }
    } else {
        Some(format!("@types/{spec}"))
    }
}

#[derive(Default)]
struct WorkspacePatterns {
    include: Vec<GlobMatcher>,
    exclude: Vec<GlobMatcher>,
}
impl WorkspacePatterns {
    fn from_patterns<'a>(patterns: impl Iterator<Item = &'a str>) -> Result<Self, String> {
        let mut result = Self::default();
        for pattern in patterns {
            if let Some(negative) = pattern.strip_prefix('!') {
                result.exclude.push(matcher(negative)?);
            } else {
                result.include.push(matcher(pattern)?);
            }
        }
        Ok(result)
    }
    fn matches(&self, path: &Path) -> bool {
        self.include.iter().any(|p| p.is_match(path))
            && !self.exclude.iter().any(|p| p.is_match(path))
    }
}

fn workspace_patterns(manifest: &Value) -> Result<WorkspacePatterns, String> {
    WorkspacePatterns::from_patterns(
        manifest
            .pointer("/workspaces/packages")
            .or(manifest.get("workspaces"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str),
    )
}

#[inline]
fn dir_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(d, _)| d)
}

#[inline]
fn code_ext_of(path: &str) -> Option<&'static str> {
    let last = path.rsplit('/').next().unwrap_or(path);
    let ext = last.rsplit_once('.')?.1;
    EXTENSIONS.iter().copied().find(|&e| e == ext)
}

fn normalize_relative_into(dir: &str, spec: &str, out: &mut String) -> bool {
    let mut segs = [""; 64];
    let mut len = 0usize;
    for part in dir.split('/') {
        if !part.is_empty() {
            if len == segs.len() {
                return false;
            }
            segs[len] = part;
            len += 1;
        }
    }
    for part in spec.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if len == 0 {
                    return false;
                }
                len -= 1;
            }
            seg => {
                if len == segs.len() {
                    return false;
                }
                segs[len] = seg;
                len += 1;
            }
        }
    }
    if len == 0 {
        return false;
    }
    out.clear();
    for (i, seg) in segs[..len].iter().enumerate() {
        if i > 0 {
            out.push('/');
        }
        out.push_str(seg);
    }
    true
}

fn is_node_builtin(name: &str) -> bool {
    matches!(
        name,
        "assert"
            | "assert/strict"
            | "async_hooks"
            | "buffer"
            | "child_process"
            | "cluster"
            | "console"
            | "constants"
            | "crypto"
            | "dgram"
            | "diagnostics_channel"
            | "dns"
            | "dns/promises"
            | "domain"
            | "events"
            | "fs"
            | "fs/promises"
            | "http"
            | "http2"
            | "https"
            | "inspector"
            | "inspector/promises"
            | "module"
            | "net"
            | "os"
            | "path"
            | "path/posix"
            | "path/win32"
            | "perf_hooks"
            | "process"
            | "punycode"
            | "querystring"
            | "readline"
            | "readline/promises"
            | "repl"
            | "sqlite"
            | "stream"
            | "stream/consumers"
            | "stream/promises"
            | "stream/web"
            | "string_decoder"
            | "sys"
            | "test"
            | "timers"
            | "timers/promises"
            | "tls"
            | "trace_events"
            | "tty"
            | "url"
            | "util"
            | "util/types"
            | "v8"
            | "vm"
            | "wasi"
            | "worker_threads"
            | "zlib"
    )
}

fn compute_dir_ids(paths: &[String]) -> Vec<u32> {
    let mut ids = Vec::with_capacity(paths.len());
    let mut prev: Option<&str> = None;
    let mut cur = 0u32;
    for p in paths {
        let d = dir_of(p);
        if prev != Some(d) {
            if prev.is_some() {
                cur += 1;
            }
            prev = Some(d);
        }
        ids.push(cur);
    }
    ids
}

fn closest_directories(paths: &[String], directories: &BTreeMap<PathBuf, usize>) -> Vec<usize> {
    if directories.is_empty() || (directories.len() == 1 && directories.contains_key(Path::new("")))
    {
        return vec![0; paths.len()];
    }
    let mut out = Vec::with_capacity(paths.len());
    let mut last_dir: Option<&str> = None;
    let mut last_val = 0usize;
    for p in paths {
        let dir = dir_of(p);
        if last_dir != Some(dir) {
            last_dir = Some(dir);
            last_val = Path::new(p)
                .ancestors()
                .skip(1)
                .find_map(|a| directories.get(a).copied())
                .unwrap_or(0);
        }
        out.push(last_val);
    }
    out
}

const STANDARD_MANIFEST_KEYS: &[&str] = &[
    "scripts",
    "exports",
    "imports",
    "bin",
    "dependencies",
    "devDependencies",
    "peerDependencies",
    "optionalDependencies",
    "bundledDependencies",
    "overrides",
    "resolutions",
    "patchedDependencies",
    "workspaces",
    "engines",
    "directories",
    "publishConfig",
    "repository",
    "bugs",
    "author",
    "nx",
    "knip",
    "jest",
    "eslintConfig",
    "babel",
    "browserslist",
];

/// Discover plugin/node runtime entries declared in `package.json` under any
/// plugin-host namespace containing `{ nodes, plugins }` maps of JS/TS files.
fn collect_plugin_entries(pkg: &Value) -> Vec<(String, &str)> {
    let Some(root_map) = pkg.as_object() else {
        return vec![];
    };
    let mut out = Vec::new();
    for (section, section_val) in root_map {
        if STANDARD_MANIFEST_KEYS.contains(&section.as_str()) {
            continue;
        }
        let Some(section_obj) = section_val.as_object() else {
            continue;
        };
        for sub in ["nodes", "plugins"] {
            let Some(map) = section_obj.get(sub).and_then(Value::as_object) else {
                continue;
            };
            for (name, val) in map {
                let file = val
                    .as_str()
                    .or_else(|| val.get("file").and_then(Value::as_str))
                    .map(str::trim)
                    .filter(|s| {
                        !s.is_empty()
                            && Path::new(s)
                                .extension()
                                .and_then(|e| e.to_str())
                                .is_some_and(|e| EXTENSIONS.contains(&e))
                    });
                if let Some(f) = file {
                    out.push((format!("{section}.{sub}.{name}"), f));
                }
            }
        }
    }
    out
}

pub(crate) fn html_script_sources(
    repo_root: &Path,
    html_rel: &Path,
    paths: &[String],
) -> Vec<String> {
    let abs = repo_root.join(html_rel);
    let Ok(text) = fs::read_to_string(&abs) else {
        return vec![];
    };
    let html_dir = html_rel.parent().unwrap_or_else(|| Path::new(""));
    let mut results = Vec::new();
    let lower = text.to_ascii_lowercase();
    let mut cursor = 0;
    while let Some(pos) = lower[cursor..].find("<script") {
        let tag_start = cursor + pos;
        let Some(tag_end_rel) = text[tag_start..].find('>') else {
            break;
        };
        let tag = &text[tag_start..tag_start + tag_end_rel];
        cursor = tag_start + tag_end_rel + 1;
        // Find src="..." or src='...'
        let tag_lower = tag.to_ascii_lowercase();
        let Some(src_pos) = tag_lower.find("src=") else {
            continue;
        };
        let after = tag[src_pos + 4..].trim_start();
        let (quote, rest) = match after.as_bytes().first() {
            Some(b'"') => ('"', &after[1..]),
            Some(b'\'') => ('\'', &after[1..]),
            _ => continue,
        };
        let Some(end_q) = rest.find(quote) else {
            continue;
        };
        let src = rest[..end_q].trim();
        if src.is_empty()
            || src.starts_with("http://")
            || src.starts_with("https://")
            || src.starts_with("//")
            || src.starts_with("data:")
        {
            continue;
        }
        let clean = src.split(['?', '#']).next().unwrap_or("");
        let candidate = if let Some(rooted) = clean.strip_prefix('/') {
            // Try relative to package root (html_dir's ancestor) or html_dir.
            let mut found = None;
            for anc in html_dir.ancestors() {
                if let Some(norm) = normalize_rel(&anc.join(rooted)) {
                    let s = to_slash(&norm);
                    if paths.binary_search(&s).is_ok() {
                        found = Some(s);
                        break;
                    }
                }
            }
            found
        } else {
            normalize_rel(&html_dir.join(clean)).and_then(|norm| {
                let s = to_slash(&norm);
                paths.binary_search(&s).ok().map(|_| s)
            })
        };
        if let Some(file) = candidate {
            results.push(file);
        }
    }
    results
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
                .find(|(k, _)| matches!(k.as_str(), "source" | "node" | "import" | "default"))
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
