//! Static discovery of runtime and tooling entries from package.json scripts,
//! Vite/webpack configs (including Module Federation exposes), and Storybook configs.
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use oxc_allocator::Allocator;
use oxc_ast::{
    AstKind,
    ast::{Expression, ObjectPropertyKind, PropertyKey},
};
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::SourceType;
use serde_json::Value;

use crate::graph::{Role, Root, RootEntryMode};
use crate::project::{EXTENSIONS, Project, html_script_sources};
use crate::source_map::{normalize_rel, to_slash};

pub enum ScriptOutcome {
    Entries(Vec<String>),
    DelegatedOrNoop,
    Ambiguous,
}

fn split_shell_words(input: &str) -> Vec<String> {
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
    if !matches!(runner, "node" | "bun" | "ts-node" | "tsx") {
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
        if matches!(arg, "-e" | "--eval" | "-p" | "--print" | "-c" | "--check") {
            return None;
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

fn extract_config_path(expr: &Expression<'_>) -> Option<String> {
    let inner = expr.get_inner_expression();
    if let Some(s) = static_str(inner) {
        let trimmed = s.trim();
        return (!trimmed.is_empty()).then(|| trimmed.to_owned());
    }
    let Expression::CallExpression(call) = inner else {
        return None;
    };
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
    let first_ok = call
        .arguments
        .first()
        .and_then(|a| a.as_expression())
        .is_some_and(|e| match e.get_inner_expression() {
            Expression::Identifier(id) => {
                matches!(id.name.as_str(), "__dirname" | "ROOT" | "root" | "dir")
            }
            Expression::StaticMemberExpression(m) => m.property.name == "dirname",
            _ => false,
        });
    if !first_ok {
        return None;
    }
    let mut parts = Vec::new();
    for arg in call.arguments.iter().skip(1) {
        let s = arg.as_expression().and_then(static_str)?;
        parts.push(s.trim_start_matches("./").trim_end_matches('/'));
    }
    Some(parts.join("/"))
}

fn collect_paths_from_expr(expr: &Expression<'_>, out: &mut Vec<String>) {
    match expr.get_inner_expression() {
        Expression::ArrayExpression(arr) => {
            for elem in &arr.elements {
                if let Some(e) = elem.as_expression() {
                    collect_paths_from_expr(e, out);
                }
            }
        }
        Expression::ObjectExpression(obj) => {
            for prop in &obj.properties {
                if let ObjectPropertyKind::ObjectProperty(p) = prop {
                    collect_paths_from_expr(&p.value, out);
                }
            }
        }
        other => {
            if let Some(p) = extract_config_path(other) {
                out.push(p);
            }
        }
    }
}

/// Inspect Vite and webpack configs statically to discover runtime entries (`input`, `entry`,
/// `.html` `<script src>` targets, and Module Federation `exposes`).
pub fn discover_bundler_entries(project: &mut Project) {
    let config_files: Vec<String> = project
        .graph
        .paths
        .iter()
        .filter(|p| {
            if p.contains("/.storybook/") || p.starts_with(".storybook/") {
                return false;
            }
            let name = Path::new(p)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            (name.starts_with("vite.") || name.starts_with("webpack.")) && name.contains(".config.")
        })
        .cloned()
        .collect();

    let mut arena = Allocator::default();
    let mut new_roots = Vec::new();
    for cfg in config_files {
        let abs = project.root.join(&cfg);
        let Ok(source) = fs::read_to_string(&abs) else {
            continue;
        };
        arena.reset();
        let parsed = Parser::new(
            &arena,
            &source,
            SourceType::from_path(&abs).unwrap_or_default(),
        )
        .parse();
        if parsed.panicked {
            continue;
        }
        let sem = SemanticBuilder::new().build(&parsed.program).semantic;
        let nodes = sem.nodes();
        let mut exposes = Vec::new();
        let mut inputs = Vec::new();
        for node in nodes.iter() {
            let AstKind::ObjectProperty(prop) = node.kind() else {
                continue;
            };
            match prop_key_name(&prop.key) {
                Some("exposes") => collect_paths_from_expr(&prop.value, &mut exposes),
                Some("input") => collect_paths_from_expr(&prop.value, &mut inputs),
                Some("entry") => {
                    let in_remotes = nodes.ancestor_kinds(node.id()).any(|k| {
                        matches!(k, AstKind::ObjectProperty(anc) if prop_key_name(&anc.key) == Some("remotes"))
                    });
                    if !in_remotes {
                        collect_paths_from_expr(&prop.value, &mut inputs);
                    }
                }
                _ => {}
            }
        }
        let cfg_dir = Path::new(&cfg).parent().unwrap_or_else(|| Path::new(""));
        for raw in exposes {
            let rel = cfg_dir.join(raw.trim_start_matches("./"));
            if let Some(id) =
                project
                    .source_mapper
                    .map_to_source(&project.root, &project.graph.paths, &rel)
            {
                new_roots.push(Root {
                    file: project.graph.paths[id].clone(),
                    role: Role::Runtime,
                    origin: "discovered",
                    input: format!("{cfg}#exposes"),
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
                        new_roots.push(Root {
                            file: script_file,
                            role: Role::Runtime,
                            origin: "discovered",
                            input: format!("{cfg}#input"),
                            mode: RootEntryMode::ModuleOnly,
                        });
                    }
                }
            } else if let Some(id) =
                project
                    .source_mapper
                    .map_to_source(&project.root, &project.graph.paths, &rel)
            {
                new_roots.push(Root {
                    file: project.graph.paths[id].clone(),
                    role: Role::Runtime,
                    origin: "discovered",
                    input: format!("{cfg}#entry"),
                    mode: RootEntryMode::ModuleOnly,
                });
            }
        }
    }
    project.roots.extend(new_roots);
}

pub fn discover_extra_tooling_configs(project: &mut Project) {
    let existing: BTreeSet<String> = project
        .roots
        .iter()
        .filter(|r| r.role == Role::Tooling)
        .map(|r| r.file.clone())
        .collect();
    let extra: Vec<String> = project
        .graph
        .paths
        .iter()
        .filter(|p| {
            if existing.contains(*p) {
                return false;
            }
            let name = Path::new(p)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            let ext_ok = Path::new(p)
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| EXTENSIONS.contains(&e));
            ext_ok
                && ((name.starts_with("vite.") && name.contains(".config."))
                    || (name.starts_with("vitest.") && name.contains(".config."))
                    || (name.starts_with("webpack.") && name.contains(".config."))
                    || name.starts_with("tsdown.config."))
        })
        .cloned()
        .collect();
    for file in extra {
        project.roots.push(Root {
            file,
            role: Role::Tooling,
            origin: "discovered",
            input: "bundler-config".into(),
            mode: RootEntryMode::ModuleOnly,
        });
    }
}

/// Discover Storybook stories declared in `.storybook/main.*` as tooling entries whose CSF exports count as used.
pub fn discover_storybook_stories(project: &mut Project) -> Result<(), String> {
    let mains: Vec<String> = project
        .graph
        .paths
        .iter()
        .filter(|p| {
            let path = Path::new(p);
            path.parent()
                .and_then(|d| d.file_name())
                .and_then(|n| n.to_str())
                == Some(".storybook")
                && path.file_stem().and_then(|s| s.to_str()) == Some("main")
        })
        .cloned()
        .collect();

    let mut arena = Allocator::default();
    let mut story_globs = Vec::new();
    let mut configs = Vec::new();

    for main_file in mains {
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
        let abs = project.root.join(&main_file);
        let Ok(source) = fs::read_to_string(&abs) else {
            continue;
        };
        arena.reset();
        let parsed = Parser::new(
            &arena,
            &source,
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
