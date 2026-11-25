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

use crate::graph::{Graph, Id, ROLES, Role, Root};

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

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct Config {
    pub entries: Entries,
    pub public_entries: Option<Vec<String>>,
    pub include: Option<Vec<String>>,
    pub exclude: Option<Vec<String>>,
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

enum WorkspacePackage {
    Unique { dir: PathBuf, has_exports: bool },
    Ambiguous,
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
        let nx_enabled = root
            .join("nx.json")
            .try_exists()
            .map_err(|e| e.to_string())?;
        let mut nx_dirs = BTreeSet::new();
        let mut manifests = vec![];
        let mut config_dirs = BTreeMap::new();
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
            if nx_enabled && name == "project.json" {
                nx_dirs.insert(Path::new(&file).parent().unwrap().to_owned());
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
        let options = ResolveOptions {
            extensions: EXTENSIONS.iter().map(|e| format!(".{e}")).collect(),
            extension_alias: vec![
                (".js".into(), strings(&[".js", ".ts", ".tsx"])),
                (".jsx".into(), strings(&[".jsx", ".tsx"])),
                (".mjs".into(), strings(&[".mjs", ".mts"])),
                (".cjs".into(), strings(&[".cjs", ".cts"])),
            ],
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
                    workspaces
                        .entry(name.to_owned())
                        .and_modify(|pkg| *pkg = WorkspacePackage::Ambiguous)
                        .or_insert_with(|| WorkspacePackage::Unique {
                            dir: dir.to_owned(),
                            has_exports: package.get("exports").is_some(),
                        });
                }
                if !uses_pnpm {
                    patterns.insert(rel.to_owned(), workspace_patterns(&package)?);
                }
                directories.insert(rel.to_owned(), packages.len());
                packages.push((rel.to_owned(), package));
            }
        }
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
        let workspace_for_file = closest_directories(&paths, &directories);
        let mut project = Self {
            root,
            graph: Graph::new(paths),
            excluded,
            workspaces,
            roots: vec![],
            resolvers,
            file_profile,
            workspace_for_file,
        };
        for (role, supplied) in
            ROLES
                .into_iter()
                .zip([entries.runtime, entries.test, entries.tooling, public])
        {
            if supplied.is_none()
                && matches!(role, Role::Runtime | Role::Tooling)
                && let Some(nx) = &nx
            {
                nx.discover(&mut project, role)?;
            }
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

    /// None means a resolved external package, Node builtin or plain stylesheet.
    pub fn resolve(&self, file: Id, spec: &str, type_only: bool) -> Result<Option<Id>, String> {
        let origin = self.root.join(&self.graph.paths[file]);
        let profile = if spec.starts_with('.') || spec.starts_with('/') {
            0
        } else {
            self.file_profile[file]
        };
        let resolver = &self.resolvers[profile][usize::from(type_only)];
        let mut result = resolver.resolve(origin.parent().unwrap(), spec);
        if result.is_err() {
            let parts: Vec<_> = spec.split('/').collect();
            let count = if spec.starts_with('@') && parts.len() >= 2 {
                2
            } else {
                1
            };
            if let Some(package) = self.workspaces.get(&parts[..count].join("/")) {
                let WorkspacePackage::Unique {
                    dir: pkg,
                    has_exports,
                } = package
                else {
                    return Err(format!(
                        "Multiple workspace packages match {spec:?}. Supply an unambiguous path mapping."
                    ));
                };
                // Resolve as a package self-reference to respect its exports map.
                let package_resolver = &self.resolvers[0][usize::from(type_only)];
                result = package_resolver.resolve(pkg, spec);
                if !has_exports && matches!(result, Err(ResolveError::NotFound(_))) {
                    result =
                        package_resolver.resolve(pkg, &format!("./{}", parts[count..].join("/")));
                }
            }
        }
        match result {
            Ok(resolved) => {
                let path = resolved.path();
                if resolved.query().is_some() {
                    return Err(format!(
                        "{spec:?} includes a loader query with bundler-specific module semantics."
                    ));
                }
                if spec.ends_with(".css") && path.extension().is_some_and(|ext| ext == "css") {
                    return Ok(None);
                }
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
