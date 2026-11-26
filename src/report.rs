use oxc_allocator::Allocator;
use rayon::prelude::*;
use std::path::Path;

use crate::evidence::Evidence;
use serde::{Serialize, Serializer};
use serde_json::{Value, json};

use crate::deps::{DependencyFinding, analyze_dependencies};
use crate::extract::summarize;
use crate::graph::{Gap, Graph, Id, Loc, NodeKind, ROLES, Reach, Role, Root, Visit};
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

struct SubjectId(usize);

impl Serialize for SubjectId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut buf = [0u8; 24];
        let mut n = self.0;
        let mut pos = buf.len();
        loop {
            pos -= 1;
            buf[pos] = b'0' + (n % 10) as u8;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        pos -= 1;
        buf[pos] = b's';
        // SAFETY: buf[pos..] contains only ASCII 's' and '0'..='9'.
        let s = std::str::from_utf8(&buf[pos..]).expect("ASCII subject id");
        serializer.serialize_str(s)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Record<'a> {
    id: SubjectId,
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

const TEST_SUPPORT_DIRS: &[&str] = &[
    "__tests__",
    "__mocks__",
    "__fixtures__",
    "__helpers__",
    "__support__",
    "test",
    "tests",
    "testing",
    "test-utils",
    "test-helpers",
    "test-support",
    "testUtils",
    "testHelpers",
    "testSupport",
    "browser-tests",
    "e2e",
    "e2e-tests",
    "integration-tests",
    "unit-tests",
    "playwright",
    "cypress",
];

const TEST_SUPPORT_MARKERS: &[&str] = &[
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

const TEST_SETUP_PREFIXES: &[&str] = &["test-setup.", "setup-tests.", "setuptests."];

fn is_test_support_file(path: &str, is_test_root: bool) -> bool {
    if is_test_root {
        return true;
    }
    let mut parts = path.rsplit('/');
    let file_name = parts.next().unwrap_or("");
    if parts.any(|dir| TEST_SUPPORT_DIRS.contains(&dir)) {
        return true;
    }
    if file_name.bytes().any(|b| b.is_ascii_uppercase()) {
        let lower = file_name.to_ascii_lowercase();
        TEST_SUPPORT_MARKERS.iter().any(|m| lower.contains(m))
            || TEST_SETUP_PREFIXES.iter().any(|p| lower.starts_with(p))
    } else {
        TEST_SUPPORT_MARKERS.iter().any(|m| file_name.contains(m))
            || TEST_SETUP_PREFIXES.iter().any(|p| file_name.starts_with(p))
    }
}

#[inline]
fn classification(mask: u8) -> Option<&'static str> {
    let runtime = (mask & 0b0000_0011) != 0;
    let test = (mask & 0b0000_1100) != 0;
    let tooling = (mask & 0b0011_0000) != 0;
    let public = (mask & 0b1100_0000) != 0;
    match (runtime, test, tooling, public) {
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
        let profile = std::env::var_os("SMELLA_PROFILE").is_some();
        let t_start = std::time::Instant::now();
        let mut project = Project::open(root, config)?;
        let t_open = std::time::Instant::now();
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
        let read = |(arena, buf): &mut (Allocator, Vec<u8>), file| {
            summarize(
                &project,
                file,
                roles[file] & !(1 << Role::Public as usize) != 0,
                arena,
                buf,
            )
        };
        let mut summaries: Vec<_> = if file_count >= 32
            && let Some(pool) = crate::io::pool()
        {
            pool.install(|| {
                (0..file_count)
                    .into_par_iter()
                    .map_init(
                        || (Allocator::default(), Vec::with_capacity(16 * 1024)),
                        read,
                    )
                    .collect()
            })
        } else {
            let mut state = (Allocator::default(), Vec::with_capacity(16 * 1024));
            (0..file_count).map(|file| read(&mut state, file)).collect()
        };
        project.sources = Vec::new();
        let t_extract = std::time::Instant::now();
        let usable = summaries
            .iter()
            .enumerate()
            .any(|(file, summary)| roles[file] != 0 && summary.parsed);
        let mut node_offsets = Vec::with_capacity(file_count);
        let mut name_offsets = Vec::with_capacity(file_count);
        let mut extra_nodes = 0usize;
        let mut total_name_bytes = project.graph.names.len();
        let mut total_edges = 0usize;
        let mut total_links = 0usize;
        let mut total_gaps = 0usize;
        let mut total_pkg_refs = 0usize;
        for summary in &summaries {
            node_offsets.push(extra_nodes);
            name_offsets.push(total_name_bytes);
            extra_nodes += summary.node_count();
            total_name_bytes += summary.name_bytes();
            total_edges += summary.edge_count();
            total_links += summary.link_count();
            total_gaps += summary.gap_count();
            total_pkg_refs += summary.package_refs.len();
        }
        if extra_nodes >= 10_000
            && let Some(pool) = crate::io::pool()
        {
            pool.install(|| {
                summaries
                    .par_iter_mut()
                    .zip(node_offsets.par_iter().zip(name_offsets.par_iter()))
                    .for_each(|(summary, (&node_off, &name_off))| {
                        summary.relocate(node_off, name_off)
                    });
            });
        } else {
            for (summary, (&node_off, &name_off)) in summaries
                .iter_mut()
                .zip(node_offsets.iter().zip(&name_offsets))
            {
                summary.relocate(node_off, name_off);
            }
        }
        project.graph.nodes.reserve_exact(extra_nodes);
        project
            .graph
            .names
            .reserve_exact(total_name_bytes - project.graph.names.len());
        project.graph.pending_edges.reserve_exact(total_edges);
        project.graph.links.reserve_exact(total_links);
        project.graph.gaps.reserve_exact(total_gaps);
        let mut package_refs = Vec::with_capacity(total_pkg_refs);
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
            summary.move_into(&mut project.graph);
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
        let t_append = std::time::Instant::now();
        project.graph.link();
        let t_link = std::time::Instant::now();
        let reach = Reach::compute(&project.graph, &project.roots);
        let t_reach = std::time::Instant::now();
        let dependencies = analyze_dependencies(&project, &reach, &package_refs);
        let t_deps = std::time::Instant::now();
        let mut graph = project.graph;
        let mut workspace_gaps = Vec::new();
        let evidence = Evidence::build(
            &graph,
            &reach,
            &roles,
            &project.workspace_for_file,
            &mut workspace_gaps,
        );
        let t_evidence = std::time::Instant::now();
        let mut gaps = reach.gaps(
            &graph,
            |id| evidence.gap_for(id).is_some(),
            &roles,
            multi_workspace,
        );
        gaps.append(&mut workspace_gaps);
        let t_gaps = std::time::Instant::now();
        let mut exported = vec![false; graph.nodes.len()];
        let mut export_count = 0usize;
        for m in &graph.modules {
            export_count += m.exports.len();
            for e in m.exports.values() {
                exported[e.binding] = true;
            }
        }
        let mut private_offsets = vec![0u32; file_count + 1];
        for (id, n) in graph.nodes.iter().enumerate() {
            if n.kind == NodeKind::Binding && !exported[id] {
                private_offsets[n.file_id() + 1] += 1;
            }
        }
        for i in 1..=file_count {
            private_offsets[i] += private_offsets[i - 1];
        }
        let private_count = private_offsets[file_count] as usize;
        let mut private_cursor = private_offsets[..file_count].to_vec();
        let mut private_ids = vec![0u32; private_count];
        for (id, n) in graph.nodes.iter().enumerate() {
            if n.kind == NodeKind::Binding && !exported[id] {
                let f = n.file_id();
                let slot = private_cursor[f] as usize;
                private_ids[slot] = id as u32;
                private_cursor[f] += 1;
            }
        }
        drop(exported);
        drop(private_cursor);
        let mut subjects = Vec::with_capacity(file_count + export_count + private_count);
        for (file, module) in graph.modules.iter().enumerate() {
            let test_support = is_test_support_file(
                &graph.paths[file],
                roles[file] & (1 << Role::Test as usize) != 0,
            );
            let kind = (!reach.any(file)).then_some("unreferenced-module");
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
                    match classification(reach.mask(e.binding)) {
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
            let p_start = private_offsets[file] as usize;
            let p_end = private_offsets[file + 1] as usize;
            if p_end - p_start > 1 {
                private_ids[p_start..p_end]
                    .sort_by(|&a, &b| graph.node_name(a as usize).cmp(graph.node_name(b as usize)));
            }
            for &binding_u32 in &private_ids[p_start..p_end] {
                let binding = binding_u32 as usize;
                let test_only = !test_support
                    && classification(reach.mask(binding)) == Some("test-only-export");
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
        let mut config_supp_map: rustc_hash::FxHashMap<(String, String), usize> =
            rustc_hash::FxHashMap::default();
        for (idx, rule) in project.suppressions.iter().enumerate() {
            let file_norm = rule.file.trim().trim_start_matches("./").replace('\\', "/");
            config_supp_map.insert((file_norm, rule.symbol.trim().to_owned()), idx);
        }
        let mut inline_supp_used = vec![false; inline_suppressions.len()];
        let mut inline_supp_map: rustc_hash::FxHashMap<(Id, String), usize> =
            rustc_hash::FxHashMap::default();
        for (idx, s) in inline_suppressions.iter().enumerate() {
            inline_supp_map.insert((s.file, s.symbol.clone()), idx);
        }
        let mut suppressed = Vec::new();
        if !inline_supp_map.is_empty() || !config_supp_map.is_empty() {
            for subj in &mut subjects {
                if subj.form == Form::Module {
                    continue;
                }
                let Some(kind) = subj.kind else {
                    continue;
                };
                let sym_name = graph.node_name(subj.surface);
                if !inline_supp_map.is_empty()
                    && let Some(&idx) = inline_supp_map.get(&(subj.file, sym_name.to_owned()))
                {
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
                    inline_supp_used[idx] = true;
                    suppressed.push(SuppressedFinding {
                        workspace,
                        file: graph.paths[subj.file].clone(),
                        symbol: sym_name.to_owned(),
                        kind,
                        reason: inline_suppressions[idx].reason.clone(),
                        source: "jsdoc",
                        location: graph.nodes[subj.surface].loc,
                    });
                    subj.kind = None;
                } else if !config_supp_map.is_empty()
                    && let Some(&idx) =
                        config_supp_map.get(&(graph.paths[subj.file].clone(), sym_name.to_owned()))
                {
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
                    config_supp_used[idx] = true;
                    suppressed.push(SuppressedFinding {
                        workspace,
                        file: graph.paths[subj.file].clone(),
                        symbol: sym_name.to_owned(),
                        kind,
                        reason: project.suppressions[idx].reason.trim().to_owned(),
                        source: "config",
                        location: graph.nodes[subj.surface].loc,
                    });
                    subj.kind = None;
                }
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
        graph.clear_edges();
        let modules = std::mem::take(&mut graph.modules);
        if modules.len() >= 1024
            && let Some(pool) = crate::io::pool()
        {
            pool.install(|| modules.into_par_iter().for_each(drop));
        }
        graph.gaps = Vec::new();
        if profile {
            let t_end = std::time::Instant::now();
            let ms = |a: std::time::Instant, b: std::time::Instant| {
                b.duration_since(a).as_secs_f64() * 1000.0
            };
            eprintln!(
                "[profile] files={file_count} nodes={} total={:.2}ms | open={:.2}ms extract={:.2}ms append={:.2}ms link={:.2}ms reach={:.2}ms deps={:.2}ms evidence={:.2}ms gaps={:.2}ms finalize={:.2}ms",
                graph.nodes.len(),
                ms(t_start, t_end),
                ms(t_start, t_open),
                ms(t_open, t_extract),
                ms(t_extract, t_append),
                ms(t_append, t_link),
                ms(t_link, t_reach),
                ms(t_reach, t_deps),
                ms(t_deps, t_evidence),
                ms(t_evidence, t_gaps),
                ms(t_gaps, t_end),
            );
        }
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
        let binding_mask = self.reach.mask(s.binding);
        let file_mask = self.reach.mask(s.file);
        let contexts = |mask: u8, phase: u8| {
            let mut out = 0u8;
            if (mask & (1u8 << phase)) != 0 {
                out |= 1;
            }
            if (mask & (1u8 << (2 + phase))) != 0 {
                out |= 2;
            }
            if (mask & (1u8 << (4 + phase))) != 0 {
                out |= 4;
            }
            Contexts(out)
        };
        let node = &self.graph.nodes[s.surface];
        let node_name = self.graph.node_name(s.surface);
        let ws_id = self.workspace_for_file.get(s.file).copied().unwrap_or(0);
        let workspace = self.workspaces.get(ws_id).map_or(".", |w| w.dir.as_str());
        Record {
            id: SubjectId(index),
            workspace_gap: self.evidence.gap_for(s.binding),
            kind: s.kind,
            workspace,
            file: &self.graph.paths[s.file],
            location: node.loc,
            export_name: (s.form == Form::Export).then_some(node_name),
            binding_name: (s.form == Form::Binding).then_some(node_name),
            usage: Usage {
                value: if s.form == Form::Module {
                    Contexts(0)
                } else {
                    contexts(binding_mask, 0)
                },
                types: contexts(binding_mask, 1),
                contract: (binding_mask & 0b1100_0000) != 0,
                evaluation: contexts(file_mask, 0),
            },
        }
    }

    pub fn findings(&self, kind: Option<&str>) -> Result<String, serde_json::Error> {
        let count = self
            .subjects
            .iter()
            .filter(|s| s.kind.is_some() && (kind.is_none() || s.kind == kind))
            .count();
        let records = self
            .subjects
            .iter()
            .enumerate()
            .filter(|(_, s)| s.kind.is_some() && (kind.is_none() || s.kind == kind))
            .map(|(i, s)| self.record(i, s));
        let mut output = Vec::with_capacity(count.saturating_mul(220).max(256));
        serde_json::Serializer::new(&mut output).collect_seq(records)?;
        Ok(String::from_utf8(output).expect("JSON is UTF-8"))
    }
    pub fn dependencies(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&self.dependencies)
    }
    fn witness(&self, mut id: Id, mut lane: usize) -> Value {
        let mut path = vec![];
        loop {
            let visit = self.reach.visit(id, lane).unwrap();
            let via = match visit {
                Visit::Root => None,
                Visit::From {
                    node: from, loc, ..
                } => Some((self.graph.nodes[from].file_id(), loc)),
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
                        (s.file, s.form, self.graph.node_name(s.surface)).cmp(&(f, form, name))
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
                if self.reach.has(s.binding, lane) {
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
            if s.form != Form::Module && self.reach.has(s.file, lane) {
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
            None if !self.reach.any(s.binding) => "No effective root reaches this binding.",
            _ => "The effective roots retain this subject."
        }.into();
        Ok(result)
    }
}
