//! Reverse reachability for questions about a single binding. Keep only states
//! reached by tests or by a concrete read in an unrooted workspace consumer.
use std::collections::{BTreeMap, VecDeque};

use rustc_hash::FxHashMap;
use serde_json::{Value, json};

use crate::graph::{Gap, Graph, Id, Loc, Reach, Role};

#[derive(Clone, Copy, Default)]
struct Step {
    state: usize,
    loc: Loc,
}

#[derive(Default)]
pub struct Evidence {
    offsets: Vec<usize>,
    incoming: Vec<Step>,
    test_files: Vec<bool>,
    reads: FxHashMap<usize, Vec<Step>>,
    uncertain: Vec<bool>,
    gap_id: Option<String>,
}

#[inline]
fn state(node: Id, phase: usize) -> usize {
    node * 2 + phase
}

#[inline]
fn transitions(graph: &Graph, source: usize, mut visit: impl FnMut(usize, Loc)) {
    let id = source / 2;
    let phase = source % 2;
    let node = &graph.nodes[id];
    if phase == 1 {
        if node.file == id {
            return;
        }
        visit(state(node.file, 1), node.loc);
    }
    for edge in graph.edges(id) {
        visit(
            state(edge.target, phase | usize::from(edge.type_only)),
            edge.loc,
        );
    }
}

// Follow forwarding edges so that a local barrel in front of a workspace
// boundary has the same meaning as a direct cross-package import.
fn crossing_imports(graph: &Graph, workspace: &[usize]) -> Vec<bool> {
    let n = graph.nodes.len();
    let mut crossing = vec![false; n];
    let mut queue: Vec<u32> = Vec::new();
    let mut in_offsets = vec![0u32; n + 1];
    for (id, node) in graph.nodes.iter().enumerate() {
        if !matches!(node.kind, "import" | "export" | "re-export") {
            continue;
        }
        let ws = workspace[node.file];
        for edge in graph.edges(id) {
            if ws != workspace[graph.nodes[edge.target].file] {
                if !crossing[id] {
                    crossing[id] = true;
                    queue.push(id as u32);
                }
            } else {
                in_offsets[edge.target + 1] += 1;
            }
        }
    }
    if queue.is_empty() {
        return crossing;
    }
    for i in 1..=n {
        in_offsets[i] += in_offsets[i - 1];
    }
    let mut cursor = in_offsets[..n].to_vec();
    let mut incoming = vec![0u32; in_offsets[n] as usize];
    for (id, node) in graph.nodes.iter().enumerate() {
        if !matches!(node.kind, "import" | "export" | "re-export") {
            continue;
        }
        let ws = workspace[node.file];
        for edge in graph.edges(id) {
            if ws == workspace[graph.nodes[edge.target].file] {
                let slot = cursor[edge.target] as usize;
                incoming[slot] = id as u32;
                cursor[edge.target] += 1;
            }
        }
    }
    let mut head = 0;
    while head < queue.len() {
        let target = queue[head] as usize;
        head += 1;
        let range = in_offsets[target] as usize..in_offsets[target + 1] as usize;
        for &id_u32 in &incoming[range] {
            let id = id_u32 as usize;
            if !crossing[id] {
                crossing[id] = true;
                queue.push(id as u32);
            }
        }
    }
    crossing
}

/// Encode common suffixes once at the native boundary. A chain of N callers
/// can have N distinct paths to a subject with only O(N) distinct steps.
#[derive(Default)]
struct WorkspacePaths(Vec<Value>);

impl WorkspacePaths {
    fn push(&mut self, graph: &Graph, id: Id, tail: Option<(usize, Loc)>) -> usize {
        let node = &graph.nodes[id];
        let index = self.0.len();
        self.0.push(json!({
            "file": graph.paths[node.file], "kind": node.kind, "name": node.name, "location": node.loc,
            "next": tail.map(|(next, _)| next), "edge": tail.map(|(_, loc)| loc),
        }));
        index
    }

    fn intern(
        &mut self,
        graph: &Graph,
        start: usize,
        next: &FxHashMap<usize, Option<Step>>,
        known: &mut FxHashMap<usize, usize>,
    ) -> usize {
        let mut missing = Vec::new();
        let mut current = start;
        while !known.contains_key(&current) {
            missing.push(current);
            let Some(step) = next[&current] else {
                break;
            };
            current = step.state;
        }
        for state in missing.into_iter().rev() {
            let tail = next[&state].map(|step| (known[&step.state], step.loc));
            known.insert(state, self.push(graph, state / 2, tail));
        }
        known[&start]
    }
}

impl Evidence {
    pub fn build(
        graph: &Graph,
        reach: &Reach,
        roles: &[u8],
        workspace: &[usize],
        gaps: &mut Vec<Gap>,
    ) -> Self {
        let mut result = Self::default();
        let mut queue = VecDeque::new();
        let mut possible = vec![false; graph.nodes.len() * 2];
        let mut location = None;
        if workspace.iter().any(|&id| id != 0) {
            let crossing = crossing_imports(graph, workspace);
            for (id, node) in graph.nodes.iter().enumerate() {
                if reach.any(node.file)
                    || !matches!(node.kind, "binding" | "module" | "declaration" | "export")
                {
                    continue;
                }
                for edge in graph.edges(id) {
                    let target_node = &graph.nodes[edge.target];
                    let imports_binding = target_node.kind == "import" && crossing[edge.target];
                    let loads_module = target_node.kind == "module"
                        && workspace[node.file] != workspace[target_node.file];
                    if !imports_binding && !loads_module {
                        continue;
                    }
                    let target = state(edge.target, usize::from(edge.type_only));
                    result.reads.entry(target).or_default().push(Step {
                        state: state(id, 0),
                        loc: edge.loc,
                    });
                    location.get_or_insert((node.file, edge.loc));
                    if !possible[target] {
                        possible[target] = true;
                        queue.push_back(target);
                    }
                }
            }
        }
        if let Some((file, loc)) = location {
            let mut affected = vec![false; graph.paths.len()];
            while let Some(s) = queue.pop_front() {
                affected[graph.nodes[s / 2].file] = true;
                transitions(graph, s, |t, _| {
                    if !possible[t] {
                        possible[t] = true;
                        queue.push_back(t);
                    }
                });
            }
            result.uncertain = possible
                .as_chunks::<2>()
                .0
                .iter()
                .map(|phases| phases[0] || phases[1])
                .collect();
            for (file, &found) in affected.iter().enumerate() {
                result.uncertain[file] |= found;
            }
            let id = "unrooted-workspace".to_string();
            result.gap_id = Some(id.clone());
            let files = graph
                .paths
                .iter()
                .zip(affected)
                .filter(|(_, found)| *found)
                .map(|(p, _)| p.clone())
                .collect();
            gaps.push(Gap {
                id,
                code: "unrooted-workspace-reference",
                file: Some(graph.paths[file].clone()),
                location: Some(loc),
                affected_files: Some(files),
                message: concat!(
                    "Cross-workspace references originate in files with no effective root path. ",
                    "Supply the callers' entry roles. Explanations identify the source locations."
                )
                .into(),
                test_only: false,
            });
        }
        result.test_files = roles
            .iter()
            .map(|r| r & (1 << Role::Test as usize) != 0)
            .collect();
        if result.reads.is_empty() && !result.test_files.iter().any(|v| *v) {
            return result;
        }
        // Compressed sparse row storage avoids one allocation per incoming list.
        let states = possible.len();
        let each_edge = |emit: &mut dyn FnMut(usize, usize, Loc)| {
            for (s, &unknown) in possible.iter().enumerate() {
                if unknown || reach.has(s / 2, Role::Test as usize * 2 + s % 2) {
                    transitions(graph, s, |target, loc| emit(s, target, loc));
                }
            }
        };
        let mut offsets = vec![0; states + 1];
        each_edge(&mut |_, to, _| offsets[to + 1] += 1);
        for i in 1..offsets.len() {
            offsets[i] += offsets[i - 1];
        }
        let mut cursor = offsets[..states].to_vec();
        let mut incoming = vec![Step::default(); offsets[states]];
        each_edge(&mut |from, to, loc| {
            incoming[cursor[to]] = Step { state: from, loc };
            cursor[to] += 1;
        });
        result.offsets = offsets;
        result.incoming = incoming;
        result
    }

    pub fn gap_for(&self, node: Id) -> Option<&str> {
        if self.uncertain.get(node) == Some(&true) {
            self.gap_id.as_deref()
        } else {
            None
        }
    }

    pub fn explain(
        &self,
        graph: &Graph,
        binding: Id,
        module: bool,
    ) -> (Vec<Value>, Vec<Value>, Vec<Value>) {
        let mut tests = BTreeMap::<Id, Vec<&str>>::new();
        let mut sources = BTreeMap::new();
        let mut paths = WorkspacePaths::default();
        if self.offsets.is_empty() {
            return (vec![], vec![], vec![]);
        }
        for phase in 0..2 {
            let fact = if phase == 1 {
                "type"
            } else if module {
                "evaluation"
            } else {
                "value"
            };
            let mut known = FxHashMap::default();
            let start = state(binding, phase);
            let mut next = FxHashMap::default();
            next.insert(start, None);
            let mut queue = VecDeque::from([start]);
            while let Some(s) = queue.pop_front() {
                if s % 2 == 0 && self.test_files.get(s / 2) == Some(&true) {
                    tests.entry(s / 2).or_default().push(fact);
                }
                if let Some(reads) = self.reads.get(&s) {
                    for read in reads {
                        let owner = &graph.nodes[read.state / 2];
                        let key = (owner.file, read.loc.start, read.loc.end, phase);
                        sources.entry(key).or_insert_with(|| {
                            let suffix = paths.intern(graph, s, &next, &mut known);
                            let first = paths.push(graph, read.state / 2, Some((suffix, read.loc)));
                            json!({
                                "file": graph.paths[owner.file], "location": read.loc,
                                "fact": fact, "first": first
                            })
                        });
                    }
                }
                for parent in &self.incoming[self.offsets[s]..self.offsets[s + 1]] {
                    if let std::collections::hash_map::Entry::Vacant(entry) =
                        next.entry(parent.state)
                    {
                        entry.insert(Some(Step {
                            state: s,
                            loc: parent.loc,
                        }));
                        queue.push_back(parent.state);
                    }
                }
            }
        }
        (
            tests
                .into_iter()
                .map(|(f, facts)| json!({"file":graph.paths[f],"facts":facts}))
                .collect(),
            sources.into_values().collect(),
            paths.0,
        )
    }
}
