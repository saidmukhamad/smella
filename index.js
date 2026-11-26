import { createRequire } from 'node:module';
import { execFileSync, spawn } from 'node:child_process';
import { existsSync, readdirSync, realpathSync, symlinkSync } from 'node:fs';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, relative, resolve } from 'node:path';

const require = createRequire(import.meta.url);
let native;

export class AnalysisError extends Error {
  constructor(code, message, cause) {
    super(message, { cause });
    this.name = 'AnalysisError';
    this.code = code;
  }
}

function load() {
  try {
    if (existsSync(new URL('./smella.node', import.meta.url))) return require('./smella.node');
    if (process.platform === 'linux' && process.arch === 'x64') return require('smella-linux-x64-gnu');
    throw new Error(`No packaged native build for ${process.platform}-${process.arch}`);
  } catch (cause) {
    throw new AnalysisError('NATIVE_LOAD', 'Could not load the smella native package. In a checkout, run npm run build.', cause);
  }
}

function freeze(value) {
  if (value && typeof value === 'object') {
    for (const item of Object.values(value)) freeze(item);
    Object.freeze(value);
  }
  return value;
}

const GAP_CODE_RANK = new Map([
  ['unrooted-workspace-reference', 0],
  ['namespace-escape', 1],
  ['parse-error', 2],
  ['unresolved-import', 3],
  ['computed-import', 4],
  ['bundler-glob', 5],
  ['unsupported-runtime', 6],
  ['ambiguous-entry', 7],
  ['invalid-root', 8],
  ['missing-export', 9],
  ['external-star', 10],
]);

function toInlineGap(gap) {
  return Object.freeze({
    id: gap.id,
    code: gap.code,
    file: gap.file,
    location: gap.location,
    message: gap.message,
  });
}

function packageKey(file) {
  if (!file) return '';
  const parts = file.split('/');
  if (parts.length <= 1) return '';
  if (parts[0] === 'packages' || parts[0] === 'nodes' || parts[0] === 'utils' || parts[0] === 'apps' || parts[0] === 'libs') {
    return parts.slice(0, 2).join('/');
  }
  return parts[0];
}

// Every subject in a file shares its uncertainty. Repeating thousands of project-wide
// gap IDs in the native JSON for every finding can exceed the JS string size limit.
function indexGaps(gaps) {
  const global = [];
  const local = new Map();
  const byId = new Map();
  const inlineById = new Map();
  const globalByPkg = new Map();
  const globalByFile = new Map();
  const topGlobal = [];

  for (const gap of gaps) {
    byId.set(gap.id, gap);
    inlineById.set(gap.id, toInlineGap(gap));
    // The native graph qualifies individual subjects for unrooted workspace reads.
    if (gap.code === 'unrooted-workspace-reference') continue;
    if (gap.affectedFiles === null) {
      global.push(gap.id);
      if (gap.file) {
        if (!globalByFile.has(gap.file)) globalByFile.set(gap.file, []);
        globalByFile.get(gap.file).push(gap);
        const pkg = packageKey(gap.file);
        if (pkg) {
          if (!globalByPkg.has(pkg)) globalByPkg.set(pkg, []);
          globalByPkg.get(pkg).push(gap);
        }
      }
      topGlobal.push(gap);
    } else {
      for (const file of gap.affectedFiles) {
        if (!local.has(file)) local.set(file, []);
        local.get(file).push(gap.id);
      }
    }
  }

  const gapPriority = gap => GAP_CODE_RANK.get(gap.code) ?? 50;
  const sortGaps = list => list.sort((a, b) => gapPriority(a) - gapPriority(b));
  sortGaps(topGlobal);
  for (const list of globalByFile.values()) sortGaps(list);
  for (const list of globalByPkg.values()) sortGaps(list);

  // Pick up to 2 most representative global gaps (preferring distinct codes).
  const pickTwo = (candidates, picked = []) => {
    for (const gap of candidates) {
      if (picked.length >= 2) break;
      if (picked.some(g => g.id === gap.id)) continue;
      if (picked.length === 1 && picked[0].code === gap.code) continue;
      picked.push(gap);
    }
    if (picked.length < 2) {
      for (const gap of candidates) {
        if (picked.length >= 2) break;
        if (picked.some(g => g.id === gap.id)) continue;
        picked.push(gap);
      }
    }
    return picked;
  };
  const fallbackGlobal = pickTwo(topGlobal);

  Object.freeze(global);
  const byCombination = new Map();
  for (const [file, ids] of local) {
    const key = ids.join(',');
    if (!byCombination.has(key)) byCombination.set(key, Object.freeze(global.concat(ids)));
    local.set(file, byCombination.get(key));
  }
  const withWorkspace = new Map();
  const primaryCache = new Map();
  const emptyPrimary = Object.freeze([]);

  function primaryFor(file, localIds, workspaceGapId) {
    const cacheKey = `${file}\0${workspaceGapId ?? ''}`;
    let cached = primaryCache.get(cacheKey);
    if (cached) return cached;
    const picked = [];
    if (workspaceGapId) {
      const wg = byId.get(workspaceGapId);
      if (wg) picked.push(wg);
    }
    if (picked.length < 2 && localIds) {
      const scoped = localIds.map(id => byId.get(id)).filter(Boolean);
      sortGaps(scoped);
      pickTwo(scoped, picked);
    }
    if (picked.length < 2) {
      const sameFile = globalByFile.get(file);
      if (sameFile) pickTwo(sameFile, picked);
    }
    if (picked.length < 2) {
      const samePkg = globalByPkg.get(packageKey(file));
      if (samePkg) pickTwo(samePkg, picked);
    }
    if (picked.length < 2 && fallbackGlobal.length) {
      pickTwo(fallbackGlobal, picked);
    }
    cached = picked.length ? Object.freeze(picked.map(g => inlineById.get(g.id))) : emptyPrimary;
    primaryCache.set(cacheKey, cached);
    return cached;
  }

  // Keep raw local-only IDs per file for primaryGap prioritization before merging with global.
  const localOnly = new Map();
  for (const gap of gaps) {
    if (gap.code === 'unrooted-workspace-reference' || gap.affectedFiles === null) continue;
    for (const file of gap.affectedFiles) {
      if (!localOnly.has(file)) localOnly.set(file, []);
      localOnly.get(file).push(gap.id);
    }
  }

  // Separate test-only gaps (which can never establish runtime usage, so they cannot
  // make test-only-export / test-only-binding / tooling-only-export findings conditional).
  const localNonTest = new Map();
  const globalNonTest = [];
  for (const gap of gaps) {
    if (gap.code === 'unrooted-workspace-reference' || gap.testOnly) continue;
    if (gap.affectedFiles === null) {
      globalNonTest.push(gap.id);
    } else {
      for (const file of gap.affectedFiles) {
        if (!localNonTest.has(file)) localNonTest.set(file, []);
        localNonTest.get(file).push(gap.id);
      }
    }
  }
  Object.freeze(globalNonTest);
  const byNonTestCombo = new Map();
  for (const [file, ids] of localNonTest) {
    const key = ids.join(',');
    if (!byNonTestCombo.has(key)) byNonTestCombo.set(key, Object.freeze(globalNonTest.concat(ids)));
    localNonTest.set(file, byNonTestCombo.get(key));
  }

  return record => {
    const isTestOrToolingFinding =
      record.kind === 'test-only-export' ||
      record.kind === 'test-only-binding' ||
      record.kind === 'tooling-only-export';
    let ids = isTestOrToolingFinding
      ? (localNonTest.get(record.file) ?? globalNonTest)
      : (local.get(record.file) ?? global);
    const workspaceGap = record.workspaceGap ?? null;
    if (workspaceGap) {
      if (!withWorkspace.has(ids)) withWorkspace.set(ids, Object.freeze([...ids, workspaceGap]));
      ids = withWorkspace.get(ids);
      delete record.workspaceGap;
    }
    record.gapIds = ids;
    record.status = record.gapIds.length ? 'conditional' : 'complete';
    record.primaryGaps = record.gapIds.length
      ? primaryFor(
          record.file,
          isTestOrToolingFinding ? localNonTest.get(record.file) : localOnly.get(record.file),
          workspaceGap,
        )
      : emptyPrimary;
    return record;
  };
}

function decodeExplanation(record) {
  const steps = record.workspaceSteps;
  delete record.workspaceSteps;
  for (const read of record.unrootedWorkspaceReads) {
    const path = [];
    let index = read.first;
    let via = null;
    while (index !== null) {
      const step = steps[index];
      path.push({file:step.file, kind:step.kind, name:step.name, location:{...step.location}, via});
      via = step.next === null ? null : {file:step.file, location:{...step.edge}};
      index = step.next;
    }
    delete read.first;
    read.path = path;
  }
  return record;
}

export function createConfig(config) { return config; }

function findingKey(record, fileOverride) {
  return `${record.kind}|${fileOverride ?? record.file}|${record.exportName ?? ''}|${record.bindingName ?? ''}`;
}

function linkNodeModules(srcDir, dstDir, depth = 0) {
  if (depth > 3 || !existsSync(srcDir) || !existsSync(dstDir)) return;
  let entries;
  try { entries = readdirSync(srcDir, { withFileTypes: true }); } catch { return; }
  for (const entry of entries) {
    if (!entry.isDirectory()) continue;
    const name = entry.name;
    if (name === 'node_modules') {
      const dst = join(dstDir, name);
      if (!existsSync(dst)) {
        try { symlinkSync(join(srcDir, name), dst, 'dir'); } catch {}
      }
      continue;
    }
    if (name === '.git' || name === 'dist' || name === 'build' || name === '.nx' || name === 'target') continue;
    linkNodeModules(join(srcDir, name), join(dstDir, name), depth + 1);
  }
}

function gitRun(cwd, args) {
  return execFileSync('git', args, { cwd, encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 });
}

function collectGitDiff(rootDir, base) {
  let topLevel;
  try {
    const out = gitRun(rootDir, ['rev-parse', '--show-toplevel', '--verify', `${base}^{commit}`]).trim().split('\n');
    topLevel = out[0];
  } catch (cause) {
    throw new AnalysisError('ANALYSIS_FAILED', `Cannot resolve git base ref ${JSON.stringify(base)} in ${rootDir}: ${cause.message}`, cause);
  }
  const canonTop = realpathSync(topLevel);
  const canonRoot = realpathSync(rootDir);
  const relPrefixRaw = relative(canonTop, canonRoot).replaceAll('\\', '/');
  const relPrefix = relPrefixRaw === '' || relPrefixRaw === '.' ? '' : `${relPrefixRaw}/`;
  const stripPrefix = path => {
    const norm = path.replaceAll('\\', '/');
    if (!relPrefix) return norm;
    return norm.startsWith(relPrefix) ? norm.slice(relPrefix.length) : null;
  };
  const changedFiles = new Set();
  const renameMap = new Map();
  const raw = gitRun(topLevel, ['diff', '--name-status', '-M', '-z', base]);
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
  const untracked = gitRun(topLevel, ['ls-files', '--others', '--exclude-standard', '-z']);
  for (const filePath of untracked.split('\0')) {
    if (!filePath) continue;
    const rel = stripPrefix(filePath);
    if (rel) changedFiles.add(rel);
  }
  return { topLevel, relPrefix: relPrefixRaw === '.' ? '' : relPrefixRaw, changedFiles, renameMap };
}

async function extractGitArchive(topLevel, base, relPrefix, destDir) {
  await new Promise((resolvePromise, reject) => {
    const archiveArgs = ['archive', base];
    if (relPrefix) archiveArgs.push('--', relPrefix);
    const git = spawn('git', archiveArgs, { cwd: topLevel, stdio: ['ignore', 'pipe', 'pipe'] });
    const tar = spawn('tar', ['-xf', '-', '-C', destDir], { stdio: ['pipe', 'ignore', 'pipe'] });
    let errText = '';
    git.stderr.on('data', d => { errText += d; });
    tar.stderr.on('data', d => { errText += d; });
    git.stdout.pipe(tar.stdin);
    let gitCode = null;
    let tarCode = null;
    let settled = false;
    const done = err => {
      if (settled) return;
      settled = true;
      err ? reject(err) : resolvePromise();
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
    git.on('close', code => { gitCode = code; check(); });
    tar.on('close', code => { tarCode = code; check(); });
  });
}

export async function analyze({ root = process.cwd(), base, ...options } = {}) {
  native ??= load();
  const rootDir = resolve(root);
  const optionsJson = JSON.stringify(options);

  let diffContext = null;
  let snapshot;
  if (base !== undefined && base !== null) {
    const gitInfo = collectGitDiff(rootDir, base);
    const tmpDir = gitInfo.changedFiles.size > 0 ? await mkdtemp(join(tmpdir(), 'smella-base-')) : null;
    try {
      const headPromise = native.analyzeNative(rootDir, optionsJson);
      let baseKeys = new Set();
      if (tmpDir) {
        await extractGitArchive(gitInfo.topLevel, base, gitInfo.relPrefix, tmpDir);
        const baseRoot = gitInfo.relPrefix ? join(tmpDir, gitInfo.relPrefix) : tmpDir;
        linkNodeModules(rootDir, baseRoot);
        let baseSnapshot;
        try {
          baseSnapshot = await native.analyzeNative(baseRoot, optionsJson);
          for (const f of JSON.parse(baseSnapshot.findings(undefined))) {
            baseKeys.add(findingKey(f));
          }
        } finally {
          baseSnapshot?.dispose();
        }
      }
      snapshot = await headPromise;
      const headFindings = JSON.parse(snapshot.findings(undefined));
      let introduced = 0;
      let preExisting = 0;
      for (const f of headFindings) {
        if (!gitInfo.changedFiles.has(f.file)) continue;
        const baseFile = gitInfo.renameMap.get(f.file) ?? f.file;
        if (baseKeys.has(findingKey(f, baseFile))) preExisting++;
        else introduced++;
      }
      diffContext = {
        baseKeys,
        changedFiles: gitInfo.changedFiles,
        renameMap: gitInfo.renameMap,
        summary: Object.freeze({
          base,
          changedFiles: Object.freeze([...gitInfo.changedFiles].sort()),
          introduced,
          preExisting,
        }),
      };
    } catch (cause) {
      snapshot?.dispose();
      if (cause instanceof AnalysisError) throw cause;
      throw new AnalysisError('ANALYSIS_FAILED', cause.message, cause);
    } finally {
      if (tmpDir) await rm(tmpDir, { recursive: true, force: true });
    }
  } else {
    try {
      snapshot = await native.analyzeNative(rootDir, optionsJson);
    } catch (cause) {
      throw new AnalysisError('ANALYSIS_FAILED', cause.message, cause);
    }
  }

  let metadata = freeze(JSON.parse(snapshot.metadata()));
  let withGaps = indexGaps(metadata.coverage.gaps);
  function live() {
    if (!snapshot) throw new AnalysisError('REPORT_DISPOSED', 'This analysis report has been disposed');
    return snapshot;
  }
  function classifyDiff(record) {
    if (!diffContext || !record.kind || !diffContext.changedFiles.has(record.file)) return record;
    const baseFile = diffContext.renameMap.get(record.file) ?? record.file;
    record.diff = diffContext.baseKeys.has(findingKey(record, baseFile)) ? 'pre-existing' : 'introduced';
    return record;
  }
  function normalizeWorkspaceFilter(workspace) {
    if (!workspace) return null;
    const rawList = Array.isArray(workspace) ? workspace : [workspace];
    if (rawList.length === 0) return null;
    const allowed = new Set();
    const byName = new Map();
    for (const ws of metadata.workspaces) {
      if (ws.name) byName.set(ws.name, ws.dir);
    }
    for (const item of rawList) {
      const cleaned = String(item).trim().replaceAll('\\', '/').replace(/^\.\//, '').replace(/\/+$/, '');
      const dir = cleaned === '' ? '.' : (byName.get(cleaned) ?? cleaned);
      allowed.add(dir);
    }
    return allowed;
  }
  return Object.freeze({
    get roots() { live(); return metadata.roots; },
    get workspaces() { live(); return metadata.workspaces; },
    get coverage() { live(); return metadata.coverage; },
    get diff() { live(); return diffContext?.summary; },
    findings({ kind, diff, workspace } = {}) {
      let records = JSON.parse(live().findings(kind)).map(withGaps);
      const allowedWs = normalizeWorkspaceFilter(workspace);
      if (allowedWs) {
        records = records.filter(r => allowedWs.has(r.workspace));
      }
      if (diffContext) {
        records = records.filter(r => diffContext.changedFiles.has(r.file)).map(classifyDiff);
        if (diff) records = records.filter(r => r.diff === diff);
      }
      return records;
    },
    explain(subject) {
      const current = live();
      try { return classifyDiff(withGaps(decodeExplanation(JSON.parse(current.explain(JSON.stringify(subject)))))); }
      catch (cause) { throw new AnalysisError('UNKNOWN_SUBJECT', cause.message, cause); }
    },
    dispose() {
      snapshot?.dispose();
      snapshot = metadata = withGaps = diffContext = undefined;
    },
  });
}
