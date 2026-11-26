import { execFileSync, spawn } from 'node:child_process';
import { existsSync, readdirSync, realpathSync, symlinkSync } from 'node:fs';
import { join, relative } from 'node:path';

export interface GitDiffInfo {
  topLevel: string;
  relPrefix: string;
  parents: string[];
  label: string;
  stagedTree: string | null;
  changedFiles: Set<string>;
  renameMap: Map<string, string>;
}

export interface GitDiffOptions {
  base?: string;
  staged?: boolean;
  mergeBase?: string | boolean;
}

export function linkNodeModules(srcDir: string, dstDir: string, depth = 0): void {
  if (depth > 3 || !existsSync(srcDir) || !existsSync(dstDir)) return;
  let entries;
  try {
    entries = readdirSync(srcDir, { withFileTypes: true });
  } catch {
    return;
  }
  for (const entry of entries) {
    if (!entry.isDirectory()) continue;
    const name = entry.name;
    if (name === 'node_modules') {
      const dst = join(dstDir, name);
      if (!existsSync(dst)) {
        try {
          symlinkSync(join(srcDir, name), dst, 'dir');
        } catch {
          // Best-effort symlink for read-only archive snapshots.
        }
      }
      continue;
    }
    if (name === '.git' || name === 'dist' || name === 'build' || name === '.nx' || name === 'target') {
      continue;
    }
    linkNodeModules(join(srcDir, name), join(dstDir, name), depth + 1);
  }
}

function gitRun(cwd: string, args: string[]): string {
  return execFileSync('git', args, { cwd, encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 });
}

function gitTry(cwd: string, args: string[]): string | null {
  try {
    return execFileSync('git', args, {
      cwd,
      encoding: 'utf8',
      maxBuffer: 64 * 1024 * 1024,
      stdio: ['ignore', 'pipe', 'pipe'],
    }).trim();
  } catch {
    return null;
  }
}

function parseNameStatusZ(
  raw: string,
  stripPrefix: (p: string) => string | null,
): { changedFiles: Set<string>; renameMap: Map<string, string> } {
  const changedFiles = new Set<string>();
  const renameMap = new Map<string, string>();
  const tokens = raw.split('\0');
  for (let i = 0; i < tokens.length; ) {
    const status = tokens[i++];
    if (!status) break;
    const code = status[0];
    if (code === 'R' || code === 'C') {
      const oldPath = tokens[i++];
      const newPath = tokens[i++];
      const newRel = newPath ? stripPrefix(newPath) : null;
      const oldRel = oldPath ? stripPrefix(oldPath) : null;
      if (newRel) {
        changedFiles.add(newRel);
        if (code === 'R' && oldRel) renameMap.set(newRel, oldRel);
      }
    } else {
      const filePath = tokens[i++];
      if (code !== 'D' && filePath) {
        const rel = stripPrefix(filePath);
        if (rel) changedFiles.add(rel);
      }
    }
  }
  return { changedFiles, renameMap };
}

function resolveGitRoot(
  rootDir: string,
  onError: (message: string, cause?: unknown) => never,
): { topLevel: string; relPrefix: string; stripPrefix: (p: string) => string | null } {
  let topLevel: string;
  try {
    topLevel = gitRun(rootDir, ['rev-parse', '--show-toplevel']).trim();
  } catch (cause) {
    const msg = cause instanceof Error ? cause.message : String(cause);
    onError(`Cannot locate git repository for ${rootDir}: ${msg}`, cause);
  }
  const canonTop = realpathSync(topLevel);
  const canonRoot = realpathSync(rootDir);
  const relPrefixRaw = relative(canonTop, canonRoot).replaceAll('\\', '/');
  const relPrefix = relPrefixRaw === '' || relPrefixRaw === '.' ? '' : `${relPrefixRaw}/`;
  const stripPrefix = (path: string): string | null => {
    const norm = path.replaceAll('\\', '/');
    if (!relPrefix) return norm;
    return norm.startsWith(relPrefix) ? norm.slice(relPrefix.length) : null;
  };
  return { topLevel, relPrefix: relPrefixRaw === '.' ? '' : relPrefixRaw, stripPrefix };
}

function resolveMergeBase(
  topLevel: string,
  ref: string | null,
  onError: (message: string, cause?: unknown) => never,
): string {
  const candidate =
    ref ||
    ['origin/main', 'origin/master', 'main', 'master', 'HEAD'].find(r =>
      gitTry(topLevel, ['rev-parse', '--verify', `${r}^{commit}`]),
    );
  if (!candidate) {
    onError(`Cannot resolve git base ref in ${topLevel}`);
  }
  const mb = gitTry(topLevel, ['merge-base', 'HEAD', candidate]);
  return mb || candidate;
}

export function collectGitDiff(
  rootDir: string,
  { base, staged = false, mergeBase = undefined }: GitDiffOptions,
  onError: (message: string, cause?: unknown) => never,
): GitDiffInfo {
  const { topLevel, relPrefix, stripPrefix } = resolveGitRoot(rootDir, onError);
  let parents: string[] = [];
  let label = '';
  let stagedTree: string | null = null;

  if (staged) {
    stagedTree = gitRun(topLevel, ['write-tree']).trim();
    const headCommit = gitTry(topLevel, ['rev-parse', '--verify', 'HEAD^{commit}']);
    const mergeHead = gitTry(topLevel, ['rev-parse', '--verify', 'MERGE_HEAD^{commit}']);
    if (headCommit) parents.push(headCommit);
    if (mergeHead) parents.push(mergeHead);
    label = mergeHead ? 'HEAD+MERGE_HEAD (staged)' : 'HEAD (staged)';
    const changedFiles = new Set<string>();
    const renameMap = new Map<string, string>();
    if (parents.length === 0) {
      const cached = gitRun(topLevel, ['ls-files', '--cached', '-z']);
      for (const p of cached.split('\0')) {
        if (!p) continue;
        const rel = stripPrefix(p);
        if (rel) changedFiles.add(rel);
      }
    } else {
      for (const p of parents) {
        const raw = gitRun(topLevel, ['diff', '--cached', '--name-status', '-M', '-z', p]);
        const parsed = parseNameStatusZ(raw, stripPrefix);
        for (const f of parsed.changedFiles) changedFiles.add(f);
        for (const [k, v] of parsed.renameMap) if (!renameMap.has(k)) renameMap.set(k, v);
      }
    }
    return { topLevel, relPrefix, parents, label, stagedTree, changedFiles, renameMap };
  }

  if (mergeBase !== undefined && mergeBase !== false) {
    const explicitRef = typeof mergeBase === 'string' && mergeBase.length > 0 ? mergeBase : null;
    const headParentsLine = gitTry(topLevel, ['rev-list', '--parents', '-n', '1', 'HEAD']) ?? '';
    const headParents = headParentsLine.split(/\s+/).slice(1).filter(Boolean);
    const workDirty = Boolean(gitTry(topLevel, ['status', '--porcelain']));
    if (!explicitRef && headParents.length >= 2 && !workDirty) {
      parents = headParents;
      label = `merge(${headParents.map(p => p.slice(0, 7)).join(',')})`;
    } else {
      const mb = resolveMergeBase(topLevel, explicitRef, onError);
      parents = [mb];
      label = explicitRef ? `merge-base(${explicitRef})` : mb.slice(0, 7);
    }
  } else {
    const ref = base ?? '';
    try {
      gitRun(rootDir, ['rev-parse', '--verify', `${ref}^{commit}`]);
    } catch (cause) {
      const msg = cause instanceof Error ? cause.message : String(cause);
      onError(`Cannot resolve git base ref ${JSON.stringify(ref)} in ${rootDir}: ${msg}`, cause);
    }
    parents = [ref];
    label = ref;
  }

  const changedFiles = new Set<string>();
  const renameMap = new Map<string, string>();
  for (const p of parents) {
    const raw = gitRun(topLevel, ['diff', '--name-status', '-M', '-z', p]);
    const parsed = parseNameStatusZ(raw, stripPrefix);
    for (const f of parsed.changedFiles) changedFiles.add(f);
    for (const [k, v] of parsed.renameMap) if (!renameMap.has(k)) renameMap.set(k, v);
  }
  const untracked = gitRun(topLevel, ['ls-files', '--others', '--exclude-standard', '-z']);
  for (const filePath of untracked.split('\0')) {
    if (!filePath) continue;
    const rel = stripPrefix(filePath);
    if (rel) changedFiles.add(rel);
  }
  return { topLevel, relPrefix, parents, label, stagedTree, changedFiles, renameMap };
}

export async function extractGitArchive(
  topLevel: string,
  treeish: string,
  relPrefix: string,
  destDir: string,
): Promise<void> {
  await new Promise<void>((resolvePromise, reject) => {
    const archiveArgs = ['archive', treeish];
    if (relPrefix) archiveArgs.push('--', relPrefix);
    const git = spawn('git', archiveArgs, { cwd: topLevel, stdio: ['ignore', 'pipe', 'pipe'] });
    const tar = spawn('tar', ['-xf', '-', '-C', destDir], { stdio: ['pipe', 'ignore', 'pipe'] });
    let errText = '';
    git.stderr.on('data', d => {
      errText += String(d);
    });
    tar.stderr.on('data', d => {
      errText += String(d);
    });
    git.stdout.pipe(tar.stdin);
    let gitCode: number | null = null;
    let tarCode: number | null = null;
    let settled = false;
    const done = (err?: Error) => {
      if (settled) return;
      settled = true;
      if (err) reject(err);
      else resolvePromise();
    };
    git.on('error', done);
    tar.on('error', done);
    const check = () => {
      if (gitCode === null || tarCode === null) return;
      if (gitCode !== 0 || tarCode !== 0) {
        done(new Error(`git archive/tar failed (git=${gitCode}, tar=${tarCode}): ${errText.trim()}`));
      } else {
        done();
      }
    };
    git.on('close', code => {
      gitCode = code;
      check();
    });
    tar.on('close', code => {
      tarCode = code;
      check();
    });
  });
}
