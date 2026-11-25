//! React wrapper semantics are selected by the imported lexical binding.
use std::collections::{HashMap, HashSet};

use oxc_ast::ast::{
    BindingPatternKind, CallExpression, Expression, FunctionBody, IdentifierReference,
    ObjectPropertyKind, PropertyKey, Statement,
};
use oxc_semantic::Scoping;
use oxc_span::GetSpan;
use oxc_syntax::{
    module_record::{ImportEntry, ImportImportName},
    symbol::SymbolId,
};

#[derive(Clone, Copy, PartialEq)]
enum Api {
    Namespace,
    Lazy,
    Memo,
    ForwardRef,
}

impl Api {
    fn named(name: &str) -> Option<Self> {
        match name {
            "lazy" => Some(Self::Lazy),
            "memo" => Some(Self::Memo),
            "forwardRef" => Some(Self::ForwardRef),
            "default" => Some(Self::Namespace),
            _ => None,
        }
    }
}

#[derive(Default)]
pub struct ReactCalls<'a> {
    imports: HashMap<SymbolId, Api>,
    initializers: HashSet<u32>,
    deferred: HashSet<u32>,
    members: HashMap<u32, &'a str>,
}

fn symbol(scoping: &Scoping, ident: &IdentifierReference<'_>) -> Option<SymbolId> {
    scoping.get_reference(ident.reference_id.get()?).symbol_id()
}

// Match only callbacks whose returned value has an unambiguous origin.
fn returned<'a>(callback: &'a Expression<'a>) -> Option<&'a Expression<'a>> {
    fn single_return<'a>(body: &'a FunctionBody<'a>) -> Option<&'a Expression<'a>> {
        let [Statement::ReturnStatement(stmt)] = &body.statements[..] else {
            return None;
        };
        stmt.argument.as_ref()
    }
    match callback.get_inner_expression() {
        Expression::ArrowFunctionExpression(arrow) => arrow
            .get_expression()
            .or_else(|| single_return(&arrow.body)),
        Expression::FunctionExpression(function) => single_return(function.body.as_deref()?),
        _ => None,
    }
    .map(Expression::get_inner_expression)
}

fn lazy_member<'a>(loader: &'a Expression<'a>, scoping: &Scoping) -> Option<(u32, &'a str)> {
    let value = returned(loader)?;
    if let Expression::ImportExpression(import) = value {
        return Some((import.span.start, "default"));
    }
    // import("./ui").then(m => ({ default: m.Panel }))
    let Expression::CallExpression(then) = value else {
        return None;
    };
    let Expression::StaticMemberExpression(method) = then.callee.get_inner_expression() else {
        return None;
    };
    let Expression::ImportExpression(import) = method.object.get_inner_expression() else {
        return None;
    };
    let [adapter] = &then.arguments[..] else {
        return None;
    };
    let adapter = adapter.as_expression()?.get_inner_expression();
    let Expression::ArrowFunctionExpression(arrow) = adapter else {
        return None;
    };
    let [param] = &arrow.params.items[..] else {
        return None;
    };
    let Expression::ObjectExpression(object) = returned(adapter)? else {
        return None;
    };
    let [ObjectPropertyKind::ObjectProperty(property)] = &object.properties[..] else {
        return None;
    };
    if method.property.name != "then"
        || property.computed
        || property.key.static_name()?.as_ref() != "default"
    {
        return None;
    }
    let member = match (&param.pattern.kind, property.value.get_inner_expression()) {
        (
            BindingPatternKind::BindingIdentifier(binding),
            Expression::StaticMemberExpression(read),
        ) => {
            let Expression::Identifier(namespace) = read.object.get_inner_expression() else {
                return None;
            };
            (symbol(scoping, namespace)? == binding.symbol_id.get()?)
                .then_some(read.property.name.as_str())?
        }
        (BindingPatternKind::ObjectPattern(pattern), Expression::Identifier(local)) => {
            let [field] = &pattern.properties[..] else {
                return None;
            };
            let PropertyKey::StaticIdentifier(key) = &field.key else {
                return None;
            };
            let BindingPatternKind::BindingIdentifier(binding) = &field.value.kind else {
                return None;
            };
            (pattern.rest.is_none()
                && !field.computed
                && symbol(scoping, local)? == binding.symbol_id.get()?)
            .then_some(key.name.as_str())?
        }
        _ => return None,
    };
    Some((import.span.start, member))
}

impl<'a> ReactCalls<'a> {
    pub fn register(&mut self, import: &ImportEntry<'_>, scoping: &Scoping) {
        if import.is_type || import.module_request.name != "react" {
            return;
        }
        let api = match &import.import_name {
            ImportImportName::Name(name) => Api::named(name.name.as_str()),
            _ => Some(Api::Namespace),
        };
        if let (Some(api), Some(symbol)) = (
            api,
            scoping.get_root_binding(import.local_name.name.as_str()),
        ) {
            self.imports.insert(symbol, api);
        }
    }

    pub fn initializer(&mut self, call: &CallExpression<'_>) {
        if !self.imports.is_empty() {
            self.initializers.insert(call.span.start);
        }
    }

    pub fn observe(&mut self, call: &'a CallExpression<'a>, scoping: &Scoping) {
        if self.imports.is_empty() {
            return;
        }
        let api = match call.callee.get_inner_expression() {
            Expression::Identifier(id) => {
                symbol(scoping, id).and_then(|s| self.imports.get(&s).copied())
            }
            Expression::StaticMemberExpression(member) => {
                let Expression::Identifier(id) = member.object.get_inner_expression() else {
                    return;
                };
                if symbol(scoping, id).and_then(|s| self.imports.get(&s)) != Some(&Api::Namespace) {
                    return;
                }
                Api::named(member.property.name.as_str())
            }
            _ => None,
        };
        let Some(api @ (Api::Lazy | Api::Memo | Api::ForwardRef)) = api else {
            return;
        };
        if let Some(callback) = call.arguments.first().and_then(|arg| arg.as_expression())
            && api == Api::Lazy
            && let Some((pos, member)) = lazy_member(callback, scoping)
        {
            self.members.insert(pos, member);
        }
        if self.initializers.remove(&call.span.start) {
            if api == Api::Memo
                && let Some(Expression::CallExpression(inner)) = call
                    .arguments
                    .first()
                    .and_then(|a| a.as_expression())
                    .map(Expression::get_inner_expression)
            {
                self.initializer(inner);
            }
            for arg in call
                .arguments
                .iter()
                .take(if api == Api::Memo { 2 } else { 1 })
            {
                if let Some(expr) = arg.as_expression().map(Expression::get_inner_expression)
                    && expr.is_function()
                {
                    self.deferred.insert(expr.span().start);
                }
            }
        }
    }

    pub fn is_deferred(&self, pos: u32) -> bool {
        self.deferred.contains(&pos)
    }
    pub fn member(&self, pos: u32) -> Option<&'a str> {
        self.members.get(&pos).copied()
    }
}
