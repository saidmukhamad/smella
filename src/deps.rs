//! Per-workspace package dependency analysis: unused declared dependencies,
//! unlisted imported packages, unlisted script binaries, and devDependencies
//! imported by runtime code.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use rustc_hash::FxHashMap;
use serde::Serialize;
use serde_json::Value;

use crate::discover::{is_bundler_config_name, split_shell_words};
use crate::graph::{Id, Loc, Reach};
use crate::project::Project;
use crate::source_map::to_slash;

#[derive(Clone, Debug)]
pub struct PackageRef {
    pub file: Id,
    pub package: String,
    pub type_only: bool,
    pub loc: Loc,
}

#[derive(Clone, Debug)]
pub struct WorkspaceManifest {
    pub ws_id: usize,
    pub ws_dir: String,
    pub rel_dir: PathBuf,
    pub manifest_path: String,
    pub name: Option<String>,
    pub dependencies: BTreeSet<String>,
    pub dev_dependencies: BTreeSet<String>,
    pub peer_dependencies: BTreeSet<String>,
    pub optional_dependencies: BTreeSet<String>,
    pub scripts: BTreeMap<String, String>,
    pub bins: BTreeSet<String>,
    pub config_deps: BTreeSet<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DependencyFinding {
    pub id: String,
    pub kind: &'static str,
    pub workspace: String,
    pub file: String,
    pub manifest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dependency: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dependency_group: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<Loc>,
    pub status: &'static str,
}

const NODE_BUILTINS: &[&str] = &[
    "assert",
    "async_hooks",
    "buffer",
    "child_process",
    "cluster",
    "console",
    "constants",
    "crypto",
    "dgram",
    "diagnostics_channel",
    "dns",
    "domain",
    "events",
    "fs",
    "http",
    "http2",
    "https",
    "inspector",
    "module",
    "net",
    "os",
    "path",
    "perf_hooks",
    "process",
    "punycode",
    "querystring",
    "readline",
    "repl",
    "sqlite",
    "stream",
    "string_decoder",
    "sys",
    "test",
    "timers",
    "tls",
    "trace_events",
    "tty",
    "url",
    "util",
    "v8",
    "vm",
    "wasi",
    "worker_threads",
    "zlib",
];

const SHELL_AND_OS_BUILTINS: &[&str] = &[
    "awk", "bash", "bun", "bunx", "cargo", "cat", "cd", "chmod", "chown", "clang", "cmake",
    "command", "cp", "curl", "cut", "date", "deno", "dirname", "do", "docker", "done", "echo",
    "elif", "else", "env", "exec", "exit", "export", "false", "fi", "find", "for", "gcc", "git",
    "go", "grep", "head", "if", "java", "jq", "kill", "ln", "ls", "make", "mkdir", "mktemp", "mv",
    "node", "nohup", "npm", "npx", "open", "pbcopy", "perl", "pnpm", "printf", "pwd", "python",
    "python3", "read", "rm", "rmdir", "rsync", "ruby", "rustc", "rustfmt", "scp", "sed", "set",
    "sh", "shift", "sleep", "sort", "source", "ssh", "tail", "tar", "tee", "test", "then", "time",
    "touch", "tr", "true", "uname", "uniq", "unset", "unzip", "wc", "wget", "which", "while",
    "xargs", "yarn", "zip", "zsh", "[", "[[", ":", ".", "!",
];

pub fn is_node_builtin(pkg: &str) -> bool {
    pkg.starts_with("node:") || pkg.starts_with("bun:") || NODE_BUILTINS.contains(&pkg)
}

pub fn extract_dep_package(spec: &str) -> Option<String> {
    let clean = spec.split('?').next().unwrap_or(spec).trim();
    if clean.is_empty()
        || clean.starts_with('.')
        || clean.starts_with('/')
        || clean.starts_with('#')
        || clean.starts_with("node:")
        || clean.starts_with("bun:")
        || clean.starts_with("virtual:")
        || clean.starts_with("http://")
        || clean.starts_with("https://")
        || clean.starts_with("data:")
    {
        return None;
    }
    let pkg = if let Some(rest) = clean.strip_prefix('@') {
        let mut it = rest.splitn(3, '/');
        let scope = it.next()?;
        let name = it.next()?;
        if scope.is_empty() || name.is_empty() {
            return None;
        }
        format!("@{scope}/{name}")
    } else {
        let name = clean.split('/').next()?;
        if name.is_empty() {
            return None;
        }
        name.to_owned()
    };
    (!is_node_builtin(&pkg)).then_some(pkg)
}

fn known_bin_for_pkg(pkg: &str) -> Vec<&'static str> {
    match pkg {
        "typescript" => vec!["tsc", "tsserver"],
        "@biomejs/biome" => vec!["biome"],
        "@tailwindcss/cli" => vec!["tailwindcss"],
        "tailwindcss" => vec!["tailwindcss", "tailwind"],
        "@napi-rs/cli" => vec!["napi"],
        "@playwright/test" => vec!["playwright"],
        "esbuild" => vec!["esbuild"],
        "eslint" => vec!["eslint"],
        "jest" => vec!["jest"],
        "next" => vec!["next"],
        "nodemon" => vec!["nodemon"],
        "nx" => vec!["nx"],
        "oxlint" => vec!["oxlint"],
        "prettier" => vec!["prettier"],
        "rimraf" => vec!["rimraf"],
        "rollup" => vec!["rollup"],
        "storybook" => vec!["storybook"],
        "ts-node" => vec!["ts-node"],
        "tsdown" => vec!["tsdown"],
        "tsx" => vec!["tsx"],
        "turbo" => vec!["turbo"],
        "vite" => vec!["vite"],
        "vitest" => vec!["vitest"],
        "webpack-cli" => vec!["webpack"],
        "webpack" => vec!["webpack"],
        _ => vec![],
    }
}

fn read_pkg_bins(
    repo_root: &Path,
    ws_rel: &Path,
    pkg: &str,
    workspace_bins: &FxHashMap<String, Vec<String>>,
    ws_cache: &mut FxHashMap<(PathBuf, String), Vec<String>>,
    nm_exists: &mut FxHashMap<PathBuf, bool>,
    pkg_json_cache: &mut FxHashMap<PathBuf, Option<Vec<String>>>,
) -> Vec<String> {
    if let Some(wb) = workspace_bins.get(pkg) {
        return wb.clone();
    }
    let key = (ws_rel.to_owned(), pkg.to_owned());
    if let Some(cached) = ws_cache.get(&key) {
        return cached.clone();
    }
    let mut bins = Vec::new();
    let abs_start = repo_root.join(ws_rel);
    for anc in abs_start.ancestors() {
        let nm_dir = anc.join("node_modules");
        let has_nm = *nm_exists
            .entry(nm_dir.clone())
            .or_insert_with(|| nm_dir.is_dir());
        if has_nm {
            let pkg_json = nm_dir.join(pkg).join("package.json");
            let found = pkg_json_cache.entry(pkg_json.clone()).or_insert_with(|| {
                let bytes = fs::read(&pkg_json).ok()?;
                let val = serde_json::from_slice::<Value>(&bytes).ok()?;
                let mut list = Vec::new();
                match &val["bin"] {
                    Value::String(_) => {
                        let default_name = pkg.rsplit('/').next().unwrap_or(pkg);
                        list.push(default_name.to_owned());
                    }
                    Value::Object(map) => {
                        list.extend(map.keys().cloned());
                    }
                    _ => {}
                }
                Some(list)
            });
            if let Some(list) = found {
                bins.extend(list.iter().cloned());
                break;
            }
        }
        if anc == repo_root {
            break;
        }
    }
    for known in known_bin_for_pkg(pkg) {
        if !bins.iter().any(|b| b == known) {
            bins.push(known.to_owned());
        }
    }
    if bins.is_empty() && !pkg.starts_with("@types/") {
        let fallback = pkg.rsplit('/').next().unwrap_or(pkg);
        bins.push(fallback.to_owned());
    }
    ws_cache.insert(key, bins.clone());
    bins
}

fn extract_script_binaries(script: &str, out: &mut Vec<String>) {
    let words = split_shell_words(script);
    let mut subcmds: Vec<Vec<String>> = Vec::new();
    let mut cur = Vec::new();
    let mut i = 0;
    while i < words.len() {
        let w = words[i].as_str();
        if matches!(w, "&&" | "||" | ";" | "|") {
            if !cur.is_empty() {
                subcmds.push(std::mem::take(&mut cur));
            }
            i += 1;
            continue;
        }
        if cur.is_empty()
            && matches!(w, "sh" | "bash" | "/bin/sh" | "/bin/bash")
            && words.get(i + 1).map(String::as_str) == Some("-c")
            && let Some(inner) = words.get(i + 2)
        {
            extract_script_binaries(inner, out);
            i += 3;
            continue;
        }
        cur.push(words[i].clone());
        i += 1;
    }
    if !cur.is_empty() {
        subcmds.push(cur);
    }

    for cmd in subcmds {
        let mut idx = 0;
        while idx < cmd.len() {
            let w = cmd[idx].as_str();
            let is_env = w.split_once('=').is_some_and(|(k, _)| {
                !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            });
            if is_env || matches!(w, "exec" | "then" | "else" | "do" | "!" | "time" | "nohup") {
                idx += 1;
            } else {
                break;
            }
        }
        let Some(first) = cmd.get(idx).map(String::as_str) else {
            continue;
        };
        if first.starts_with('.')
            || first.starts_with('/')
            || first.starts_with('$')
            || first.contains('=')
            || first.contains('(')
        {
            continue;
        }
        // Unwrap `npx <bin>`, `bunx <bin>`, `pnpm exec <bin>`, `yarn exec <bin>`, `cross-env K=V <bin>`.
        let target = match first {
            "npx" | "bunx" => {
                let mut j = idx + 1;
                while j < cmd.len() && cmd[j].starts_with('-') {
                    j += 1;
                }
                cmd.get(j).map(String::as_str)
            }
            "pnpm" | "yarn" if cmd.get(idx + 1).map(String::as_str) == Some("exec") => {
                cmd.get(idx + 2).map(String::as_str)
            }
            "cross-env" => {
                out.push("cross-env".into());
                let mut j = idx + 1;
                while j < cmd.len() && cmd[j].contains('=') {
                    j += 1;
                }
                cmd.get(j).map(String::as_str)
            }
            _ => Some(first),
        };
        if let Some(bin) = target
            && !bin.is_empty()
            && !bin.starts_with('.')
            && !bin.starts_with('/')
            && !bin.starts_with('$')
            && !bin.starts_with('-')
            && !SHELL_AND_OS_BUILTINS.contains(&bin)
        {
            out.push(bin.to_owned());
        }
    }
}

fn types_base_package(pkg: &str) -> Option<String> {
    let rest = pkg.strip_prefix("@types/")?;
    if let Some((scope, name)) = rest.split_once("__") {
        Some(format!("@{scope}/{name}"))
    } else {
        Some(rest.to_owned())
    }
}

fn mangle_to_types(pkg: &str) -> Option<String> {
    if pkg.starts_with("@types/") {
        return None;
    }
    if let Some(rest) = pkg.strip_prefix('@') {
        let (scope, name) = rest.split_once('/')?;
        Some(format!("@types/{scope}__{name}"))
    } else {
        Some(format!("@types/{pkg}"))
    }
}

const BUNDLER_BINS: &[&str] = &[
    "vite", "webpack", "tsdown", "tsup", "esbuild", "rollup", "rspack", "rsbuild", "parcel",
    "next", "nuxt", "astro",
];

fn has_bundler_script(m: &WorkspaceManifest) -> bool {
    let mut bins = Vec::new();
    for script_cmd in m.scripts.values() {
        bins.clear();
        extract_script_binaries(script_cmd, &mut bins);
        if bins.iter().any(|b| BUNDLER_BINS.contains(&b.as_str()))
            || script_cmd.contains("bun build")
        {
            return true;
        }
    }
    false
}

/// Resolve a binary name via `node_modules/.bin/<bin>` symlink to the providing package name
/// in a single `readlink` syscall without reading hundreds of `package.json` files.
fn resolve_bin_via_symlink(
    repo_root: &Path,
    ws_rel: &Path,
    bin: &str,
    nm_bin_exists: &mut FxHashMap<PathBuf, bool>,
) -> Option<String> {
    let abs_start = repo_root.join(ws_rel);
    for anc in abs_start.ancestors() {
        let dot_bin = anc.join("node_modules").join(".bin");
        let exists = *nm_bin_exists
            .entry(dot_bin.clone())
            .or_insert_with(|| dot_bin.is_dir());
        if exists
            && let Ok(target) = fs::read_link(dot_bin.join(bin))
            && let Some(s) = target.to_str()
        {
            let clean = s.replace('\\', "/");
            let rest = clean
                .strip_prefix("../")
                .or_else(|| clean.split_once("node_modules/").map(|(_, r)| r));
            if let Some(r) = rest
                && let Some(pkg) = extract_dep_package(r)
            {
                return Some(pkg);
            }
        }
        if anc == repo_root {
            break;
        }
    }
    None
}

fn bin_matches_declared(
    bin: &str,
    declared: &BTreeSet<&str>,
    workspace_bins: &FxHashMap<String, Vec<String>>,
    out: &mut Vec<String>,
) {
    for &dep in declared {
        if let Some(wb) = workspace_bins.get(dep) {
            if wb.iter().any(|b| b == bin) {
                out.push(dep.to_owned());
            }
            continue;
        }
        if known_bin_for_pkg(dep).contains(&bin) {
            out.push(dep.to_owned());
            continue;
        }
        if !dep.starts_with("@types/") {
            let short = dep.rsplit('/').next().unwrap_or(dep);
            if short == bin {
                out.push(dep.to_owned());
            }
        }
    }
}

pub fn analyze_dependencies(
    project: &Project,
    reach: &Reach,
    package_refs: &[PackageRef],
) -> Vec<DependencyFinding> {
    let mut workspace_bins: FxHashMap<String, Vec<String>> = FxHashMap::default();
    let mut by_ws_id: FxHashMap<usize, &WorkspaceManifest> = FxHashMap::default();
    for m in &project.manifests {
        by_ws_id.insert(m.ws_id, m);
        if let Some(name) = &m.name {
            workspace_bins.insert(name.clone(), m.bins.iter().cloned().collect());
        }
    }
    let root_manifest = by_ws_id.get(&0).copied();
    let root_declared: BTreeSet<&str> = root_manifest
        .map(|rm| {
            rm.dependencies
                .iter()
                .chain(&rm.dev_dependencies)
                .chain(&rm.peer_dependencies)
                .chain(&rm.optional_dependencies)
                .map(String::as_str)
                .collect()
        })
        .unwrap_or_default();

    // Single pass over files to mark workspaces containing a bundler config.
    let mut bundled_by_config = vec![false; project.workspace_infos.len()];
    for (file_id, path) in project.graph.paths.iter().enumerate() {
        let ws_id = project
            .workspace_for_file
            .get(file_id)
            .copied()
            .unwrap_or(0);
        if !bundled_by_config.get(ws_id).copied().unwrap_or(false)
            && let Some(name) = Path::new(path).file_name().and_then(|s| s.to_str())
            && is_bundler_config_name(name)
            && ws_id < bundled_by_config.len()
        {
            bundled_by_config[ws_id] = true;
        }
    }

    // Group imported package refs by owning workspace.
    let mut refs_by_ws: FxHashMap<usize, Vec<&PackageRef>> = FxHashMap::default();
    for r in package_refs {
        let ws_id = project.workspace_for_file.get(r.file).copied().unwrap_or(0);
        refs_by_ws.entry(ws_id).or_default().push(r);
    }

    let mut bin_cache = FxHashMap::default();
    let mut nm_exists = FxHashMap::default();
    let mut nm_bin_exists = FxHashMap::default();
    let mut pkg_json_cache = FxHashMap::default();
    let mut root_bin_map_full: Option<FxHashMap<String, Vec<String>>> = None;
    let mut root_used_deps: BTreeSet<String> = BTreeSet::new();
    let mut findings = Vec::new();

    for m in &project.manifests {
        let mut used_deps: BTreeSet<String> = m.config_deps.clone();
        let mut declared_all: BTreeSet<&str> = BTreeSet::new();
        for d in m
            .dependencies
            .iter()
            .chain(&m.dev_dependencies)
            .chain(&m.peer_dependencies)
            .chain(&m.optional_dependencies)
        {
            declared_all.insert(d.as_str());
        }

        // Check imported packages in this workspace's files.
        let bundled =
            bundled_by_config.get(m.ws_id).copied().unwrap_or(false) || has_bundler_script(m);
        let mut seen_unlisted: BTreeSet<(Id, String)> = BTreeSet::new();
        let mut seen_dev_runtime: BTreeSet<(Id, String)> = BTreeSet::new();
        if let Some(refs) = refs_by_ws.get(&m.ws_id) {
            for r in refs {
                if m.name.as_deref() == Some(r.package.as_str()) {
                    continue;
                }
                let types_pkg = mangle_to_types(&r.package);
                let is_declared = declared_all.contains(r.package.as_str())
                    || (r.type_only
                        && types_pkg
                            .as_deref()
                            .is_some_and(|tp| declared_all.contains(tp)));
                if is_declared {
                    used_deps.insert(r.package.clone());
                    if let Some(tp) = types_pkg
                        && declared_all.contains(tp.as_str())
                    {
                        used_deps.insert(tp);
                    }
                    // Check devDependency imported as runtime value from a non-bundled runtime/public file.
                    if !bundled
                        && !r.type_only
                        && m.dev_dependencies.contains(&r.package)
                        && !m.dependencies.contains(&r.package)
                        && !m.peer_dependencies.contains(&r.package)
                        && !m.optional_dependencies.contains(&r.package)
                        && (reach.has(r.file, 0) || reach.has(r.file, 6))
                        && seen_dev_runtime.insert((r.file, r.package.clone()))
                    {
                        findings.push(DependencyFinding {
                            id: String::new(),
                            kind: "dev-dependency-in-runtime",
                            workspace: m.ws_dir.clone(),
                            file: project.graph.paths[r.file].clone(),
                            manifest: m.manifest_path.clone(),
                            dependency: Some(r.package.clone()),
                            dependency_group: Some("devDependencies"),
                            binary: None,
                            script: None,
                            location: Some(r.loc),
                            status: "complete",
                        });
                    }
                } else if seen_unlisted.insert((r.file, r.package.clone())) {
                    findings.push(DependencyFinding {
                        id: String::new(),
                        kind: "unlisted-dependency",
                        workspace: m.ws_dir.clone(),
                        file: project.graph.paths[r.file].clone(),
                        manifest: m.manifest_path.clone(),
                        dependency: Some(r.package.clone()),
                        dependency_group: None,
                        binary: None,
                        script: None,
                        location: Some(r.loc),
                        status: "complete",
                    });
                }
            }
        }

        // Check binaries invoked in package.json#scripts lazily (avoiding disk reads for packages
        // whose binaries are never invoked).
        let mut seen_unlisted_bins: BTreeSet<(String, String)> = BTreeSet::new();
        let mut ws_bin_map_full: Option<FxHashMap<String, Vec<String>>> = None;
        let mut fast_matches = Vec::new();
        for (script_name, script_cmd) in &m.scripts {
            let mut bins = Vec::new();
            extract_script_binaries(script_cmd, &mut bins);
            for bin in bins {
                if m.bins.contains(&bin) {
                    continue;
                }
                // 1. Fast zero-I/O check against workspace declared deps.
                fast_matches.clear();
                bin_matches_declared(&bin, &declared_all, &workspace_bins, &mut fast_matches);
                if !fast_matches.is_empty() {
                    for p in &fast_matches {
                        used_deps.insert(p.clone());
                    }
                    continue;
                }
                // 2. Single-syscall `.bin/<bin>` symlink probe.
                if let Some(pkg) =
                    resolve_bin_via_symlink(&project.root, &m.rel_dir, &bin, &mut nm_bin_exists)
                {
                    if declared_all.contains(pkg.as_str()) {
                        used_deps.insert(pkg);
                        continue;
                    }
                    if m.ws_id != 0 && root_declared.contains(pkg.as_str()) {
                        root_used_deps.insert(pkg);
                        continue;
                    }
                }
                // 3. Fast zero-I/O check against hoisted root declared deps.
                if m.ws_id != 0 {
                    fast_matches.clear();
                    bin_matches_declared(&bin, &root_declared, &workspace_bins, &mut fast_matches);
                    if !fast_matches.is_empty() {
                        for p in &fast_matches {
                            root_used_deps.insert(p.clone());
                        }
                        continue;
                    }
                }
                // 4. Slow fallback: inspect declared package.json `bin` maps (e.g. non-symlinked fixtures).
                let ws_map = ws_bin_map_full.get_or_insert_with(|| {
                    let mut map: FxHashMap<String, Vec<String>> = FxHashMap::default();
                    for dep in &declared_all {
                        for b in read_pkg_bins(
                            &project.root,
                            &m.rel_dir,
                            dep,
                            &workspace_bins,
                            &mut bin_cache,
                            &mut nm_exists,
                            &mut pkg_json_cache,
                        ) {
                            map.entry(b).or_default().push((*dep).to_owned());
                        }
                    }
                    map
                });
                if let Some(providers) = ws_map.get(&bin) {
                    for p in providers {
                        used_deps.insert(p.clone());
                    }
                    continue;
                }
                if m.ws_id != 0
                    && let Some(rm) = root_manifest
                {
                    let rmap = root_bin_map_full.get_or_insert_with(|| {
                        let mut map: FxHashMap<String, Vec<String>> = FxHashMap::default();
                        for dep in &root_declared {
                            for b in read_pkg_bins(
                                &project.root,
                                &rm.rel_dir,
                                dep,
                                &workspace_bins,
                                &mut bin_cache,
                                &mut nm_exists,
                                &mut pkg_json_cache,
                            ) {
                                map.entry(b).or_default().push((*dep).to_owned());
                            }
                        }
                        map
                    });
                    if let Some(providers) = rmap.get(&bin) {
                        for p in providers {
                            root_used_deps.insert(p.clone());
                        }
                        continue;
                    }
                }
                if seen_unlisted_bins.insert((script_name.clone(), bin.clone())) {
                    findings.push(DependencyFinding {
                        id: String::new(),
                        kind: "unlisted-binary",
                        workspace: m.ws_dir.clone(),
                        file: m.manifest_path.clone(),
                        manifest: m.manifest_path.clone(),
                        dependency: None,
                        dependency_group: None,
                        binary: Some(bin),
                        script: Some(script_name.clone()),
                        location: None,
                        status: "complete",
                    });
                }
            }
        }

        // Defer root unused-dependency check until all child workspaces have marked hoisted tools used.
        if m.ws_id != 0 {
            emit_unused_deps(m, &used_deps, &mut findings);
        } else {
            root_used_deps.extend(used_deps);
        }
    }

    if let Some(rm) = root_manifest {
        emit_unused_deps(rm, &root_used_deps, &mut findings);
    }

    findings.sort_by(|a, b| {
        (
            &a.workspace,
            &a.file,
            a.kind,
            &a.dependency,
            &a.binary,
            &a.script,
        )
            .cmp(&(
                &b.workspace,
                &b.file,
                b.kind,
                &b.dependency,
                &b.binary,
                &b.script,
            ))
    });
    for (idx, f) in findings.iter_mut().enumerate() {
        f.id = format!("d{idx}");
    }
    findings
}

fn emit_unused_deps(
    m: &WorkspaceManifest,
    used_deps: &BTreeSet<String>,
    findings: &mut Vec<DependencyFinding>,
) {
    for (group, deps) in [
        ("dependencies", &m.dependencies),
        ("devDependencies", &m.dev_dependencies),
    ] {
        for dep in deps {
            if used_deps.contains(dep) {
                continue;
            }
            if let Some(base) = types_base_package(dep)
                && (used_deps.contains(&base) || base == "node")
            {
                continue;
            }
            findings.push(DependencyFinding {
                id: String::new(),
                kind: "unused-dependency",
                workspace: m.ws_dir.clone(),
                file: m.manifest_path.clone(),
                manifest: m.manifest_path.clone(),
                dependency: Some(dep.clone()),
                dependency_group: Some(group),
                binary: None,
                script: None,
                location: None,
                status: "complete",
            });
        }
    }
}

pub fn manifest_dep_set(pkg: &Value, key: &str) -> BTreeSet<String> {
    pkg[key]
        .as_object()
        .map(|obj| obj.keys().cloned().collect())
        .unwrap_or_default()
}

pub fn manifest_scripts(pkg: &Value) -> BTreeMap<String, String> {
    pkg["scripts"]
        .as_object()
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                .collect()
        })
        .unwrap_or_default()
}

pub fn manifest_bins(pkg: &Value) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    match &pkg["bin"] {
        Value::String(_) => {
            if let Some(name) = pkg["name"].as_str() {
                out.insert(name.rsplit('/').next().unwrap_or(name).to_owned());
            }
        }
        Value::Object(map) => {
            out.extend(map.keys().cloned());
        }
        _ => {}
    }
    out
}

pub fn manifest_path_for(dir: &Path) -> String {
    let p = to_slash(&dir.join("package.json"));
    p.trim_start_matches("./").to_owned()
}
