#!/usr/bin/env node
import { readFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { parseArgs } from 'node:util';
import {
  analyze,
  type AnalyzeOptions,
  type Context,
  type DependencyFinding,
  type DependencyFindingKind,
  type DiffStatus,
  type Finding,
  type FindingKind,
  type Report,
} from './index.ts';

const USAGE = `Usage: smella [options]

Options:
  --root <dir>        Repository root to analyze (default: cwd)
  --entries <spec>    Override entry points (repeatable). Format:
                        <role>:<path1>,<path2>  where role is runtime|test|tooling|public
                        <path>                  shorthand for runtime:<path>
                        <role>:                 empty list disables discovery for that role
  --workspace <dir>   Filter findings to one or more workspaces by directory or
                      package name (repeatable or comma-separated)
  --base <ref>        Git ref to diff against (only findings in changed files,
                      marked "introduced" or "pre-existing")
  --diff [ref|status] Without --base: diff working tree against merge-base of [ref]
                      (or judge a clean merge commit against both parents), exiting 1
                      if any finding is introduced.
                      With --base: filter diff status ("introduced" | "pre-existing").
  --staged            Diff staged git index against HEAD (or HEAD + MERGE_HEAD),
                      exiting 1 if any finding is introduced
  --all               Report all findings without failing (always exits 0)
  --dependencies      Include dependency findings (unused-dependency,
                      unlisted-dependency, unlisted-binary, dev-dependency-in-runtime)
  --kind <kind>       Filter finding kind (repeatable or comma-separated):
                        unused-export, test-only-export, test-only-binding,
                        tooling-only-export, unreferenced-module,
                        unused-dependency, unlisted-dependency,
                        unlisted-binary, dev-dependency-in-runtime
  --format <fmt>      Output format: ndjson | human (default: human for --staged/--diff/--all,
                      ndjson otherwise)
  --json              Shorthand for --format ndjson
  --human             Shorthand for --format human
  --include-gap-ids   Include full gapIds array on each NDJSON line (omitted by default)
  --summary           Print summary counts to stderr (NDJSON mode)
  -h, --help          Show this help message
`;

type AnyKind = FindingKind | DependencyFindingKind;

const DEP_KINDS = new Set<string>([
  'unused-dependency',
  'unlisted-dependency',
  'unlisted-binary',
  'dev-dependency-in-runtime',
]);

const VALID_KINDS = new Set<string>([
  'unused-export',
  'test-only-export',
  'test-only-binding',
  'tooling-only-export',
  'unreferenced-module',
  ...DEP_KINDS,
]);

const VALID_DIFF = new Set<string>(['introduced', 'pre-existing']);
const VALID_FORMATS = new Set<string>(['ndjson', 'human']);

function isDepFinding(f: Finding | DependencyFinding): f is DependencyFinding {
  return DEP_KINDS.has(f.kind);
}

function parseEntrySpecs(specs: string[] | undefined): Pick<AnalyzeOptions, 'entries' | 'publicEntries'> {
  if (!specs || specs.length === 0) return {};
  const entries: Partial<Record<Context, string[]>> = {};
  let publicEntries: string[] | undefined;
  for (const raw of specs) {
    const colon = raw.indexOf(':');
    let role: Context | 'public' = 'runtime';
    let listStr = raw;
    if (colon !== -1) {
      const prefix = raw.slice(0, colon);
      if (prefix === 'runtime' || prefix === 'test' || prefix === 'tooling' || prefix === 'public') {
        role = prefix;
        listStr = raw.slice(colon + 1);
      }
    }
    const items = listStr === '' ? [] : listStr.split(',').map(s => s.trim()).filter(Boolean);
    if (role === 'public') {
      publicEntries = publicEntries ? [...publicEntries, ...items] : items;
    } else {
      entries[role] = entries[role] ? [...entries[role]!, ...items] : items;
    }
  }
  const result: Pick<AnalyzeOptions, 'entries' | 'publicEntries'> = {};
  if (Object.keys(entries).length > 0) result.entries = entries;
  if (publicEntries !== undefined) result.publicEntries = publicEntries;
  return result;
}

// Allow `--diff` to take an optional ref argument (e.g. `--diff` or `--diff origin/main`)
// while remaining backward-compatible with `--base <ref> --diff introduced`.
function preprocessArgv(argv: string[]): { args: string[]; diffMode: boolean; diffRef: string } {
  const hasBase = argv.some(a => a === '--base' || a.startsWith('--base='));
  const out: string[] = [];
  let diffMode = false;
  let diffRef = '';
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    if (!hasBase && arg === '--diff') {
      diffMode = true;
      const next = argv[i + 1];
      if (next && !next.startsWith('-')) {
        diffRef = next;
        i++;
      }
      continue;
    }
    if (!hasBase && arg.startsWith('--diff=')) {
      diffMode = true;
      diffRef = arg.slice('--diff='.length);
      continue;
    }
    out.push(arg);
  }
  return { args: out, diffMode, diffRef };
}

function lineNumberCache(rootDir: string) {
  const cache = new Map<string, number[] | null>();
  return (relFile: string, byteOffset: number | undefined): number | null => {
    if (byteOffset === undefined) return null;
    let offsets = cache.get(relFile);
    if (offsets === undefined) {
      try {
        const buf = readFileSync(join(rootDir, relFile));
        offsets = [0];
        for (let i = 0; i < buf.length; i++) {
          if (buf[i] === 0x0a) offsets.push(i + 1);
        }
      } catch {
        offsets = null;
      }
      cache.set(relFile, offsets);
    }
    if (!offsets) return null;
    let lo = 0;
    let hi = offsets.length - 1;
    while (lo <= hi) {
      const mid = (lo + hi) >> 1;
      if (offsets[mid] <= byteOffset) lo = mid + 1;
      else hi = mid - 1;
    }
    return hi + 1;
  };
}

function formatWitnessPath(report: Report, finding: Finding): string {
  if (finding.kind !== 'test-only-export' && finding.kind !== 'test-only-binding') return '';
  try {
    const exp = report.explain(finding.id);
    const witness = exp.witnesses.find(w => w.role === 'test') ?? exp.witnesses[0];
    if (witness?.path?.length) {
      const files: string[] = [];
      for (const step of witness.path) {
        if (files[files.length - 1] !== step.file) files.push(step.file);
      }
      return ` via ${files.join(' -> ')}`;
    }
  } catch {
    // Best-effort witness lookup.
  }
  return '';
}

function renderHuman(
  report: Report,
  findings: readonly (Finding | DependencyFinding)[],
  rootDir: string,
  hookMode: boolean,
): string {
  const lineOf = lineNumberCache(rootDir);
  const byWorkspace = new Map<string, (Finding | DependencyFinding)[]>();
  for (const f of findings) {
    const ws = f.workspace || '.';
    let list = byWorkspace.get(ws);
    if (!list) {
      list = [];
      byWorkspace.set(ws, list);
    }
    list.push(f);
  }
  const lines: string[] = [];
  for (const [ws, list] of [...byWorkspace.entries()].sort((a, b) => a[0].localeCompare(b[0]))) {
    lines.push(`[${ws}] (${list.length})`);
    for (const f of list) {
      const diffTag = f.diff ? `[${f.diff}] ` : '';
      const line = f.location ? lineOf(f.file, f.location.start) : null;
      const locStr = line ? `${f.file}:${line}` : f.file;
      if (isDepFinding(f)) {
        const target = f.dependency ?? f.binary ?? '';
        const extra = f.script
          ? ` (script ${f.script} in ${f.manifest})`
          : f.file !== f.manifest
            ? ` (manifest ${f.manifest})`
            : '';
        lines.push(`  ${diffTag}${f.kind}  ${target}  ${locStr}${extra}`);
      } else {
        const sym = f.exportName ?? f.bindingName ?? '<module>';
        const statusNote =
          f.status === 'conditional' && f.primaryGaps.length > 0
            ? ` [conditional: ${f.primaryGaps[0].code} - ${f.primaryGaps[0].message}]`
            : '';
        const witness = formatWitnessPath(report, f);
        lines.push(`  ${diffTag}${f.kind}  ${locStr}  ${sym}${witness}${statusNote}`);
      }
    }
  }
  if (report.staleConfig.length > 0) {
    lines.push(`[stale-config] (${report.staleConfig.length})`);
    for (const item of report.staleConfig) {
      lines.push(`  ${item.kind}  ${item.source}  ${item.message}`);
    }
  }
  const introduced = findings.filter(f => f.diff === 'introduced').length;
  const conditional = findings.filter(f => f.status === 'conditional').length;
  const header =
    hookMode && report.diff
      ? `smella: ${introduced} introduced (${findings.length} in diff against ${report.diff.base}, ${conditional} conditional, ${report.staleConfig.length} stale config)`
      : `smella: ${findings.length} findings (${conditional} conditional, ${report.coverage.gaps.length} gaps, ${report.staleConfig.length} stale config)`;
  lines.push(header);
  return `${lines.join('\n')}\n`;
}

async function main(): Promise<void> {
  const { args, diffMode, diffRef } = preprocessArgv(process.argv.slice(2));
  let values;
  try {
    ({ values } = parseArgs({
      args,
      options: {
        root: { type: 'string' },
        entries: { type: 'string', multiple: true },
        workspace: { type: 'string', multiple: true },
        base: { type: 'string' },
        diff: { type: 'string' },
        staged: { type: 'boolean', default: false },
        all: { type: 'boolean', default: false },
        kind: { type: 'string', multiple: true },
        dependencies: { type: 'boolean', default: false },
        format: { type: 'string' },
        json: { type: 'boolean', default: false },
        human: { type: 'boolean', default: false },
        'include-gap-ids': { type: 'boolean', default: false },
        summary: { type: 'boolean', default: false },
        help: { type: 'boolean', short: 'h', default: false },
      },
      allowPositionals: false,
    }));
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    process.stderr.write(`smella: ${msg}\n${USAGE}`);
    process.exitCode = 2;
    return;
  }

  if (values.help) {
    process.stdout.write(USAGE);
    return;
  }

  const kinds = new Set<AnyKind>();
  for (const raw of values.kind ?? []) {
    for (const part of raw
      .split(',')
      .map(s => s.trim())
      .filter(Boolean)) {
      if (!VALID_KINDS.has(part)) {
        process.stderr.write(`smella: invalid --kind ${JSON.stringify(part)}\n`);
        process.exitCode = 2;
        return;
      }
      kinds.add(part as AnyKind);
    }
  }

  if (values.diff !== undefined) {
    if (!values.base) {
      process.stderr.write('smella: --diff requires --base\n');
      process.exitCode = 2;
      return;
    }
    if (!VALID_DIFF.has(values.diff)) {
      process.stderr.write(`smella: invalid --diff ${JSON.stringify(values.diff)}\n`);
      process.exitCode = 2;
      return;
    }
  }

  if (values.format && !VALID_FORMATS.has(values.format)) {
    process.stderr.write(`smella: invalid --format ${JSON.stringify(values.format)}\n`);
    process.exitCode = 2;
    return;
  }

  const hookMode = Boolean(values.staged) || diffMode;
  const format =
    values.format ??
    (values.json ? 'ndjson' : values.human ? 'human' : hookMode || values.all ? 'human' : 'ndjson');

  const workspaces: string[] = [];
  for (const raw of values.workspace ?? []) {
    for (const part of raw
      .split(',')
      .map(s => s.trim())
      .filter(Boolean)) {
      workspaces.push(part);
    }
  }

  const includeDeps = Boolean(values.dependencies) || [...kinds].some(k => DEP_KINDS.has(k));
  const entryOpts = parseEntrySpecs(values.entries);
  const rootDir = resolve(values.root ?? process.cwd());
  const options: AnalyzeOptions = {
    ...(values.root ? { root: values.root } : {}),
    ...(values.base ? { base: values.base } : {}),
    ...(values.staged ? { staged: true } : {}),
    ...(diffMode ? { mergeBase: diffRef || true } : {}),
    ...(includeDeps ? { dependencies: true } : {}),
    ...entryOpts,
  };

  let report: Report | undefined;
  try {
    report = await analyze(options);
    const kindFilter = kinds.size === 1 ? [...kinds][0] : undefined;
    const diffFilter = (hookMode ? 'introduced' : values.diff) as DiffStatus | undefined;
    let findings = report.findings({
      kind: kindFilter,
      diff: diffFilter,
      ...(workspaces.length > 0 ? { workspace: workspaces } : {}),
    });
    if (kinds.size > 1) {
      findings = findings.filter(f => kinds.has(f.kind));
    }

    if (format === 'human') {
      process.stdout.write(renderHuman(report, findings, rootDir, hookMode));
    } else {
      const includeGapIds = Boolean(values['include-gap-ids']);
      for (const f of findings) {
        const record = isDepFinding(f)
          ? f
          : {
              id: f.id,
              kind: f.kind,
              ...(f.diff !== undefined ? { diff: f.diff } : {}),
              workspace: f.workspace,
              file: f.file,
              exportName: f.exportName,
              ...(f.bindingName !== undefined ? { bindingName: f.bindingName } : {}),
              location: f.location,
              usage: f.usage,
              status: f.status,
              gapCount: f.gapIds.length,
              primaryGaps: f.primaryGaps,
              ...(includeGapIds ? { gapIds: f.gapIds } : {}),
            };
        if (!process.stdout.write(`${JSON.stringify(record)}\n`)) {
          await new Promise<void>(resolveDrain => process.stdout.once('drain', resolveDrain));
        }
      }

      if (values.summary) {
        const conditional = findings.filter(f => f.status === 'conditional').length;
        const info = {
          findings: findings.length,
          conditional,
          gaps: report.coverage.gaps.length,
          staleConfig: report.staleConfig.length,
          ...(report.diff ? { diff: report.diff } : {}),
        };
        process.stderr.write(`${JSON.stringify(info)}\n`);
      }
    }

    if (hookMode && !values.all && findings.some(f => f.diff === 'introduced')) {
      process.exitCode = 1;
    }
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    process.stderr.write(`smella: ${msg}\n`);
    process.exitCode = 1;
  } finally {
    report?.dispose();
  }
}

await main();
