//! Static discovery of runtime and tooling entries from package.json scripts,
//! Vite/webpack configs (including Module Federation exposes), and Storybook configs.
use std::fs;
use std::path::{Path, PathBuf};

use oxc_allocator::Allocator;
use oxc_ast::{
    AstKind,
    ast::{
        Expression, ObjectPropertyKind, PropertyKey, Statement, TSModuleDeclarationKind,
        TSModuleDeclarationName,
    },
};
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::SourceType;
use rayon::prelude::*;
use rustc_hash::FxHashSet;
use serde_json::Value;

use crate::graph::{Role, Root, RootEntryMode};
use crate::project::{EXTENSIONS, Project, html_script_sources};
use crate::source_map::{normalize_rel, to_slash};

pub enum ScriptOutcome {
    Entries(Vec<String>),
    DelegatedOrNoop,
    Ambiguous,
}

pub(crate) fn split_shell_words(input: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut chars = input.chars().peekable();
    let mut quote = None;
    let mut in_word = false;
    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => quote = None,
            Some('"') if c == '\\' => {
                if let Some(next) = chars.next() {
                    cur.push(next);
                }
            }
            Some(_) => cur.push(c),
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    in_word = true;
                }
                '\\' => {
                    if let Some(next) = chars.next() {
                        cur.push(next);
                        in_word = true;
                    }
                }
                c if c.is_whitespace() => {
                    if in_word {
                        words.push(std::mem::take(&mut cur));
                        in_word = false;
                    }
                }
                ';' | '|' | '&' => {
                    if in_word {
                        words.push(std::mem::take(&mut cur));
                        in_word = false;
                    }
                    let mut op = String::from(c);
                    if (c == '|' || c == '&') && chars.peek() == Some(&c) {
                        op.push(chars.next().unwrap());
                    }
                    words.push(op);
                }
                _ => {
                    cur.push(c);
                    in_word = true;
                }
            },
        }
    }
    if in_word {
        words.push(cur);
    }
    words
}

fn is_env_assign(word: &str) -> bool {
    word.split_once('=').is_some_and(|(k, _)| {
        !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

fn is_noop_command(cmd: &[String]) -> bool {
    matches!(
        cmd.first().map(String::as_str),
        None | Some(
            "echo" | "true" | "/bin/true" | "/usr/bin/true" | "test" | "[" | ":" | "if" | "fi"
        )
    )
}

fn extract_runner_entry(cmd: &[String]) -> Option<String> {
    let mut i = 0;
    while i < cmd.len()
        && (is_env_assign(&cmd[i]) || matches!(cmd[i].as_str(), "exec" | "then" | "else" | "do"))
    {
        i += 1;
    }
    let runner = Path::new(cmd.get(i)?.as_str())
        .file_name()
        .and_then(|s| s.to_str())?;
    if !matches!(runner, "node" | "bun" | "ts-node" | "tsx" | "nodemon") {
        return None;
    }
    i += 1;
    if runner == "bun" && cmd.get(i).map(String::as_str) == Some("run") {
        i += 1;
    }
    while let Some(arg) = cmd.get(i).map(String::as_str) {
        if arg == "--" {
            return cmd.get(i + 1).filter(|a| !a.starts_with('$')).cloned();
        }
        if matches!(arg, "-e" | "--eval" | "-p" | "--print" | "-c" | "--check")
            && runner != "nodemon"
        {
            return None;
        }
        if runner == "nodemon" && matches!(arg, "-x" | "--exec") {
            if let Some(exec_str) = cmd.get(i + 1) {
                let inner_words = split_shell_words(exec_str);
                if let Some(found) = extract_runner_entry(&inner_words) {
                    return Some(found);
                }
            }
            i += 2;
            continue;
        }
        if matches!(
            arg,
            "-r" | "--require"
                | "--import"
                | "--loader"
                | "--experimental-loader"
                | "--conditions"
                | "-C"
                | "--cwd"
                | "-P"
                | "--project"
                | "--tsconfig"
                | "--env-file"
                | "--preload"
                | "-w"
                | "--watch"
                | "-e"
                | "--ext"
                | "-i"
                | "--ignore"
                | "-d"
                | "--delay"
                | "--config"
                | "--signal"
        ) {
            i += 2;
        } else if arg.starts_with('-') {
            i += 1;
        } else {
            return (!arg.starts_with('$')).then(|| arg.to_owned());
        }
    }
    None
}

fn discover_framework_start_entries(
    project: &Project,
    pkg_dir: &Path,
    cmd: &[String],
) -> Option<Vec<String>> {
    let mut i = 0;
    while i < cmd.len()
        && (is_env_assign(&cmd[i]) || matches!(cmd[i].as_str(), "exec" | "then" | "else" | "do"))
    {
        i += 1;
    }
    let bin = Path::new(cmd.get(i)?.as_str())
        .file_name()
        .and_then(|s| s.to_str())?;
    fn strip_pkg_prefix<'a>(prefix: &str, p: &'a str) -> Option<&'a str> {
        if prefix.is_empty() {
            Some(p)
        } else {
            p.strip_prefix(prefix).and_then(|r| r.strip_prefix('/'))
        }
    }
    let prefix = to_slash(pkg_dir);
    match bin {
        "next" => {
            let mut found = Vec::new();
            for p in &project.graph.paths {
                let Some(rel) = strip_pkg_prefix(&prefix, p) else {
                    continue;
                };
                let stripped = rel.strip_prefix("src/").unwrap_or(rel);
                let is_next_entry = stripped.starts_with("pages/")
                    || (stripped.starts_with("app/")
                        && Path::new(stripped)
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .is_some_and(|s| {
                                matches!(
                                    s,
                                    "page"
                                        | "layout"
                                        | "route"
                                        | "loading"
                                        | "error"
                                        | "global-error"
                                        | "not-found"
                                        | "template"
                                        | "default"
                                        | "opengraph-image"
                                        | "twitter-image"
                                        | "icon"
                                        | "apple-icon"
                                        | "sitemap"
                                        | "robots"
                                        | "manifest"
                                )
                            }))
                    || matches!(
                        stripped,
                        "middleware.ts"
                            | "middleware.js"
                            | "instrumentation.ts"
                            | "instrumentation.js"
                    );
                if is_next_entry {
                    found.push(p.clone());
                }
            }
            Some(found)
        }
        "vite" => {
            let html = pkg_dir.join("index.html");
            let found = html_script_sources(&project.root, &html, &project.graph.paths);
            if !found.is_empty()
                || project.roots.iter().any(|r| {
                    r.role == Role::Runtime && strip_pkg_prefix(&prefix, &r.file).is_some()
                })
            {
                Some(found)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn extract_delegation(cmd: &[String]) -> Option<(String, String)> {
    let mut i = 0;
    while i < cmd.len() && (is_env_assign(&cmd[i]) || cmd[i] == "exec") {
        i += 1;
    }
    if !matches!(cmd.get(i)?.as_str(), "bun" | "npm" | "pnpm" | "yarn") {
        return None;
    }
    i += 1;
    if cmd.get(i).map(String::as_str) == Some("run") {
        i += 1;
    }
    let mut cwd = None;
    while let Some(arg) = cmd.get(i).map(String::as_str) {
        if matches!(arg, "--cwd" | "--prefix" | "-C") {
            cwd = cmd.get(i + 1).cloned();
            i += 2;
        } else if arg.starts_with('-') {
            i += 1;
        } else {
            return cwd.map(|dir| (dir, arg.to_owned()));
        }
    }
    None
}

fn collect_shell_subcommands(script: &str, out: &mut Vec<Vec<String>>) {
    let words = split_shell_words(script);
    let mut current = Vec::new();
    let mut i = 0;
    while i < words.len() {
        let w = words[i].as_str();
        if matches!(w, "&&" | "||" | ";" | "|") {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            i += 1;
            continue;
        }
        if current.is_empty()
            && matches!(w, "sh" | "bash" | "/bin/sh" | "/bin/bash")
            && words.get(i + 1).map(String::as_str) == Some("-c")
            && let Some(inner) = words.get(i + 2)
        {
            collect_shell_subcommands(inner, out);
            break;
        }
        current.push(words[i].clone());
        i += 1;
    }
    if !current.is_empty() {
        out.push(current);
    }
}

pub fn discover_tooling_scripts(project: &mut Project, packages: &[(PathBuf, Value)]) {
    for (dir, pkg) in packages {
        let Some(scripts) = pkg["scripts"].as_object() else {
            continue;
        };
        let manifest = dir.join("package.json");
        for (name, val) in scripts {
            if name == "start" {
                continue;
            }
            let Some(script) = val.as_str() else {
                continue;
            };
            let mut subcommands = Vec::new();
            collect_shell_subcommands(script, &mut subcommands);
            let label = format!("{}#scripts.{name}", manifest.display());
            for cmd in &subcommands {
                if let Some(entry_arg) = extract_runner_entry(cmd) {
                    let rel = dir.join(entry_arg.trim_start_matches("./"));
                    if let Some(id) = project.source_mapper.map_to_source(
                        &project.root,
                        &project.graph.paths,
                        &rel,
                    ) {
                        let file = project.graph.paths[id].clone();
                        project.roots.push(Root {
                            file,
                            role: Role::Tooling,
                            origin: "discovered",
                            input: label.clone(),
                            mode: RootEntryMode::ModuleOnly,
                        });
                    }
                }
            }
        }
    }
}

pub fn parse_start_script(
    project: &Project,
    pkg_dir: &Path,
    script: &str,
    packages: &[(PathBuf, Value)],
    depth: usize,
) -> ScriptOutcome {
    if depth > 4 {
        return ScriptOutcome::Ambiguous;
    }
    let mut subcommands = Vec::new();
    collect_shell_subcommands(script, &mut subcommands);
    if subcommands.is_empty() {
        return ScriptOutcome::DelegatedOrNoop;
    }
    let mut entries = Vec::new();
    let mut saw_delegation = false;
    let mut all_noop = true;
    for cmd in &subcommands {
        if is_noop_command(cmd) {
            continue;
        }
        all_noop = false;
        if let Some((target_dir, script_name)) = extract_delegation(cmd) {
            saw_delegation = true;
            if let Some(norm_dir) = normalize_rel(&pkg_dir.join(&target_dir))
                && let Some((_, target_pkg)) = packages.iter().find(|(d, _)| d == &norm_dir)
                && let Some(target_script) = target_pkg
                    .get("scripts")
                    .and_then(|s| s.get(&script_name))
                    .and_then(Value::as_str)
                && let ScriptOutcome::Entries(sub) =
                    parse_start_script(project, &norm_dir, target_script, packages, depth + 1)
            {
                entries.extend(sub);
            }
            continue;
        }
        if let Some(fw_entries) = discover_framework_start_entries(project, pkg_dir, cmd) {
            saw_delegation = true;
            for path in fw_entries {
                if !entries.contains(&path) {
                    entries.push(path);
                }
            }
            continue;
        }
        if let Some(entry_arg) = extract_runner_entry(cmd) {
            let rel = pkg_dir.join(entry_arg.trim_start_matches("./"));
            if let Some(id) =
                project
                    .source_mapper
                    .map_to_source(&project.root, &project.graph.paths, &rel)
            {
                let path = project.graph.paths[id].clone();
                if !entries.contains(&path) {
                    entries.push(path);
                }
            }
        }
    }
    if !entries.is_empty() {
        ScriptOutcome::Entries(entries)
    } else if all_noop || saw_delegation {
        ScriptOutcome::DelegatedOrNoop
    } else {
        ScriptOutcome::Ambiguous
    }
}

fn static_str<'a>(expr: &'a Expression<'a>) -> Option<&'a str> {
    match expr.get_inner_expression() {
        Expression::StringLiteral(s) => Some(s.value.as_str()),
        Expression::TemplateLiteral(t) if t.expressions.is_empty() && t.quasis.len() == 1 => {
            t.quasis[0].value.cooked.as_ref().map(|c| c.as_str())
        }
        _ => None,
    }
}

fn prop_key_name<'a>(key: &'a PropertyKey<'a>) -> Option<&'a str> {
    match key {
        PropertyKey::StaticIdentifier(id) => Some(id.name.as_str()),
        PropertyKey::StringLiteral(s) => Some(s.value.as_str()),
        _ => None,
    }
}

fn extract_config_path_with_locals(
    expr: &Expression<'_>,
    locals: &std::collections::HashMap<String, String>,
) -> Option<String> {
    let inner = expr.get_inner_expression();
    if let Some(s) = static_str(inner) {
        let trimmed = s.trim();
        return (!trimmed.is_empty()).then(|| trimmed.to_owned());
    }
    if let Expression::Identifier(id) = inner {
        return locals.get(id.name.as_str()).cloned();
    }
    if let Expression::NewExpression(ne) = inner
        && matches!(ne.callee.get_inner_expression(), Expression::Identifier(id) if id.name == "URL")
        && ne.arguments.len() >= 2
        && let Some(first) = ne
            .arguments
            .first()
            .and_then(|a| a.as_expression())
            .and_then(static_str)
    {
        return Some(first.trim().to_owned());
    }
    let Expression::CallExpression(call) = inner else {
        return None;
    };
    if matches!(
        call.callee.get_inner_expression(),
        Expression::Identifier(id) if id.name == "fileURLToPath"
    ) && let Some(arg) = call.arguments.first().and_then(|a| a.as_expression())
    {
        return extract_config_path_with_locals(arg, locals);
    }
    let is_join_or_resolve = match call.callee.get_inner_expression() {
        Expression::Identifier(id) => matches!(id.name.as_str(), "resolve" | "join"),
        Expression::StaticMemberExpression(m) => {
            matches!(m.property.name.as_str(), "resolve" | "join")
        }
        _ => false,
    };
    if !is_join_or_resolve || call.arguments.len() < 2 {
        return None;
    }
    let first_expr = call
        .arguments
        .first()?
        .as_expression()?
        .get_inner_expression();
    let mut parts = Vec::new();
    let first_ok = match first_expr {
        Expression::Identifier(id) => {
            if let Some(base) = locals.get(id.name.as_str()) {
                if !base.is_empty() && base != "." {
                    parts.push(
                        base.trim_start_matches("./")
                            .trim_end_matches('/')
                            .to_owned(),
                    );
                }
                true
            } else {
                matches!(id.name.as_str(), "__dirname" | "ROOT" | "root" | "dir")
            }
        }
        Expression::StaticMemberExpression(m) => m.property.name == "dirname",
        Expression::CallExpression(c) => match c.callee.get_inner_expression() {
            Expression::StaticMemberExpression(m) => {
                m.property.name == "cwd"
                    && matches!(m.object.get_inner_expression(), Expression::Identifier(id) if id.name == "process")
            }
            _ => false,
        },
        _ => false,
    };
    if !first_ok {
        return None;
    }
    for arg in call.arguments.iter().skip(1) {
        let s = arg.as_expression().and_then(static_str)?;
        let cleaned = s.trim_start_matches("./").trim_end_matches('/');
        if !cleaned.is_empty() {
            parts.push(cleaned.to_owned());
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn collect_paths_from_expr(
    expr: &Expression<'_>,
    locals: &std::collections::HashMap<String, String>,
    out: &mut Vec<String>,
) {
    match expr.get_inner_expression() {
        Expression::ArrayExpression(arr) => {
            for elem in &arr.elements {
                if let Some(e) = elem.as_expression() {
                    collect_paths_from_expr(e, locals, out);
                }
            }
        }
        Expression::ObjectExpression(obj) => {
            for prop in &obj.properties {
                if let ObjectPropertyKind::ObjectProperty(p) = prop {
                    collect_paths_from_expr(&p.value, locals, out);
                }
            }
        }
        other => {
            if let Some(p) = extract_config_path_with_locals(other, locals) {
                out.push(p);
            }
        }
    }
}

fn resolve_discovered_path(project: &Project, cfg_dir: &Path, raw: &str) -> Option<usize> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.contains(['*', '$']) {
        return None;
    }
    let clean = trimmed.trim_start_matches("./");
    let candidates = [cfg_dir.join(clean), PathBuf::from(clean)];
    for rel in candidates {
        if let Some(id) =
            project
                .source_mapper
                .map_to_source(&project.root, &project.graph.paths, &rel)
        {
            return Some(id);
        }
    }
    None
}

const BUNDLER_TOOLS: &[&str] = &[
    "vite", "webpack", "tsdown", "tsup", "rollup", "esbuild", "rspack", "rsbuild", "next", "nuxt",
    "astro", "svelte",
];

const WEBPACK_ENV_CONFIGS: &[&str] = &["common", "dev", "prod", "base"];

const TOOL_CONFIGS: &[&str] = &[
    "vite",
    "vitest",
    "jest",
    "webpack",
    "rollup",
    "eslint",
    "next",
    "tailwind",
    "postcss",
    "babel",
    "prettier",
    "playwright",
    "cypress",
    "tsdown",
    "tsup",
    "esbuild",
    "rspack",
    "rsbuild",
    "nuxt",
    "astro",
    "svelte",
    "metro",
    "uno",
    "knip",
    "commitlint",
    "lint-staged",
    "stylelint",
    "drizzle",
    "prisma",
    "oxlint",
    "swc",
    "turbo",
];

const RC_CONFIGS: &[&str] = &[
    ".eslintrc",
    ".prettierrc",
    ".babelrc",
    ".lintstagedrc",
    ".stylelintrc",
];

pub fn is_bundler_config_name(name: &str) -> bool {
    let Some((stem, ext)) = name.rsplit_once('.') else {
        return false;
    };
    if !EXTENSIONS.contains(&ext) {
        return false;
    }
    if let Some(tool) = stem.strip_suffix(".config") {
        let base = tool.split('.').next().unwrap_or(tool);
        return BUNDLER_TOOLS.contains(&base);
    }
    stem.strip_prefix("webpack.")
        .is_some_and(|env| WEBPACK_ENV_CONFIGS.contains(&env))
}

fn is_tool_config_name(name: &str) -> bool {
    let Some((stem, ext)) = name.rsplit_once('.') else {
        return false;
    };
    if !EXTENSIONS.contains(&ext) {
        return false;
    }
    if is_bundler_config_name(name) || RC_CONFIGS.contains(&stem) {
        return true;
    }
    stem.strip_suffix(".config")
        .is_some_and(|tool| TOOL_CONFIGS.contains(&tool.split('.').next().unwrap_or(tool)))
}

fn is_bundler_config_file(path: &str) -> bool {
    if path.contains("/.storybook/") || path.starts_with(".storybook/") {
        return false;
    }
    Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .is_some_and(is_bundler_config_name)
}

fn is_build_script_candidate(path: &str) -> bool {
    let has_scripts_dir = path.starts_with("scripts/")
        || path.contains("/scripts/")
        || path.starts_with("tools/")
        || path.contains("/tools/");
    let name = Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    has_scripts_dir
        && (name.contains("build") || name.contains("bundle") || name.contains("esbuild"))
}

fn read_project_source<'a>(
    project: &'a Project,
    id: usize,
    rel: &str,
    scratch: &'a mut String,
) -> Option<&'a str> {
    if let Some(res) = project.source_text(id) {
        return res.ok();
    }
    scratch.clear();
    let abs = project.root.join(rel);
    *scratch = fs::read_to_string(&abs).ok()?;
    Some(scratch.as_str())
}

/// Inspect Vite, webpack, esbuild, tsdown, rollup configs and build scripts statically to
/// discover runtime entries (`input`, `entry`, `entryPoints`, `.html` `<script src>`,
/// Module Federation `exposes`, `resolve.alias`, and Vite `resolveId` targets).
pub fn discover_bundler_entries(project: &mut Project) {
    let config_files: Vec<(usize, String, bool)> = project
        .graph
        .paths
        .iter()
        .enumerate()
        .filter_map(|(id, p)| {
            if is_bundler_config_file(p) {
                Some((id, p.clone(), true))
            } else if is_build_script_candidate(p) {
                Some((id, p.clone(), false))
            } else {
                None
            }
        })
        .collect();

    if config_files.is_empty() {
        return;
    }
    let inspect = |(arena, scratch): &mut (Allocator, String),
                   &(id, ref cfg, is_config): &(usize, String, bool)|
     -> Vec<Root> {
        let mut out = Vec::new();
        let Some(source) = read_project_source(project, id, cfg, scratch) else {
            return out;
        };
        if !is_config
            && !source.contains("entryPoints")
            && !source.contains("entry")
            && !source.contains("input")
            && !source.contains("exposes")
        {
            return out;
        }
        let abs = project.root.join(cfg);
        arena.reset();
        let parsed = Parser::new(
            arena,
            source,
            SourceType::from_path(&abs).unwrap_or_default(),
        )
        .parse();
        if parsed.panicked {
            return out;
        }
        let sem = SemanticBuilder::new().build(&parsed.program).semantic;
        let nodes = sem.nodes();
        let mut locals = std::collections::HashMap::new();
        for node in nodes.iter() {
            if let AstKind::VariableDeclarator(v) = node.kind()
                && let oxc_ast::ast::BindingPatternKind::BindingIdentifier(bind) = &v.id.kind
                && let Some(init) = v.init.as_ref()
                && let Some(val) = extract_config_path_with_locals(init, &locals)
            {
                locals.insert(bind.name.to_string(), val);
            }
        }
        let mut exposes = Vec::new();
        let mut inputs = Vec::new();
        let mut aliases = Vec::new();
        for node in nodes.iter() {
            match node.kind() {
                AstKind::ObjectProperty(prop) => match prop_key_name(&prop.key) {
                    Some("exposes") => collect_paths_from_expr(&prop.value, &locals, &mut exposes),
                    Some("input") | Some("entryPoints") => {
                        collect_paths_from_expr(&prop.value, &locals, &mut inputs)
                    }
                    Some("entry") => {
                        let in_remotes = nodes.ancestor_kinds(node.id()).any(|k| {
                            matches!(k, AstKind::ObjectProperty(anc) if prop_key_name(&anc.key) == Some("remotes"))
                        });
                        if !in_remotes {
                            collect_paths_from_expr(&prop.value, &locals, &mut inputs);
                        }
                    }
                    Some("alias") if is_config => {
                        collect_paths_from_expr(&prop.value, &locals, &mut aliases);
                    }
                    Some("replacement") if is_config => {
                        let in_alias = nodes.ancestor_kinds(node.id()).any(|k| {
                            matches!(k, AstKind::ObjectProperty(anc) if prop_key_name(&anc.key) == Some("alias"))
                        });
                        if in_alias {
                            collect_paths_from_expr(&prop.value, &locals, &mut aliases);
                        }
                    }
                    _ => {}
                },
                AstKind::ReturnStatement(ret) if is_config => {
                    let in_resolve_id = nodes.ancestor_kinds(node.id()).any(|k| match k {
                        AstKind::ObjectProperty(anc) => {
                            prop_key_name(&anc.key) == Some("resolveId")
                        }
                        AstKind::MethodDefinition(m) => prop_key_name(&m.key) == Some("resolveId"),
                        _ => false,
                    });
                    if in_resolve_id && let Some(arg) = ret.argument.as_ref() {
                        collect_paths_from_expr(arg, &locals, &mut aliases);
                    }
                }
                _ => {}
            }
        }
        let cfg_dir = Path::new(cfg).parent().unwrap_or_else(|| Path::new(""));
        for raw in exposes {
            if let Some(id) = resolve_discovered_path(project, cfg_dir, &raw) {
                out.push(Root {
                    file: project.graph.paths[id].clone(),
                    role: Role::Runtime,
                    origin: "discovered",
                    input: format!("{cfg}#exposes"),
                    mode: RootEntryMode::AllExports,
                });
            }
        }
        for raw in aliases {
            if let Some(id) = resolve_discovered_path(project, cfg_dir, &raw)
                && project.graph.paths[id] != *cfg
            {
                out.push(Root {
                    file: project.graph.paths[id].clone(),
                    role: Role::Runtime,
                    origin: "discovered",
                    input: format!("{cfg}#alias"),
                    mode: RootEntryMode::AllExports,
                });
            }
        }
        for raw in inputs {
            let rel = cfg_dir.join(raw.trim_start_matches("./"));
            if rel.extension().is_some_and(|e| e == "html") {
                if let Some(norm) = normalize_rel(&rel) {
                    for script_file in
                        html_script_sources(&project.root, &norm, &project.graph.paths)
                    {
                        out.push(Root {
                            file: script_file,
                            role: Role::Runtime,
                            origin: "discovered",
                            input: format!("{cfg}#input"),
                            mode: RootEntryMode::ModuleOnly,
                        });
                    }
                }
            } else if let Some(id) = resolve_discovered_path(project, cfg_dir, &raw)
                && project.graph.paths[id] != *cfg
            {
                out.push(Root {
                    file: project.graph.paths[id].clone(),
                    role: Role::Runtime,
                    origin: "discovered",
                    input: format!("{cfg}#entry"),
                    mode: RootEntryMode::ModuleOnly,
                });
            }
        }
        out
    };
    let batches: Vec<Vec<Root>> = if config_files.len() >= 4
        && let Some(pool) = crate::io::pool()
    {
        pool.install(|| {
            config_files
                .par_iter()
                .map_init(|| (Allocator::default(), String::new()), inspect)
                .collect()
        })
    } else {
        let mut state = (Allocator::default(), String::new());
        config_files
            .iter()
            .map(|c| inspect(&mut state, c))
            .collect()
    };
    for batch in batches {
        project.roots.extend(batch);
    }
}

/// Inspect Jest configs (`jest.config.*`, `jest.*.config.*`) to discover test setup files,
/// custom environments/transformers, and `moduleNameMapper` mock targets as test roots.
pub fn discover_jest_config_entries(project: &mut Project) {
    let configs: Vec<(usize, String)> = project
        .graph
        .paths
        .iter()
        .enumerate()
        .filter(|(_, p)| {
            let name = Path::new(p)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            let ext_ok = Path::new(p)
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| EXTENSIONS.contains(&e));
            ext_ok && name.starts_with("jest.") && name.contains(".config.")
        })
        .map(|(id, p)| (id, p.clone()))
        .collect();
    if configs.is_empty() {
        return;
    }
    let inspect = |(arena, scratch): &mut (Allocator, String), &(id, ref cfg): &(usize, String)| {
        let mut out = Vec::new();
        let Some(source) = read_project_source(project, id, cfg, scratch) else {
            return out;
        };
        let abs = project.root.join(cfg);
        arena.reset();
        let parsed = Parser::new(
            arena,
            source,
            SourceType::from_path(&abs).unwrap_or_default(),
        )
        .parse();
        if parsed.panicked {
            return out;
        }
        let sem = SemanticBuilder::new().build(&parsed.program).semantic;
        let nodes = sem.nodes();
        let mut locals = std::collections::HashMap::new();
        for node in nodes.iter() {
            if let AstKind::VariableDeclarator(v) = node.kind()
                && let oxc_ast::ast::BindingPatternKind::BindingIdentifier(bind) = &v.id.kind
                && let Some(init) = v.init.as_ref()
                && let Some(val) = extract_config_path_with_locals(init, &locals)
            {
                locals.insert(bind.name.to_string(), val);
            }
        }
        let mut targets = Vec::new();
        for node in nodes.iter() {
            if let AstKind::ObjectProperty(prop) = node.kind()
                && matches!(
                    prop_key_name(&prop.key),
                    Some(
                        "setupFiles"
                            | "setupFilesAfterEnv"
                            | "globalSetup"
                            | "globalTeardown"
                            | "testEnvironment"
                            | "resolver"
                            | "transform"
                            | "moduleNameMapper"
                    )
                )
            {
                collect_paths_from_expr(&prop.value, &locals, &mut targets);
            }
        }
        let cfg_dir = Path::new(cfg).parent().unwrap_or_else(|| Path::new(""));
        for raw in targets {
            let cleaned = raw
                .trim()
                .trim_start_matches("<rootDir>/")
                .trim_start_matches("<rootDir>");
            if let Some(id) = resolve_discovered_path(project, cfg_dir, cleaned)
                && project.graph.paths[id] != *cfg
            {
                out.push(Root {
                    file: project.graph.paths[id].clone(),
                    role: Role::Test,
                    origin: "discovered",
                    input: format!("{cfg}#jest"),
                    mode: RootEntryMode::AllExports,
                });
            }
        }
        out
    };
    let batches: Vec<Vec<Root>> = if configs.len() >= 4
        && let Some(pool) = crate::io::pool()
    {
        pool.install(|| {
            configs
                .par_iter()
                .map_init(|| (Allocator::default(), String::new()), inspect)
                .collect()
        })
    } else {
        let mut state = (Allocator::default(), String::new());
        configs.iter().map(|c| inspect(&mut state, c)).collect()
    };
    for batch in batches {
        project.roots.extend(batch);
    }
}

/// Discover served static JS files inside workspace `public/` directories (e.g. service workers).
pub fn discover_public_static_files(project: &mut Project) {
    let mut new_roots = Vec::new();
    for w in &project.workspace_infos {
        let dir = if w.dir == "." {
            PathBuf::from("public")
        } else {
            Path::new(&w.dir).join("public")
        };
        let (start, end) = project.files_in_dir_range(&dir);
        for file in &project.graph.paths[start as usize..end as usize] {
            new_roots.push(Root {
                file: file.clone(),
                role: Role::Runtime,
                origin: "discovered",
                input: "public-static".into(),
                mode: RootEntryMode::ModuleOnly,
            });
        }
    }
    project.roots.extend(new_roots);
}

/// Discover ambient declaration files (`.d.ts`/`.d.mts`/`.d.cts` containing `declare global`
/// or `declare module` augmentations) that TypeScript automatically includes in compilation.
pub fn discover_ambient_declarations(project: &mut Project) {
    let candidates: Vec<(usize, String)> = project
        .graph
        .paths
        .iter()
        .enumerate()
        .filter(|(_, p)| p.ends_with(".d.ts") || p.ends_with(".d.mts") || p.ends_with(".d.cts"))
        .map(|(id, p)| (id, p.clone()))
        .collect();
    if candidates.is_empty() {
        return;
    }
    let check = |(arena, scratch): &mut (Allocator, String), &(id, ref file): &(usize, String)| {
        let source = read_project_source(project, id, file, scratch)?;
        if !source.contains("declare") || (!source.contains("global") && !source.contains("module"))
        {
            return None;
        }
        let abs = project.root.join(file);
        arena.reset();
        let parsed = Parser::new(
            arena,
            source,
            SourceType::from_path(&abs).unwrap_or_default(),
        )
        .parse();
        if parsed.panicked {
            return None;
        }
        let is_ambient = parsed.program.body.iter().any(|stmt| {
            matches!(
                stmt,
                Statement::TSModuleDeclaration(m)
                    if m.kind == TSModuleDeclarationKind::Global
                        || matches!(m.id, TSModuleDeclarationName::StringLiteral(_))
            )
        });
        is_ambient.then(|| Root {
            file: file.clone(),
            role: Role::Runtime,
            origin: "discovered",
            input: "ambient-declaration".into(),
            mode: RootEntryMode::ModuleOnly,
        })
    };
    let ambient: Vec<Root> = if candidates.len() >= 8
        && let Some(pool) = crate::io::pool()
    {
        pool.install(|| {
            candidates
                .par_iter()
                .map_init(|| (Allocator::default(), String::new()), check)
                .flatten()
                .collect()
        })
    } else {
        let mut state = (Allocator::default(), String::new());
        candidates
            .iter()
            .filter_map(|c| check(&mut state, c))
            .collect()
    };
    project.roots.extend(ambient);
}

pub fn discover_extra_tooling_configs(project: &mut Project) {
    let existing: FxHashSet<&str> = project
        .roots
        .iter()
        .filter(|r| r.role == Role::Tooling)
        .map(|r| r.file.as_str())
        .collect();
    let extra: Vec<String> = project
        .graph
        .paths
        .iter()
        .filter(|p| {
            !existing.contains(p.as_str())
                && Path::new(p)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .is_some_and(is_tool_config_name)
        })
        .cloned()
        .collect();
    for file in extra {
        project.roots.push(Root {
            file,
            role: Role::Tooling,
            origin: "discovered",
            input: "tool-config".into(),
            mode: RootEntryMode::AllExports,
        });
    }
}

/// Discover Storybook stories declared in `.storybook/main.*` as tooling entries whose CSF exports count as used.
pub fn discover_storybook_stories(project: &mut Project) -> Result<(), String> {
    let mains: Vec<(usize, String)> = project
        .graph
        .paths
        .iter()
        .enumerate()
        .filter(|(_, p)| {
            let path = Path::new(p);
            path.parent()
                .and_then(|d| d.file_name())
                .and_then(|n| n.to_str())
                == Some(".storybook")
                && path.file_stem().and_then(|s| s.to_str()) == Some("main")
        })
        .map(|(id, p)| (id, p.clone()))
        .collect();

    let mut arena = Allocator::default();
    let mut scratch = String::new();
    let mut story_globs = Vec::new();
    let mut configs = Vec::new();

    for (id, main_file) in mains {
        configs.push(main_file.clone());
        let sb_dir = Path::new(&main_file)
            .parent()
            .unwrap_or_else(|| Path::new(""));
        for ext in EXTENSIONS {
            let preview = to_slash(&sb_dir.join(format!("preview.{ext}")));
            if project.graph.paths.binary_search(&preview).is_ok() {
                configs.push(preview);
            }
        }
        let Some(source) = read_project_source(project, id, &main_file, &mut scratch) else {
            continue;
        };
        let abs = project.root.join(&main_file);
        arena.reset();
        let parsed = Parser::new(
            &arena,
            source,
            SourceType::from_path(&abs).unwrap_or_default(),
        )
        .parse();
        if parsed.panicked {
            continue;
        }
        let sem = SemanticBuilder::new().build(&parsed.program).semantic;
        let mut patterns = Vec::new();
        for node in sem.nodes().iter() {
            if let AstKind::ObjectProperty(prop) = node.kind()
                && prop_key_name(&prop.key) == Some("stories")
            {
                collect_story_entries(&prop.value, &mut patterns);
            }
        }
        for pat in patterns {
            let joined = sb_dir.join(convert_extglob(&pat).trim_start_matches("./"));
            if let Some(norm) = normalize_rel(&joined) {
                story_globs.push((main_file.clone(), to_slash(&norm)));
            }
        }
    }

    for cfg in configs {
        project.roots.push(Root {
            file: cfg,
            role: Role::Tooling,
            origin: "discovered",
            input: ".storybook".into(),
            mode: RootEntryMode::AllExports,
        });
    }
    for (source_main, pattern) in story_globs {
        project.select_with_mode(
            Role::Tooling,
            &pattern,
            "discovered",
            &format!("{source_main}#stories"),
            false,
            RootEntryMode::AllExports,
        )?;
    }
    Ok(())
}

fn convert_extglob(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let bytes = pattern.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'@'
            && bytes.get(i + 1) == Some(&b'(')
            && let Some(close_rel) = pattern[i + 2..].find(')')
        {
            out.push('{');
            out.push_str(&pattern[i + 2..i + 2 + close_rel].replace('|', ","));
            out.push('}');
            i += 2 + close_rel + 1;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn collect_story_entries(expr: &Expression<'_>, out: &mut Vec<String>) {
    match expr.get_inner_expression() {
        Expression::ArrayExpression(arr) => {
            for elem in &arr.elements {
                if let Some(e) = elem.as_expression() {
                    collect_story_entries(e, out);
                }
            }
        }
        Expression::ObjectExpression(obj) => {
            let mut dir = None;
            let mut files = None;
            for prop in &obj.properties {
                if let ObjectPropertyKind::ObjectProperty(p) = prop {
                    match prop_key_name(&p.key) {
                        Some("directory") => dir = static_str(&p.value),
                        Some("files") => files = static_str(&p.value),
                        _ => {}
                    }
                }
            }
            if let Some(d) = dir {
                let f = files.unwrap_or("**/*.stories.@(mdx|tsx|ts|jsx|js)");
                out.push(format!(
                    "{}/{}",
                    d.trim_end_matches('/'),
                    f.trim_start_matches("./")
                ));
            }
        }
        other => {
            if let Some(s) = static_str(other) {
                out.push(s.to_owned());
            }
        }
    }
}
