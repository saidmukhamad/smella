use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::Serialize;

pub type Id = usize;
pub type Link = (Id, Id, String, bool, Loc);

#[derive(Clone, Copy, Default, Serialize)]
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
}

impl Graph {
    pub fn new(paths: Vec<String>) -> Self {
        let mut g = Self {
            paths,
            nodes: vec![],
            modules: vec![],
            gaps: vec![],
            links: vec![],
        };
        // Module vertex IDs coincide with file IDs.
        for f in 0..g.paths.len() {
            g.node(f, "module", "", Loc::default());
            g.modules.push(Module::default());
        }
        g
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
        let record = Gap {
            code,
            file: file.map(|f| self.paths[f].clone()),
            location: loc,
            message,
            affected_files: None,
            id: String::new(),
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
        let mut queue = VecDeque::new();
        for (file, module) in self.modules.iter().enumerate() {
            for star in &module.stars {
                reverse[star.target].push((file, star.type_only, star.loc));
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
                queue.push_back((file, name.clone()));
            }
        }
        // Resolve identity through named forwards and star diamonds. A star name
        // with two distinct declarations is absent from the ESM namespace.
        let links = std::mem::take(&mut self.links);
        let mut forwarded = BTreeSet::new();
        for (from, file, name, type_only, loc) in &links {
            if self.nodes[*from].kind == "re-export" {
                forwarded.insert(*from);
                if let Some(target) = self.modules[*file].exports.get(name) {
                    self.edge(*from, target.surface, *type_only, *loc);
                }
            }
        }
        let mut origins = vec![Vec::new(); self.nodes.len()];
        let mut dependents = vec![Vec::new(); self.nodes.len()];
        let mut queue = VecDeque::new();
        for module in &self.modules {
            for export in module.exports.values() {
                let id = export.surface;
                if export.explicit && !forwarded.contains(&id) {
                    origins[id].push(export.binding);
                    queue.push_back((id, export.binding));
                } else {
                    for edge in &self.nodes[id].edges {
                        dependents[edge.target].push(id);
                    }
                }
            }
        }
        while let Some((id, origin)) = queue.pop_front() {
            for &next in &dependents[id] {
                if origins[next].len() < 2 && !origins[next].contains(&origin) {
                    origins[next].push(origin);
                    queue.push_back((next, origin));
                }
            }
        }
        for module in &mut self.modules {
            module
                .exports
                .retain(|_, export| export.explicit || origins[export.surface].len() == 1);
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
            if let Some(target) = target.filter(|e| origins[e.surface].len() == 1) {
                if !forwarded.contains(&from) {
                    self.edge(from, target.surface, type_only, loc);
                }
            } else if self.modules[file].cjs_opaque_default
                && let Some(default_target) = self.modules[file].exports.get("default")
            {
                self.edge(from, default_target.surface, type_only, loc);
            } else {
                if forwarded.contains(&from) {
                    self.nodes[from].edges.clear();
                }
                self.gap(
                    "missing-export",
                    Some(self.nodes[from].file),
                    Some(loc),
                    format!("{} has no unambiguous export {name}.", self.paths[file]),
                    None,
                    None,
                );
            }
        }
        for node in &mut self.nodes {
            node.edges
                .sort_by_key(|e| (e.loc.start, e.target, e.type_only));
        }
    }
}

#[derive(Clone, Copy)]
pub enum Visit {
    Root,
    From { node: Id, lane: usize, loc: Loc },
}

pub struct Reach {
    pub visited: Vec<[Option<Visit>; 8]>,
}

impl Reach {
    pub fn compute(g: &Graph, roots: &[Root]) -> Self {
        let mut visited = vec![[None; 8]; g.nodes.len()];
        let mut queue = VecDeque::new();
        for root in roots {
            let file = g.paths.binary_search(&root.file).unwrap();
            let lane = root.role as usize * 2;
            visited[file][lane] = Some(Visit::Root);
            queue.push_back((file, lane));
            if root.role == Role::Public || root.mode == RootEntryMode::AllExports {
                for export in g.modules[file].exports.values() {
                    let surface = export.surface;
                    if visited[surface][lane].is_none() {
                        visited[surface][lane] = Some(Visit::From {
                            node: file,
                            lane,
                            loc: g.nodes[surface].loc,
                        });
                        queue.push_back((surface, lane));
                    }
                }
            } else if root.mode == RootEntryMode::DefaultExport
                && let Some(export) = g.modules[file].exports.get("default")
            {
                let surface = export.surface;
                if visited[surface][lane].is_none() {
                    visited[surface][lane] = Some(Visit::From {
                        node: file,
                        lane,
                        loc: g.nodes[surface].loc,
                    });
                    queue.push_back((surface, lane));
                }
            }
        }
        while let Some((from, lane)) = queue.pop_front() {
            // Type retention of a module never activates its initializers.
            if lane % 2 == 1 && from < g.paths.len() {
                continue;
            }
            let file = g.nodes[from].file;
            if lane % 2 == 1 && visited[file][lane].is_none() {
                visited[file][lane] = Some(Visit::From {
                    node: from,
                    lane,
                    loc: g.nodes[from].loc,
                });
            }
            for edge in &g.nodes[from].edges {
                let next = lane | usize::from(edge.type_only);
                if visited[edge.target][next].is_none() {
                    visited[edge.target][next] = Some(Visit::From {
                        node: from,
                        lane,
                        loc: edge.loc,
                    });
                    queue.push_back((edge.target, next));
                }
            }
        }
        Self { visited }
    }

    pub fn gaps(&self, graph: &Graph, uncertain: impl Fn(Id) -> bool) -> Vec<Gap> {
        let active = |guard: Option<Id>| {
            guard.is_none_or(|n| self.visited[n].iter().any(Option::is_some) || uncertain(n))
        };
        let mut records = vec![];
        // Escaped namespace values can retain implementations beyond the target file.
        // Follow their possible dependencies without recording any positive usage.
        let mut closures = BTreeMap::new();
        for pending in &graph.gaps {
            if !active(pending.guard) {
                continue;
            }
            let mut record = pending.record.clone();
            if pending
                .guard
                .is_some_and(|n| self.visited[n].iter().all(Option::is_none))
            {
                record.message =
                    format!("If the unrooted workspace caller runs: {}", record.message);
            }
            if let Some(target) = pending.target_file {
                record.affected_files = closures
                    .entry(target)
                    .or_insert_with(|| {
                        let mut stack = vec![target];
                        stack.extend(graph.modules[target].exports.values().map(|e| e.surface));
                        let mut seen = BTreeSet::new();
                        let mut files = BTreeSet::new();
                        while let Some(n) = stack.pop() {
                            if seen.insert(n) {
                                files.insert(graph.paths[graph.nodes[n].file].clone());
                                stack.extend(graph.nodes[n].edges.iter().map(|e| e.target));
                            }
                        }
                        if graph
                            .gaps
                            .iter()
                            .any(|gap| gap.guard.is_some_and(|n| seen.contains(&n)))
                        {
                            None
                        } else {
                            Some(files.into_iter().collect())
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
