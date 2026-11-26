use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use oxc_allocator::Allocator;
use oxc_ast::{
    AstKind,
    ast::{
        AssignmentTarget, BindingPatternKind, Expression, ObjectAssignmentTarget, ObjectExpression,
        ObjectPattern, ObjectPropertyKind, PropertyKey, TSLiteral, TSModuleReference, TSType,
        TSTypeName,
    },
};
use oxc_index::Idx;
use oxc_parser::Parser;
use oxc_semantic::{AstNode, AstNodes, Scoping, SemanticBuilder};
use oxc_span::{GetSpan, SourceType, Span};
use oxc_syntax::module_record::{ExportExportName, ExportImportName, ImportImportName};
use oxc_syntax::node::NodeId;
use oxc_syntax::symbol::SymbolId;

use crate::deps::{PackageRef, extract_dep_package};
use crate::graph::{Edge, Export, Gap, GapScope, Graph, Id, Link, Loc, Module, Node, PendingGap};
use crate::project::{Project, StaleConfigItem};

#[derive(Clone, Debug)]
pub struct InlineSuppression {
    pub file: Id,
    pub symbol: String,
    pub reason: String,
    pub loc: Loc,
}

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
    pub package_refs: Vec<PackageRef>,
    pub inline_suppressions: Vec<InlineSuppression>,
    pub invalid_suppressions: Vec<StaleConfigItem>,
    pub used_dynamic_imports: bool,
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
        self.gap_scoped(code, loc, message, guard, target_file, GapScope::Global);
    }
    fn gap_scoped(
        &mut self,
        code: &'static str,
        loc: Option<Loc>,
        message: String,
        guard: Option<Id>,
        target_file: Option<Id>,
        scope: GapScope,
    ) {
        self.gaps.push(PendingGap {
            record: Gap {
                code,
                file: Some(self.path.clone()),
                location: loc,
                message,
                scope,
                id: String::new(),
                test_only: false,
            },
            guard,
            target_file,
        });
    }
    fn record_pkg_ref(&mut self, project: &Project, spec: &str, type_only: bool, span: Span) {
        if let Some(package) = extract_dep_package(spec)
            && project
                .source_mapper
                .matches_tsconfig_path(Path::new(&self.path), spec)
                .is_empty()
        {
            self.package_refs.push(PackageRef {
                file: self.file,
                package,
                type_only,
                loc: span.into(),
            });
        }
    }
    fn target_with_mode(
        &mut self,
        project: &Project,
        spec: &str,
        span: Span,
        guard: Option<Id>,
        type_only: bool,
        cjs: bool,
    ) -> Option<Id> {
        self.record_pkg_ref(project, spec, type_only, span);
        match project.resolve_with_mode(self.file, spec, type_only, cjs) {
            Ok(target) => target,
            Err(message) => {
                let scope = project.scope_unresolved_import(self.file, spec);
                self.gap_scoped(
                    "unresolved-import",
                    Some(span.into()),
                    message,
                    guard,
                    None,
                    scope,
                );
                None
            }
        }
    }
    fn link_member(
        &mut self,
        from: Id,
        to: Id,
        label: &str,
        prop: &str,
        type_only: bool,
        loc: Loc,
    ) {
        let access = self.node("import", label, loc);
        self.edge(from, access, type_only, loc);
        self.links.push((access, to, prop.into(), type_only, loc));
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

fn static_property_key<'a>(key: &'a PropertyKey<'a>, computed: bool) -> Option<&'a str> {
    match key {
        PropertyKey::StaticIdentifier(id) if !computed => Some(id.name.as_str()),
        PropertyKey::StringLiteral(s) => Some(s.value.as_str()),
        _ => None,
    }
}

fn object_pattern_keys<'a>(pattern: &'a ObjectPattern<'a>) -> Option<Vec<&'a str>> {
    if pattern.rest.is_some() {
        return None;
    }
    pattern
        .properties
        .iter()
        .map(|p| static_property_key(&p.key, p.computed))
        .collect()
}

fn object_assignment_keys<'a>(target: &'a ObjectAssignmentTarget<'a>) -> Option<Vec<&'a str>> {
    if target.rest.is_some() {
        return None;
    }
    target
        .properties
        .iter()
        .map(|p| match p {
            oxc_ast::ast::AssignmentTargetProperty::AssignmentTargetPropertyIdentifier(id) => {
                Some(id.binding.name.as_str())
            }
            oxc_ast::ast::AssignmentTargetProperty::AssignmentTargetPropertyProperty(prop) => {
                static_property_key(&prop.name, prop.computed)
            }
        })
        .collect()
}

fn is_unbound_ident(scoping: &Scoping, expr: &Expression<'_>, name: &str) -> Option<Span> {
    let Expression::Identifier(id) = expr.get_inner_expression() else {
        return None;
    };
    (id.name == name
        && scoping
            .get_reference(id.reference_id.get()?)
            .symbol_id()
            .is_none())
    .then_some(id.span)
}

fn static_string_expr<'a>(expr: &'a Expression<'a>) -> Option<(&'a str, Span)> {
    match expr.get_inner_expression() {
        Expression::StringLiteral(s) => Some((s.value.as_str(), s.span)),
        Expression::TemplateLiteral(t) if t.expressions.is_empty() && t.quasis.len() == 1 => t
            .quasis[0]
            .value
            .cooked
            .as_ref()
            .map(|c| (c.as_str(), t.span)),
        _ => None,
    }
}

fn static_string_with_ext(
    expr: &Expression<'_>,
    scoping: &Scoping,
    file_ext: &str,
    file_ext_symbols: &HashSet<SymbolId>,
) -> Option<(String, Span)> {
    let inner = expr.get_inner_expression();
    if let Some((s, sp)) = static_string_expr(inner) {
        return Some((s.to_owned(), sp));
    }
    if let Expression::TemplateLiteral(t) = inner
        && !t.expressions.is_empty()
        && t.expressions.iter().all(|e| {
            matches!(e.get_inner_expression(), Expression::Identifier(id)
                if id.reference_id.get().and_then(|r| scoping.get_reference(r).symbol_id())
                    .is_some_and(|s| file_ext_symbols.contains(&s)))
        })
    {
        let mut out = String::new();
        for (idx, q) in t.quasis.iter().enumerate() {
            out.push_str(q.value.cooked.as_ref()?.as_str());
            if idx < t.expressions.len() {
                out.push_str(file_ext);
            }
        }
        return Some((out, t.span));
    }
    None
}

fn is_meta_prop(expr: &Expression<'_>, prop: &str) -> bool {
    matches!(
        expr.get_inner_expression(),
        Expression::StaticMemberExpression(m)
            if m.property.name == prop
                && matches!(m.object.get_inner_expression(), Expression::MetaProperty(mp)
                    if mp.meta.name == "import" && mp.property.name == "meta")
    )
}

fn is_require_ident(
    scoping: &Scoping,
    require_symbols: &HashSet<SymbolId>,
    expr: &Expression<'_>,
) -> Option<Span> {
    let Expression::Identifier(id) = expr.get_inner_expression() else {
        return None;
    };
    let sym = scoping.get_reference(id.reference_id.get()?).symbol_id();
    ((id.name == "require" && sym.is_none()) || sym.is_some_and(|s| require_symbols.contains(&s)))
        .then_some(id.span)
}

fn extract_path_join_call(
    call: &oxc_ast::ast::CallExpression<'_>,
    scoping: &Scoping,
    file_ext: &str,
    file_ext_symbols: &HashSet<SymbolId>,
) -> Option<(String, Span)> {
    let is_path_fn = match call.callee.get_inner_expression() {
        Expression::StaticMemberExpression(m) => {
            matches!(m.property.name.as_str(), "join" | "resolve")
        }
        Expression::Identifier(id) => matches!(id.name.as_str(), "join" | "resolve"),
        _ => false,
    };
    if !is_path_fn || call.arguments.len() < 2 {
        return None;
    }
    let first_is_dirname = call
        .arguments
        .first()
        .and_then(|a| a.as_expression())
        .is_some_and(|e| match e.get_inner_expression() {
            Expression::Identifier(id) => matches!(id.name.as_str(), "__dirname" | "ROOT" | "dir"),
            other if is_meta_prop(other, "dirname") => true,
            Expression::CallExpression(c) => {
                let is_dirname = match c.callee.get_inner_expression() {
                    Expression::Identifier(id) => id.name == "dirname",
                    Expression::StaticMemberExpression(m) => m.property.name == "dirname",
                    _ => false,
                };
                is_dirname
                    && c.arguments
                        .first()
                        .and_then(|a| a.as_expression())
                        .is_some_and(|a| {
                            matches!(a.get_inner_expression(), Expression::Identifier(id) if id.name == "__filename")
                        })
            }
            _ => false,
        });
    if !first_is_dirname {
        return None;
    }
    let mut parts = Vec::new();
    for arg in call.arguments.iter().skip(1) {
        let (s, _) = arg
            .as_expression()
            .and_then(|e| static_string_with_ext(e, scoping, file_ext, file_ext_symbols))?;
        parts.push(s.trim_start_matches("./").trim_end_matches('/').to_owned());
    }
    let joined = parts.join("/");
    let spec = if joined.starts_with('.') || joined.starts_with('/') {
        joined
    } else {
        format!("./{joined}")
    };
    Some((spec, call.span))
}

fn extract_static_path_expr(
    expr: &Expression<'_>,
    scoping: &Scoping,
    require_symbols: &HashSet<SymbolId>,
    file_ext: &str,
    file_ext_symbols: &HashSet<SymbolId>,
) -> Option<(String, Span)> {
    let inner = expr.get_inner_expression();
    if let Some(res) = static_string_with_ext(inner, scoping, file_ext, file_ext_symbols) {
        return Some(res);
    }
    match inner {
        Expression::StaticMemberExpression(m)
            if matches!(m.property.name.as_str(), "href" | "pathname") =>
        {
            extract_static_path_expr(
                &m.object,
                scoping,
                require_symbols,
                file_ext,
                file_ext_symbols,
            )
        }
        Expression::NewExpression(ne)
            if matches!(ne.callee.get_inner_expression(), Expression::Identifier(id) if id.name == "URL")
                && ne
                    .arguments
                    .get(1)
                    .and_then(|a| a.as_expression())
                    .is_some_and(|e| is_meta_prop(e, "url")) =>
        {
            let (spec, sp) = ne
                .arguments
                .first()
                .and_then(|a| a.as_expression())
                .and_then(|e| static_string_with_ext(e, scoping, file_ext, file_ext_symbols))?;
            let spec = if spec.starts_with('.') || spec.starts_with('/') {
                spec
            } else {
                format!("./{spec}")
            };
            Some((spec, sp))
        }
        Expression::CallExpression(call) => {
            if matches!(
                call.callee.get_inner_expression(),
                Expression::Identifier(id) if matches!(id.name.as_str(), "fileURLToPath" | "pathToFileURL")
            ) && let Some(arg) = call.arguments.first().and_then(|a| a.as_expression())
            {
                return extract_static_path_expr(
                    arg,
                    scoping,
                    require_symbols,
                    file_ext,
                    file_ext_symbols,
                );
            }
            if let Expression::StaticMemberExpression(m) = call.callee.get_inner_expression()
                && m.property.name == "resolve"
                && is_require_ident(scoping, require_symbols, &m.object).is_some()
            {
                return call
                    .arguments
                    .first()
                    .and_then(|a| a.as_expression())
                    .and_then(|e| static_string_with_ext(e, scoping, file_ext, file_ext_symbols));
            }
            extract_path_join_call(call, scoping, file_ext, file_ext_symbols)
        }
        _ => None,
    }
}

fn resolve_arg_paths(
    arg: &Expression<'_>,
    scoping: &Scoping,
    require_symbols: &HashSet<SymbolId>,
    file_ext: &str,
    file_ext_symbols: &HashSet<SymbolId>,
    local_paths: &HashMap<SymbolId, Vec<(String, Span)>>,
) -> Vec<(String, Span)> {
    if let Some(direct) =
        extract_static_path_expr(arg, scoping, require_symbols, file_ext, file_ext_symbols)
    {
        return vec![direct];
    }
    if let Expression::Identifier(id) = arg.get_inner_expression()
        && let Some(rid) = id.reference_id.get()
        && let Some(sym) = scoping.get_reference(rid).symbol_id()
        && let Some(list) = local_paths.get(&sym)
    {
        return list.clone();
    }
    vec![]
}

fn match_template_prefix_glob(
    project: &Project,
    file: Id,
    expr: &Expression<'_>,
) -> Option<Vec<Id>> {
    let Expression::TemplateLiteral(t) = expr.get_inner_expression() else {
        return None;
    };
    if t.expressions.is_empty() {
        return None;
    }
    let first = t.quasis.first()?.value.cooked.as_ref()?.as_str();
    if !(first.starts_with("./") || first.starts_with("../")) {
        return None;
    }
    // Require at least a concrete subdirectory or filename prefix beyond `./` or `../`.
    let trimmed = first.trim_start_matches("../").trim_start_matches("./");
    if trimmed.is_empty() {
        return None;
    }
    let mut pattern = String::new();
    for (idx, q) in t.quasis.iter().enumerate() {
        pattern.push_str(q.value.cooked.as_ref()?.as_str());
        if idx < t.expressions.len() {
            pattern.push('*');
        }
    }
    let file_dir = Path::new(&project.graph.paths[file])
        .parent()
        .unwrap_or_else(|| Path::new(""));
    let norm = crate::source_map::normalize_rel(&file_dir.join(&pattern))?;
    let base_pat = crate::source_map::to_slash(&norm);
    let mut matchers = Vec::new();
    if let Ok(m) = crate::project::matcher(&base_pat) {
        matchers.push(m);
    }
    if !base_pat.ends_with('*')
        && Path::new(&base_pat).extension().is_none()
        && let Ok(m) = crate::project::matcher(&format!(
            "{base_pat}.{{{}}}",
            crate::project::EXTENSIONS.join(",")
        ))
    {
        matchers.push(m);
    } else if base_pat.ends_with('*') {
        if let Ok(m) = crate::project::matcher(&format!(
            "{base_pat}.{{{}}}",
            crate::project::EXTENSIONS.join(",")
        )) {
            matchers.push(m);
        }
        if let Ok(m) = crate::project::matcher(&format!("{base_pat}/index.*")) {
            matchers.push(m);
        }
    }
    if matchers.is_empty() {
        return None;
    }
    let matched: Vec<Id> = project
        .graph
        .paths
        .iter()
        .enumerate()
        .filter(|&(id, p)| id != file && matchers.iter().any(|m| m.is_match(p)))
        .map(|(id, _)| id)
        .collect();
    (!matched.is_empty()).then_some(matched)
}

fn is_json_dynamic_spec(expr: &Expression<'_>) -> bool {
    match expr.get_inner_expression() {
        Expression::TemplateLiteral(t) => t
            .quasis
            .last()
            .and_then(|q| q.value.cooked.as_ref())
            .is_some_and(|s| s.ends_with(".json")),
        Expression::CallExpression(call) => {
            let is_join = match call.callee.get_inner_expression() {
                Expression::Identifier(id) => matches!(id.name.as_str(), "join" | "resolve"),
                Expression::StaticMemberExpression(m) => {
                    matches!(m.property.name.as_str(), "join" | "resolve")
                }
                _ => false,
            };
            is_join
                && call
                    .arguments
                    .last()
                    .and_then(|a| a.as_expression())
                    .and_then(static_string_expr)
                    .is_some_and(|(s, _)| s.ends_with(".json"))
        }
        _ => false,
    }
}

fn relative_template_scope(project: &Project, file: Id, expr: &Expression<'_>) -> GapScope {
    let Expression::TemplateLiteral(t) = expr.get_inner_expression() else {
        return GapScope::Global;
    };
    let Some(first) = t
        .quasis
        .first()
        .and_then(|q| q.value.cooked.as_ref())
        .map(|s| s.as_str())
    else {
        return GapScope::Global;
    };
    if !(first.starts_with("./") || first.starts_with("../")) {
        return GapScope::Global;
    }
    let dir_part = first.rsplit_once('/').map_or(".", |(d, _)| d);
    let file_dir = Path::new(&project.graph.paths[file])
        .parent()
        .unwrap_or_else(|| Path::new(""));
    let Some(norm) = crate::source_map::normalize_rel(&file_dir.join(dir_part)) else {
        return GapScope::Global;
    };
    GapScope::from_range_and_file(project.files_in_dir_range(&norm), file)
}

fn cjs_exports_target(scoping: &Scoping, expr: &Expression<'_>) -> Option<Span> {
    is_unbound_ident(scoping, expr, "exports").or_else(|| {
        if let Expression::StaticMemberExpression(inner) = expr.get_inner_expression()
            && inner.property.name == "exports"
        {
            is_unbound_ident(scoping, &inner.object, "module")
        } else {
            None
        }
    })
}

fn resolve_promise_all_use<'a>(
    nodes: &'a AstNodes<'a>,
    import_id: NodeId,
    expr_span: Span,
) -> Option<NsUse<'a>> {
    // Match `const [a, { b }] = await Promise.all([import(...), ...])`.
    let arr_node = nodes.ancestors(import_id).skip(1).find(|n| {
        !matches!(
            n.kind(),
            AstKind::ParenthesizedExpression(_) | AstKind::Argument(_)
        )
    })?;
    let AstKind::ArrayExpression(arr) = arr_node.kind() else {
        return None;
    };
    let elem_idx = arr
        .elements
        .iter()
        .position(|el| el.as_expression().is_some_and(|e| e.span() == expr_span))?;
    let call_node = nodes.ancestors(arr_node.id()).skip(1).find(|n| {
        !matches!(
            n.kind(),
            AstKind::Argument(_) | AstKind::ParenthesizedExpression(_)
        )
    })?;
    let AstKind::CallExpression(call) = call_node.kind() else {
        return None;
    };
    let Expression::StaticMemberExpression(m) = call.callee.get_inner_expression() else {
        return None;
    };
    if m.property.name != "all"
        || !matches!(m.object.get_inner_expression(), Expression::Identifier(id) if id.name == "Promise")
    {
        return None;
    }
    let (call_span, parent, awaited) = unwrap_parent(nodes, call_node.id(), call.span, true);
    if !awaited {
        return None;
    }
    let Some(AstKind::VariableDeclarator(v)) = parent.map(|p| p.kind()) else {
        return None;
    };
    if !v.init.as_ref().is_some_and(|i| i.span() == call_span) {
        return None;
    }
    let BindingPatternKind::ArrayPattern(pat) = &v.id.kind else {
        return None;
    };
    let Some(Some(slot)) = pat.elements.get(elem_idx) else {
        return Some(NsUse::Discarded);
    };
    match &slot.kind {
        BindingPatternKind::ObjectPattern(obj) => {
            Some(object_pattern_keys(obj).map_or(NsUse::Escape, NsUse::Props))
        }
        BindingPatternKind::BindingIdentifier(bind) => {
            Some(bind.symbol_id.get().map_or(NsUse::Escape, NsUse::Alias))
        }
        _ => Some(NsUse::Escape),
    }
}

fn extract_spawn_exec_paths(
    call: &oxc_ast::ast::CallExpression<'_>,
    scoping: &Scoping,
    require_symbols: &HashSet<SymbolId>,
    file_ext: &str,
    file_ext_symbols: &HashSet<SymbolId>,
    local_paths: &HashMap<SymbolId, Vec<(String, Span)>>,
) -> Vec<(String, Span)> {
    let Some(cmd) = call.arguments.first().and_then(|a| a.as_expression()) else {
        return vec![];
    };
    let is_js_exec = match cmd.get_inner_expression() {
        Expression::StaticMemberExpression(m) => {
            m.property.name == "execPath"
                && matches!(m.object.get_inner_expression(), Expression::Identifier(id) if id.name == "process")
        }
        other => static_string_expr(other).is_some_and(|(s, _)| {
            matches!(
                Path::new(s).file_name().and_then(|n| n.to_str()),
                Some("node" | "bun" | "tsx" | "ts-node")
            )
        }),
    };
    if !is_js_exec {
        return vec![];
    }
    let Some(Expression::ArrayExpression(args)) = call
        .arguments
        .get(1)
        .and_then(|a| a.as_expression())
        .map(Expression::get_inner_expression)
    else {
        return vec![];
    };
    let mut i = 0;
    while i < args.elements.len() {
        let Some(el) = args.elements[i].as_expression() else {
            return vec![];
        };
        if let Some((flag, _)) = static_string_expr(el) {
            if flag == "--" {
                i += 1;
                break;
            }
            if matches!(
                flag,
                "-r" | "--require"
                    | "--import"
                    | "--loader"
                    | "--experimental-loader"
                    | "--conditions"
                    | "--env-file"
                    | "--preload"
            ) {
                i += 2;
                continue;
            }
            if matches!(flag, "-e" | "--eval" | "-p" | "--print") {
                return vec![];
            }
            if flag.starts_with('-') {
                i += 1;
                continue;
            }
        }
        break;
    }
    args.elements
        .get(i)
        .and_then(|e| e.as_expression())
        .map(|arg| {
            resolve_arg_paths(
                arg,
                scoping,
                require_symbols,
                file_ext,
                file_ext_symbols,
                local_paths,
            )
        })
        .unwrap_or_default()
}

fn collect_decl_names(decl: &oxc_ast::ast::Declaration<'_>, out: &mut Vec<String>) {
    use oxc_ast::ast::Declaration;
    match decl {
        Declaration::VariableDeclaration(v) => {
            for d in &v.declarations {
                collect_pattern_names(&d.id.kind, out);
            }
        }
        Declaration::FunctionDeclaration(f) => {
            if let Some(id) = &f.id {
                out.push(id.name.to_string());
            }
        }
        Declaration::ClassDeclaration(c) => {
            if let Some(id) = &c.id {
                out.push(id.name.to_string());
            }
        }
        Declaration::TSTypeAliasDeclaration(t) => out.push(t.id.name.to_string()),
        Declaration::TSInterfaceDeclaration(i) => out.push(i.id.name.to_string()),
        Declaration::TSEnumDeclaration(e) => out.push(e.id.name.to_string()),
        Declaration::TSModuleDeclaration(m) => {
            if let oxc_ast::ast::TSModuleDeclarationName::Identifier(id) = &m.id {
                out.push(id.name.to_string());
            }
        }
        _ => {}
    }
}

fn collect_pattern_names(kind: &BindingPatternKind<'_>, out: &mut Vec<String>) {
    match kind {
        BindingPatternKind::BindingIdentifier(id) => out.push(id.name.to_string()),
        BindingPatternKind::ObjectPattern(obj) => {
            for p in &obj.properties {
                collect_pattern_names(&p.value.kind, out);
            }
            if let Some(rest) = &obj.rest {
                collect_pattern_names(&rest.argument.kind, out);
            }
        }
        BindingPatternKind::ArrayPattern(arr) => {
            for el in arr.elements.iter().flatten() {
                collect_pattern_names(&el.kind, out);
            }
            if let Some(rest) = &arr.rest {
                collect_pattern_names(&rest.argument.kind, out);
            }
        }
        BindingPatternKind::AssignmentPattern(a) => collect_pattern_names(&a.left.kind, out),
    }
}

fn statement_symbol_names(stmt: &oxc_ast::ast::Statement<'_>) -> Vec<String> {
    use oxc_ast::ast::Statement;
    let mut names = Vec::new();
    match stmt {
        Statement::ExportDefaultDeclaration(_) => names.push("default".into()),
        Statement::ExportNamedDeclaration(exp) => {
            if let Some(decl) = &exp.declaration {
                collect_decl_names(decl, &mut names);
            }
            for spec in &exp.specifiers {
                names.push(spec.exported.name().to_string());
            }
        }
        other => {
            if let Some(decl) = other.as_declaration() {
                collect_decl_names(decl, &mut names);
            }
        }
    }
    names
}

fn extract_inline_suppressions(
    source: &str,
    program: &oxc_ast::ast::Program<'_>,
    out: &mut FileSummary,
) {
    if !source.contains("@smella-ignore") {
        return;
    }
    for comment in &program.comments {
        let start = comment.span.start as usize;
        let end = comment.span.end as usize;
        let Some(text) = source.get(start..end) else {
            continue;
        };
        let Some(tag_pos) = text.find("@smella-ignore") else {
            continue;
        };
        let after = &text[tag_pos + "@smella-ignore".len()..];
        let raw_line = after.lines().next().unwrap_or("");
        let reason = raw_line
            .trim_end_matches("*/")
            .trim()
            .trim_start_matches(':')
            .trim();
        let loc: Loc = comment.span.into();
        if reason.is_empty() {
            out.invalid_suppressions.push(StaleConfigItem {
                kind: "invalid-suppression",
                source: out.path.clone(),
                pattern: None,
                file: Some(out.path.clone()),
                symbol: None,
                location: Some(loc),
                message: "@smella-ignore requires a non-empty reason.".into(),
            });
            continue;
        }
        // Attach only to a top-level declaration immediately following this comment (no code in between).
        let target_stmt = program
            .body
            .iter()
            .find(|stmt| stmt.span().start >= comment.span.end);
        let attached_names = target_stmt
            .filter(|stmt| {
                let gap_start = end;
                let gap_end = stmt.span().start as usize;
                let between = source.get(gap_start..gap_end).unwrap_or("");
                // Allow whitespace and additional comment lines between JSDoc and declaration.
                between.lines().all(|l| {
                    let t = l.trim();
                    t.is_empty()
                        || t.starts_with("//")
                        || t.starts_with("/*")
                        || t.starts_with('*')
                        || t.starts_with('@')
                })
            })
            .map(statement_symbol_names)
            .unwrap_or_default();
        if attached_names.is_empty() {
            out.invalid_suppressions.push(StaleConfigItem {
                kind: "invalid-suppression",
                source: out.path.clone(),
                pattern: None,
                file: Some(out.path.clone()),
                symbol: None,
                location: Some(loc),
                message: "@smella-ignore must precede a specific export or binding declaration (file-level ignores are not supported).".into(),
            });
            continue;
        }
        for symbol in attached_names {
            out.inline_suppressions.push(InlineSuppression {
                file: out.file,
                symbol,
                reason: reason.to_owned(),
                loc,
            });
        }
    }
}

fn leftmost_type_name<'a>(name: &'a TSTypeName<'a>) -> Option<&'a str> {
    match name {
        TSTypeName::IdentifierReference(id) => Some(id.name.as_str()),
        TSTypeName::QualifiedName(q) => leftmost_type_name(&q.left),
    }
}

fn register_cjs_export(
    out: &mut FileSummary,
    name: &str,
    rhs: &Expression<'_>,
    loc: Loc,
    scoping: &Scoping,
    bindings: &HashMap<SymbolId, Id>,
    owner_overrides: &mut HashMap<u32, Owner>,
) -> Id {
    let surface = out.node("export", name, loc);
    let inner = rhs.get_inner_expression();
    let binding = if let Expression::Identifier(id) = inner
        && let Some(rid) = id.reference_id.get()
        && let Some(sym) = scoping.get_reference(rid).symbol_id()
        && let Some(&bid) = bindings.get(&sym)
    {
        out.edge(surface, bid, false, loc);
        owner_overrides.insert(
            rhs.span().start,
            Owner {
                binding: surface,
                eager: false,
            },
        );
        bid
    } else {
        owner_overrides.insert(
            rhs.span().start,
            Owner {
                binding: surface,
                eager: !inner.is_function(),
            },
        );
        surface
    };
    out.module.exports.insert(
        name.into(),
        Export {
            surface,
            binding,
            explicit: true,
        },
    );
    surface
}

fn register_cjs_object_exports(
    out: &mut FileSummary,
    obj: &ObjectExpression<'_>,
    loc: Loc,
    scoping: &Scoping,
    bindings: &HashMap<SymbolId, Id>,
    owner_overrides: &mut HashMap<u32, Owner>,
    register_default: bool,
) {
    let mut surfaces = Vec::new();
    let mut has_spread = false;
    for prop in &obj.properties {
        match prop {
            ObjectPropertyKind::ObjectProperty(p) => {
                if let Some(key) = static_property_key(&p.key, p.computed) {
                    surfaces.push(register_cjs_export(
                        out,
                        key,
                        &p.value,
                        p.key.span().into(),
                        scoping,
                        bindings,
                        owner_overrides,
                    ));
                } else {
                    has_spread = true;
                }
            }
            ObjectPropertyKind::SpreadProperty(_) => has_spread = true,
        }
    }
    if register_default {
        let default_surface = out.node("export", "default", loc);
        for s in surfaces {
            out.edge(default_surface, s, false, loc);
        }
        out.module.exports.insert(
            "default".into(),
            Export {
                surface: default_surface,
                binding: default_surface,
                explicit: true,
            },
        );
        if has_spread {
            out.module.cjs_opaque_default = true;
            owner_overrides.insert(
                obj.span.start,
                Owner {
                    binding: default_surface,
                    eager: true,
                },
            );
        }
    }
}

/// Walk past transparent AST wrappers to find the meaningful consumer of a namespace expression.
fn unwrap_parent<'a>(
    nodes: &'a AstNodes<'a>,
    id: NodeId,
    mut span: Span,
    track_await: bool,
) -> (Span, Option<&'a AstNode<'a>>, bool) {
    let mut awaited = false;
    for anc in nodes.ancestors(id).skip(1) {
        match anc.kind() {
            AstKind::AwaitExpression(_) if track_await => {
                awaited = true;
                span = anc.span();
            }
            AstKind::TSTypeName(_)
            | AstKind::Argument(_)
            | AstKind::ParenthesizedExpression(_)
            | AstKind::TSAsExpression(_)
            | AstKind::TSNonNullExpression(_)
            | AstKind::TSSatisfiesExpression(_)
            | AstKind::TSTypeAssertion(_)
            | AstKind::ChainExpression(_) => span = anc.span(),
            _ => return (span, Some(anc), awaited),
        }
    }
    (span, None, awaited)
}

enum NsUse<'a> {
    Discarded,
    Props(Vec<&'a str>),
    Alias(SymbolId),
    Escape,
}

fn classify_ns_use<'a>(
    nodes: &'a AstNodes<'a>,
    scoping: &Scoping,
    parent: Option<&'a AstNode<'a>>,
    expr_span: Span,
    is_cjs: bool,
    allow_discard: bool,
    can_alias: impl Fn(SymbolId) -> bool,
) -> NsUse<'a> {
    if allow_discard
        && parent.is_some_and(|p| {
            matches!(p.kind(), AstKind::ExpressionStatement(_))
                && !nodes
                    .parent_id(p.id())
                    .and_then(|id| nodes.parent_kind(id))
                    .is_some_and(
                        |k| matches!(k, AstKind::ArrowFunctionExpression(a) if a.expression),
                    )
        })
    {
        return NsUse::Discarded;
    }
    match parent.map(|p| p.kind()) {
        Some(AstKind::StaticMemberExpression(m)) if m.object.span() == expr_span => {
            NsUse::Props(vec![m.property.name.as_str()])
        }
        Some(AstKind::ComputedMemberExpression(m)) if m.object.span() == expr_span => {
            if let Expression::StringLiteral(s) = m.expression.get_inner_expression() {
                NsUse::Props(vec![s.value.as_str()])
            } else {
                NsUse::Escape
            }
        }
        Some(AstKind::JSXMemberExpression(m)) => NsUse::Props(vec![m.property.name.as_str()]),
        Some(AstKind::TSQualifiedName(n)) => NsUse::Props(vec![n.right.name.as_str()]),
        Some(AstKind::VariableDeclarator(v))
            if v.init.as_ref().is_some_and(|i| i.span() == expr_span) =>
        {
            match &v.id.kind {
                BindingPatternKind::ObjectPattern(pat) => {
                    object_pattern_keys(pat).map_or(NsUse::Escape, NsUse::Props)
                }
                BindingPatternKind::BindingIdentifier(bind) => bind
                    .symbol_id
                    .get()
                    .filter(|&sym| can_alias(sym))
                    .map_or(NsUse::Escape, NsUse::Alias),
                _ => NsUse::Escape,
            }
        }
        Some(AstKind::AssignmentExpression(a)) if a.right.span() == expr_span => match &a.left {
            AssignmentTarget::ObjectAssignmentTarget(obj) => {
                object_assignment_keys(obj).map_or(NsUse::Escape, NsUse::Props)
            }
            AssignmentTarget::AssignmentTargetIdentifier(id) if is_cjs => id
                .reference_id
                .get()
                .and_then(|r| scoping.get_reference(r).symbol_id())
                .filter(|&sym| can_alias(sym))
                .map_or(NsUse::Escape, NsUse::Alias),
            _ => NsUse::Escape,
        },
        Some(AstKind::CallExpression(call)) => {
            if is_cjs && call.callee.span() == expr_span {
                NsUse::Props(vec!["default"])
            } else if let Expression::StaticMemberExpression(m) = call.callee.get_inner_expression()
                && m.property.name == "spyOn"
                && call
                    .arguments
                    .first()
                    .is_some_and(|a| a.span() == expr_span)
                && let Some(Expression::StringLiteral(s)) = call
                    .arguments
                    .get(1)
                    .and_then(|a| a.as_expression())
                    .map(Expression::get_inner_expression)
            {
                NsUse::Props(vec![s.value.as_str()])
            } else {
                NsUse::Escape
            }
        }
        Some(AstKind::NewExpression(ne)) if is_cjs && ne.callee.span() == expr_span => {
            NsUse::Props(vec!["default"])
        }
        _ => NsUse::Escape,
    }
}

fn handle_import_meta_glob(
    project: &Project,
    out: &mut FileSummary,
    file: Id,
    owner: Owner,
    call: &oxc_ast::ast::CallExpression<'_>,
    loc: Loc,
) {
    let mut raw_patterns = Vec::new();
    let mut all_static = false;
    if let Some(first) = call.arguments.first().and_then(|a| a.as_expression()) {
        match first.get_inner_expression() {
            Expression::ArrayExpression(arr) => {
                all_static = !arr.elements.is_empty();
                for elem in &arr.elements {
                    if let Some((s, _)) = elem.as_expression().and_then(static_string_expr) {
                        raw_patterns.push(s.to_owned());
                    } else {
                        all_static = false;
                        break;
                    }
                }
            }
            other => {
                if let Some((s, _)) = static_string_expr(other) {
                    raw_patterns.push(s.to_owned());
                    all_static = true;
                }
            }
        }
    }
    let mut named_import = None;
    let mut raw_query = false;
    if let Some(Expression::ObjectExpression(opts)) = call
        .arguments
        .get(1)
        .and_then(|a| a.as_expression())
        .map(Expression::get_inner_expression)
    {
        for prop in &opts.properties {
            let ObjectPropertyKind::ObjectProperty(p) = prop else {
                continue;
            };
            match static_property_key(&p.key, p.computed) {
                Some("import") => {
                    named_import = static_string_expr(&p.value).map(|(s, _)| s.to_owned())
                }
                Some("query" | "as") => {
                    if let Some((s, _)) = static_string_expr(&p.value)
                        && matches!(s, "?raw" | "?url" | "raw" | "url")
                    {
                        raw_query = true;
                    }
                }
                _ => {}
            }
        }
    }
    let mut emit_gap = || {
        for from in owner.sources(file) {
            out.gap(
                "bundler-glob",
                Some(loc),
                "import.meta.glob can load additional modules selected by the bundler.".into(),
                Some(from),
                None,
            );
        }
    };
    if !all_static {
        emit_gap();
        return;
    }
    let file_dir = Path::new(&project.graph.paths[file])
        .parent()
        .unwrap_or_else(|| Path::new(""));
    let ws_dir = project.workspace_dir_for(file);
    let (mut pos, mut neg) = (Vec::new(), Vec::new());
    for pat in &raw_patterns {
        let (is_neg, body) = pat
            .strip_prefix('!')
            .map_or((false, pat.as_str()), |r| (true, r));
        let resolved = body.strip_prefix('/').map_or_else(
            || crate::source_map::normalize_rel(&file_dir.join(body)),
            |r| crate::source_map::normalize_rel(&ws_dir.join(r)),
        );
        let Some(m) = resolved
            .as_deref()
            .and_then(|n| crate::project::matcher(&crate::source_map::to_slash(n)).ok())
        else {
            emit_gap();
            return;
        };
        if is_neg {
            neg.push(m);
        } else {
            pos.push(m);
        }
    }
    if pos.is_empty() {
        emit_gap();
        return;
    }
    for (target_id, candidate) in project.graph.paths.iter().enumerate() {
        if target_id == file
            || !pos.iter().any(|m| m.is_match(candidate))
            || neg.iter().any(|m| m.is_match(candidate))
        {
            continue;
        }
        for from in owner.sources(file) {
            out.edge(from, target_id, false, loc);
            if !raw_query {
                let prop = named_import
                    .as_deref()
                    .filter(|&n| n != "*")
                    .unwrap_or("**");
                out.link_member(from, target_id, prop, prop, false, loc);
            }
        }
    }
}

pub fn summarize(
    project: &Project,
    file: Id,
    entry_file: bool,
    arena: &mut Allocator,
    read_buf: &mut Vec<u8>,
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
        package_refs: vec![],
        inline_suppressions: vec![],
        invalid_suppressions: vec![],
        used_dynamic_imports: false,
        parsed: false,
    };
    summary.parsed = extract(project, &mut summary, arena, read_buf, entry_file);
    summary
}

fn extract(
    project: &Project,
    out: &mut FileSummary,
    arena: &Allocator,
    read_buf: &mut Vec<u8>,
    entry_file: bool,
) -> bool {
    let file = out.file;
    let path = project.root.join(&project.graph.paths[file]);
    let source: &str = match project.source_text(file) {
        Some(Ok(text)) => text,
        Some(Err(e)) => {
            out.gap("read-error", None, e.to_owned(), None, None);
            return false;
        }
        None => {
            read_buf.clear();
            let res = File::open(&path).and_then(|mut f| {
                if let Ok(meta) = f.metadata() {
                    let len = meta.len() as usize;
                    if len > read_buf.capacity() {
                        read_buf.reserve(len - read_buf.capacity());
                    }
                }
                f.read_to_end(read_buf)
            });
            if let Err(e) = res {
                out.gap("read-error", None, e.to_string(), None, None);
                return false;
            }
            match std::str::from_utf8(read_buf) {
                Ok(s) => s,
                Err(e) => {
                    out.gap("read-error", None, e.to_string(), None, None);
                    return false;
                }
            }
        }
    };

    let mut source_type =
        SourceType::from_path(&path).unwrap_or_else(|_| SourceType::unambiguous());
    if path.extension().is_some_and(|ext| ext == "js") {
        source_type = source_type.with_jsx(true);
    }
    let mut parsed = Parser::new(arena, source, source_type).parse();
    let declaration_file = parsed.program.source_type.is_typescript_definition();
    if declaration_file {
        parsed.program.source_type = parsed.program.source_type.with_typescript(true);
    }
    let built = SemanticBuilder::new()
        .with_check_syntax_error(!declaration_file)
        .build(&parsed.program);
    let mut ok = true;
    for error in parsed.errors.iter().chain(&built.errors) {
        ok = false;
        let loc = error.labels.as_ref().and_then(|l| l.first()).map(|s| Loc {
            start: s.offset() as u32,
            end: (s.offset() + s.len()) as u32,
        });
        let scope = if project.is_multi_workspace() {
            GapScope::Files(vec![file as u32])
        } else {
            GapScope::Global
        };
        out.gap_scoped(
            "parse-error",
            loc,
            error.message.to_string(),
            None,
            None,
            scope,
        );
    }
    if parsed.panicked {
        return false;
    }
    extract_inline_suppressions(source, &parsed.program, out);
    let sem = built.semantic;
    let nodes = sem.nodes();
    let scoping = sem.scoping();
    let record = parsed.module_record;
    let mut bindings = HashMap::default();
    let mut declarations: HashMap<NodeId, Vec<Id>> = HashMap::default();
    for symbol in scoping
        .symbol_ids()
        .filter(|&s| scoping.symbol_scope_id(s) == scoping.root_scope_id())
    {
        let flags = scoping.symbol_flags(symbol);
        let id = out.node(
            if flags.is_import() || flags.is_type_import() {
                "import"
            } else {
                "binding"
            },
            scoping.symbol_name(symbol),
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

    let mut import_phases = HashMap::default();
    for import in &record.import_entries {
        *import_phases
            .entry(import.statement_span.start)
            .or_insert(true) &= import.is_type;
    }
    for export in &record.indirect_export_entries {
        *import_phases
            .entry(export.statement_span.start)
            .or_insert(true) &= export.is_type;
    }
    for export in &record.star_export_entries {
        *import_phases
            .entry(export.statement_span.start)
            .or_insert(true) &= export.is_type;
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
        let to = out.target_with_mode(project, spec, item.span, None, type_only, false);
        requests.insert(item.statement_span.start, to);
        if let Some(to) = to {
            out.edge(file, to, type_only, item.span.into());
        }
    }

    let file_ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    let mut namespaces = HashMap::default();
    let mut symbol_namespaces: HashMap<SymbolId, (Id, bool, bool)> = HashMap::default();
    let mut require_symbols: HashSet<SymbolId> = HashSet::default();
    let mut file_ext_symbols: HashSet<SymbolId> = HashSet::default();
    let mut local_paths: HashMap<SymbolId, Vec<(String, Span)>> = HashMap::default();
    let mut handled_cjs_idents: HashSet<u32> = HashSet::default();
    let mut owner_overrides: HashMap<u32, Owner> = HashMap::default();
    let mut react = crate::react::ReactCalls::default();

    for import in &record.import_entries {
        let Some(from) = local(import.local_name.name.as_str()) else {
            continue;
        };
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
        if import
            .module_request
            .name
            .split_once('?')
            .is_some_and(|(_, q)| {
                matches!(
                    q,
                    "worker" | "sharedworker" | "worker&inline" | "inline&worker"
                )
            })
        {
            out.edge(from, to, false, import.local_name.span.into());
            continue;
        }
        let name = match &import.import_name {
            ImportImportName::Name(n) => n.name.as_str(),
            ImportImportName::Default(_) => "default",
            ImportImportName::NamespaceObject => {
                namespaces.insert(from, (to, import.is_type));
                if let Some(sym) = scoping.get_root_binding(import.local_name.name.as_str()) {
                    symbol_namespaces.insert(sym, (to, import.is_type, false));
                }
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
        let surface = out.node(
            if export.module_request.is_some() {
                "re-export"
            } else {
                "export"
            },
            name,
            loc,
        );
        let mut binding = surface;
        if export.module_request.is_some() {
            if let Some(to) = requests
                .get(&export.statement_span.start)
                .copied()
                .flatten()
            {
                if let ExportImportName::Name(n) = &export.import_name {
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
        if let Some(existing) = out.module.exports.get(name) {
            if binding != surface {
                out.edge(existing.surface, binding, export.is_type, loc);
            } else {
                out.edge(existing.surface, surface, export.is_type, loc);
            }
        } else {
            out.module.exports.insert(
                name.into(),
                Export {
                    surface,
                    binding,
                    explicit: true,
                },
            );
        }
    }

    // Companion declaration files (`.d.ts`/`.d.mts`/`.d.cts`) augment their sibling JS module's type surface.
    if let Some(js_base) = [".js", ".mjs", ".cjs"]
        .iter()
        .find_map(|ext| out.path.strip_suffix(ext))
    {
        for dts in ["d.ts", "d.mts", "d.cts"] {
            if let Ok(companion) = project
                .graph
                .paths
                .binary_search(&format!("{js_base}.{dts}"))
            {
                out.module.stars.push(Edge {
                    target: companion,
                    type_only: true,
                    loc: Loc::default(),
                });
                break;
            }
        }
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

    let exported: HashSet<_> = out.module.exports.values().map(|e| e.binding).collect();
    let module_owner = Owner {
        binding: file,
        eager: false,
    };
    let mut owners = vec![module_owner; nodes.len()];

    for node in nodes.iter() {
        let mut owner = nodes
            .parent_id(node.id())
            .map_or(module_owner, |p| owners[p.index()]);
        if let Some(&override_owner) = owner_overrides.get(&node.span().start)
            && !matches!(
                node.kind(),
                AstKind::ObjectProperty(_) | AstKind::AssignmentExpression(_)
            )
        {
            owner = override_owner;
        }
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
            AstKind::VariableDeclarator(v) => {
                if let BindingPatternKind::BindingIdentifier(bind) = &v.id.kind
                    && let Some(sym) = bind.symbol_id.get()
                    && let Some(init) = v.init.as_ref()
                {
                    if let Expression::CallExpression(call) = init.get_inner_expression() {
                        let callee_name = match call.callee.get_inner_expression() {
                            Expression::Identifier(id) => Some(id.name.as_str()),
                            Expression::StaticMemberExpression(m) => Some(m.property.name.as_str()),
                            _ => None,
                        };
                        if callee_name == Some("createRequire") {
                            require_symbols.insert(sym);
                        } else if callee_name == Some("extname")
                            && call.arguments.first().and_then(|a| a.as_expression()).is_some_and(|e| {
                                matches!(e.get_inner_expression(), Expression::Identifier(id) if id.name == "__filename")
                            })
                        {
                            file_ext_symbols.insert(sym);
                        }
                    }
                    react.register_loader(sym, init, scoping);
                    let paths = resolve_arg_paths(
                        init,
                        scoping,
                        &require_symbols,
                        &file_ext,
                        &file_ext_symbols,
                        &local_paths,
                    );
                    if !paths.is_empty() {
                        local_paths.entry(sym).or_default().extend(paths);
                    }
                }
            }
            AstKind::UnaryExpression(un) if un.operator == oxc_ast::ast::UnaryOperator::Typeof => {
                if let Some(sp) = is_unbound_ident(scoping, &un.argument, "require")
                    .or_else(|| is_unbound_ident(scoping, &un.argument, "exports"))
                    .or_else(|| is_unbound_ident(scoping, &un.argument, "module"))
                {
                    handled_cjs_idents.insert(sp.start);
                }
            }
            AstKind::BinaryExpression(bin) => {
                let is_req_main = |e: &Expression<'_>| {
                    matches!(e.get_inner_expression(), Expression::StaticMemberExpression(m)
                        if m.property.name == "main" && is_unbound_ident(scoping, &m.object, "require").is_some())
                };
                if is_req_main(&bin.left)
                    && let Some(sp) = is_unbound_ident(scoping, &bin.right, "module")
                {
                    handled_cjs_idents.insert(sp.start);
                } else if is_req_main(&bin.right)
                    && let Some(sp) = is_unbound_ident(scoping, &bin.left, "module")
                {
                    handled_cjs_idents.insert(sp.start);
                }
            }
            AstKind::StaticMemberExpression(m) => {
                if let Some(sp) = is_unbound_ident(scoping, &m.object, "require")
                    && matches!(
                        m.property.name.as_str(),
                        "main" | "resolve" | "cache" | "extensions"
                    )
                {
                    handled_cjs_idents.insert(sp.start);
                } else if let Some(sp) = is_unbound_ident(scoping, &m.object, "module")
                    && matches!(
                        m.property.name.as_str(),
                        "id" | "filename" | "path" | "paths" | "parent" | "children" | "loaded"
                    )
                {
                    handled_cjs_idents.insert(sp.start);
                }
            }
            AstKind::AssignmentExpression(assign) => {
                let cjs_exports_obj = |e: &Expression<'_>| -> Option<Span> {
                    is_unbound_ident(scoping, e, "exports").or_else(|| {
                        if let Expression::StaticMemberExpression(inner) = e.get_inner_expression()
                            && inner.property.name == "exports"
                        {
                            is_unbound_ident(scoping, &inner.object, "module")
                        } else {
                            None
                        }
                    })
                };
                match &assign.left {
                    AssignmentTarget::StaticMemberExpression(lhs)
                        if lhs.property.name == "exports"
                            && let Some(sp) = is_unbound_ident(scoping, &lhs.object, "module") =>
                    {
                        handled_cjs_idents.insert(sp.start);
                        if let Expression::ObjectExpression(obj) =
                            assign.right.get_inner_expression()
                        {
                            register_cjs_object_exports(
                                out,
                                obj,
                                loc,
                                scoping,
                                &bindings,
                                &mut owner_overrides,
                                true,
                            );
                        } else {
                            register_cjs_export(
                                out,
                                "default",
                                &assign.right,
                                loc,
                                scoping,
                                &bindings,
                                &mut owner_overrides,
                            );
                            out.module.cjs_opaque_default = true;
                        }
                    }
                    AssignmentTarget::StaticMemberExpression(lhs)
                        if let Some(sp) = cjs_exports_obj(&lhs.object) =>
                    {
                        handled_cjs_idents.insert(sp.start);
                        register_cjs_export(
                            out,
                            lhs.property.name.as_str(),
                            &assign.right,
                            lhs.property.span.into(),
                            scoping,
                            &bindings,
                            &mut owner_overrides,
                        );
                    }
                    AssignmentTarget::ComputedMemberExpression(lhs)
                        if let Some((prop_name, prop_span)) =
                            static_string_expr(&lhs.expression)
                            && let Some(sp) = cjs_exports_obj(&lhs.object) =>
                    {
                        handled_cjs_idents.insert(sp.start);
                        register_cjs_export(
                            out,
                            prop_name,
                            &assign.right,
                            prop_span.into(),
                            scoping,
                            &bindings,
                            &mut owner_overrides,
                        );
                    }
                    AssignmentTarget::AssignmentTargetIdentifier(lhs_id)
                        if let Some(rid) = lhs_id.reference_id.get()
                            && let Some(sym) = scoping.get_reference(rid).symbol_id() =>
                    {
                        let paths = resolve_arg_paths(
                            &assign.right,
                            scoping,
                            &require_symbols,
                            &file_ext,
                            &file_ext_symbols,
                            &local_paths,
                        );
                        if !paths.is_empty() {
                            local_paths.entry(sym).or_default().extend(paths);
                        }
                    }
                    _ => {}
                }
            }
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
                let is_type = declaration_file
                    || reference.is_type()
                    || reference.flags().is_value_as_type()
                    || nodes
                        .ancestor_kinds(node.id())
                        .any(|k| matches!(k, AstKind::TSTypeQuery(_)));

                if let Some((to, type_import, is_cjs)) = reference
                    .symbol_id()
                    .and_then(|s| symbol_namespaces.get(&s).copied())
                {
                    let (expr_span, parent, _) = unwrap_parent(nodes, node.id(), ident.span, false);
                    let effective_type = is_type || type_import;
                    match classify_ns_use(nodes, scoping, parent, expr_span, is_cjs, false, |sym| {
                        !bindings.get(&sym).is_some_and(|id| exported.contains(id))
                    }) {
                        NsUse::Props(props) => {
                            for from in owner.sources(file) {
                                for p in &props {
                                    out.link_member(
                                        from,
                                        to,
                                        &format!("{}.{p}", ident.name),
                                        p,
                                        effective_type,
                                        loc,
                                    );
                                }
                            }
                        }
                        NsUse::Alias(sym) => {
                            symbol_namespaces.insert(sym, (to, effective_type, is_cjs));
                        }
                        NsUse::Discarded => {}
                        NsUse::Escape => {
                            for from in owner.sources(file) {
                                if is_cjs {
                                    out.link_member(
                                        from,
                                        to,
                                        ident.name.as_str(),
                                        "*",
                                        effective_type,
                                        loc,
                                    );
                                } else if effective_type {
                                    out.link_member(from, to, ident.name.as_str(), "**", true, loc);
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
                            }
                        }
                    }
                } else if let Some(&binding) = reference.symbol_id().and_then(|s| bindings.get(&s))
                {
                    for from in owner.sources(file) {
                        out.edge(from, binding, is_type, loc);
                    }
                } else if !is_type
                    && reference.symbol_id().is_none()
                    && !handled_cjs_idents.contains(&ident.span.start)
                    && matches!(
                        ident.name.as_str(),
                        "require" | "exports" | "module" | "eval"
                    )
                {
                    let scope = if ident.name == "require" && !project.is_multi_workspace() {
                        GapScope::Global
                    } else if ident.name == "require" {
                        let dir = Path::new(&out.path)
                            .parent()
                            .unwrap_or_else(|| Path::new(""));
                        let (s, e) = project.files_in_dir_range(dir);
                        GapScope::Range(s, e)
                    } else {
                        GapScope::Files(vec![file as u32])
                    };
                    for from in owner.sources(file) {
                        out.gap_scoped(
                            "unsupported-runtime",
                            Some(loc),
                            format!("Runtime use of {} is outside the ESM model.", ident.name),
                            Some(from),
                            None,
                            scope.clone(),
                        );
                    }
                }
            }
            AstKind::CallExpression(call) => {
                react.observe(call, scoping);
                match call.callee.get_inner_expression() {
                    Expression::Identifier(id)
                        if matches!(
                            id.name.as_str(),
                            "spawn" | "spawnSync" | "execFile" | "execFileSync"
                        ) =>
                    {
                        for (spec, _) in extract_spawn_exec_paths(
                            call,
                            scoping,
                            &require_symbols,
                            &file_ext,
                            &file_ext_symbols,
                            &local_paths,
                        ) {
                            if let Ok(Some(to)) =
                                project.resolve_with_mode(file, &spec, false, true)
                            {
                                for from in owner.sources(file) {
                                    out.edge(from, to, false, loc);
                                }
                            }
                        }
                    }
                    Expression::Identifier(id) if id.name == "fork" => {
                        if let Some(arg) = call.arguments.first().and_then(|a| a.as_expression()) {
                            for (spec, _) in resolve_arg_paths(
                                arg,
                                scoping,
                                &require_symbols,
                                &file_ext,
                                &file_ext_symbols,
                                &local_paths,
                            ) {
                                if let Ok(Some(to)) =
                                    project.resolve_with_mode(file, &spec, false, true)
                                {
                                    for from in owner.sources(file) {
                                        out.edge(from, to, false, loc);
                                    }
                                }
                            }
                        }
                    }
                    Expression::Identifier(id)
                        if matches!(id.name.as_str(), "resolve" | "join") =>
                    {
                        if let Some((spec, _)) =
                            extract_path_join_call(call, scoping, &file_ext, &file_ext_symbols)
                            && let Ok(Some(to)) =
                                project.resolve_with_mode(file, &spec, false, true)
                            && to != file
                        {
                            for from in owner.sources(file) {
                                out.edge(from, to, false, loc);
                                out.link_member(from, to, "*", "**", false, loc);
                            }
                        }
                    }
                    Expression::Identifier(_)
                        if let Some(req_span) =
                            is_require_ident(scoping, &require_symbols, &call.callee) =>
                    {
                        handled_cjs_idents.insert(req_span.start);
                        let req_arg = call.arguments.first().and_then(|a| a.as_expression());
                        let mut specs = req_arg
                            .map(|a| {
                                resolve_arg_paths(
                                    a,
                                    scoping,
                                    &require_symbols,
                                    &file_ext,
                                    &file_ext_symbols,
                                    &local_paths,
                                )
                            })
                            .unwrap_or_default();
                        let mut glob_targets = Vec::new();
                        if specs.is_empty() {
                            if let Some(arg) = req_arg
                                && let Some(matched) =
                                    match_template_prefix_glob(project, file, arg)
                            {
                                glob_targets = matched;
                            } else if let Some(configured) =
                                project.dynamic_imports.get(&project.graph.paths[file])
                            {
                                out.used_dynamic_imports = true;
                                for s in configured {
                                    specs.push((s.clone(), call.span));
                                }
                            }
                        }
                        if specs.is_empty() && glob_targets.is_empty() {
                            if !req_arg.is_some_and(is_json_dynamic_spec) {
                                let scope = req_arg.map_or(GapScope::Global, |a| {
                                    relative_template_scope(project, file, a)
                                });
                                for from in owner.sources(file) {
                                    out.gap_scoped(
                                        "computed-import",
                                        Some(loc),
                                        "Supply the possible module specifiers for this dynamic require."
                                            .into(),
                                        Some(from),
                                        None,
                                        scope.clone(),
                                    );
                                }
                            }
                        } else {
                            let is_glob = !glob_targets.is_empty();
                            let mut targets = glob_targets;
                            let mut first_err = None;
                            let mut any_external = false;
                            for (spec, spec_span) in &specs {
                                out.record_pkg_ref(project, spec, false, *spec_span);
                                match project.resolve_with_mode(file, spec, false, true) {
                                    Ok(Some(to)) if !targets.contains(&to) => targets.push(to),
                                    Ok(Some(_)) => {}
                                    Ok(None) => any_external = true,
                                    Err(msg) => {
                                        first_err.get_or_insert((spec.clone(), *spec_span, msg));
                                    }
                                }
                            }
                            if targets.is_empty()
                                && !any_external
                                && let Some((err_spec, err_span, msg)) = first_err
                            {
                                let scope = project.scope_unresolved_import(file, &err_spec);
                                for from in owner.sources(file) {
                                    out.gap_scoped(
                                        "unresolved-import",
                                        Some(err_span.into()),
                                        msg.clone(),
                                        Some(from),
                                        None,
                                        scope.clone(),
                                    );
                                }
                            }
                            let (expr_span, parent, _) =
                                unwrap_parent(nodes, node.id(), call.span, false);
                            let usage = classify_ns_use(
                                nodes,
                                scoping,
                                parent,
                                expr_span,
                                true,
                                true,
                                |_| true,
                            );
                            for to in targets {
                                for from in owner.sources(file) {
                                    out.edge(from, to, false, loc);
                                    if is_glob {
                                        out.link_member(from, to, "*", "**", false, loc);
                                        continue;
                                    }
                                    match &usage {
                                        NsUse::Discarded => {}
                                        NsUse::Props(props) => {
                                            for p in props {
                                                out.link_member(from, to, p, p, false, loc);
                                            }
                                        }
                                        NsUse::Alias(sym) => {
                                            symbol_namespaces.insert(*sym, (to, false, true));
                                        }
                                        NsUse::Escape => {
                                            out.link_member(from, to, "*", "*", false, loc)
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Expression::StaticMemberExpression(m) => match m.property.name.as_str() {
                        "defineProperty" if matches!(m.object.get_inner_expression(), Expression::Identifier(id) if id.name == "Object") => {
                            if let Some(first_arg) =
                                call.arguments.first().and_then(|a| a.as_expression())
                                && let Some(sp) = cjs_exports_target(scoping, first_arg)
                            {
                                handled_cjs_idents.insert(sp.start);
                                if let Some((prop_name, prop_span)) = call
                                    .arguments
                                    .get(1)
                                    .and_then(|a| a.as_expression())
                                    .and_then(static_string_expr)
                                    && prop_name != "__esModule"
                                    && let Some(Expression::ObjectExpression(desc)) = call
                                        .arguments
                                        .get(2)
                                        .and_then(|a| a.as_expression())
                                        .map(Expression::get_inner_expression)
                                {
                                    let val_expr = desc.properties.iter().find_map(|p| {
                                        let ObjectPropertyKind::ObjectProperty(prop) = p else {
                                            return None;
                                        };
                                        matches!(
                                            static_property_key(&prop.key, prop.computed),
                                            Some("value" | "get")
                                        )
                                        .then_some(&prop.value)
                                    });
                                    if let Some(rhs) = val_expr {
                                        register_cjs_export(
                                            out,
                                            prop_name,
                                            rhs,
                                            prop_span.into(),
                                            scoping,
                                            &bindings,
                                            &mut owner_overrides,
                                        );
                                    }
                                }
                            }
                        }
                        "assign" if matches!(m.object.get_inner_expression(), Expression::Identifier(id) if id.name == "Object") => {
                            if let Some(first_arg) =
                                call.arguments.first().and_then(|a| a.as_expression())
                            {
                                let sp =
                                    is_unbound_ident(scoping, first_arg, "exports").or_else(|| {
                                        if let Expression::StaticMemberExpression(inner) =
                                            first_arg.get_inner_expression()
                                            && inner.property.name == "exports"
                                        {
                                            is_unbound_ident(scoping, &inner.object, "module")
                                        } else {
                                            None
                                        }
                                    });
                                if let Some(sp) = sp {
                                    handled_cjs_idents.insert(sp.start);
                                    for arg in call.arguments.iter().skip(1) {
                                        if let Some(Expression::ObjectExpression(obj)) = arg
                                            .as_expression()
                                            .map(Expression::get_inner_expression)
                                        {
                                            register_cjs_object_exports(
                                                out,
                                                obj,
                                                loc,
                                                scoping,
                                                &bindings,
                                                &mut owner_overrides,
                                                false,
                                            );
                                        }
                                    }
                                }
                            }
                        }
                        "resolve"
                            if let Some(sp) =
                                is_require_ident(scoping, &require_symbols, &m.object) =>
                        {
                            handled_cjs_idents.insert(sp.start);
                            if let Some((spec, spec_span)) = call
                                .arguments
                                .first()
                                .and_then(|a| a.as_expression())
                                .and_then(static_string_expr)
                            {
                                for from in owner.sources(file) {
                                    if let Some(to) = out.target_with_mode(
                                        project,
                                        spec,
                                        spec_span,
                                        Some(from),
                                        false,
                                        true,
                                    ) {
                                        out.edge(from, to, false, loc);
                                        out.link_member(from, to, "*", "**", false, loc);
                                    }
                                }
                            }
                        }
                        "resolve"
                            if matches!(m.object.get_inner_expression(), Expression::MetaProperty(mp)
                                if mp.meta.name == "import" && mp.property.name == "meta") =>
                        {
                            if let Some((spec, _)) = call
                                .arguments
                                .first()
                                .and_then(|a| a.as_expression())
                                .and_then(static_string_expr)
                                && let Ok(Some(to)) =
                                    project.resolve_with_mode(file, spec, false, false)
                                && to != file
                            {
                                for from in owner.sources(file) {
                                    out.edge(from, to, false, loc);
                                    out.link_member(from, to, "*", "**", false, loc);
                                }
                            }
                        }
                        "resolve" | "join" => {
                            if let Some((spec, _)) =
                                extract_path_join_call(call, scoping, &file_ext, &file_ext_symbols)
                                && let Ok(Some(to)) =
                                    project.resolve_with_mode(file, &spec, false, true)
                                && to != file
                            {
                                for from in owner.sources(file) {
                                    out.edge(from, to, false, loc);
                                    out.link_member(from, to, "*", "**", false, loc);
                                }
                            }
                        }
                        "glob"
                            if matches!(m.object.get_inner_expression(), Expression::MetaProperty(mp)
                                if mp.meta.name == "import" && mp.property.name == "meta") =>
                        {
                            handle_import_meta_glob(project, out, file, owner, call, loc);
                        }
                        "spawn" | "spawnSync" | "execFile" | "execFileSync" => {
                            for (spec, _) in extract_spawn_exec_paths(
                                call,
                                scoping,
                                &require_symbols,
                                &file_ext,
                                &file_ext_symbols,
                                &local_paths,
                            ) {
                                if let Ok(Some(to)) =
                                    project.resolve_with_mode(file, &spec, false, true)
                                {
                                    for from in owner.sources(file) {
                                        out.edge(from, to, false, loc);
                                    }
                                }
                            }
                        }
                        "fork" if !matches!(m.object.get_inner_expression(), Expression::Identifier(id) if id.name == "cluster") => {
                            if let Some(arg) =
                                call.arguments.first().and_then(|a| a.as_expression())
                            {
                                for (spec, _) in resolve_arg_paths(
                                    arg,
                                    scoping,
                                    &require_symbols,
                                    &file_ext,
                                    &file_ext_symbols,
                                    &local_paths,
                                ) {
                                    if let Ok(Some(to)) =
                                        project.resolve_with_mode(file, &spec, false, true)
                                    {
                                        for from in owner.sources(file) {
                                            out.edge(from, to, false, loc);
                                        }
                                    }
                                }
                            }
                        }
                        "setupPrimary" | "setupMaster" => {
                            let exec_arg = call
                                .arguments
                                .first()
                                .and_then(|a| a.as_expression())
                                .map(Expression::get_inner_expression)
                                .and_then(|e| match e {
                                    Expression::ObjectExpression(obj) => {
                                        obj.properties.iter().find_map(|p| {
                                            let ObjectPropertyKind::ObjectProperty(prop) = p else {
                                                return None;
                                            };
                                            (static_property_key(&prop.key, prop.computed)
                                                == Some("exec"))
                                            .then_some(&prop.value)
                                        })
                                    }
                                    _ => None,
                                });
                            if let Some(arg) = exec_arg {
                                for (spec, _) in resolve_arg_paths(
                                    arg,
                                    scoping,
                                    &require_symbols,
                                    &file_ext,
                                    &file_ext_symbols,
                                    &local_paths,
                                ) {
                                    if let Ok(Some(to)) =
                                        project.resolve_with_mode(file, &spec, false, true)
                                    {
                                        for from in owner.sources(file) {
                                            out.edge(from, to, false, loc);
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
            AstKind::NewExpression(ne) => {
                let is_worker = match ne.callee.get_inner_expression() {
                    Expression::Identifier(id) => {
                        matches!(id.name.as_str(), "Worker" | "SharedWorker")
                    }
                    Expression::StaticMemberExpression(m) => {
                        matches!(m.property.name.as_str(), "Worker" | "SharedWorker")
                    }
                    _ => false,
                };
                if is_worker && let Some(arg) = ne.arguments.first().and_then(|a| a.as_expression())
                {
                    for (spec, _) in resolve_arg_paths(
                        arg,
                        scoping,
                        &require_symbols,
                        &file_ext,
                        &file_ext_symbols,
                        &local_paths,
                    ) {
                        if let Ok(Some(to)) = project.resolve_with_mode(file, &spec, false, true) {
                            for from in owner.sources(file) {
                                out.edge(from, to, false, loc);
                            }
                        }
                    }
                }
            }
            AstKind::ImportExpression(expr) => {
                let static_spec = extract_static_path_expr(
                    &expr.source,
                    scoping,
                    &require_symbols,
                    &file_ext,
                    &file_ext_symbols,
                );
                let glob_targets = if static_spec.is_none() {
                    match_template_prefix_glob(project, file, &expr.source)
                } else {
                    None
                };
                let configured_specs = if static_spec.is_none() && glob_targets.is_none() {
                    let cfg = project.dynamic_imports.get(&project.graph.paths[file]);
                    if cfg.is_some() {
                        out.used_dynamic_imports = true;
                    }
                    cfg
                } else {
                    None
                };
                for from in owner.sources(file) {
                    let mut targets = Vec::new();
                    if let Some((spec, spec_span)) = &static_spec {
                        if let Some(to) = out.target_with_mode(
                            project,
                            spec.as_str(),
                            *spec_span,
                            Some(from),
                            false,
                            false,
                        ) {
                            targets.push(to);
                        }
                    } else if let Some(matched) = &glob_targets {
                        targets.extend_from_slice(matched);
                    } else if let Some(configured) = configured_specs {
                        for spec in configured {
                            if let Some(to) = out.target_with_mode(
                                project,
                                spec.as_str(),
                                expr.span,
                                Some(from),
                                false,
                                false,
                            ) && !targets.contains(&to)
                            {
                                targets.push(to);
                            }
                        }
                    } else {
                        if !is_json_dynamic_spec(&expr.source) {
                            let scope = relative_template_scope(project, file, &expr.source);
                            out.gap_scoped(
                                "computed-import",
                                Some(loc),
                                "Supply the possible module specifiers for this dynamic import."
                                    .into(),
                                Some(from),
                                None,
                                scope,
                            );
                        }
                        continue;
                    }
                    for to in targets {
                        out.edge(from, to, false, loc);
                        if let Some(member) = react.member(expr.span.start) {
                            out.link_member(from, to, member, member, false, loc);
                            continue;
                        }
                        if glob_targets.is_some() {
                            out.link_member(from, to, "*", "**", false, loc);
                            continue;
                        }
                        let (expr_span, parent, awaited) =
                            unwrap_parent(nodes, node.id(), expr.span, true);
                        let usage = if let Some(pa) =
                            resolve_promise_all_use(nodes, node.id(), expr_span)
                        {
                            pa
                        } else if awaited {
                            classify_ns_use(
                                nodes,
                                scoping,
                                parent,
                                expr_span,
                                false,
                                true,
                                |_| true,
                            )
                        } else if parent.is_some_and(|p| {
                            matches!(p.kind(), AstKind::ExpressionStatement(_))
                                && !nodes
                                    .parent_id(p.id())
                                    .and_then(|id| nodes.parent_kind(id))
                                    .is_some_and(|k| matches!(k, AstKind::ArrowFunctionExpression(a) if a.expression))
                        }) {
                            NsUse::Discarded
                        } else if let Some(p) = parent
                            && matches!(p.kind(), AstKind::StaticMemberExpression(m) if m.property.name == "then")
                            && let Some(AstKind::CallExpression(call)) = nodes.parent_kind(p.id())
                            && let Some(cb) = call.arguments.first().and_then(|a| a.as_expression()).map(Expression::get_inner_expression)
                        {
                            let params = match cb {
                                Expression::ArrowFunctionExpression(a) => Some(&a.params),
                                Expression::FunctionExpression(f) => Some(&f.params),
                                _ => None,
                            };
                            match params.and_then(|ps| ps.items.first()) {
                                Some(param) => match &param.pattern.kind {
                                    BindingPatternKind::ObjectPattern(pat) => {
                                        object_pattern_keys(pat).map_or(NsUse::Escape, NsUse::Props)
                                    }
                                    BindingPatternKind::BindingIdentifier(bind) => {
                                        bind.symbol_id.get().map_or(NsUse::Escape, NsUse::Alias)
                                    }
                                    _ => NsUse::Escape,
                                },
                                None => NsUse::Escape,
                            }
                        } else {
                            NsUse::Escape
                        };
                        match usage {
                            NsUse::Discarded => {}
                            NsUse::Props(props) => {
                                for p in props {
                                    out.link_member(from, to, p, p, false, loc);
                                }
                            }
                            NsUse::Alias(sym) => {
                                symbol_namespaces.insert(sym, (to, false, false));
                            }
                            NsUse::Escape => out.gap(
                                "namespace-escape",
                                Some(loc),
                                "The literal import resolves, but uses of its namespace are unknown."
                                    .into(),
                                Some(from),
                                Some(to),
                            ),
                        }
                    }
                }
            }
            AstKind::TSImportType(imp) => {
                let literal_spec = match &imp.argument {
                    TSType::TSLiteralType(lit) => match &lit.literal {
                        TSLiteral::StringLiteral(s) => Some((s.value.as_str(), s.span)),
                        TSLiteral::TemplateLiteral(t)
                            if t.expressions.is_empty() && t.quasis.len() == 1 =>
                        {
                            t.quasis[0]
                                .value
                                .cooked
                                .as_ref()
                                .map(|c| (c.as_str(), t.span))
                        }
                        _ => None,
                    },
                    _ => None,
                };
                if let Some((spec, spec_span)) = literal_spec {
                    for from in owner.sources(file) {
                        if let Some(to) =
                            out.target_with_mode(project, spec, spec_span, Some(from), true, false)
                        {
                            out.edge(from, to, true, loc);
                            if let Some(prop) = imp.qualifier.as_ref().and_then(leftmost_type_name)
                            {
                                out.link_member(from, to, prop, prop, true, loc);
                            } else {
                                // Whole-module `typeof import("...")` retains the module's type surface without evaluating it.
                                out.link_member(from, to, "*", "**", true, loc);
                            }
                        }
                    }
                } else {
                    out.gap_scoped(
                        "unsupported-syntax",
                        Some(loc),
                        "This TypeScript module construct requires additional resolution.".into(),
                        Some(owner.binding),
                        None,
                        GapScope::Files(vec![file as u32]),
                    );
                }
            }
            AstKind::TSImportEqualsDeclaration(decl) => {
                let is_type = declaration_file || decl.import_kind.is_type();
                match &decl.module_reference {
                    TSModuleReference::ExternalModuleReference(ext) => {
                        if let Some(to) = out.target_with_mode(
                            project,
                            ext.expression.value.as_str(),
                            ext.expression.span,
                            None,
                            is_type,
                            true,
                        ) {
                            out.edge(file, to, is_type, loc);
                            if let Some(sym) = decl.id.symbol_id.get() {
                                symbol_namespaces.insert(sym, (to, is_type, true));
                            }
                        }
                    }
                    other => {
                        // Internal TypeScript namespace alias (`import X = Namespace.Member`).
                        let (root_ident, member) = match other {
                            TSModuleReference::IdentifierReference(id) => (Some(id.as_ref()), None),
                            TSModuleReference::QualifiedName(q) => {
                                let mut cur = q.as_ref();
                                let mut prop = cur.right.name.as_str();
                                let found = loop {
                                    match &cur.left {
                                        TSTypeName::IdentifierReference(id) => break id.as_ref(),
                                        TSTypeName::QualifiedName(inner) => {
                                            prop = inner.right.name.as_str();
                                            cur = inner.as_ref();
                                        }
                                    }
                                };
                                (Some(found), Some(prop))
                            }
                            TSModuleReference::ExternalModuleReference(_) => unreachable!(),
                        };
                        if let Some(id) = root_ident
                            && let Some(ref_id) = id.reference_id.get()
                            && let Some(sym) = scoping.get_reference(ref_id).symbol_id()
                        {
                            if let Some(&(to, type_import, is_cjs)) = symbol_namespaces.get(&sym) {
                                let eff_type = is_type || type_import;
                                if let Some(prop) = member {
                                    for from in owner.sources(file) {
                                        out.link_member(from, to, prop, prop, eff_type, loc);
                                    }
                                } else if let Some(alias_sym) = decl.id.symbol_id.get() {
                                    symbol_namespaces.insert(alias_sym, (to, eff_type, is_cjs));
                                }
                            } else if let Some(&target_binding) = bindings.get(&sym) {
                                for from in owner.sources(file) {
                                    out.edge(from, target_binding, is_type, loc);
                                }
                            }
                        }
                    }
                }
            }
            AstKind::TSExportAssignment(exp) => {
                if let Expression::ObjectExpression(obj) = exp.expression.get_inner_expression() {
                    register_cjs_object_exports(
                        out,
                        obj,
                        loc,
                        scoping,
                        &bindings,
                        &mut owner_overrides,
                        true,
                    );
                } else {
                    register_cjs_export(
                        out,
                        "default",
                        &exp.expression,
                        loc,
                        scoping,
                        &bindings,
                        &mut owner_overrides,
                    );
                    out.module.cjs_opaque_default = true;
                }
            }
            _ => {}
        }
    }
    ok
}
