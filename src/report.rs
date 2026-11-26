use oxc_allocator::Allocator;
use rayon::prelude::*;
use std::{path::Path, sync::OnceLock};

use crate::evidence::Evidence;
use serde::{Serialize, Serializer};
use serde_json::{Value, json};

use crate::deps::{DependencyFinding, analyze_dependencies};
use crate::extract::summarize;
use crate::graph::{Gap, Graph, Id, Loc, ROLES, Reach, Role, Root, Visit};
use crate::project::{Config, Project, StaleConfigItem, WorkspaceInfo};

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SuppressedFinding {
    pub workspace: String,
    pub file: String,
    pub symbol: String,
    pub kind: &'static str,
    pub reason: String,
    pub source: &'static str,
    pub location: Loc,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Form {
    Module,
    Export,
    Binding,
}

struct Subject {
    file: Id,
    surface: Id,
    binding: Id,
    form: Form,
    kind: Option<&'static str>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Record<'a> {
    id: String,
    kind: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace_gap: Option<&'a str>,
    workspace: &'a str,
    file: &'a str,
    export_name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    binding_name: Option<&'a str>,
    location: crate::graph::Loc,
    usage: Usage,
}

#[derive(Serialize)]
struct Usage {
    value: Contexts,
    #[serde(rename = "type")]
    types: Contexts,
    contract: bool,
    evaluation: Contexts,
}

struct Contexts(u8);
impl Serialize for Contexts {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(
            ROLES[..3]
                .iter()
                .filter(|&&role| self.0 & (1 << role as usize) != 0),
        )
    }
}

fn is_test_support_file(path: &str, is_test_root: bool) -> bool {
    if is_test_root {
        return true;
    }
    let parts: Vec<&str> = path.split('/').collect();
    for dir in &parts[..parts.len().saturating_sub(1)] {
        if matches!(
            *dir,
            "__tests__"
                | "__mocks__"
                | "__fixtures__"
                | "__helpers__"
                | "__support__"
                | "test"
                | "tests"
                | "testing"
                | "test-utils"
                | "test-helpers"
                | "test-support"
                | "testUtils"
                | "testHelpers"
                | "testSupport"
                | "browser-tests"
                | "e2e"
                | "e2e-tests"
                | "integration-tests"
                | "unit-tests"
                | "playwright"
                | "cypress"
        ) {
            return true;
        }
    }
    if let Some(name) = parts.last() {
        let lower = name.to_ascii_lowercase();
        let markers = [
            ".test.",
            ".spec.",
            ".mock.",
            ".mocks.",
            ".fixture.",
            ".fixtures.",
            ".stub.",
            ".stubs.",
            ".setup.",
            ".test-util.",
            ".test-utils.",
            ".test-helper.",
            ".test-helpers.",
        ];
        if markers.iter().any(|m| lower.contains(m))
            || lower.starts_with("test-setup.")
            || lower.starts_with("setup-tests.")
            || lower.starts_with("setuptests.")
        {
            return true;
        }
    }
    false
}

fn classification(lanes: &[Option<Visit>; 8]) -> Option<&'static str> {
    let used = |role: Role| {
        lanes[role as usize * 2..role as usize * 2 + 2]
            .iter()
            .any(Option::is_some)
    };
    match (
        used(Role::Runtime),
        used(Role::Test),
        used(Role::Tooling),
        used(Role::Public),
    ) {
        (false, false, false, false) => Some("unused-export"),
        (false, true, false, false) => Some("test-only-export"),
        (false, false, true, false) => Some("tooling-only-export"),
        _ => None,
    }
}

pub struct Report {
    graph: Graph,
    roots: Vec<Root>,
    excluded: Vec<String>,
    gaps: Vec<Gap>,
    workspaces: Vec<WorkspaceInfo>,
    workspace_for_file: Vec<usize>,
    dependencies: Vec<DependencyFinding>,
    suppressed: Vec<SuppressedFinding>,
    stale_config: Vec<StaleConfigItem>,
    reach: Reach,
    subjects: Vec<Subject>,
    evidence: Evidence,
}

impl Report {
    pub fn build(root: &Path, config: Config) -> Result<Self, String> {
        let mut project = Project::open(root, config)?;
        let file_count = project.graph.paths.len();
        let mut roles = vec![0u8; file_count];
        let mut entry_exports_used = vec![false; file_count];
        for root in &project.roots {
            let file = project.graph.paths.binary_search(&root.file).unwrap();
            roles[file] |= 1 << root.role as usize;
            if root.mode == crate::graph::RootEntryMode::AllExports {
                entry_exports_used[file] = true;
            }
        }
        // Reuse workers across concurrent snapshots, and arenas across files.
        static POOL: OnceLock<Option<rayon::ThreadPool>> = OnceLock::new();
        let read = |arena: &mut Allocator, file| {
            summarize(
                &project,
                file,
                roles[file] & !(1 << Role::Public as usize) != 0,
                arena,
            )
        };
        let pool = (file_count >= 32)
            .then(|| {
                POOL.get_or_init(|| {
                    let threads = std::thread::available_parallelism().ok()?.get().min(4);
                    rayon::ThreadPoolBuilder::new()
                        .num_threads(threads)
                        .build()
                        .ok()
                })
            })
            .and_then(Option::as_ref);
        let summaries: Vec<_> = if let Some(pool) = pool {
            pool.install(|| {
                (0..file_count)
                    .into_par_iter()
                    .map_init(Allocator::default, read)
                    .collect()
            })
        } else {
            let mut arena = Allocator::default();
            (0..file_count).map(|file| read(&mut arena, file)).collect()
        };
        let usable = summaries
            .iter()
            .enumerate()
            .any(|(file, summary)| roles[file] != 0 && summary.parsed);
        let mut package_refs = Vec::new();
        let mut inline_suppressions = Vec::new();
        let mut stale_config = std::mem::take(&mut project.stale_config);
        let mut dynamic_import_files_used = std::collections::BTreeSet::new();
        for (file_id, mut summary) in summaries.into_iter().enumerate() {
            if summary.used_dynamic_imports {
                dynamic_import_files_used.insert(project.graph.paths[file_id].clone());
            }
            package_refs.append(&mut summary.package_refs);
            inline_suppressions.append(&mut summary.inline_suppressions);
            stale_config.append(&mut summary.invalid_suppressions);
            summary.append(&mut project.graph);
        }
        for file_key in project.dynamic_imports.keys() {
            if !dynamic_import_files_used.contains(file_key) {
                stale_config.push(StaleConfigItem {
                    kind: "stale-dynamic-import",
                    source: "config#dynamicImports".into(),
                    pattern: Some(file_key.clone()),
                    file: Some(file_key.clone()),
                    symbol: None,
                    location: None,
                    message: format!(
                        "dynamicImports entry for {file_key:?} was not consumed by any dynamic import or require."
                    ),
                });
            }
        }
        if !usable {
            let first = project.graph.gaps.iter().find(|g| {
                g.record
                    .file
                    .as_ref()
                    .is_some_and(|file| project.roots.iter().any(|r| &r.file == file))
            });
            return Err(format!(
                "No usable entry could be parsed. {}",
                first.map_or(String::new(), |g| format!(
                    "{} at {:?}: {}",
                    g.record.file.as_deref().unwrap_or(""),
                    g.record.location.map(|l| l.start),
                    g.record.message
                ))
            ));
        }
        let multi_workspace = project.is_multi_workspace();
        project.graph.link();
        let reach = Reach::compute(&project.graph, &project.roots);
        let dependencies = analyze_dependencies(&project, &reach, &package_refs);
        let mut graph = project.graph;
        let mut workspace_gaps = Vec::new();
        let evidence = Evidence::build(
            &graph,
            &reach,
            &roles,
            &project.workspace_for_file,
            &mut workspace_gaps,
        );
        let mut gaps = reach.gaps(
            &graph,
            |id| evidence.gap_for(id).is_some(),
            &roles,
            multi_workspace,
        );
        gaps.append(&mut workspace_gaps);
        let mut exported = vec![false; graph.nodes.len()];
        for e in graph.modules.iter().flat_map(|m| m.exports.values()) {
            exported[e.binding] = true;
        }
        let mut private = vec![vec![]; file_count];
        for (id, n) in graph.nodes.iter().enumerate() {
            if n.kind == "binding" && !exported[id] {
                private[n.file].push(id);
            }
        }
        let mut subjects = vec![];
        for (file, module) in graph.modules.iter().enumerate() {
            let test_support = is_test_support_file(
                &graph.paths[file],
                roles[file] & (1 << Role::Test as usize) != 0,
            );
            let kind =
                (!reach.visited[file].iter().any(Option::is_some)).then_some("unreferenced-module");
            subjects.push(Subject {
                file,
                surface: file,
                binding: file,
                form: Form::Module,
                kind,
            });
            for e in module.exports.values() {
                let kind = if entry_exports_used[file] {
                    None
                } else {
                    match classification(&reach.visited[e.binding]) {
                        Some("test-only-export") if test_support => None,
                        other => other,
                    }
                };
                subjects.push(Subject {
                    file,
                    surface: e.surface,
                    binding: e.binding,
                    form: Form::Export,
                    kind,
                });
            }
            private[file].sort_by(|&a, &b| graph.nodes[a].name.cmp(&graph.nodes[b].name));
            for &binding in &private[file] {
                let test_only = !test_support
                    && classification(&reach.visited[binding]) == Some("test-only-export");
                subjects.push(Subject {
                    file,
                    surface: binding,
                    binding,
                    form: Form::Binding,
                    kind: test_only.then_some("test-only-binding"),
                });
            }
        }
        // Apply per-symbol suppressions (config + JSDoc) and detect stale suppressions.
        let mut config_supp_used = vec![false; project.suppressions.len()];
        let mut config_supp_map: std::collections::HashMap<(String, String), usize> =
            std::collections::HashMap::new();
        for (idx, rule) in project.suppressions.iter().enumerate() {
            let file_norm = rule.file.trim().trim_start_matches("./").replace('\\', "/");
            config_supp_map.insert((file_norm, rule.symbol.trim().to_owned()), idx);
        }
        let mut inline_supp_used = vec![false; inline_suppressions.len()];
        let mut inline_supp_map: std::collections::HashMap<(Id, String), usize> =
            std::collections::HashMap::new();
        for (idx, s) in inline_suppressions.iter().enumerate() {
            inline_supp_map.insert((s.file, s.symbol.clone()), idx);
        }
        let mut suppressed = Vec::new();
        for subj in &mut subjects {
            if subj.form == Form::Module {
                continue;
            }
            let Some(kind) = subj.kind else {
                continue;
            };
            let file_path = &graph.paths[subj.file];
            let sym_name = &graph.nodes[subj.surface].name;
            let ws_id = project
                .workspace_for_file
                .get(subj.file)
                .copied()
                .unwrap_or(0);
            let workspace = project
                .workspace_infos
                .get(ws_id)
                .map_or(".", |w| w.dir.as_str())
                .to_owned();
            if let Some(&idx) = inline_supp_map.get(&(subj.file, sym_name.clone())) {
                inline_supp_used[idx] = true;
                suppressed.push(SuppressedFinding {
                    workspace,
                    file: file_path.clone(),
                    symbol: sym_name.clone(),
                    kind,
                    reason: inline_suppressions[idx].reason.clone(),
                    source: "jsdoc",
                    location: graph.nodes[subj.surface].loc,
                });
                subj.kind = None;
            } else if let Some(&idx) = config_supp_map.get(&(file_path.clone(), sym_name.clone())) {
                config_supp_used[idx] = true;
                suppressed.push(SuppressedFinding {
                    workspace,
                    file: file_path.clone(),
                    symbol: sym_name.clone(),
                    kind,
                    reason: project.suppressions[idx].reason.trim().to_owned(),
                    source: "config",
                    location: graph.nodes[subj.surface].loc,
                });
                subj.kind = None;
            }
        }
        for (rule, used) in project.suppressions.iter().zip(config_supp_used) {
            if !used {
                stale_config.push(StaleConfigItem {
                    kind: "stale-suppression",
                    source: "config#suppressions".into(),
                    pattern: None,
                    file: Some(rule.file.clone()),
                    symbol: Some(rule.symbol.clone()),
                    location: None,
                    message: format!(
                        "Suppression for {}#{} matched no finding (symbol is used or no longer exists).",
                        rule.file, rule.symbol
                    ),
                });
            }
        }
        for (item, used) in inline_suppressions.iter().zip(inline_supp_used) {
            if !used {
                let file = graph.paths[item.file].clone();
                stale_config.push(StaleConfigItem {
                    kind: "stale-suppression",
                    source: file.clone(),
                    pattern: None,
                    file: Some(file.clone()),
                    symbol: Some(item.symbol.clone()),
                    location: Some(item.loc),
                    message: format!(
                        "@smella-ignore on {file}#{} is stale (symbol is already used).",
                        item.symbol
                    ),
                });
            }
        }
        stale_config.sort_by(|a, b| {
            (&a.kind, &a.file, &a.symbol, &a.pattern, &a.message)
                .cmp(&(&b.kind, &b.file, &b.symbol, &b.pattern, &b.message))
        });
        // Queries need the predecessor forest and source labels. Drop build-time
        // adjacency lists and module tables before handing the snapshot to JS.
        for node in &mut graph.nodes {
            node.edges = Vec::new();
        }
        graph.modules = Vec::new();
        graph.gaps = Vec::new();
        Ok(Self {
            graph,
            reach,
            roots: project.roots,
            excluded: project.excluded,
            gaps,
            workspaces: project.workspace_infos,
            workspace_for_file: project.workspace_for_file,
            dependencies,
            suppressed,
            stale_config,
            subjects,
            evidence,
        })
    }
    pub fn metadata(&self) -> Result<String, serde_json::Error> {
        #[derive(Serialize)]
        struct Coverage<'a> {
            status: &'static str,
            files: &'a [String],
            excluded: &'a [String],
            gaps: &'a [Gap],
        }
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Metadata<'a> {
            roots: &'a [Root],
            workspaces: &'a [WorkspaceInfo],
            suppressed: &'a [SuppressedFinding],
            stale_config: &'a [StaleConfigItem],
            coverage: Coverage<'a>,
        }
        serde_json::to_string(&Metadata {
            roots: &self.roots,
            workspaces: &self.workspaces,
            suppressed: &self.suppressed,
            stale_config: &self.stale_config,
            coverage: Coverage {
                status: if self.gaps.is_empty() {
                    "complete"
                } else {
                    "partial"
                },
                files: &self.graph.paths,
                excluded: &self.excluded,
                gaps: &self.gaps,
            },
        })
    }
    fn record(&self, index: usize, s: &Subject) -> Record<'_> {
        let visits = &self.reach.visited[s.binding];
        let contexts = |lanes: &[Option<Visit>; 8], phase: usize| {
            Contexts((0usize..3).fold(0, |mask, role| {
                mask | (u8::from(lanes[2 * role + phase].is_some()) << role)
            }))
        };
        let node = &self.graph.nodes[s.surface];
        let ws_id = self.workspace_for_file.get(s.file).copied().unwrap_or(0);
        let workspace = self.workspaces.get(ws_id).map_or(".", |w| w.dir.as_str());
        Record {
            id: format!("s{index}"),
            workspace_gap: self.evidence.gap_for(s.binding),
            kind: s.kind,
            workspace,
            file: &self.graph.paths[s.file],
            location: node.loc,
            export_name: (s.form == Form::Export).then_some(node.name.as_str()),
            binding_name: (s.form == Form::Binding).then_some(node.name.as_str()),
            usage: Usage {
                value: if s.form == Form::Module {
                    Contexts(0)
                } else {
                    contexts(visits, 0)
                },
                types: contexts(visits, 1),
                contract: visits[6..].iter().any(Option::is_some),
                evaluation: contexts(&self.reach.visited[s.file], 0),
            },
        }
    }

    pub fn findings(&self, kind: Option<&str>) -> Result<String, serde_json::Error> {
        let records = self
            .subjects
            .iter()
            .enumerate()
            .filter(|(_, s)| s.kind.is_some() && (kind.is_none() || s.kind == kind))
            .map(|(i, s)| self.record(i, s));
        let mut output = Vec::new();
        serde_json::Serializer::new(&mut output).collect_seq(records)?;
        Ok(String::from_utf8(output).expect("JSON is UTF-8"))
    }
    pub fn dependencies(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&self.dependencies)
    }
    fn witness(&self, mut id: Id, mut lane: usize) -> Value {
        let mut path = vec![];
        loop {
            let visit = self.reach.visited[id][lane].unwrap();
            let via = match visit {
                Visit::Root => None,
                Visit::From {
                    node: from, loc, ..
                } => Some((self.graph.nodes[from].file, loc)),
            };
            path.push(self.graph.step(id, via));
            match visit {
                Visit::Root => break,
                Visit::From {
                    node, lane: parent, ..
                } => {
                    id = node;
                    lane = parent;
                }
            }
        }
        path.reverse();
        Value::Array(path)
    }
    pub fn explain(&self, selector: &Value) -> Result<Value, String> {
        let index = if let Some(id) = selector.as_str() {
            id.strip_prefix('s')
                .and_then(|i| i.parse::<usize>().ok())
                .filter(|&i| i < self.subjects.len())
        } else {
            let file = selector["file"]
                .as_str()
                .ok_or("Expected a file in the subject selector.")?;
            let file = self
                .graph
                .paths
                .binary_search_by(|path| path.as_str().cmp(file))
                .ok();
            let (form, name) = match (selector.get("exportName"), selector.get("bindingName")) {
                (Some(Value::String(s)), None) => (Form::Export, s.as_str()),
                (None, Some(Value::String(s))) => (Form::Binding, s.as_str()),
                (None, None) => (Form::Module, ""),
                _ => return Err("Expected one exportName or bindingName.".into()),
            };
            file.and_then(|f| {
                self.subjects
                    .binary_search_by(|s| {
                        (s.file, s.form, self.graph.nodes[s.surface].name.as_str())
                            .cmp(&(f, form, name))
                    })
                    .ok()
            })
        }
        .ok_or("Unknown analysis subject.")?;
        let s = &self.subjects[index];
        let mut result = serde_json::to_value(self.record(index, s)).map_err(|e| e.to_string())?;
        let mut witnesses = vec![];
        for role in ROLES {
            for phase in 0..2 {
                let lane = (role as usize) * 2 + phase;
                if self.reach.visited[s.binding][lane].is_some() {
                    let fact = if role == Role::Public {
                        "contract"
                    } else if phase == 1 {
                        "type"
                    } else if s.form == Form::Module {
                        "evaluation"
                    } else {
                        "value"
                    };
                    witnesses
                        .push(json!({"role":role,"fact":fact,"path":self.witness(s.binding,lane)}));
                }
            }
            let lane = (role as usize) * 2;
            if s.form != Form::Module && self.reach.visited[s.file][lane].is_some() {
                witnesses.push(
                    json!({"role":role,"fact":"evaluation","path":self.witness(s.file,lane)}),
                );
            }
        }
        result["witnesses"] = Value::Array(witnesses);
        let (tests, reads, steps) =
            self.evidence
                .explain(&self.graph, s.binding, s.form == Form::Module);
        result["tests"] = Value::Array(tests);
        result["unrootedWorkspaceReads"] = Value::Array(reads);
        result["workspaceSteps"] = Value::Array(steps);
        result["reason"] = match s.kind {
            Some("unused-export") => "No retaining binding reference from the effective roots. Module initialization is tracked separately.",
            Some("unreferenced-module") => "No evaluation, type, or public-contract path from the effective roots.",
            Some("test-only-export" | "test-only-binding") => "Only test roots have known retaining references.",
            Some("tooling-only-export") => "Only tooling roots have known retaining references.",
            None if self.reach.visited[s.binding].iter().all(Option::is_none) => "No effective root reaches this binding.",
            _ => "The effective roots retain this subject."
        }.into();
        Ok(result)
    }
}
