use std::path::Path;

use serde_json::{Value, json};

use crate::extract::extract;
use crate::graph::{Gap, Graph, Id, ROLES, Reach, Role, Root, Visit};
use crate::project::{Config, Project};

struct Subject {
    file: Id,
    surface: Id,
    binding: Id,
    export_name: Option<String>,
    kind: Option<&'static str>,
}

pub struct Report {
    graph: Graph,
    roots: Vec<Root>,
    excluded: Vec<String>,
    gaps: Vec<Gap>,
    reach: Reach,
    subjects: Vec<Subject>,
}

impl Report {
    pub fn build(root: &Path, config: Config) -> Result<Self, String> {
        let mut project = Project::open(root, config)?;
        let parsed: Vec<_> = (0..project.graph.paths.len())
            .map(|f| extract(&mut project, f))
            .collect();
        if !project
            .roots
            .iter()
            .any(|r| parsed[project.graph.paths.binary_search(&r.file).unwrap()])
        {
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
        let mut graph = project.graph;
        graph.link();
        let reach = Reach::compute(&graph, &project.roots);
        let gaps = reach.gaps(&graph);
        let mut subjects = vec![];
        for (file, module) in graph.modules.iter().enumerate() {
            let kind =
                (!reach.visited[file].iter().any(Option::is_some)).then_some("unreferenced-module");
            subjects.push(Subject {
                file,
                surface: file,
                binding: file,
                export_name: None,
                kind,
            });
            for (name, e) in &module.exports {
                let lanes = &reach.visited[e.binding];
                let used = |role: Role| {
                    lanes[(role as usize) * 2..(role as usize) * 2 + 2]
                        .iter()
                        .any(Option::is_some)
                };
                let kind = match (
                    used(Role::Runtime),
                    used(Role::Test),
                    used(Role::Tooling),
                    used(Role::Public),
                ) {
                    (false, false, false, false) => Some("unused-export"),
                    (false, true, false, false) => Some("test-only-export"),
                    (false, false, true, false) => Some("tooling-only-export"),
                    _ => None,
                };
                subjects.push(Subject {
                    file,
                    surface: e.surface,
                    binding: e.binding,
                    export_name: Some(name.clone()),
                    kind,
                });
            }
        }
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
            subjects,
        })
    }
    pub fn metadata(&self) -> Value {
        json!({
            "roots": self.roots,
            "coverage": {
                "status": if self.gaps.is_empty() { "complete" } else { "partial" },
                "files": self.graph.paths,
                "excluded": self.excluded,
                "gaps": self.gaps
            }
        })
    }
    fn record(&self, index: usize, s: &Subject) -> Value {
        let visits = &self.reach.visited[s.binding];
        let module = &self.reach.visited[s.file];
        let contexts = |visits: &[Option<Visit>; 8], phase: usize| {
            ROLES[..3]
                .iter()
                .copied()
                .filter(|r| visits[(*r as usize) * 2 + phase].is_some())
                .collect::<Vec<_>>()
        };
        let module_only = s.export_name.is_none();
        json!({
            "id": format!("s{index}"),
            "kind": s.kind,
            "file": self.graph.paths[s.file],
            "exportName": s.export_name,
            "location": self.graph.nodes[s.surface].loc,
            "usage": {
                "value": if module_only { vec![] } else { contexts(visits, 0) },
                "type": contexts(visits, 1),
                "contract": visits[6..].iter().any(Option::is_some),
                "evaluation": contexts(module, 0)
            }
        })
    }
    pub fn findings(&self, kind: Option<&str>) -> Value {
        Value::Array(
            self.subjects
                .iter()
                .enumerate()
                .filter(|(_, s)| s.kind.is_some() && (kind.is_none() || s.kind == kind))
                .map(|(i, s)| self.record(i, s))
                .collect(),
        )
    }
    fn witness(&self, mut id: Id, mut lane: usize) -> Value {
        let mut path = vec![];
        loop {
            let node = &self.graph.nodes[id];
            let visit = self.reach.visited[id][lane].unwrap();
            let via = match visit {
                Visit::Root => Value::Null,
                Visit::From {
                    node: from, loc, ..
                } => json!({"file":self.graph.paths[self.graph.nodes[from].file],"location":loc}),
            };
            path.push(json!({"file":self.graph.paths[node.file],"kind":node.kind,"name":node.name,"location":node.loc,"via":via}));
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
            file.and_then(|file| {
                self.subjects
                    .binary_search_by(|s| {
                        (s.file, s.export_name.as_deref())
                            .cmp(&(file, selector["exportName"].as_str()))
                    })
                    .ok()
            })
        }
        .ok_or("Unknown analysis subject.")?;
        let s = &self.subjects[index];
        let mut result = self.record(index, s);
        let mut witnesses = vec![];
        for role in ROLES {
            for phase in 0..2 {
                let lane = (role as usize) * 2 + phase;
                if self.reach.visited[s.binding][lane].is_some() {
                    let fact = if role == Role::Public {
                        "contract"
                    } else if phase == 1 {
                        "type"
                    } else if s.export_name.is_none() {
                        "evaluation"
                    } else {
                        "value"
                    };
                    witnesses
                        .push(json!({"role":role,"fact":fact,"path":self.witness(s.binding,lane)}));
                }
            }
            let lane = (role as usize) * 2;
            if s.export_name.is_some() && self.reach.visited[s.file][lane].is_some() {
                witnesses.push(
                    json!({"role":role,"fact":"evaluation","path":self.witness(s.file,lane)}),
                );
            }
        }
        result["witnesses"] = Value::Array(witnesses);
        result["reason"] = match s.kind {
            Some("unused-export") => "No retaining binding reference from the effective roots. Module initialization is tracked separately.",
            Some("unreferenced-module") => "No evaluation, type, or public-contract path from the effective roots.",
            Some("test-only-export") => "Only test roots have known retaining references.",
            Some("tooling-only-export") => "Only tooling roots have known retaining references.",
            _ => "The effective roots retain this subject."
        }.into();
        Ok(result)
    }
}
