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

use crate::graph::{Edge, Export, Gap, Graph, Id, Link, Loc, Module, Node, PendingGap};
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

/// File IDs occupy the prefix shared by all workers. Binding IDs are local to
/// a summary until it is appended in file order.
pub struct FileSummary {
    file: Id,
    base: usize,
    path: String,
    nodes: Vec<Node>,
    evaluation: Vec<Edge>,
    module: Module,
    links: Vec<Link>,
    gaps: Vec<PendingGap>,
    pub parsed: bool,
}

impl FileSummary {
    fn node(&mut self, kind: &'static str, name: &str, loc: Loc) -> Id {
        let id = self.base + self.nodes.len();
        self.nodes.push(Node {
            file: self.file,
            kind,
            name: name.into(),
            loc,
            edges: vec![],
        });
        id
    }
    fn edge(&mut self, from: Id, target: Id, type_only: bool, loc: Loc) {
        let edges = if from == self.file {
            &mut self.evaluation
        } else {
            &mut self.nodes[from - self.base].edges
        };
        edges.push(Edge {
            target,
            type_only,
            loc,
        });
    }
    fn gap(
        &mut self,
        code: &'static str,
        loc: Option<Loc>,
        message: String,
        guard: Option<Id>,
        target_file: Option<Id>,
    ) {
        self.gaps.push(PendingGap {
            record: Gap {
                code,
                file: Some(self.path.clone()),
                location: loc,
                message,
                affected_files: None,
                id: String::new(),
            },
            guard,
            target_file,
        });
    }
    fn target(
        &mut self,
        project: &Project,
        spec: &str,
        span: Span,
        guard: Option<Id>,
        type_only: bool,
    ) -> Option<Id> {
        match project.resolve(self.file, spec, type_only) {
            Ok(target) => target,
            Err(message) => {
                self.gap("unresolved-import", Some(span.into()), message, guard, None);
                None
            }
        }
    }

    pub fn append(mut self, graph: &mut Graph) {
        let offset = graph.nodes.len() - self.base;
        let relocate = |id: Id| if id >= self.base { id + offset } else { id };
        for edges in
            std::iter::once(&mut self.evaluation).chain(self.nodes.iter_mut().map(|n| &mut n.edges))
        {
            for edge in edges {
                edge.target = relocate(edge.target);
            }
        }
        for export in self.module.exports.values_mut() {
            export.binding = relocate(export.binding);
            export.surface = relocate(export.surface);
        }
        for (from, ..) in &mut self.links {
            *from = relocate(*from);
        }
        for gap in &mut self.gaps {
            gap.guard = gap.guard.map(relocate);
        }
        graph.nodes[self.file].edges = self.evaluation;
        graph.modules[self.file] = self.module;
        graph.nodes.append(&mut self.nodes);
        graph.links.append(&mut self.links);
        graph.gaps.append(&mut self.gaps);
    }
}

pub fn summarize(
    project: &Project,
    file: Id,
    entry_file: bool,
    arena: &mut Allocator,
) -> FileSummary {
    arena.reset();
    let mut summary = FileSummary {
        file,
        path: project.graph.paths[file].clone(),
        base: project.graph.paths.len(),
        nodes: vec![],
        evaluation: vec![],
        module: Module::default(),
        links: vec![],
        gaps: vec![],
        parsed: false,
    };
    summary.parsed = extract(project, &mut summary, arena, entry_file);
    summary
}

fn extract(project: &Project, out: &mut FileSummary, arena: &Allocator, entry_file: bool) -> bool {
    let file = out.file;
    let path = project.root.join(&project.graph.paths[file]);
    let source = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) => {
            out.gap("read-error", None, e.to_string(), None, None);
            return false;
        }
    };

    let mut source_type = SourceType::from_path(&path).unwrap();
    if path.extension().is_some_and(|ext| ext == "js") {
        source_type = source_type.with_jsx(true);
    }
    let mut parsed = Parser::new(arena, &source, source_type).parse();
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
        out.gap("parse-error", loc, error.message.to_string(), None, None);
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
        let id = out.node(
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
        let to = out.target(project, spec, item.span, None, type_only);
        requests.insert(item.statement_span.start, to);
        if let Some(to) = to {
            out.edge(file, to, type_only, item.span.into());
        }
    }
    let mut namespaces = HashMap::new();
    let mut react = crate::react::ReactCalls::default();
    for import in &record.import_entries {
        let Some(from) = local(import.local_name.name.as_str()) else {
            continue;
        };
        // Recognize the package's own source entry in workspaces such as React.
        // A tsconfig alias to an unrelated local implementation has different semantics.
        if import.module_request.name == "react"
            && let Some(target) = requests.get(&import.statement_span.start)
            && (target.is_none() || *target == project.react_entry)
        {
            react.register(import, scoping);
        }
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
        out.links.push((
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
        let surface = out.node(kind, name, loc);
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
                    out.links
                        .push((surface, to, key.into(), export.is_type, loc));
                } else {
                    out.gap(
                        "namespace-escape",
                        Some(loc),
                        "A re-export exposes the module namespace.".into(),
                        Some(surface),
                        Some(to),
                    );
                }
            }
        } else if let Some(id) = export.local_name.name().and_then(|n| local(n.as_str())) {
            binding = id;
            out.edge(surface, id, export.is_type, loc);
            if let Some(&(to, _)) = namespaces.get(&id) {
                out.gap(
                    "namespace-escape",
                    Some(loc),
                    "A re-export exposes the module namespace.".into(),
                    Some(surface),
                    Some(to),
                );
            }
        } else if name == "default" {
            default_expr = Some(surface);
        } else {
            out.gap(
                "missing-binding",
                Some(loc),
                format!("No local declaration resolved for {name}."),
                None,
                None,
            );
        }
        out.module.exports.insert(
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
                out.module.stars.push(Edge {
                    target: to,
                    type_only: export.is_type,
                    loc: spec.span.into(),
                });
            } else if !out.gaps.iter().any(|g| {
                g.record.code == "unresolved-import"
                    && g.record.file.as_ref() == Some(&project.graph.paths[file])
            }) {
                out.gap(
                    "external-star",
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
    let exported: std::collections::HashSet<_> =
        out.module.exports.values().map(|e| e.binding).collect();
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
                let group = out.node("declaration", "", node.span().into());
                for &id in ids {
                    out.edge(id, group, false, node.span().into());
                }
                group
            };
            let eager = match node.kind() {
                AstKind::VariableDeclarator(v) => {
                    let init = v.init.as_ref().map(Expression::get_inner_expression);
                    if let Some(Expression::CallExpression(call)) = init {
                        react.initializer(call);
                    }
                    !init.is_some_and(Expression::is_function)
                }
                AstKind::Class(_) | AstKind::TSEnumDeclaration(_) => true,
                AstKind::TSTypeAliasDeclaration(_) | AstKind::TSInterfaceDeclaration(_) => {
                    entry_file && !exported.contains(&id)
                }
                _ => false,
            };
            owner = Owner { binding: id, eager };
        } else if let AstKind::ExportDefaultDeclaration(export) = node.kind()
            && let Some(binding) = default_expr
        {
            if let Some(Expression::CallExpression(call)) = export
                .declaration
                .as_expression()
                .map(Expression::get_inner_expression)
            {
                react.initializer(call);
            }
            owner = Owner {
                binding,
                eager: true,
            };
        } else if matches!(
            node.kind(),
            AstKind::Function(_) | AstKind::ArrowFunctionExpression(_)
        ) && (react.is_deferred(node.span().start)
            || matches!(
                nodes.parent_kind(node.id()),
                Some(AstKind::ExportDefaultDeclaration(_) | AstKind::MethodDefinition(_))
            ))
        {
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
                                let access = out.node(
                                    "import",
                                    &format!("{}.{}", ident.name, property),
                                    loc,
                                );
                                out.edge(from, access, is_type || type_import, loc);
                                out.links.push((
                                    access,
                                    to,
                                    property.into(),
                                    is_type || type_import,
                                    loc,
                                ));
                            } else {
                                out.gap(
                                    "namespace-escape",
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
                            out.edge(from, binding, is_type, loc);
                        }
                    }
                } else if reference.symbol_id().is_none()
                    && matches!(
                        ident.name.as_str(),
                        "require" | "exports" | "module" | "eval"
                    )
                {
                    for from in owner.sources(file) {
                        out.gap(
                            "unsupported-runtime",
                            Some(loc),
                            format!("Runtime use of {} is outside the ESM model.", ident.name),
                            Some(from),
                            None,
                        );
                    }
                }
            }
            AstKind::CallExpression(call) => {
                react.observe(call, scoping);
                if let Expression::StaticMemberExpression(member) =
                    call.callee.get_inner_expression()
                    && member.property.name == "glob"
                    && matches!(member.object.get_inner_expression(), Expression::MetaProperty(meta) if meta.meta.name == "import" && meta.property.name == "meta")
                {
                    for from in owner.sources(file) {
                        out.gap(
                            "bundler-glob",
                            Some(loc),
                            "import.meta.glob can load additional modules selected by the bundler."
                                .into(),
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
                            out.target(project, s.value.as_str(), s.span, Some(from), false)
                        {
                            out.edge(from, to, false, loc);
                            if let Some(member) = react.member(expr.span.start) {
                                let access = out.node("import", member, loc);
                                out.edge(from, access, false, loc);
                                out.links.push((access, to, member.into(), false, loc));
                                continue;
                            }
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
                            // Oxc represents the value returned by a concise arrow as an
                            // expression statement in a synthetic function body.
                            let discarded = parent.is_some_and(|p| {
                                matches!(p.kind(), AstKind::ExpressionStatement(_)) &&
                                !nodes.parent_id(p.id()).and_then(|id| nodes.parent_kind(id))
                                    .is_some_and(|k| matches!(k, AstKind::ArrowFunctionExpression(a) if a.expression))
                            });
                            match parent.map(|p| p.kind()) {
                                _ if discarded => {}
                                Some(AstKind::StaticMemberExpression(m)) if awaited => {
                                    let access = out.node("import", m.property.name.as_str(), loc);
                                    out.edge(from, access, false, loc);
                                    out.links.push((access, to, m.property.name.to_string(), false, loc));
                                }
                                _ => out.gap(
                                    "namespace-escape", Some(loc),
                                    "The literal import resolves, but uses of its namespace are unknown.".into(),
                                    Some(from), Some(to),
                                ),
                            }
                        }
                    } else {
                        out.gap(
                            "computed-import",
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
                out.gap(
                    "unsupported-syntax",
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
