import { existsSync } from 'node:fs';
import { mkdtemp, rm } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { collectGitDiff, extractGitArchive, linkNodeModules } from './git.ts';
import type {
  AnalysisErrorCode,
  AnalyzeOptions,
  Config,
  Coverage,
  DependencyFilter,
  DependencyFinding,
  DependencyFindingKind,
  DiffSummary,
  Explanation,
  Finding,
  FindingsFilter,
  Gap,
  InlineGap,
  Location,
  Report,
  Root,
  StaleConfigItem,
  Step,
  Subject,
  SuppressedFinding,
  TestFact,
  WorkspaceInfo,
} from './types.ts';

export type * from './types.ts';

interface NativeSnapshot {
  metadata(): string;
  findings(kind: string | undefined): string;
  dependencies(): string;
  explain(selector: string): string;
  dispose(): void;
}

interface NativeBinding {
  analyzeNative(root: string, options: string): Promise<NativeSnapshot>;
}

type RawGapScope =
  | 'global'
  | { range: [number, number] }
  | { rangePlus: [number, number, number] }
  | { files: number[] };

interface RawGap {
  id: string;
  code: string;
  file: string | null;
  location: Location | null;
  message: string;
  affectedScope?: RawGapScope;
  affectedFiles?: string[] | null;
  testOnly?: boolean;
}

interface RawMetadata {
  roots: Root[];
  workspaces: WorkspaceInfo[];
  suppressed: SuppressedFinding[];
  staleConfig: StaleConfigItem[];
  coverage: {
    status: 'complete' | 'partial';
    files: string[];
    excluded: string[];
    gaps: RawGap[];
  };
}

interface RawSubjectRecord extends Omit<Finding, 'status' | 'gapIds' | 'primaryGaps' | 'kind'> {
  kind: Finding['kind'] | null;
  workspaceGap?: string | null;
  status?: 'complete' | 'conditional';
  gapIds?: readonly string[];
  primaryGaps?: readonly InlineGap[];
}

interface RawWorkspaceStep {
  file: string;
  kind: string;
  name: string;
  location: Location;
  next: number | null;
  edge: Location | null;
}

interface RawWorkspaceRead {
  file: string;
  location: Location;
  fact: TestFact;
  first?: number;
  path?: Step[];
}

interface RawExplanation extends RawSubjectRecord {
  reason: string;
  witnesses: Explanation['witnesses'];
  tests: Explanation['tests'];
  unrootedWorkspaceReads: RawWorkspaceRead[];
  workspaceSteps?: RawWorkspaceStep[];
}

interface DiffContext {
  baseKeys: Set<string>;
  baseDepKeys: Set<string>;
  changedFiles: Set<string>;
  renameMap: Map<string, string>;
  summary: DiffSummary;
}

const metaUrl =
  typeof import.meta?.url === 'string' && import.meta.url.startsWith('file:')
    ? import.meta.url
    : undefined;
const nativeRequire = createRequire(metaUrl ?? process.execPath);
let native: NativeBinding | undefined;

export class AnalysisError extends Error {
  readonly code: AnalysisErrorCode;

  constructor(code: AnalysisErrorCode, message: string, cause?: unknown) {
    super(message, { cause });
    this.name = 'AnalysisError';
    this.code = code;
  }
}

function load(): NativeBinding {
  try {
    if (metaUrl && !metaUrl.includes('/$bunfs/')) {
      const parentAddon = fileURLToPath(new URL('../smella.node', import.meta.url));
      if (existsSync(parentAddon)) return nativeRequire(parentAddon) as NativeBinding;
      const siblingAddon = fileURLToPath(new URL('./smella.node', import.meta.url));
      if (existsSync(siblingAddon)) return nativeRequire(siblingAddon) as NativeBinding;
    }
    const dir = dirname(process.execPath);
    const execSibling = join(dir, 'smella.node');
    if (existsSync(execSibling)) return nativeRequire(execSibling) as NativeBinding;
    const execParent = join(dir, '../smella.node');
    if (existsSync(execParent)) return nativeRequire(execParent) as NativeBinding;
    if (process.platform === 'linux' && process.arch === 'x64') {
      return nativeRequire('smella-linux-x64-gnu') as NativeBinding;
    }
    throw new Error(`No packaged native build for ${process.platform}-${process.arch}`);
  } catch (cause) {
    throw new AnalysisError(
      'NATIVE_LOAD',
      'Could not load the smella native package. In a checkout, run bun run build.',
      cause,
    );
  }
}

function freeze<T>(value: T): T {
  if (value && typeof value === 'object' && !Object.isFrozen(value)) {
    for (const item of Object.values(value as Record<string, unknown>)) freeze(item);
    Object.freeze(value);
  }
  return value;
}

function hydrateMetadata(raw: RawMetadata): {
  metadata: {
    roots: readonly Root[];
    workspaces: readonly WorkspaceInfo[];
    suppressed: readonly SuppressedFinding[];
    staleConfig: readonly StaleConfigItem[];
    coverage: Coverage;
  };
  scopes: readonly (RawGapScope | undefined)[];
} {
  const files = raw.coverage.files;
  const rangeCache = new Map<string, readonly string[]>();
  const getRange = (start: number, end: number): readonly string[] => {
    const key = `${start}:${end}`;
    let cached = rangeCache.get(key);
    if (!cached) {
      cached = Object.freeze(files.slice(start, end));
      rangeCache.set(key, cached);
    }
    return cached;
  };
  const scopes: (RawGapScope | undefined)[] = new Array(raw.coverage.gaps.length);
  const gaps: Gap[] = raw.coverage.gaps.map((g, idx) => {
    const s = g.affectedScope;
    scopes[idx] = s;
    const base = {
      id: g.id,
      code: g.code,
      file: g.file,
      location: g.location ? Object.freeze(g.location) : null,
      message: g.message,
      ...(g.testOnly ? { testOnly: true } : {}),
    } as Gap;

    if (s === undefined) {
      base.affectedFiles = g.affectedFiles ? (Object.freeze(g.affectedFiles) as string[]) : null;
    } else if (s === 'global') {
      base.affectedFiles = null;
    } else {
      let materialized: readonly string[] | undefined;
      Object.defineProperty(base, 'affectedFiles', {
        enumerable: true,
        configurable: false,
        get(): string[] {
          if (materialized !== undefined) return materialized as string[];
          if ('range' in s) {
            materialized = getRange(s.range[0], s.range[1]);
          } else if ('rangePlus' in s) {
            const [start, end, extra] = s.rangePlus;
            const list = files.slice(start, end);
            const extraPath = files[extra];
            if (extraPath !== undefined) {
              if (extra < start) list.unshift(extraPath);
              else if (extra >= end) list.push(extraPath);
            }
            materialized = Object.freeze(list);
          } else {
            materialized = Object.freeze(s.files.map(id => files[id]));
          }
          return materialized as string[];
        },
      });
    }
    return Object.freeze(base);
  });
  Object.freeze(files);
  Object.freeze(gaps);
  const metadata = freeze({
    roots: raw.roots,
    workspaces: raw.workspaces,
    suppressed: raw.suppressed,
    staleConfig: raw.staleConfig,
    coverage: {
      status: raw.coverage.status,
      files,
      excluded: raw.coverage.excluded,
      gaps,
    },
  });
  return { metadata, scopes };
}

const DEP_KINDS = new Set<string>([
  'unused-dependency',
  'unlisted-dependency',
  'unlisted-binary',
  'dev-dependency-in-runtime',
]);

const GAP_CODE_RANK = new Map<string, number>([
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

function toInlineGap(gap: Gap): InlineGap {
  return Object.freeze({
    id: gap.id,
    code: gap.code,
    file: gap.file,
    location: gap.location,
    message: gap.message,
  });
}

function packageKey(file: string | null): string {
  if (!file) return '';
  const parts = file.split('/');
  if (parts.length <= 1) return '';
  if (
    parts[0] === 'packages' ||
    parts[0] === 'nodes' ||
    parts[0] === 'utils' ||
    parts[0] === 'apps' ||
    parts[0] === 'libs'
  ) {
    return parts.slice(0, 2).join('/');
  }
  return parts[0];
}

// Every subject in a file shares its uncertainty. Index gaps by compact file ranges and
// direct file IDs so large repositories with thousands of scoped gaps never expand O(G * F) arrays.
function indexGaps(
  gaps: readonly Gap[],
  files: readonly string[],
  scopes: readonly (RawGapScope | undefined)[],
) {
  const fileIndex = new Map<string, number>();
  for (let i = 0; i < files.length; i++) fileIndex.set(files[i], i);

  const byId = new Map<string, Gap>();
  const inlineById = new Map<string, InlineGap>();
  const globalByPkg = new Map<string, Gap[]>();
  const globalByFile = new Map<string, Gap[]>();
  const topGlobal: Gap[] = [];

  const gapPriority = (gap: Gap): number => GAP_CODE_RANK.get(gap.code) ?? 50;
  const sortGaps = (list: Gap[]): Gap[] => list.sort((a, b) => gapPriority(a) - gapPriority(b));

  const pickTwo = (candidates: readonly Gap[], picked: Gap[] = []): Gap[] => {
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

  interface Bucket {
    ids: string[];
    top: Gap[];
  }
  const addToBucket = (bucket: Bucket, gap: Gap) => {
    bucket.ids.push(gap.id);
    if (bucket.top.length < 2) {
      bucket.top.push(gap);
      if (bucket.top.length === 2) sortGaps(bucket.top);
      return;
    }
    const prio = gapPriority(gap);
    const p0 = gapPriority(bucket.top[0]);
    const p1 = gapPriority(bucket.top[1]);
    if (prio < p0) {
      if (gap.code !== bucket.top[0].code) bucket.top[1] = bucket.top[0];
      bucket.top[0] = gap;
    } else if (prio < p1 && gap.code !== bucket.top[0].code) {
      bucket.top[1] = gap;
    } else if (bucket.top[0].code === bucket.top[1].code && gap.code !== bucket.top[0].code) {
      bucket.top[1] = gap;
      sortGaps(bucket.top);
    }
  };

  interface RangeBucket extends Bucket {
    start: number;
    end: number;
  }

  interface LaneIndex {
    globalIds: string[];
    ranges: RangeBucket[];
    rangeMap: Map<string, RangeBucket>;
    direct: Map<number, Bucket>;
    fileCache: Map<string, { ids: readonly string[]; localTop: readonly Gap[] }>;
    comboCache: Map<string, readonly string[]>;
  }
  const makeLane = (): LaneIndex => ({
    globalIds: [],
    ranges: [],
    rangeMap: new Map(),
    direct: new Map(),
    fileCache: new Map(),
    comboCache: new Map(),
  });
  const allLane = makeLane();
  const nonTestLane = makeLane();

  const addToLane = (lane: LaneIndex, gap: Gap, scope: RawGapScope | undefined) => {
    const addRange = (start: number, end: number) => {
      if (start >= end) return;
      const key = `${start}:${end}`;
      let b = lane.rangeMap.get(key);
      if (!b) {
        b = { start, end, ids: [], top: [] };
        lane.rangeMap.set(key, b);
        lane.ranges.push(b);
      }
      addToBucket(b, gap);
    };
    const addDirect = (fid: number) => {
      let b = lane.direct.get(fid);
      if (!b) {
        b = { ids: [], top: [] };
        lane.direct.set(fid, b);
      }
      addToBucket(b, gap);
    };
    if (scope === undefined) {
      const af = gap.affectedFiles;
      if (af === null) {
        lane.globalIds.push(gap.id);
      } else {
        for (const f of af) {
          const fid = fileIndex.get(f);
          if (fid !== undefined) addDirect(fid);
        }
      }
    } else if (scope === 'global') {
      lane.globalIds.push(gap.id);
    } else if ('range' in scope) {
      addRange(scope.range[0], scope.range[1]);
    } else if ('rangePlus' in scope) {
      addRange(scope.rangePlus[0], scope.rangePlus[1]);
      addDirect(scope.rangePlus[2]);
    } else {
      for (const fid of scope.files) addDirect(fid);
    }
  };

  for (let i = 0; i < gaps.length; i++) {
    const gap = gaps[i];
    const scope = scopes[i];
    byId.set(gap.id, gap);
    inlineById.set(gap.id, toInlineGap(gap));
    // The native graph qualifies individual subjects for unrooted workspace reads.
    if (gap.code === 'unrooted-workspace-reference') continue;
    const isGlobal = scope !== undefined ? scope === 'global' : gap.affectedFiles === null;
    if (isGlobal) {
      if (gap.file) {
        let byFile = globalByFile.get(gap.file);
        if (!byFile) {
          byFile = [];
          globalByFile.set(gap.file, byFile);
        }
        byFile.push(gap);
        const pkg = packageKey(gap.file);
        if (pkg) {
          let byPkg = globalByPkg.get(pkg);
          if (!byPkg) {
            byPkg = [];
            globalByPkg.set(pkg, byPkg);
          }
          byPkg.push(gap);
        }
      }
      topGlobal.push(gap);
    }
    addToLane(allLane, gap, scope);
    if (!gap.testOnly) addToLane(nonTestLane, gap, scope);
  }

  sortGaps(topGlobal);
  for (const list of globalByFile.values()) sortGaps(list);
  for (const list of globalByPkg.values()) sortGaps(list);
  const fallbackGlobal = pickTwo(topGlobal);

  const frozenAllGlobal: readonly string[] = Object.freeze(allLane.globalIds);
  const frozenNonTestGlobal: readonly string[] = Object.freeze(nonTestLane.globalIds);
  const emptyTop: readonly Gap[] = Object.freeze([]);

  function resolveLane(
    lane: LaneIndex,
    frozenGlobal: readonly string[],
    file: string,
  ): { ids: readonly string[]; localTop: readonly Gap[] } {
    const cached = lane.fileCache.get(file);
    if (cached) return cached;
    const fid = fileIndex.get(file);
    if (fid === undefined) {
      const res = { ids: frozenGlobal, localTop: emptyTop };
      lane.fileCache.set(file, res);
      return res;
    }
    const matchedRanges: RangeBucket[] = [];
    for (const rb of lane.ranges) {
      if (fid >= rb.start && fid < rb.end) matchedRanges.push(rb);
    }
    const direct = lane.direct.get(fid);
    if (matchedRanges.length === 0 && !direct) {
      const res = { ids: frozenGlobal, localTop: emptyTop };
      lane.fileCache.set(file, res);
      return res;
    }
    // Share frozen gapIds arrays across files matching the exact same ranges (with no direct gaps).
    const comboKey = !direct
      ? matchedRanges.map(r => `${r.start}:${r.end}`).join('|')
      : matchedRanges.length === 0
        ? `d:${direct.ids.join(',')}`
        : `${matchedRanges.map(r => `${r.start}:${r.end}`).join('|')}|d:${direct.ids.join(',')}`;
    let ids = lane.comboCache.get(comboKey);
    if (!ids) {
      const out = frozenGlobal.slice();
      for (const rb of matchedRanges) {
        for (const id of rb.ids) out.push(id);
      }
      if (direct) {
        for (const id of direct.ids) out.push(id);
      }
      ids = Object.freeze(out);
      lane.comboCache.set(comboKey, ids);
    }
    const topCandidates: Gap[] = [];
    for (const rb of matchedRanges) {
      for (const g of rb.top) topCandidates.push(g);
    }
    if (direct) {
      for (const g of direct.top) topCandidates.push(g);
    }
    sortGaps(topCandidates);
    const localTop = Object.freeze(pickTwo(topCandidates));
    const res = { ids, localTop };
    lane.fileCache.set(file, res);
    return res;
  }

  const withWorkspace = new Map<readonly string[], readonly string[]>();
  const primaryCache = new Map<string, readonly InlineGap[]>();
  const emptyPrimary: readonly InlineGap[] = Object.freeze([]);

  function primaryFor(
    file: string,
    localTop: readonly Gap[],
    workspaceGapId: string | null,
    nonTest: boolean,
  ): readonly InlineGap[] {
    const cacheKey = `${file}\0${workspaceGapId ?? ''}\0${nonTest ? 1 : 0}`;
    const existing = primaryCache.get(cacheKey);
    if (existing) return existing;
    const picked: Gap[] = [];
    if (workspaceGapId) {
      const wg = byId.get(workspaceGapId);
      if (wg) picked.push(wg);
    }
    if (picked.length < 2 && localTop.length) {
      pickTwo(localTop, picked);
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
    const result = picked.length
      ? Object.freeze(picked.map(g => inlineById.get(g.id)!))
      : emptyPrimary;
    primaryCache.set(cacheKey, result);
    return result;
  }

  return <T extends RawSubjectRecord>(record: T): T => {
    const isTestOrToolingFinding =
      record.kind === 'test-only-export' ||
      record.kind === 'test-only-binding' ||
      record.kind === 'tooling-only-export';
    const resolved = isTestOrToolingFinding
      ? resolveLane(nonTestLane, frozenNonTestGlobal, record.file)
      : resolveLane(allLane, frozenAllGlobal, record.file);
    let ids = resolved.ids;
    const workspaceGap = record.workspaceGap ?? null;
    if (workspaceGap) {
      let extended = withWorkspace.get(ids);
      if (!extended) {
        extended = Object.freeze([...ids, workspaceGap]);
        withWorkspace.set(ids, extended);
      }
      ids = extended;
      delete record.workspaceGap;
    }
    record.gapIds = ids;
    record.status = record.gapIds.length ? 'conditional' : 'complete';
    record.primaryGaps = record.gapIds.length
      ? primaryFor(record.file, resolved.localTop, workspaceGap, isTestOrToolingFinding)
      : emptyPrimary;
    return record;
  };
}

function decodeExplanation(record: RawExplanation): Explanation {
  const steps = record.workspaceSteps ?? [];
  delete record.workspaceSteps;
  for (const read of record.unrootedWorkspaceReads) {
    const path: Step[] = [];
    let index: number | null = read.first ?? null;
    let via: { file: string; location: Location } | null = null;
    while (index !== null) {
      const step = steps[index];
      path.push({
        file: step.file,
        kind: step.kind,
        name: step.name,
        location: { ...step.location },
        via,
      });
      via = step.next === null || !step.edge ? null : { file: step.file, location: { ...step.edge } };
      index = step.next;
    }
    delete read.first;
    read.path = path;
  }
  return record as unknown as Explanation;
}

export function createConfig<T extends Config>(config: T): T {
  return config;
}

function findingKey(
  record: { kind: string | null; file: string; exportName: string | null; bindingName?: string },
  fileOverride?: string,
): string {
  return `${record.kind}|${fileOverride ?? record.file}|${record.exportName ?? ''}|${record.bindingName ?? ''}`;
}

function depFindingKey(record: DependencyFinding, fileOverride?: string): string {
  return `${record.kind}|${fileOverride ?? record.file}|${record.manifest ?? ''}|${record.dependency ?? ''}|${record.binary ?? ''}|${record.script ?? ''}`;
}

export async function analyze({
  root = process.cwd(),
  base,
  staged = false,
  mergeBase,
  dependencies = false,
  ...options
}: AnalyzeOptions = {}): Promise<Report> {
  native ??= load();
  const rootDir = resolve(root);
  const optionsJson = JSON.stringify(options);

  let diffContext: DiffContext | null = null;
  let snapshot: NativeSnapshot | undefined;
  const hasDiffMode =
    (base !== undefined && base !== null) ||
    Boolean(staged) ||
    (mergeBase !== undefined && mergeBase !== false);

  if (hasDiffMode) {
    const gitInfo = collectGitDiff(
      rootDir,
      { base, staged, mergeBase },
      (message, cause) => {
        throw new AnalysisError('ANALYSIS_FAILED', message, cause);
      },
    );
    const tmpDirs: string[] = [];
    try {
      let headPromise: Promise<NativeSnapshot>;
      if (gitInfo.stagedTree) {
        const stagedDir = await mkdtemp(join(tmpdir(), 'smella-staged-'));
        tmpDirs.push(stagedDir);
        await extractGitArchive(gitInfo.topLevel, gitInfo.stagedTree, gitInfo.relPrefix, stagedDir);
        const stagedRoot = gitInfo.relPrefix ? join(stagedDir, gitInfo.relPrefix) : stagedDir;
        linkNodeModules(rootDir, stagedRoot);
        headPromise = native.analyzeNative(stagedRoot, optionsJson);
      } else {
        headPromise = native.analyzeNative(rootDir, optionsJson);
      }
      const baseKeys = new Set<string>();
      const baseDepKeys = new Set<string>();
      if (gitInfo.changedFiles.size > 0) {
        for (const parentRef of gitInfo.parents) {
          const tmpDir = await mkdtemp(join(tmpdir(), 'smella-base-'));
          tmpDirs.push(tmpDir);
          await extractGitArchive(gitInfo.topLevel, parentRef, gitInfo.relPrefix, tmpDir);
          const baseRoot = gitInfo.relPrefix ? join(tmpDir, gitInfo.relPrefix) : tmpDir;
          linkNodeModules(rootDir, baseRoot);
          let baseSnapshot: NativeSnapshot | undefined;
          try {
            baseSnapshot = await native.analyzeNative(baseRoot, optionsJson);
            for (const f of JSON.parse(baseSnapshot.findings(undefined)) as RawSubjectRecord[]) {
              baseKeys.add(findingKey(f));
            }
            for (const d of JSON.parse(baseSnapshot.dependencies()) as DependencyFinding[]) {
              baseDepKeys.add(depFindingKey(d));
            }
          } finally {
            baseSnapshot?.dispose();
          }
        }
      }
      snapshot = await headPromise;
      const headFindings = JSON.parse(snapshot.findings(undefined)) as RawSubjectRecord[];
      let introduced = 0;
      let preExisting = 0;
      for (const f of headFindings) {
        if (!gitInfo.changedFiles.has(f.file)) continue;
        const baseFile = gitInfo.renameMap.get(f.file) ?? f.file;
        if (baseKeys.has(findingKey(f, baseFile))) preExisting++;
        else introduced++;
      }
      if (dependencies) {
        for (const d of JSON.parse(snapshot.dependencies()) as DependencyFinding[]) {
          if (!gitInfo.changedFiles.has(d.file) && !gitInfo.changedFiles.has(d.manifest)) continue;
          const baseFile = gitInfo.renameMap.get(d.file) ?? d.file;
          if (baseDepKeys.has(depFindingKey(d, baseFile))) preExisting++;
          else introduced++;
        }
      }
      diffContext = {
        baseKeys,
        baseDepKeys,
        changedFiles: gitInfo.changedFiles,
        renameMap: gitInfo.renameMap,
        summary: Object.freeze({
          base: gitInfo.label,
          parents: Object.freeze([...gitInfo.parents]),
          staged: Boolean(staged),
          changedFiles: Object.freeze([...gitInfo.changedFiles].sort()),
          introduced,
          preExisting,
        }),
      };
    } catch (cause) {
      snapshot?.dispose();
      if (cause instanceof AnalysisError) throw cause;
      const msg = cause instanceof Error ? cause.message : String(cause);
      throw new AnalysisError('ANALYSIS_FAILED', msg, cause);
    } finally {
      for (const dir of tmpDirs) {
        await rm(dir, { recursive: true, force: true });
      }
    }
  } else {
    try {
      snapshot = await native.analyzeNative(rootDir, optionsJson);
    } catch (cause) {
      const msg = cause instanceof Error ? cause.message : String(cause);
      throw new AnalysisError('ANALYSIS_FAILED', msg, cause);
    }
  }

  const hydrated = hydrateMetadata(JSON.parse(snapshot.metadata()) as RawMetadata);
  let metadata:
    | {
        roots: readonly Root[];
        workspaces: readonly WorkspaceInfo[];
        suppressed: readonly SuppressedFinding[];
        staleConfig: readonly StaleConfigItem[];
        coverage: Coverage;
      }
    | undefined = hydrated.metadata;
  let withGaps: (<T extends RawSubjectRecord>(record: T) => T) | undefined = indexGaps(
    metadata.coverage.gaps,
    metadata.coverage.files,
    hydrated.scopes,
  );

  function live(): NativeSnapshot {
    if (!snapshot || !metadata || !withGaps) {
      throw new AnalysisError('REPORT_DISPOSED', 'This analysis report has been disposed');
    }
    return snapshot;
  }

  function classifyDiff<T extends { kind: string | null; file: string; exportName: string | null; bindingName?: string; diff?: Finding['diff'] }>(
    record: T,
  ): T {
    if (!diffContext || !record.kind || !diffContext.changedFiles.has(record.file)) return record;
    const baseFile = diffContext.renameMap.get(record.file) ?? record.file;
    record.diff = diffContext.baseKeys.has(findingKey(record, baseFile))
      ? 'pre-existing'
      : 'introduced';
    return record;
  }

  function normalizeWorkspaceFilter(workspace?: string | readonly string[]): Set<string> | null {
    if (!workspace || !metadata) return null;
    const rawList = Array.isArray(workspace) ? workspace : [workspace];
    if (rawList.length === 0) return null;
    const allowed = new Set<string>();
    const byName = new Map<string, string>();
    for (const ws of metadata.workspaces) {
      if (ws.name) byName.set(ws.name, ws.dir);
    }
    for (const item of rawList) {
      const cleaned = String(item)
        .trim()
        .replaceAll('\\', '/')
        .replace(/^\.\//, '')
        .replace(/\/+$/, '');
      const dir = cleaned === '' ? '.' : (byName.get(cleaned) ?? cleaned);
      allowed.add(dir);
    }
    return allowed;
  }

  function classifyDepDiff(record: DependencyFinding): DependencyFinding {
    if (!diffContext) return record;
    const baseFile = diffContext.renameMap.get(record.file) ?? record.file;
    record.diff = diffContext.baseDepKeys.has(depFindingKey(record, baseFile))
      ? 'pre-existing'
      : 'introduced';
    return record;
  }

  function filterDeps({ kind, diff, workspace }: DependencyFilter = {}): DependencyFinding[] {
    let records = JSON.parse(live().dependencies()) as DependencyFinding[];
    const allowedWs = normalizeWorkspaceFilter(workspace);
    if (allowedWs) records = records.filter(r => allowedWs.has(r.workspace));
    if (kind) records = records.filter(r => r.kind === kind);
    if (diffContext) {
      records = records
        .filter(
          r => diffContext!.changedFiles.has(r.file) || diffContext!.changedFiles.has(r.manifest),
        )
        .map(classifyDepDiff);
      if (diff) records = records.filter(r => r.diff === diff);
    }
    return records;
  }

  return Object.freeze({
    get roots() {
      live();
      return metadata!.roots;
    },
    get workspaces() {
      live();
      return metadata!.workspaces;
    },
    get suppressed() {
      live();
      return metadata!.suppressed;
    },
    get staleConfig() {
      live();
      return metadata!.staleConfig;
    },
    get coverage() {
      live();
      return metadata!.coverage;
    },
    get diff() {
      live();
      return diffContext?.summary;
    },
    dependencies(filter: DependencyFilter = {}) {
      return filterDeps(filter);
    },
    findings({ kind, diff, workspace }: FindingsFilter = {}) {
      if (kind && DEP_KINDS.has(kind)) {
        return filterDeps({ kind: kind as DependencyFindingKind, diff, workspace });
      }
      const applyGaps = withGaps!;
      let records = (JSON.parse(live().findings(kind)) as RawSubjectRecord[]).map(r =>
        applyGaps(r),
      ) as unknown as Finding[];
      const allowedWs = normalizeWorkspaceFilter(workspace);
      if (allowedWs) {
        records = records.filter(r => allowedWs.has(r.workspace));
      }
      if (diffContext) {
        records = records.filter(r => diffContext!.changedFiles.has(r.file)).map(classifyDiff);
        if (diff) records = records.filter(r => r.diff === diff);
      }
      if (dependencies && !kind) {
        return [...records, ...filterDeps({ diff, workspace })];
      }
      return records;
    },
    explain(subject: Subject) {
      const current = live();
      try {
        const parsed = JSON.parse(current.explain(JSON.stringify(subject))) as RawExplanation;
        return classifyDiff(withGaps!(decodeExplanation(parsed)));
      } catch (cause) {
        const msg = cause instanceof Error ? cause.message : String(cause);
        throw new AnalysisError('UNKNOWN_SUBJECT', msg, cause);
      }
    },
    dispose() {
      snapshot?.dispose();
      snapshot = undefined;
      metadata = undefined;
      withGaps = undefined;
      diffContext = null;
    },
  });
}
