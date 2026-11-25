use std::collections::{BTreeMap, HashMap};
use std::fs;

use oxc_allocator::Allocator;
use oxc_ast::{AstKind, ast::Expression};
use oxc_index::Idx;
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::{GetSpan, SourceType, Span};
use oxc_syntax::module_record::{ExportExportName, ExportImportName, ImportImportName};
use oxc_syntax::node::NodeId;

use crate::graph::{Edge, Export, Id, Loc};
use crate::project::Project;

#[derive(Clone, Copy)]
struct Owner {
    binding: Id,
    // An eager initializer can retain dependencies even if its result is never read.
    eager: bool,
}
impl Owner {
    fn sources(self, module: Id) -> impl Iterator<Item = Id> {
        std::iter::once(self.binding)
            .chain((self.eager && self.binding != module).then_some(module))
    }
}

fn target(
    project: &mut Project,
    file: Id,
    spec: &str,
    span: Span,
    guard: Option<Id>,
    type_only: bool,
) -> Option<Id> {
    match project.resolve(file, spec, type_only) {
        Ok(target) => target,
        Err(message) => {
            project.graph.gap(
                "unresolved-import",
                Some(file),
                Some(span.into()),
                message,
                guard,
                None,
            );
            None
        }
    }
}

pub fn extract(project: &mut Project, file: Id) -> bool {
    let path = project.root.join(&project.graph.paths[file]);
    let source = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) => {
            project
                .graph
                .gap("read-error", Some(file), None, e.to_string(), None, None);
            return false;
        }
    };
    let arena = Allocator::default();
    let mut parsed = Parser::new(&arena, &source, SourceType::from_path(&path).unwrap()).parse();
    let declaration_file = parsed.program.source_type.is_typescript_definition();
    if declaration_file {
        // Oxc parses ambient syntax in declaration mode but skips its semantic walk.
        // Bind the already parsed declarations using the TS scope builder.
        parsed.program.source_type = parsed.program.source_type.with_typescript(true);
    }
    let built = SemanticBuilder::new()
        .with_check_syntax_error(!declaration_file)
        .build(&parsed.program);
    let mut ok = true;
    for error in parsed.errors.iter().chain(&built.errors) {
        ok = false;
        let loc = error
            .labels
            .as_ref()
            .and_then(|labels| labels.first())
            .map(|s| Loc {
                start: s.offset() as u32,
                end: (s.offset() + s.len()) as u32,
            });
        project.graph.gap(
            "parse-error",
            Some(file),
            loc,
            error.message.to_string(),
            None,
            None,
        );
    }
    if parsed.panicked {
        return false;
    }
    let sem = built.semantic;
    let nodes = sem.nodes();
    let scoping = sem.scoping();
    let record = parsed.module_record;
    let mut bindings = HashMap::new();
    let mut declarations: HashMap<NodeId, Vec<Id>> = HashMap::new();
    for symbol in scoping
        .symbol_ids()
        .filter(|&s| scoping.symbol_scope_id(s) == scoping.root_scope_id())
    {
        let name = scoping.symbol_name(symbol);
        let flags = scoping.symbol_flags(symbol);
        let id = project.graph.node(
            file,
            if flags.is_import() || flags.is_type_import() {
                "import"
            } else {
                "binding"
            },
            name,
            scoping.symbol_span(symbol).into(),
        );
        bindings.insert(symbol, id);
        declarations
            .entry(scoping.symbol_declaration(symbol))
            .or_default()
            .push(id);
        for decl in scoping.symbol_redeclarations(symbol) {
            declarations.entry(decl.declaration).or_default().push(id);
        }
    }
    let local = |name: &str| {
        scoping
            .get_root_binding(name)
            .and_then(|s| bindings.get(&s).copied())
    };

    let mut import_phases = HashMap::new();
    for import in &record.import_entries {
        let only_types = import_phases
            .entry(import.statement_span.start)
            .or_insert(true);
        *only_types &= import.is_type;
    }
    for export in &record.indirect_export_entries {
        let only_types = import_phases
            .entry(export.statement_span.start)
            .or_insert(true);
        *only_types &= export.is_type;
    }
    let mut requests = BTreeMap::new();
    let mut occurrences: Vec<_> = record
        .requested_modules
        .iter()
        .flat_map(|(s, items)| items.iter().map(move |item| (s.as_str(), item)))
        .collect();
    occurrences.sort_by_key(|(_, item)| item.span.start);
    for (spec, item) in occurrences {
        let type_only = declaration_file
            || item.is_type
            || import_phases.get(&item.statement_span.start) == Some(&true);
        let to = target(project, file, spec, item.span, None, type_only);
        requests.insert(item.statement_span.start, to);
        if let Some(to) = to {
            project.graph.edge(file, to, type_only, item.span.into());
        }
    }
    let mut namespaces = HashMap::new();
    for import in &record.import_entries {
        let Some(from) = local(import.local_name.name.as_str()) else {
            continue;
        };
        let Some(to) = requests
            .get(&import.statement_span.start)
            .copied()
            .flatten()
        else {
            continue;
        };
        let name = match &import.import_name {
            ImportImportName::Name(n) => n.name.as_str(),
            ImportImportName::Default(_) => "default",
            ImportImportName::NamespaceObject => {
                namespaces.insert(from, (to, import.is_type));
                continue;
            }
        };
        project.graph.links.push((
            from,
            to,
            name.into(),
            import.is_type,
            import.local_name.span.into(),
        ));
    }
    let mut default_expr = None;
    for export in record
        .local_export_entries
        .iter()
        .chain(&record.indirect_export_entries)
    {
        let name = match &export.export_name {
            ExportExportName::Name(n) => n.name.as_str(),
            ExportExportName::Default(_) => "default",
            ExportExportName::Null => continue,
        };
        let loc = export.export_name.span().unwrap_or(export.span).into();
        let kind = if export.module_request.is_some() {
            "re-export"
        } else {
            "export"
        };
        let surface = project.graph.node(file, kind, name, loc);
        let mut binding = surface;
        if export.module_request.is_some() {
            if let Some(to) = requests
                .get(&export.statement_span.start)
                .copied()
                .flatten()
            {
                if let ExportImportName::Name(n) = &export.import_name {
                    // Oxc's synthetic entry for 'import local from ...; export {local}'
                    // uses the import statement span and the local default-import name.
                    let imported_default = record.import_entries.iter().any(|i| {
                        i.statement_span == export.statement_span
                            && i.import_name.is_default()
                            && i.local_name.name == n.name
                    });
                    let key = if imported_default {
                        "default"
                    } else {
                        n.name.as_str()
                    };
                    project
                        .graph
                        .links
                        .push((surface, to, key.into(), export.is_type, loc));
                } else {
                    project.graph.gap(
                        "namespace-escape",
                        Some(file),
                        Some(loc),
                        "A re-export exposes the module namespace.".into(),
                        Some(surface),
                        Some(to),
                    );
                }
            }
        } else if let Some(id) = export.local_name.name().and_then(|n| local(n.as_str())) {
            binding = id;
            project.graph.edge(surface, id, export.is_type, loc);
            if let Some(&(to, _)) = namespaces.get(&id) {
                project.graph.gap(
                    "namespace-escape",
                    Some(file),
                    Some(loc),
                    "A re-export exposes the module namespace.".into(),
                    Some(surface),
                    Some(to),
                );
            }
        } else if name == "default" {
            default_expr = Some(surface);
        } else {
            project.graph.gap(
                "missing-binding",
                Some(file),
                Some(loc),
                format!("No local declaration resolved for {name}."),
                None,
                None,
            );
        }
        project.graph.modules[file].exports.insert(
            name.into(),
            Export {
                surface,
                binding,
                explicit: true,
            },
        );
    }
    for export in &record.star_export_entries {
        if let Some(spec) = &export.module_request {
            if let Some(to) = requests
                .get(&export.statement_span.start)
                .copied()
                .flatten()
            {
                project.graph.modules[file].stars.push(Edge {
                    target: to,
                    type_only: export.is_type,
                    loc: spec.span.into(),
                });
            } else if !project.graph.gaps.iter().any(|g| {
                g.record.code == "unresolved-import"
                    && g.record.file.as_ref() == Some(&project.graph.paths[file])
            }) {
                project.graph.gap(
                    "external-star",
                    Some(file),
                    Some(spec.span.into()),
                    format!(
                        "The export names of external module {:?} are outside the source scope.",
                        spec.name.as_str()
                    ),
                    None,
                    None,
                );
            }
        }
    }
    let exported: std::collections::HashSet<_> = project.graph.modules[file]
        .exports
        .values()
        .map(|e| e.binding)
        .collect();
    let entry_file = project
        .roots
        .iter()
        .any(|r| r.file == project.graph.paths[file] && r.role != crate::graph::Role::Public);
    let module_owner = Owner {
        binding: file,
        eager: false,
    };
    let mut owners = vec![module_owner; nodes.len()];
    for node in nodes.iter() {
        let mut owner = nodes
            .parent_id(node.id())
            .map_or(module_owner, |p| owners[p.index()]);
        if let Some(ids) = declarations.get(&node.id()) {
            let id = if ids.len() == 1 {
                ids[0]
            } else {
                let group = project
                    .graph
                    .node(file, "declaration", "", node.span().into());
                for &id in ids {
                    project.graph.edge(id, group, false, node.span().into());
                }
                group
            };
            let eager = match node.kind() {
                AstKind::VariableDeclarator(v) => !v
                    .init
                    .as_ref()
                    .is_some_and(|e| e.get_inner_expression().is_function()),
                AstKind::Class(_) | AstKind::TSEnumDeclaration(_) => true,
                AstKind::TSTypeAliasDeclaration(_) | AstKind::TSInterfaceDeclaration(_) => {
                    entry_file && !exported.contains(&id)
                }
                _ => false,
            };
            owner = Owner { binding: id, eager };
        } else if matches!(node.kind(), AstKind::ExportDefaultDeclaration(_))
            && let Some(binding) = default_expr
        {
            owner = Owner {
                binding,
                eager: true,
            };
        } else if matches!(
            node.kind(),
            AstKind::Function(_) | AstKind::ArrowFunctionExpression(_)
        ) && (matches!(
            nodes.parent_kind(node.id()),
            Some(AstKind::ExportDefaultDeclaration(_) | AstKind::MethodDefinition(_))
        )) {
            owner.eager = false;
        }
        owners[node.id().index()] = owner;
        let loc = node.span().into();
        match node.kind() {
            AstKind::IdentifierReference(ident) => {
                if matches!(
                    nodes.parent_kind(node.id()),
                    Some(AstKind::ExportSpecifier(_))
                ) {
                    continue;
                }
                let Some(rid) = ident.reference_id.get() else {
                    continue;
                };
                let reference = scoping.get_reference(rid);
                if let Some(&binding) = reference.symbol_id().and_then(|s| bindings.get(&s)) {
                    let is_type = declaration_file
                        || reference.is_type()
                        || reference.flags().is_value_as_type()
                        || nodes
                            .ancestor_kinds(node.id())
                            .any(|k| matches!(k, AstKind::TSTypeQuery(_)));
                    for from in owner.sources(file) {
                        if let Some(&(to, type_import)) = namespaces.get(&binding) {
                            let property = match nodes.parent_kind(node.id()) {
                                Some(AstKind::StaticMemberExpression(member))
                                    if member.object.span() == ident.span =>
                                {
                                    Some(member.property.name.as_str())
                                }
                                Some(AstKind::ComputedMemberExpression(m))
                                    if m.object.span() == ident.span =>
                                {
                                    match &m.expression {
                                        Expression::StringLiteral(s) => Some(s.value.as_str()),
                                        _ => None,
                                    }
                                }
                                Some(AstKind::JSXMemberExpression(m)) => {
                                    Some(m.property.name.as_str())
                                }
                                Some(AstKind::TSQualifiedName(n)) => Some(n.right.name.as_str()),
                                _ => None,
                            };
                            if let Some(property) = property {
                                // Each access gets its own vertex so two properties of the same namespace cannot contaminate each other.
                                let access = project.graph.node(
                                    file,
                                    "import",
                                    &format!("{}.{}", ident.name, property),
                                    loc,
                                );
                                project
                                    .graph
                                    .edge(from, access, is_type || type_import, loc);
                                project.graph.links.push((
                                    access,
                                    to,
                                    property.into(),
                                    is_type || type_import,
                                    loc,
                                ));
                            } else {
                                project.graph.gap(
                                    "namespace-escape",
                                    Some(file),
                                    Some(loc),
                                    format!(
                                        "The properties read from namespace {} are unknown.",
                                        ident.name
                                    ),
                                    Some(from),
                                    Some(to),
                                );
                            }
                        } else {
                            project.graph.edge(from, binding, is_type, loc);
                        }
                    }
                } else if reference.symbol_id().is_none()
                    && matches!(
                        ident.name.as_str(),
                        "require" | "exports" | "module" | "eval"
                    )
                {
                    for from in owner.sources(file) {
                        project.graph.gap(
                            "unsupported-runtime",
                            Some(file),
                            Some(loc),
                            format!("Runtime use of {} is outside the ESM model.", ident.name),
                            Some(from),
                            None,
                        );
                    }
                }
            }
            AstKind::ImportExpression(expr) => {
                for from in owner.sources(file) {
                    if let Expression::StringLiteral(s) = &expr.source {
                        if let Some(to) =
                            target(project, file, s.value.as_str(), s.span, Some(from), false)
                        {
                            project.graph.edge(from, to, false, loc);
                            let mut awaited = false;
                            let parent =
                                nodes.ancestors(node.id()).skip(1).find(|n| match n.kind() {
                                    AstKind::AwaitExpression(_) => {
                                        awaited = true;
                                        false
                                    }
                                    AstKind::ParenthesizedExpression(_) => false,
                                    _ => true,
                                });
                            match parent.map(|p| p.kind()) {
                                Some(AstKind::ExpressionStatement(_)) => {}
                                Some(AstKind::StaticMemberExpression(m)) if awaited => {
                                    project.graph.links.push((from, to, m.property.name.to_string(), false, loc));
                                }
                                _ => project.graph.gap(
                                    "namespace-escape", Some(file), Some(loc),
                                    "The literal import resolves, but uses of its namespace are unknown.".into(),
                                    Some(from), Some(to),
                                ),
                            }
                        }
                    } else {
                        project.graph.gap(
                            "computed-import",
                            Some(file),
                            Some(loc),
                            "Supply the possible module specifiers for this dynamic import.".into(),
                            Some(from),
                            None,
                        );
                    }
                }
            }
            AstKind::TSImportType(_)
            | AstKind::TSImportEqualsDeclaration(_)
            | AstKind::TSExportAssignment(_) => {
                project.graph.gap(
                    "unsupported-syntax",
                    Some(file),
                    Some(loc),
                    "This TypeScript module construct requires additional resolution.".into(),
                    None,
                    None,
                );
            }
            _ => {}
        }
    }
    ok
}
