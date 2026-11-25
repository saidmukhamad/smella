//! Reverse reachability for questions about a single binding. Keep only states
//! reached by tests or by a concrete read in an unrooted workspace consumer.
use std::collections::{BTreeMap, HashMap, VecDeque};

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
    reads: HashMap<usize, Vec<Step>>,
    uncertain: Vec<bool>,
    gap_id: Option<String>,
}

fn state(node: Id, phase: usize) -> usize {
    node * 2 + phase
}

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
    for edge in &node.edges {
        visit(
            state(edge.target, phase | usize::from(edge.type_only)),
            edge.loc,
        );
    }
}

// Follow forwarding edges so that a local barrel in front of a workspace
// boundary has the same meaning as a direct cross-package import.
fn crossing_imports(graph: &Graph, workspace: &[usize]) -> Vec<bool> {
    let mut crossing = vec![false; graph.nodes.len()];
    let mut incoming = vec![Vec::new(); graph.nodes.len()];
    let mut queue = VecDeque::new();
    for (id, node) in graph.nodes.iter().enumerate() {
        if !matches!(node.kind, "import" | "export" | "re-export") {
            continue;
        }
        for edge in &node.edges {
            if workspace[node.file] != workspace[graph.nodes[edge.target].file] {
                if !crossing[id] {
                    crossing[id] = true;
                    queue.push_back(id);
                }
            } else {
                incoming[edge.target].push(id);
            }
        }
    }
    while let Some(target) = queue.pop_front() {
        for &id in &incoming[target] {
            if !crossing[id] {
                crossing[id] = true;
                queue.push_back(id);
            }
        }
    }
    crossing
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
                if reach.visited[node.file].iter().any(Option::is_some)
                    || !matches!(node.kind, "binding" | "module" | "declaration" | "export")
                {
                    continue;
                }
                for edge in &node.edges {
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
                .chunks_exact(2)
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
                if unknown || reach.visited[s / 2][Role::Test as usize * 2 + s % 2].is_some() {
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

    pub fn explain(&self, graph: &Graph, binding: Id, module: bool) -> (Vec<Value>, Vec<Value>) {
        let mut tests = BTreeMap::<Id, Vec<&str>>::new();
        let mut sources = BTreeMap::new();
        if self.offsets.is_empty() {
            return (vec![], vec![]);
        }
        for phase in 0..2 {
            let fact = if phase == 1 {
                "type"
            } else if module {
                "evaluation"
            } else {
                "value"
            };
            let start = state(binding, phase);
            let mut next = HashMap::from([(start, None)]);
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
                            let mut path = vec![graph.step(read.state / 2, None)];
                            let mut current = s;
                            let mut via = (owner.file, read.loc);
                            loop {
                                path.push(graph.step(current / 2, Some(via)));
                                let Some(step): Option<Step> = next[&current] else {
                                    break;
                                };
                                via = (graph.nodes[current / 2].file, step.loc);
                                current = step.state;
                            }
                            json!({
                                "file": graph.paths[owner.file], "location": read.loc,
                                "fact": fact, "path": path
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
        )
    }
}
