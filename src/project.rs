use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use globset::{GlobBuilder, GlobMatcher};
use oxc_resolver::{
    ResolveError, ResolveOptions, Resolver, TsconfigDiscovery, TsconfigOptions, TsconfigReferences,
};
use serde::Deserialize;
use serde_json::Value;
use walkdir::WalkDir;

use crate::deps::{
    WorkspaceManifest, manifest_bins, manifest_dep_set, manifest_path_for, manifest_scripts,
};
use crate::graph::{Graph, Id, ROLES, Role, Root, RootEntryMode};
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

pub struct Project {
    pub root: PathBuf,
    pub roots: Vec<Root>,
    pub graph: Graph,
    pub excluded: Vec<String>,
    resolvers: Vec<[Resolver; 2]>,
    file_profile: Vec<usize>,
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
                let mut excluded_by_rule = false;
                if entry.depth() > 0 {
                    for (idx, p) in exclude.iter().enumerate() {
                        if p.is_match(&file) {
                            exclude_matched[idx] = true;
                            excluded_by_rule = true;
                        }
                    }
                }
                if entry.depth() > 0 && (SKIP.contains(&name.as_ref()) || excluded_by_rule) {
                    excluded.push(format!("{file}/"));
                    walker.skip_current_dir();
                }
                continue;
            }
            if !entry.file_type().is_file() {
                continue;
            }
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
        let mut non_js_bins = BTreeSet::new();
        for manifest_path in &manifests {
            let Ok(pkg) = json(manifest_path) else {
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
        let manifest = if manifests.contains(&root_manifest) {
            json(&root_manifest)?
        } else {
            Value::Null
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
        manifests.sort_by_key(|p| (p.components().count(), p.clone()));
        for path in manifests.into_iter().filter(|p| p != &root_manifest) {
            let dir = path.parent().unwrap();
            let rel = dir.strip_prefix(&root).unwrap();
            let member = rel.ancestors().skip(1).any(|a| {
                patterns
                    .get(a)
                    .is_some_and(|globs| globs.matches(rel.strip_prefix(a).unwrap()))
            });
            if member || nx_dirs.contains(rel) {
                let package = json(&path)?;
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
        let mut project = Self {
            root,
            graph: Graph::new(paths),
            excluded,
            workspaces,
            roots: vec![],
            resolvers,
            file_profile,
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
            } else if role == Role::Tooling {
                if let Some(nx) = &nx {
                    nx.discover(&mut project, role)?;
                }
                let p =
                    "**/{vite,vitest,jest,webpack,rollup,eslint}.config.{ts,js,mjs,cjs,mts,cts}";
                project.select(role, p, "discovered", p, false)?;
                crate::discover::discover_extra_tooling_configs(&mut project);
                crate::discover::discover_tooling_scripts(&mut project, &packages);
                crate::discover::discover_storybook_stories(&mut project)?;
            } else {
                if role == Role::Runtime {
                    crate::discover::discover_bundler_entries(&mut project);
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
        let glob = matcher(active_pattern)?;
        let before = self.roots.len();
        for file in self.graph.paths.iter().filter(|f| glob.is_match(f)) {
            self.roots.push(Root {
                file: file.clone(),
                role,
                origin,
                input: input.into(),
                mode,
            });
        }
        if before == self.roots.len()
            && origin == "discovered"
            && effective.is_none()
            && let Some(mapped) =
                self.source_mapper
                    .map_pattern(&self.root, &self.graph.paths, pattern)
        {
            let mapped_glob = matcher(&mapped)?;
            for file in self.graph.paths.iter().filter(|f| mapped_glob.is_match(f)) {
                self.roots.push(Root {
                    file: file.clone(),
                    role,
                    origin,
                    input: input.into(),
                    mode,
                });
            }
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
                None
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

    pub fn is_multi_workspace(&self) -> bool {
        self.workspace_dirs.len() > 1
    }

    pub fn files_in_dir(&self, dir: &Path) -> Vec<String> {
        let prefix = to_slash(dir);
        if prefix.is_empty() {
            return self.graph.paths.clone();
        }
        let slash = format!("{prefix}/");
        let start = self
            .graph
            .paths
            .partition_point(|p| p.as_str() < slash.as_str());
        self.graph.paths[start..]
            .iter()
            .take_while(|p| p.starts_with(&slash))
            .cloned()
            .collect()
    }

    pub fn scope_for_discovered_dir(&self, dir: &Path) -> Option<Vec<String>> {
        if dir.as_os_str().is_empty() || !self.is_multi_workspace() {
            None
        } else {
            Some(self.files_in_dir(dir))
        }
    }

    pub fn scope_unresolved_import(&self, file: Id, spec: &str) -> Option<Vec<String>> {
        let importer = self.graph.paths[file].clone();
        let clean_spec = spec.split('?').next().unwrap_or(spec);
        if is_non_code_export(clean_spec) {
            return Some(vec![importer]);
        }
        if spec.contains('?')
            && let Ok(resolved) = self.resolve_with_mode(file, clean_spec, false, false)
        {
            let mut affected = vec![importer];
            if let Some(id) = resolved
                && self.graph.paths[id] != affected[0]
            {
                affected.push(self.graph.paths[id].clone());
                affected.sort();
            }
            return Some(affected);
        }
        if !self.is_multi_workspace() {
            if !clean_spec.starts_with('.')
                && !clean_spec.starts_with('/')
                && Self::split_package_spec(clean_spec)
                    .is_none_or(|(pkg, _)| !self.workspaces.contains_key(&pkg))
                && self
                    .source_mapper
                    .matches_tsconfig_path(Path::new(&importer), clean_spec)
                    .is_empty()
            {
                return Some(vec![importer]);
            }
            return None;
        }
        let mut affected = BTreeSet::from([importer.clone()]);
        if clean_spec.starts_with('.') || clean_spec.starts_with('/') {
            let file_dir = Path::new(&importer)
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
                        affected.extend(self.files_in_dir(ws_dir));
                    }
                } else if let Some(parent) = rel.parent()
                    && !parent.as_os_str().is_empty()
                {
                    affected.extend(self.files_in_dir(parent));
                }
            }
            return Some(affected.into_iter().collect());
        }
        if let Some((pkg_name, _)) = Self::split_package_spec(clean_spec)
            && let Some(WorkspacePackage::Unique(pkg)) = self.workspaces.get(&pkg_name)
        {
            affected.extend(self.files_in_dir(&pkg.rel_dir));
            return Some(affected.into_iter().collect());
        }
        let ts_targets = self
            .source_mapper
            .matches_tsconfig_path(Path::new(&importer), clean_spec);
        if !ts_targets.is_empty() {
            for t in &ts_targets {
                if let Some(parent) = t.parent()
                    && !parent.as_os_str().is_empty()
                {
                    affected.extend(self.files_in_dir(parent));
                }
            }
            return Some(affected.into_iter().collect());
        }
        Some(affected.into_iter().collect())
    }

    /// None means a resolved external package, Node builtin or plain stylesheet.
    pub fn resolve_with_mode(
        &self,
        file: Id,
        spec: &str,
        type_only: bool,
        cjs: bool,
    ) -> Result<Option<Id>, String> {
        if spec.starts_with("bun:") || spec.ends_with(".node") {
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

fn closest_directories(paths: &[String], directories: &BTreeMap<PathBuf, usize>) -> Vec<usize> {
    paths
        .iter()
        .map(|p| {
            Path::new(p)
                .ancestors()
                .skip(1)
                .find_map(|a| directories.get(a).copied())
                .unwrap_or(0)
        })
        .collect()
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
