//! Read declarative Nx project metadata without invoking tasks or plugins.
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
};

use crate::graph::{Role, Root, RootEntryMode};
use crate::project::{Project, matcher};

fn read_config(path: &Path) -> Result<Value, String> {
    let mut bytes = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    json_strip_comments::strip_slice(&mut bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    if !value.is_object() {
        return Err(format!(
            "{} must contain an Nx configuration object.",
            path.display()
        ));
    }
    Ok(value)
}

struct Definition {
    dir: PathBuf,
    layers: Vec<(String, Value)>,
    name: String,
}
impl Definition {
    fn field(&self, key: &str) -> Option<&str> {
        self.layers
            .iter()
            .rev()
            .find_map(|(_, v)| v.get(key))?
            .as_str()
    }

    fn source(&self) -> &str {
        &self.layers.last().unwrap().0
    }
}

pub struct Workspace {
    config: Value,
    projects: Vec<Definition>,
    by_name: BTreeMap<String, Option<usize>>,
}

struct Target<'a> {
    layers: Vec<(String, &'a Value)>,
}
struct BuildReference<'a> {
    project: &'a Definition,
    target: Target<'a>,
    configuration: Option<String>,
}

impl<'a> Target<'a> {
    fn field(&self, key: &str) -> Option<&'a str> {
        self.layers
            .iter()
            .rev()
            .find_map(|(_, v)| v.get(key))?
            .as_str()
    }
    fn option(&self, config: Option<&str>, key: &str) -> Option<(String, &'a Value)> {
        if let Some(name) = config {
            for (src, layer) in self.layers.iter().rev() {
                if let Some(value) = layer["configurations"][name].get(key) {
                    return Some((format!("{src}.configurations.{name}.{key}"), value));
                }
            }
        }
        self.layers.iter().rev().find_map(|(src, layer)| {
            layer["options"]
                .get(key)
                .map(|v| (format!("{src}.options.{key}"), v))
        })
    }
}

impl Workspace {
    pub fn read(
        root: &Path,
        dirs: &BTreeSet<PathBuf>,
        packages: &[(PathBuf, Value)],
    ) -> Result<Self, String> {
        let config = read_config(&root.join("nx.json"))?;
        let mut projects = BTreeMap::new();
        for (dir, pkg) in packages {
            if dirs.contains(dir) || pkg["nx"].is_object() {
                let source = format!("{}#nx", dir.join("package.json").display());
                projects.insert(
                    dir.clone(),
                    Definition {
                        name: pkg["name"].as_str().unwrap_or("").into(),
                        dir: dir.clone(),
                        layers: vec![(source, pkg["nx"].clone())],
                    },
                );
            }
        }
        for dir in dirs {
            let path = dir.join("project.json");
            let cfg = read_config(&root.join(&path))?;
            let def = projects.entry(dir.clone()).or_insert_with(|| Definition {
                dir: dir.clone(),
                name: String::new(),
                layers: vec![],
            });
            def.layers.push((path.display().to_string() + "#", cfg));
        }
        for def in projects.values_mut() {
            if let Some(name) = def.field("name") {
                def.name = name.to_owned();
            } else if def.name.is_empty() {
                def.name = def
                    .dir
                    .file_name()
                    .map_or_else(|| ".".into(), |n| n.to_string_lossy().to_lowercase());
            }
            if let Some(dir) = def.field("root") {
                def.dir = workspace_path(dir).ok_or_else(|| {
                    format!(
                        "{} selects a project root outside the workspace: {dir:?}",
                        def.source()
                    )
                })?;
            }
        }
        let projects: Vec<_> = projects.into_values().collect();
        let mut by_name = BTreeMap::new();
        for (index, def) in projects
            .iter()
            .enumerate()
            .filter(|(_, d)| !d.name.is_empty())
        {
            by_name
                .entry(def.name.clone())
                .and_modify(|v| *v = None)
                .or_insert(Some(index));
        }
        Ok(Self {
            config,
            projects,
            by_name,
        })
    }

    pub fn directories(&self) -> impl Iterator<Item = &PathBuf> {
        self.projects.iter().map(|p| &p.dir)
    }

    fn target<'a>(&'a self, project: &'a Definition, name: &str) -> Result<Target<'a>, String> {
        let mut layers = vec![];
        for (source, config) in &project.layers {
            if let Some(target) = config["targets"].get(name) {
                let sep = if source.ends_with('#') { "" } else { "." };
                layers.push((format!("{source}{sep}targets.{name}"), target));
            }
        }
        let selected_executor = layers.iter().rev().find_map(|(_, v)| executor(v));
        if let Some(defaults) = self.config["targetDefaults"].as_object() {
            let exact = selected_executor
                .and_then(|e| defaults.get_key_value(e))
                .or_else(|| defaults.get_key_value(name));
            let base = if exact.is_some() {
                exact
            } else {
                let mut best: Option<(&String, &Value)> = None;
                for (key, value) in defaults {
                    if key.contains(['*', '?', '['])
                        && matcher(key)?.is_match(name)
                        && best.is_none_or(|(old, _)| key.len() > old.len())
                    {
                        best = Some((key, value));
                    }
                }
                best
            };
            if let Some((key, v)) = base {
                layers.insert(0, (format!("nx.json#targetDefaults.{key}"), v));
            }
        }
        // A different executor replaces the lower target's settings.
        if let Some(selected) = selected_executor
            && let Some(i) = layers
                .iter()
                .rposition(|(_, v)| executor(v).is_some_and(|e| e != selected))
        {
            layers.drain(..=i);
        }
        Ok(Target { layers })
    }

    fn forwarded_build(
        &self,
        target: &Target<'_>,
        config: Option<&str>,
    ) -> Result<Option<BuildReference<'_>>, String> {
        if target
            .option(config, "buildTargetOptions")
            .is_some_and(|(_, v)| !v.as_object().is_some_and(|m| m.is_empty()))
        {
            return Ok(None);
        }
        let Some(spec) = target
            .option(config, "buildTarget")
            .and_then(|(_, v)| v.as_str())
        else {
            return Ok(None);
        };
        let mut parts = spec.split(':');
        let Some(def) = parts
            .next()
            .and_then(|p| self.by_name.get(p))
            .copied()
            .flatten()
            .map(|i| &self.projects[i])
        else {
            return Ok(None);
        };
        let Some(name) = parts.next() else {
            return Ok(None);
        };
        let variant = parts.next().map(str::to_owned);
        if parts.next().is_some()
            || !def
                .layers
                .iter()
                .any(|(_, v)| v["targets"].get(name).is_some())
        {
            return Ok(None);
        }
        let build = self.target(def, name)?;
        Ok(build.is_builder().then_some(BuildReference {
            project: def,
            target: build,
            configuration: variant,
        }))
    }

    pub fn discover(&self, project: &mut Project, role: Role) -> Result<(), String> {
        let runtime = role == Role::Runtime;
        if !runtime && let Some(plugins) = self.config["plugins"].as_array() {
            for item in plugins {
                let raw = item
                    .as_str()
                    .or_else(|| item.get("plugin").and_then(Value::as_str));
                if let Some(spec) = raw.filter(|s| s.starts_with('.'))
                    && let Some(norm) = workspace_path(spec)
                    && let Some(file) = norm.to_str()
                    && project
                        .graph
                        .paths
                        .binary_search_by(|p| p.as_str().cmp(file))
                        .is_ok()
                {
                    project.roots.push(Root {
                        file: file.to_owned(),
                        role: Role::Tooling,
                        origin: "discovered",
                        input: "nx.json#plugins".into(),
                        mode: RootEntryMode::AllExports,
                    });
                }
            }
        }
        if runtime
            && (self.config["plugins"]
                .as_array()
                .is_some_and(|p| !p.is_empty())
                || !self.config["extends"].is_null())
        {
            let scope = if project.is_multi_workspace() && !self.projects.is_empty() {
                let mut affected = Vec::<u32>::new();
                for def in &self.projects {
                    if def.field("projectType") != Some("application") {
                        continue;
                    }
                    let prefix = def.dir.to_string_lossy().replace('\\', "/");
                    let has_runtime = project.roots.iter().any(|r| {
                        r.role == Role::Runtime
                            && (prefix.is_empty() || r.file.starts_with(&format!("{prefix}/")))
                    });
                    if !has_runtime && !def.dir.as_os_str().is_empty() {
                        let (s, e) = project.files_in_dir_range(&def.dir);
                        affected.extend(s..e);
                    }
                }
                if affected.is_empty()
                    && let Some(plugins) = self.config["plugins"].as_array()
                {
                    for item in plugins {
                        let raw = item
                            .as_str()
                            .or_else(|| item.get("plugin").and_then(Value::as_str));
                        if let Some(spec) = raw.filter(|s| s.starts_with('.'))
                            && let Some(norm) = workspace_path(spec)
                            && let Some(file) = norm.to_str()
                            && let Ok(idx) = project
                                .graph
                                .paths
                                .binary_search_by(|p| p.as_str().cmp(file))
                        {
                            affected.push(idx as u32);
                        }
                    }
                }
                if affected.is_empty() {
                    crate::graph::GapScope::Global
                } else {
                    affected.sort_unstable();
                    affected.dedup();
                    crate::graph::GapScope::Files(affected)
                }
            } else {
                crate::graph::GapScope::Global
            };
            project.graph.gap_scoped(
                "ambiguous-entry",
                None,
                None,
                "nx.json uses plugins or extended configuration that can supply additional application targets. Supply entries.runtime for the deployment profile.".into(),
                None,
                None,
                scope,
            );
        }
        for def in &self.projects {
            let kind = def.field("projectType");
            if runtime && kind == Some("library") {
                continue;
            }
            let app = kind == Some("application");
            let names: BTreeSet<_> = def
                .layers
                .iter()
                .filter_map(|(_, v)| v["targets"].as_object())
                .flat_map(|targets| targets.keys())
                .collect();
            let previous_roots = project.roots.len();
            let previous_gaps = project.graph.gaps.len();
            for name in names {
                let target = self.target(def, name)?;
                let config = target.field("defaultConfiguration");
                if !target.valid_config(project, config) {
                    continue;
                }
                if !runtime {
                    target.select_tooling(project, def, config);
                }
                if app && target.is_launcher() {
                    if let Some(BuildReference {
                        project: producer,
                        target: build,
                        configuration: variant,
                    }) = self.forwarded_build(&target, config)?
                    {
                        let selected = variant
                            .as_deref()
                            .or_else(|| build.field("defaultConfiguration"));
                        if build.valid_config(project, selected) {
                            if runtime {
                                build.select_build(project, producer, selected);
                            } else {
                                build.select_tooling(project, producer, selected);
                            }
                        }
                    } else if runtime {
                        gap(
                            project,
                            format!(
                                "{} has an unresolved or overridden buildTarget. Supply entries.runtime.",
                                target.label()
                            ),
                        );
                    }
                } else if runtime {
                    if app && target.is_builder() {
                        target.select_build(project, def, config);
                    } else if target.executor() == "nx:noop" {
                        // Explicit no-op aggregator target has no entry point of its own.
                    } else if target.is_builder()
                        || target.is_launcher()
                        || (matches!(name.as_str(), "build" | "serve" | "dev" | "start")
                            && (app || !target.executor().is_empty()))
                    {
                        gap(
                            project,
                            format!(
                                "{} has no statically established application entry. Supply entries.runtime.",
                                target.label()
                            ),
                        );
                    }
                }
            }
            if runtime
                && app
                && previous_roots == project.roots.len()
                && previous_gaps == project.graph.gaps.len()
            {
                gap(
                    project,
                    format!(
                        "{} declares an application without a supported entry target. Supply entries.runtime.",
                        def.source().trim_end_matches('#')
                    ),
                );
            }
        }
        Ok(())
    }
}

fn executor(value: &Value) -> Option<&str> {
    if value.is_string() || value.get("command").is_some() {
        Some("nx:run-commands")
    } else {
        value["executor"].as_str()
    }
}

impl Target<'_> {
    fn label(&self) -> &str {
        &self.layers.last().unwrap().0
    }
    fn executor(&self) -> &str {
        self.layers
            .iter()
            .rev()
            .find_map(|(_, v)| executor(v))
            .unwrap_or("")
    }
    fn is_builder(&self) -> bool {
        matches!(
            self.executor(),
            "@nx/esbuild:esbuild"
                | "@nrwl/esbuild:esbuild"
                | "@nx/webpack:webpack"
                | "@nrwl/webpack:webpack"
        )
    }
    fn is_launcher(&self) -> bool {
        matches!(
            self.executor(),
            "@nx/js:node" | "@nrwl/js:node" | "@nx/webpack:dev-server" | "@nrwl/webpack:dev-server"
        )
    }
    fn valid_config(&self, project: &mut Project, config: Option<&str>) -> bool {
        if let Some(name) = config
            && !self
                .layers
                .iter()
                .any(|(_, v)| v["configurations"].get(name).is_some())
        {
            gap(
                project,
                format!(
                    "{} selects missing configuration {name:?}. Supply the corresponding source entries.",
                    self.label()
                ),
            );
            return false;
        }
        true
    }
    fn select_tooling(&self, project: &mut Project, def: &Definition, config: Option<&str>) {
        for key in [
            "webpackConfig",
            "esbuildConfig",
            "jestConfig",
            "viteConfig",
            "vitestConfig",
            "eslintConfig",
        ] {
            if let Some((source, value)) = self.option(config, key)
                && value
                    .as_str()
                    .and_then(|s| Path::new(s).extension()?.to_str())
                    .is_some_and(|e| crate::project::EXTENSIONS.contains(&e))
            {
                select_file(project, def, Role::Tooling, &source, value);
            }
        }
    }
    fn select_build(&self, project: &mut Project, def: &Definition, config: Option<&str>) {
        if let Some((source, value)) = self
            .option(config, "main")
            .or_else(|| self.option(config, "entryFile"))
        {
            select_file(project, def, Role::Runtime, &source, value);
        } else {
            gap(
                project,
                format!(
                    "{} declares no static main. Supply entries.runtime.",
                    self.label()
                ),
            );
        }
        for key in ["additionalEntryPoints", "scripts"] {
            let Some((source, values)) = self.option(config, key) else {
                continue;
            };
            let Some(entries) = values.as_array() else {
                gap(
                    project,
                    format!(
                        "{source} requires an array of static entries. Supply entries.runtime."
                    ),
                );
                continue;
            };
            for (index, item) in entries.iter().enumerate() {
                if key == "scripts" && item["inject"] == false {
                    continue;
                }
                let path = if item.is_object() {
                    &item[if key == "scripts" {
                        "input"
                    } else {
                        "entryPath"
                    }]
                } else {
                    item
                };
                select_file(
                    project,
                    def,
                    Role::Runtime,
                    &format!("{source}[{index}]"),
                    path,
                );
            }
        }
        for key in [
            "webpackConfig",
            "esbuildConfig",
            "fileReplacements",
            "esbuildOptions",
        ] {
            if self.option(config, key).is_some_and(|(_, v)| match v {
                Value::Null => false,
                Value::Array(a) => !a.is_empty(),
                Value::Object(o) => !o.is_empty(),
                _ => true,
            }) {
                gap(
                    project,
                    format!(
                        "{} uses {key}, which can change entry points or module resolution. Verify the selected build profile.",
                        self.label()
                    ),
                );
            }
        }
    }
}

fn gap(project: &mut Project, message: String) {
    project
        .graph
        .gap("ambiguous-entry", None, None, message, None, None);
}

fn workspace_path(value: &str) -> Option<PathBuf> {
    let mut path = PathBuf::new();
    for part in Path::new(value).components() {
        match part {
            Component::Normal(name) => path.push(name),
            Component::CurDir => {}
            Component::ParentDir if path.pop() => {}
            _ => return None,
        }
    }
    Some(path)
}

fn select_file(project: &mut Project, def: &Definition, role: Role, source: &str, value: &Value) {
    let path = value.as_str().and_then(|s| {
        let dir = def.dir.to_string_lossy();
        let s = s.strip_prefix("{workspaceRoot}/").unwrap_or(s);
        let s = s.replace("{projectRoot}", if dir.is_empty() { "." } else { &dir });
        workspace_path(&s)
    });
    if let Some(file) = path.as_deref().and_then(Path::to_str)
        && project
            .graph
            .paths
            .binary_search_by(|p| p.as_str().cmp(file))
            .is_ok()
    {
        project.roots.push(Root {
            file: file.to_owned(),
            role,
            origin: "discovered",
            input: source.into(),
            mode: RootEntryMode::ModuleOnly,
        });
    } else {
        let scope = project.scope_for_discovered_dir(&def.dir);
        project.graph.gap_scoped("invalid-root",None,None,format!("{source} in project {:?} selects {value}, which has no included source file. Supply an explicit source entry.",def.dir),None,None,scope);
    }
}
