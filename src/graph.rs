use std::collections::{BTreeMap, VecDeque};

use rayon::prelude::*;
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

/// Compact representation of which analyzed files a gap can affect.
/// Avoids cloning thousands of file path strings per gap and keeps N-API metadata tiny.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum GapScope {
    #[default]
    Global,
    /// Contiguous range `[start, end)` in `graph.paths`.
    Range(u32, u32),
    /// Contiguous range `[start, end)` in `graph.paths` union `{ extra }`.
    RangePlus(u32, u32, u32),
    /// Explicit sorted, deduplicated file IDs.
    Files(Vec<u32>),
}

impl GapScope {
    pub fn is_global(&self) -> bool {
        matches!(self, Self::Global)
    }
    pub fn from_range_and_file(range: (u32, u32), file: Id) -> Self {
        let f = file as u32;
        if range.0 >= range.1 {
            Self::Files(vec![f])
        } else if f >= range.0 && f < range.1 {
            Self::Range(range.0, range.1)
        } else {
            Self::RangePlus(range.0, range.1, f)
        }
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Gap {
    pub code: &'static str,
    pub file: Option<String>,
    pub location: Option<Loc>,
    pub message: String,
    #[serde(rename = "affectedScope")]
    pub scope: GapScope,
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

/// 16-byte edge entry accumulated during extraction and linking before CSR freeze.
#[derive(Clone, Copy)]
pub struct RawEdge {
    from: u32,
    target_and_flag: u32,
    pub loc: Loc,
}

impl RawEdge {
    const TYPE_ONLY_BIT: u32 = 1 << 31;

    #[inline]
    pub fn new(from: Id, target: Id, type_only: bool, loc: Loc) -> Self {
        debug_assert!(from < (1 << 31) && target < (1 << 31));
        Self {
            from: from as u32,
            target_and_flag: (target as u32) | if type_only { Self::TYPE_ONLY_BIT } else { 0 },
            loc,
        }
    }
    #[inline]
    pub fn source(self) -> Id {
        self.from as Id
    }
    #[inline]
    pub fn target(self) -> Id {
        (self.target_and_flag & !Self::TYPE_ONLY_BIT) as Id
    }
    #[inline]
    pub fn relocate(&mut self, base: u32, offset: u32) {
        if self.from >= base {
            self.from += offset;
        }
        let target = self.target_and_flag & !Self::TYPE_ONLY_BIT;
        if target >= base {
            self.target_and_flag = (target + offset) | (self.target_and_flag & Self::TYPE_ONLY_BIT);
        }
    }
    #[inline]
    fn into_compact(self) -> CompactEdge {
        CompactEdge {
            target_and_flag: self.target_and_flag,
            loc: self.loc,
        }
    }
}

/// 12-byte frozen CSR edge for cache-dense BFS/DFS traversal.
#[derive(Clone, Copy, Default)]
pub struct CompactEdge {
    target_and_flag: u32,
    pub loc: Loc,
}

impl CompactEdge {
    const TYPE_ONLY_BIT: u32 = 1 << 31;

    #[inline]
    pub fn target(self) -> Id {
        (self.target_and_flag & !Self::TYPE_ONLY_BIT) as Id
    }
    #[inline]
    pub fn type_only(self) -> bool {
        (self.target_and_flag & Self::TYPE_ONLY_BIT) != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum NodeKind {
    Module,
    Binding,
    Import,
    Export,
    ReExport,
    Declaration,
}

impl NodeKind {
    #[inline]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Module => "module",
            Self::Binding => "binding",
            Self::Import => "import",
            Self::Export => "export",
            Self::ReExport => "re-export",
            Self::Declaration => "declaration",
        }
    }
}

/// 24-byte graph vertex; `name_off..name_off+name_len` slices `Graph.names`.
pub struct Node {
    pub file: u32,
    pub loc: Loc,
    pub name_off: u32,
    pub name_len: u32,
    pub kind: NodeKind,
}

impl Node {
    #[inline]
    pub fn file_id(&self) -> Id {
        self.file as Id
    }
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
    /// Contiguous UTF-8 pool holding all node names (zero per-node String allocs).
    pub names: String,
    pub modules: Vec<Module>,
    pub gaps: Vec<PendingGap>,
    pub links: Vec<Link>,
    pub pending_edges: Vec<RawEdge>,
    edge_offsets: Vec<u32>,
    csr_edges: Vec<CompactEdge>,
}

impl Graph {
    pub fn new(paths: Vec<String>) -> Self {
        let n = paths.len();
        let mut g = Self {
            paths,
            nodes: Vec::with_capacity(n),
            names: String::new(),
            modules: Vec::with_capacity(n),
            gaps: vec![],
            links: vec![],
            pending_edges: Vec::new(),
            edge_offsets: Vec::new(),
            csr_edges: Vec::new(),
        };
        // Module vertex IDs coincide with file IDs.
        for f in 0..n {
            g.node(f, NodeKind::Module, "", Loc::default());
            g.modules.push(Module::default());
        }
        g
    }
    #[inline]
    pub fn node_name(&self, id: Id) -> &str {
        let n = &self.nodes[id];
        if n.name_len == 0 {
            ""
        } else {
            let start = n.name_off as usize;
            &self.names[start..start + n.name_len as usize]
        }
    }
    #[inline]
    pub fn edges(&self, id: Id) -> &[CompactEdge] {
        let start = self.edge_offsets[id] as usize;
        let end = self.edge_offsets[id + 1] as usize;
        &self.csr_edges[start..end]
    }
    pub fn clear_edges(&mut self) {
        self.pending_edges = Vec::new();
        self.edge_offsets = Vec::new();
        self.csr_edges = Vec::new();
    }
    pub fn node(&mut self, file: Id, kind: NodeKind, name: &str, loc: Loc) -> Id {
        let (name_off, name_len) = if name.is_empty() {
            (0u32, 0u32)
        } else {
            let off = self.names.len() as u32;
            self.names.push_str(name);
            (off, name.len() as u32)
        };
        self.nodes.push(Node {
            file: file as u32,
            loc,
            name_off,
            name_len,
            kind,
        });
        self.nodes.len() - 1
    }
    #[inline]
    pub fn edge(&mut self, from: Id, target: Id, type_only: bool, loc: Loc) {
        self.pending_edges
            .push(RawEdge::new(from, target, type_only, loc));
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
        self.gap_scoped(
            code,
            file,
            loc,
            message,
            guard,
            target_file,
            GapScope::Global,
        );
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
        scope: GapScope,
    ) {
        let record = Gap {
            code,
            file: file.map(|f| self.paths[f].clone()),
            location: loc,
            message,
            scope,
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
            "file": self.paths[node.file_id()], "kind": node.kind.as_str(), "name": self.node_name(id), "location": node.loc,
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
                let surface = self.node(file, NodeKind::ReExport, &name, loc);
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
            if self.nodes[*from].kind == NodeKind::ReExport {
                forwarded[*from] = true;
                if let Some(target) = self.modules[*file].exports.get(name) {
                    self.edge(*from, target.surface, *type_only, *loc);
                }
            }
        }
        // Inline 2-origin storage + CSR reverse dependents avoid 2*N small Vec allocations.
        let mut origins: Vec<([Id; 2], u8)> = vec![([0, 0], 0); node_count];
        let mut needs_origin = vec![false; node_count];
        let mut origin_queue: Vec<(Id, Id)> = Vec::new();
        for module in &self.modules {
            for export in module.exports.values() {
                let id = export.surface;
                if export.explicit && !forwarded[id] {
                    origins[id] = ([export.binding, 0], 1);
                    origin_queue.push((id, export.binding));
                } else {
                    needs_origin[id] = true;
                }
            }
        }
        let mut dep_offsets = vec![0u32; node_count + 1];
        let mut dep_total = 0usize;
        for edge in &self.pending_edges {
            let from = edge.source();
            if from < node_count && needs_origin[from] {
                dep_offsets[edge.target() + 1] += 1;
                dep_total += 1;
            }
        }
        if dep_total > 0 {
            for i in 1..=node_count {
                dep_offsets[i] += dep_offsets[i - 1];
            }
            let mut dep_cursor = dep_offsets[..node_count].to_vec();
            let mut dependents = vec![0u32; dep_total];
            for edge in &self.pending_edges {
                let from = edge.source();
                if from < node_count && needs_origin[from] {
                    let target = edge.target();
                    let slot = dep_cursor[target] as usize;
                    dependents[slot] = from as u32;
                    dep_cursor[target] += 1;
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
        }
        drop(needs_origin);
        drop(dep_offsets);
        drop(origin_queue);
        for module in &mut self.modules {
            module
                .exports
                .retain(|_, export| export.explicit || origins[export.surface].1 == 1);
        }
        let mut cleared: Option<Vec<bool>> = None;
        for (from, file, name, type_only, loc) in links {
            if name == "**" {
                for export in self.modules[file].exports.values() {
                    self.pending_edges
                        .push(RawEdge::new(from, export.surface, type_only, loc));
                }
                continue;
            }
            if name == "*" {
                if let Some(default_target) = self.modules[file].exports.get("default") {
                    self.edge(from, default_target.surface, type_only, loc);
                } else {
                    self.gap(
                        "namespace-escape",
                        Some(self.nodes[from].file_id()),
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
                    cleared.get_or_insert_with(|| vec![false; self.nodes.len()])[from] = true;
                }
                let from_file = self.nodes[from].file_id();
                let mut affected = vec![file as u32];
                if from_file != file {
                    affected.push(from_file as u32);
                    affected.sort_unstable();
                }
                self.gap_scoped(
                    "missing-export",
                    Some(from_file),
                    Some(loc),
                    format!("{} has no unambiguous export {name}.", self.paths[file]),
                    None,
                    None,
                    GapScope::Files(affected),
                );
            }
        }
        drop(origins);
        drop(forwarded);
        // Freeze pending edges into a flat 12-byte CSR for cache-dense BFS/DFS and O(1) drop.
        let pending = std::mem::take(&mut self.pending_edges);
        let n = self.nodes.len();
        let mut edge_offsets = vec![0u32; n + 1];
        if let Some(cleared) = cleared.as_deref() {
            for edge in &pending {
                let from = edge.source();
                if !cleared[from] {
                    edge_offsets[from + 1] += 1;
                }
            }
        } else {
            for edge in &pending {
                edge_offsets[edge.source() + 1] += 1;
            }
        }
        for i in 1..=n {
            edge_offsets[i] += edge_offsets[i - 1];
        }
        let total_edges = edge_offsets[n] as usize;
        let mut cursor = edge_offsets[..n].to_vec();
        let mut csr_edges = vec![CompactEdge::default(); total_edges];
        if let Some(cleared) = cleared.as_deref() {
            for edge in pending {
                let from = edge.source();
                if !cleared[from] {
                    let slot = cursor[from] as usize;
                    csr_edges[slot] = edge.into_compact();
                    cursor[from] += 1;
                }
            }
        } else {
            for edge in pending {
                let from = edge.source();
                let slot = cursor[from] as usize;
                csr_edges[slot] = edge.into_compact();
                cursor[from] += 1;
            }
        }
        drop(cursor);
        let sort_range = |offsets: &[u32], base_edge: usize, slice: &mut [CompactEdge]| {
            for w in offsets.windows(2) {
                let start = w[0] as usize - base_edge;
                let end = w[1] as usize - base_edge;
                if end - start > 1 {
                    slice[start..end]
                        .sort_unstable_by_key(|e| (e.loc.start, e.target(), e.type_only()));
                }
            }
        };
        if total_edges >= 100_000
            && let Some(pool) = crate::io::pool()
        {
            let chunk_nodes = (n / 64).max(1024);
            let mut chunks: Vec<(&[u32], usize, &mut [CompactEdge])> = Vec::new();
            let mut rest = csr_edges.as_mut_slice();
            let mut node_start = 0usize;
            let mut edge_base = 0usize;
            while node_start < n {
                let node_end = (node_start + chunk_nodes).min(n);
                let edge_end = edge_offsets[node_end] as usize;
                let (head, tail) = rest.split_at_mut(edge_end - edge_base);
                chunks.push((&edge_offsets[node_start..=node_end], edge_base, head));
                rest = tail;
                node_start = node_end;
                edge_base = edge_end;
            }
            pool.install(|| {
                chunks.into_par_iter().for_each(|(offsets, base, slice)| {
                    sort_range(offsets, base, slice);
                });
            });
        } else {
            sort_range(&edge_offsets, 0, &mut csr_edges);
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
    /// Lazy per-lane parent tables: unused lanes allocate zero memory.
    parents: [Vec<CompactVisit>; 8],
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
    pub fn lane_used(&self, lane: usize) -> bool {
        !self.parents[lane].is_empty()
    }
    #[inline]
    pub fn visit(&self, node: Id, lane: usize) -> Option<Visit> {
        if !self.has(node, lane) {
            return None;
        }
        self.parents[lane]
            .get(node)
            .copied()
            .and_then(CompactVisit::decode)
    }

    pub fn compute(g: &Graph, roots: &[Root]) -> Self {
        let n = g.nodes.len();
        let file_count = g.paths.len();
        let mut masks = vec![0u8; n];
        let mut parents: [Vec<CompactVisit>; 8] = std::array::from_fn(|_| Vec::new());
        let mut queue: Vec<(u32, u8)> = Vec::with_capacity(n.min(65536));

        #[inline]
        fn record_parent(
            parents: &mut [Vec<CompactVisit>; 8],
            n: usize,
            node: usize,
            lane: u8,
            visit: CompactVisit,
        ) {
            let slot = &mut parents[lane as usize];
            if slot.is_empty() {
                slot.resize(n, CompactVisit::unvisited());
            }
            slot[node] = visit;
        }

        for root in roots {
            let file = g.paths.binary_search(&root.file).unwrap();
            let lane = (root.role as u8) * 2;
            let bit = 1u8 << lane;
            if (masks[file] & bit) == 0 {
                masks[file] |= bit;
                record_parent(&mut parents, n, file, lane, CompactVisit::root());
                queue.push((file as u32, lane));
            }
            if root.role == Role::Public || root.mode == RootEntryMode::AllExports {
                for export in g.modules[file].exports.values() {
                    let surface = export.surface;
                    if (masks[surface] & bit) == 0 {
                        masks[surface] |= bit;
                        record_parent(
                            &mut parents,
                            n,
                            surface,
                            lane,
                            CompactVisit::from_edge(file, lane, g.nodes[surface].loc),
                        );
                        queue.push((surface as u32, lane));
                    }
                }
            } else if root.mode == RootEntryMode::DefaultExport
                && let Some(export) = g.modules[file].exports.get("default")
            {
                let surface = export.surface;
                if (masks[surface] & bit) == 0 {
                    masks[surface] |= bit;
                    record_parent(
                        &mut parents,
                        n,
                        surface,
                        lane,
                        CompactVisit::from_edge(file, lane, g.nodes[surface].loc),
                    );
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
                let file = g.nodes[from].file_id();
                let bit = 1u8 << lane;
                if (masks[file] & bit) == 0 {
                    masks[file] |= bit;
                    record_parent(
                        &mut parents,
                        n,
                        file,
                        lane,
                        CompactVisit::from_edge(from, lane, g.nodes[from].loc),
                    );
                }
            }
            for edge in g.edges(from) {
                let next = lane | u8::from(edge.type_only());
                let bit = 1u8 << next;
                let target = edge.target();
                if (masks[target] & bit) == 0 {
                    masks[target] |= bit;
                    record_parent(
                        &mut parents,
                        n,
                        target,
                        next,
                        CompactVisit::from_edge(from, lane, edge.loc),
                    );
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
        let mut closures: FxHashMap<(Id, bool, bool), GapScope> = FxHashMap::default();
        let mut seen_stamp = vec![0u32; graph.nodes.len()];
        let mut file_stamp = vec![0u32; graph.paths.len()];
        let mut stamp = 0u32;
        let mut stack: Vec<Id> = Vec::new();
        let mut touched_files: Vec<u32> = Vec::new();

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
            if record.scope.is_global()
                && pending.target_file.is_none()
                && multi_workspace
                && matches!(record.code, "computed-import" | "bundler-glob")
                && let (Some(file_id), Some(file_path)) = (origin_file_id, &record.file)
            {
                if pending.guard.is_some_and(&node_test_or_tooling_only) {
                    record.scope = GapScope::Files(vec![file_id as u32]);
                } else {
                    let prefix = file_path.rsplit_once('/').map_or("", |(dir, _)| dir);
                    let scoped: Vec<u32> = if prefix.is_empty() {
                        vec![file_id as u32]
                    } else {
                        let slash = format!("{prefix}/");
                        let start = graph.paths.partition_point(|p| p.as_str() < slash.as_str());
                        graph.paths[start..]
                            .iter()
                            .enumerate()
                            .take_while(|(_, p)| p.starts_with(&slash))
                            .filter(|(_, p)| !p[slash.len()..].contains('/'))
                            .map(|(i, _)| (start + i) as u32)
                            .collect()
                    };
                    record.scope = GapScope::Files(if scoped.is_empty() {
                        vec![file_id as u32]
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
                record.scope = closures
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
                                let f = graph.nodes[n].file_id();
                                if file_stamp[f] != stamp {
                                    file_stamp[f] = stamp;
                                    touched_files.push(f as u32);
                                }
                                for edge in graph.edges(n) {
                                    let target = edge.target();
                                    if (self.masks[target] & cover_mask) == 0
                                        && seen_stamp[target] != stamp
                                    {
                                        stack.push(target);
                                    }
                                }
                            }
                        }
                        if !multi_workspace
                            && graph.gaps.iter().any(|gap| {
                                gap.record.scope.is_global()
                                    && gap.target_file.is_none()
                                    && matches!(gap.record.code, "computed-import" | "bundler-glob")
                                    && gap.guard.is_some_and(|n| seen_stamp[n] == stamp)
                            })
                        {
                            GapScope::Global
                        } else {
                            touched_files.sort_unstable();
                            GapScope::Files(touched_files.clone())
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
                && a.scope == b.scope
        });
        for (i, gap) in records.iter_mut().enumerate() {
            gap.id = format!("g{i}");
        }
        records
    }
}
