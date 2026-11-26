use std::collections::{BTreeMap, VecDeque};

use rustc_hash::FxHashMap;
use serde::Serialize;

pub type Id = usize;
pub type Link = (Id, Id, String, bool, Loc);

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct Loc {
    pub start: u32,
    pub end: u32,
}

impl From<oxc_span::Span> for Loc {
    fn from(s: oxc_span::Span) -> Self {
        Self {
            start: s.start,
            end: s.end,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Runtime,
    Test,
    Tooling,
    Public,
}
pub const ROLES: [Role; 4] = [Role::Runtime, Role::Test, Role::Tooling, Role::Public];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum RootEntryMode {
    #[default]
    ModuleOnly,
    DefaultExport,
    AllExports,
}

#[derive(Clone, Serialize)]
pub struct Root {
    pub file: String,
    pub role: Role,
    pub origin: &'static str,
    pub input: String,
    #[serde(skip)]
    pub mode: RootEntryMode,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Gap {
    pub code: &'static str,
    pub file: Option<String>,
    pub location: Option<Loc>,
    pub message: String,
    pub affected_files: Option<Vec<String>>,
    pub id: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub test_only: bool,
}

#[derive(Clone, Copy)]
pub struct Edge {
    pub target: Id,
    pub type_only: bool,
    pub loc: Loc, // reference span in the source vertex's file
}

pub struct Node {
    pub file: Id,
    pub kind: &'static str,
    pub name: String,
    pub loc: Loc,
    pub edges: Vec<Edge>,
}

#[derive(Clone, Copy)]
pub struct Export {
    pub surface: Id,
    pub binding: Id,
    pub explicit: bool,
}

#[derive(Default)]
pub struct Module {
    pub exports: BTreeMap<String, Export>,
    pub stars: Vec<Edge>, // target file IDs
    pub cjs_opaque_default: bool,
}

pub struct PendingGap {
    pub record: Gap,
    pub guard: Option<Id>,
    pub target_file: Option<Id>,
}

pub struct Graph {
    pub paths: Vec<String>,
    pub nodes: Vec<Node>,
    pub modules: Vec<Module>,
    pub gaps: Vec<PendingGap>,
    pub links: Vec<Link>,
    edge_offsets: Vec<u32>,
    csr_edges: Vec<Edge>,
}

impl Graph {
    pub fn new(paths: Vec<String>) -> Self {
        let n = paths.len();
        let mut g = Self {
            paths,
            nodes: Vec::with_capacity(n),
            modules: Vec::with_capacity(n),
            gaps: vec![],
            links: vec![],
            edge_offsets: Vec::new(),
            csr_edges: Vec::new(),
        };
        // Module vertex IDs coincide with file IDs.
        for f in 0..n {
            g.node(f, "module", "", Loc::default());
            g.modules.push(Module::default());
        }
        g
    }
    #[inline]
    pub fn edges(&self, id: Id) -> &[Edge] {
        let start = self.edge_offsets[id] as usize;
        let end = self.edge_offsets[id + 1] as usize;
        &self.csr_edges[start..end]
    }
    pub fn clear_edges(&mut self) {
        self.edge_offsets = Vec::new();
        self.csr_edges = Vec::new();
    }
    pub fn node(&mut self, file: Id, kind: &'static str, name: &str, loc: Loc) -> Id {
        self.nodes.push(Node {
            file,
            kind,
            name: name.into(),
            loc,
            edges: vec![],
        });
        self.nodes.len() - 1
    }
    pub fn edge(&mut self, from: Id, target: Id, type_only: bool, loc: Loc) {
        self.nodes[from].edges.push(Edge {
            target,
            type_only,
            loc,
        });
    }
    pub fn gap(
        &mut self,
        code: &'static str,
        file: Option<Id>,
        loc: Option<Loc>,
        message: String,
        guard: Option<Id>,
        target_file: Option<Id>,
    ) {
        self.gap_scoped(code, file, loc, message, guard, target_file, None);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn gap_scoped(
        &mut self,
        code: &'static str,
        file: Option<Id>,
        loc: Option<Loc>,
        message: String,
        guard: Option<Id>,
        target_file: Option<Id>,
        affected_files: Option<Vec<String>>,
    ) {
        let record = Gap {
            code,
            file: file.map(|f| self.paths[f].clone()),
            location: loc,
            message,
            affected_files,
            id: String::new(),
            test_only: false,
        };
        self.gaps.push(PendingGap {
            record,
            guard,
            target_file,
        });
    }
    pub fn step(&self, id: Id, via: Option<(Id, Loc)>) -> serde_json::Value {
        let node = &self.nodes[id];
        serde_json::json!({
            "file": self.paths[node.file], "kind": node.kind, "name": node.name, "location": node.loc,
            "via": via.map(|(file,loc)| serde_json::json!({"file": self.paths[file], "location": loc}))
        })
    }
    pub fn link(&mut self) {
        // Propagate names backwards across star edges, once per (file, name).
        // A named export always overrides a star; cycles cannot introduce new names.
        let mut reverse = vec![vec![]; self.paths.len()];
        for (file, module) in self.modules.iter().enumerate() {
            for star in &module.stars {
                reverse[star.target].push((file, star.type_only, star.loc));
            }
        }
        let mut queue = VecDeque::new();
        for (file, module) in self.modules.iter().enumerate() {
            if reverse[file].is_empty() {
                continue;
            }
            for name in module.exports.keys().filter(|n| n.as_str() != "default") {
                queue.push_back((file, name.clone()));
            }
        }
        while let Some((source, name)) = queue.pop_front() {
            let source_node = self.modules[source].exports[&name].surface;
            for &(file, type_only, loc) in &reverse[source] {
                if let Some(existing) = self.modules[file].exports.get(&name) {
                    if !existing.explicit {
                        self.edge(existing.surface, source_node, type_only, loc);
                    }
                    continue;
                }
                let surface = self.node(file, "re-export", &name, loc);
                self.edge(surface, source_node, type_only, loc);
                self.modules[file].exports.insert(
                    name.clone(),
                    Export {
                        surface,
                        binding: surface,
                        explicit: false,
                    },
                );
                if !reverse[file].is_empty() {
                    queue.push_back((file, name.clone()));
                }
            }
        }
        // Resolve identity through named forwards and star diamonds. A star name
        // with two distinct declarations is absent from the ESM namespace.
        let links = std::mem::take(&mut self.links);
        let node_count = self.nodes.len();
        let mut forwarded = vec![false; node_count];
        for (from, file, name, type_only, loc) in &links {
            if self.nodes[*from].kind == "re-export" {
                forwarded[*from] = true;
                if let Some(target) = self.modules[*file].exports.get(name) {
                    self.edge(*from, target.surface, *type_only, *loc);
                }
            }
        }
        // Inline 2-origin storage + CSR reverse dependents avoid 2*N small Vec allocations.
        let mut origins: Vec<([Id; 2], u8)> = vec![([0, 0], 0); node_count];
        let mut dep_offsets = vec![0u32; node_count + 1];
        let mut origin_queue: Vec<(Id, Id)> = Vec::new();
        for module in &self.modules {
            for export in module.exports.values() {
                let id = export.surface;
                if export.explicit && !forwarded[id] {
                    origins[id] = ([export.binding, 0], 1);
                    origin_queue.push((id, export.binding));
                } else {
                    for edge in &self.nodes[id].edges {
                        dep_offsets[edge.target + 1] += 1;
                    }
                }
            }
        }
        for i in 1..=node_count {
            dep_offsets[i] += dep_offsets[i - 1];
        }
        let mut dep_cursor = dep_offsets[..node_count].to_vec();
        let mut dependents = vec![0u32; dep_offsets[node_count] as usize];
        for module in &self.modules {
            for export in module.exports.values() {
                let id = export.surface;
                if !(export.explicit && !forwarded[id]) {
                    for edge in &self.nodes[id].edges {
                        let slot = dep_cursor[edge.target] as usize;
                        dependents[slot] = id as u32;
                        dep_cursor[edge.target] += 1;
                    }
                }
            }
        }
        let mut head = 0;
        while head < origin_queue.len() {
            let (id, origin) = origin_queue[head];
            head += 1;
            let range = dep_offsets[id] as usize..dep_offsets[id + 1] as usize;
            for &next_u32 in &dependents[range] {
                let next = next_u32 as usize;
                let (pair, len) = &mut origins[next];
                if *len == 0 {
                    pair[0] = origin;
                    *len = 1;
                    origin_queue.push((next, origin));
                } else if *len == 1 && pair[0] != origin {
                    pair[1] = origin;
                    *len = 2;
                    origin_queue.push((next, origin));
                }
            }
        }
        for module in &mut self.modules {
            module
                .exports
                .retain(|_, export| export.explicit || origins[export.surface].1 == 1);
        }
        for (from, file, name, type_only, loc) in links {
            if name == "**" {
                for export in self.modules[file].exports.values() {
                    self.nodes[from].edges.push(Edge {
                        target: export.surface,
                        type_only,
                        loc,
                    });
                }
                continue;
            }
            if name == "*" {
                if let Some(default_target) = self.modules[file].exports.get("default") {
                    self.edge(from, default_target.surface, type_only, loc);
                } else {
                    self.gap(
                        "namespace-escape",
                        Some(self.nodes[from].file),
                        Some(loc),
                        format!("The properties read from {} are unknown.", self.paths[file]),
                        Some(from),
                        Some(file),
                    );
                }
                continue;
            }
            let target = self.modules[file].exports.get(&name);
            if let Some(target) = target.filter(|e| origins[e.surface].1 == 1) {
                if !forwarded[from] {
                    self.edge(from, target.surface, type_only, loc);
                }
            } else if self.modules[file].cjs_opaque_default
                && let Some(default_target) = self.modules[file].exports.get("default")
            {
                self.edge(from, default_target.surface, type_only, loc);
            } else {
                if forwarded[from] {
                    self.nodes[from].edges.clear();
                }
                let from_file = self.nodes[from].file;
                let mut affected = vec![self.paths[file].clone()];
                if from_file != file {
                    affected.push(self.paths[from_file].clone());
                    affected.sort();
                }
                self.gap_scoped(
                    "missing-export",
                    Some(from_file),
                    Some(loc),
                    format!("{} has no unambiguous export {name}.", self.paths[file]),
                    None,
                    None,
                    Some(affected),
                );
            }
        }
        // Freeze adjacency into a flat CSR for cache-friendly BFS/DFS and O(1) drop.
        let total_edges: usize = self.nodes.iter().map(|n| n.edges.len()).sum();
        let mut edge_offsets = Vec::with_capacity(self.nodes.len() + 1);
        let mut csr_edges = Vec::with_capacity(total_edges);
        edge_offsets.push(0);
        for node in &mut self.nodes {
            let start = csr_edges.len();
            csr_edges.extend_from_slice(&node.edges);
            let end = csr_edges.len();
            if end - start > 1 {
                csr_edges[start..end]
                    .sort_unstable_by_key(|e| (e.loc.start, e.target, e.type_only));
            }
            edge_offsets.push(end as u32);
            node.edges = Vec::new();
        }
        self.edge_offsets = edge_offsets;
        self.csr_edges = csr_edges;
    }
}

#[derive(Clone, Copy)]
pub enum Visit {
    Root,
    From { node: Id, lane: usize, loc: Loc },
}

#[derive(Clone, Copy)]
struct CompactVisit {
    /// `u32::MAX` = unvisited, `u32::MAX - 1` = Root, otherwise `(node << 3) | lane`.
    packed: u32,
    loc: Loc,
}

impl CompactVisit {
    const UNVISITED: u32 = u32::MAX;
    const ROOT: u32 = u32::MAX - 1;

    #[inline]
    const fn unvisited() -> Self {
        Self {
            packed: Self::UNVISITED,
            loc: Loc { start: 0, end: 0 },
        }
    }
    #[inline]
    const fn root() -> Self {
        Self {
            packed: Self::ROOT,
            loc: Loc { start: 0, end: 0 },
        }
    }
    #[inline]
    const fn from_edge(node: Id, lane: u8, loc: Loc) -> Self {
        Self {
            packed: ((node as u32) << 3) | (lane as u32),
            loc,
        }
    }
    #[inline]
    fn decode(self) -> Option<Visit> {
        match self.packed {
            Self::UNVISITED => None,
            Self::ROOT => Some(Visit::Root),
            p => Some(Visit::From {
                node: (p >> 3) as Id,
                lane: (p & 7) as usize,
                loc: self.loc,
            }),
        }
    }
}

pub struct Reach {
    /// One byte per node: bit `lane` is set iff `(node, lane)` was reached.
    pub masks: Vec<u8>,
    parents: Vec<CompactVisit>,
}

impl Reach {
    #[inline]
    pub fn mask(&self, node: Id) -> u8 {
        self.masks[node]
    }
    #[inline]
    pub fn has(&self, node: Id, lane: usize) -> bool {
        (self.masks[node] & (1u8 << lane)) != 0
    }
    #[inline]
    pub fn any(&self, node: Id) -> bool {
        self.masks[node] != 0
    }
    #[inline]
    pub fn visit(&self, node: Id, lane: usize) -> Option<Visit> {
        if !self.has(node, lane) {
            return None;
        }
        self.parents[node * 8 + lane].decode()
    }

    pub fn compute(g: &Graph, roots: &[Root]) -> Self {
        let n = g.nodes.len();
        let file_count = g.paths.len();
        let mut masks = vec![0u8; n];
        let mut parents = vec![CompactVisit::unvisited(); n * 8];
        let mut queue: Vec<(u32, u8)> = Vec::with_capacity(n.min(65536));

        for root in roots {
            let file = g.paths.binary_search(&root.file).unwrap();
            let lane = (root.role as u8) * 2;
            let bit = 1u8 << lane;
            if (masks[file] & bit) == 0 {
                masks[file] |= bit;
                parents[file * 8 + lane as usize] = CompactVisit::root();
                queue.push((file as u32, lane));
            }
            if root.role == Role::Public || root.mode == RootEntryMode::AllExports {
                for export in g.modules[file].exports.values() {
                    let surface = export.surface;
                    if (masks[surface] & bit) == 0 {
                        masks[surface] |= bit;
                        parents[surface * 8 + lane as usize] =
                            CompactVisit::from_edge(file, lane, g.nodes[surface].loc);
                        queue.push((surface as u32, lane));
                    }
                }
            } else if root.mode == RootEntryMode::DefaultExport
                && let Some(export) = g.modules[file].exports.get("default")
            {
                let surface = export.surface;
                if (masks[surface] & bit) == 0 {
                    masks[surface] |= bit;
                    parents[surface * 8 + lane as usize] =
                        CompactVisit::from_edge(file, lane, g.nodes[surface].loc);
                    queue.push((surface as u32, lane));
                }
            }
        }

        let mut head = 0;
        while head < queue.len() {
            let (from_u32, lane) = queue[head];
            head += 1;
            let from = from_u32 as usize;
            // Type retention of a module never activates its initializers.
            if (lane & 1) == 1 {
                if from < file_count {
                    continue;
                }
                let file = g.nodes[from].file;
                let bit = 1u8 << lane;
                if (masks[file] & bit) == 0 {
                    masks[file] |= bit;
                    parents[file * 8 + lane as usize] =
                        CompactVisit::from_edge(from, lane, g.nodes[from].loc);
                }
            }
            for edge in g.edges(from) {
                let next = lane | u8::from(edge.type_only);
                let bit = 1u8 << next;
                let target = edge.target;
                if (masks[target] & bit) == 0 {
                    masks[target] |= bit;
                    parents[target * 8 + next as usize] =
                        CompactVisit::from_edge(from, lane, edge.loc);
                    queue.push((target as u32, next));
                }
            }
        }
        Self { masks, parents }
    }

    pub fn gaps(
        &self,
        graph: &Graph,
        uncertain: impl Fn(Id) -> bool,
        roles: &[u8],
        multi_workspace: bool,
    ) -> Vec<Gap> {
        let active = |guard: Option<Id>| guard.is_none_or(|n| self.any(n) || uncertain(n));
        let node_test_only = |n: Id| {
            let m = self.masks[n];
            !uncertain(n) && (m & 0b1111_0011) == 0 && (m & 0b0000_1100) != 0
        };
        let node_test_or_tooling_only = |n: Id| {
            let m = self.masks[n];
            !uncertain(n) && (m & 0b1100_0011) == 0 && (m & 0b0011_1100) != 0
        };
        let mut records = Vec::with_capacity(graph.gaps.len());
        // Escaped namespace values can retain implementations beyond the target file.
        // Follow their possible dependencies without recording any positive usage.
        let mut closures: FxHashMap<(Id, bool, bool), Option<Vec<String>>> = FxHashMap::default();
        let mut seen_stamp = vec![0u32; graph.nodes.len()];
        let mut file_stamp = vec![0u32; graph.paths.len()];
        let mut stamp = 0u32;
        let mut stack: Vec<Id> = Vec::new();
        let mut touched_files: Vec<usize> = Vec::new();

        for pending in &graph.gaps {
            if !active(pending.guard) {
                continue;
            }
            let mut record = pending.record.clone();
            if pending.guard.is_some_and(|n| !self.any(n)) {
                record.message =
                    format!("If the unrooted workspace caller runs: {}", record.message);
            }
            let origin_file_id = record
                .file
                .as_deref()
                .and_then(|p| graph.paths.binary_search_by(|x| x.as_str().cmp(p)).ok());
            record.test_only = pending.guard.is_some_and(&node_test_only)
                || (pending.guard.is_none()
                    && origin_file_id.is_some_and(|f| {
                        roles[f] == (1 << Role::Test as usize) && node_test_only(f)
                    }));
            if record.affected_files.is_none()
                && pending.target_file.is_none()
                && multi_workspace
                && matches!(record.code, "computed-import" | "bundler-glob")
                && let Some(file_path) = &record.file
            {
                if pending.guard.is_some_and(&node_test_or_tooling_only) {
                    record.affected_files = Some(vec![file_path.clone()]);
                } else {
                    let prefix = file_path.rsplit_once('/').map_or("", |(dir, _)| dir);
                    let scoped: Vec<String> = if prefix.is_empty() {
                        vec![file_path.clone()]
                    } else {
                        let slash = format!("{prefix}/");
                        graph
                            .paths
                            .iter()
                            .filter(|p| {
                                p.strip_prefix(&slash)
                                    .is_some_and(|rest| !rest.contains('/'))
                            })
                            .cloned()
                            .collect()
                    };
                    record.affected_files = Some(if scoped.is_empty() {
                        vec![file_path.clone()]
                    } else {
                        scoped
                    });
                }
            }
            if let Some(target) = pending.target_file {
                let guard_runtime = pending
                    .guard
                    .is_none_or(|n| uncertain(n) || (self.masks[n] & 0b1100_0011) != 0);
                let guard_tooling = pending
                    .guard
                    .is_some_and(|n| (self.masks[n] & 0b0011_0000) != 0);
                let cover_mask: u8 = if guard_runtime {
                    0b0100_0001
                } else if guard_tooling {
                    0b0101_0001
                } else {
                    0b0100_0101
                };
                record.affected_files = closures
                    .entry((target, guard_runtime, guard_tooling))
                    .or_insert_with(|| {
                        stamp = stamp.wrapping_add(1);
                        if stamp == 0 {
                            seen_stamp.fill(0);
                            file_stamp.fill(0);
                            stamp = 1;
                        }
                        stack.clear();
                        touched_files.clear();
                        if (self.masks[target] & cover_mask) == 0 {
                            stack.push(target);
                        }
                        for e in graph.modules[target].exports.values() {
                            if (self.masks[e.surface] & cover_mask) == 0
                                && (self.masks[e.binding] & cover_mask) == 0
                            {
                                stack.push(e.surface);
                            }
                        }
                        while let Some(n) = stack.pop() {
                            if seen_stamp[n] != stamp {
                                seen_stamp[n] = stamp;
                                let f = graph.nodes[n].file;
                                if file_stamp[f] != stamp {
                                    file_stamp[f] = stamp;
                                    touched_files.push(f);
                                }
                                for edge in graph.edges(n) {
                                    if (self.masks[edge.target] & cover_mask) == 0
                                        && seen_stamp[edge.target] != stamp
                                    {
                                        stack.push(edge.target);
                                    }
                                }
                            }
                        }
                        if !multi_workspace
                            && graph.gaps.iter().any(|gap| {
                                gap.record.affected_files.is_none()
                                    && gap.target_file.is_none()
                                    && matches!(gap.record.code, "computed-import" | "bundler-glob")
                                    && gap.guard.is_some_and(|n| seen_stamp[n] == stamp)
                            })
                        {
                            None
                        } else {
                            touched_files.sort_unstable();
                            Some(
                                touched_files
                                    .iter()
                                    .map(|&f| graph.paths[f].clone())
                                    .collect(),
                            )
                        }
                    })
                    .clone();
            }
            records.push(record);
        }
        records.sort_by(|a, b| {
            (&a.file, a.location.map(|l| l.start), a.code, &a.message).cmp(&(
                &b.file,
                b.location.map(|l| l.start),
                b.code,
                &b.message,
            ))
        });
        records.dedup_by(|a, b| {
            a.code == b.code
                && a.file == b.file
                && a.location.map(|l| l.start) == b.location.map(|l| l.start)
                && a.message == b.message
                && a.affected_files == b.affected_files
        });
        for (i, gap) in records.iter_mut().enumerate() {
            gap.id = format!("g{i}");
        }
        records
    }
}
