#!/usr/bin/env node
import { parseArgs } from 'node:util';
import { analyze } from '../index.js';

const USAGE = `Usage: smella [options]

Options:
  --root <dir>        Repository root to analyze (default: cwd)
  --entries <spec>    Override entry points (repeatable). Format:
                        <role>:<path1>,<path2>  where role is runtime|test|tooling|public
                        <path>                  shorthand for runtime:<path>
                        <role>:                 empty list disables discovery for that role
  --base <ref>        Git ref to diff against (only findings in changed files,
                      marked "introduced" or "pre-existing")
  --diff <status>     Filter diff findings: introduced | pre-existing (requires --base)
  --kind <kind>       Filter finding kind (repeatable or comma-separated):
                        unused-export, test-only-export, test-only-binding,
                        tooling-only-export, unreferenced-module
  --include-gap-ids   Include full gapIds array on each NDJSON line (omitted by default)
  --summary           Print summary counts to stderr
  -h, --help          Show this help message
`;

const VALID_KINDS = new Set([
  'unused-export',
  'test-only-export',
  'test-only-binding',
  'tooling-only-export',
  'unreferenced-module',
]);

const VALID_DIFF = new Set(['introduced', 'pre-existing']);

function parseEntrySpecs(specs) {
  if (!specs || specs.length === 0) return {};
  const entries = {};
  let publicEntries;
  for (const raw of specs) {
    const colon = raw.indexOf(':');
    let role = 'runtime';
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
      entries[role] = entries[role] ? [...entries[role], ...items] : items;
    }
  }
  const result = {};
  if (Object.keys(entries).length > 0) result.entries = entries;
  if (publicEntries !== undefined) result.publicEntries = publicEntries;
  return result;
}

async function main() {
  let values;
  try {
    ({ values } = parseArgs({
      options: {
        root: { type: 'string' },
        entries: { type: 'string', multiple: true },
        base: { type: 'string' },
        diff: { type: 'string' },
        kind: { type: 'string', multiple: true },
        'include-gap-ids': { type: 'boolean', default: false },
        summary: { type: 'boolean', default: false },
        help: { type: 'boolean', short: 'h', default: false },
      },
      allowPositionals: false,
    }));
  } catch (err) {
    process.stderr.write(`smella: ${err.message}\n${USAGE}`);
    process.exitCode = 2;
    return;
  }

  if (values.help) {
    process.stdout.write(USAGE);
    return;
  }

  const kinds = new Set();
  for (const raw of values.kind ?? []) {
    for (const part of raw.split(',').map(s => s.trim()).filter(Boolean)) {
      if (!VALID_KINDS.has(part)) {
        process.stderr.write(`smella: invalid --kind ${JSON.stringify(part)}\n`);
        process.exitCode = 2;
        return;
      }
      kinds.add(part);
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

  const entryOpts = parseEntrySpecs(values.entries);
  const options = {
    ...(values.root ? { root: values.root } : {}),
    ...(values.base ? { base: values.base } : {}),
    ...entryOpts,
  };

  let report;
  try {
    report = await analyze(options);
    const kindFilter = kinds.size === 1 ? [...kinds][0] : undefined;
    let findings = report.findings({ kind: kindFilter, diff: values.diff });
    if (kinds.size > 1) {
      findings = findings.filter(f => kinds.has(f.kind));
    }

    const includeGapIds = Boolean(values['include-gap-ids']);
    // Stream newline-delimited JSON one finding per line to avoid V8 max string limits on large repos.
    for (const f of findings) {
      const record = {
        id: f.id,
        kind: f.kind,
        ...(f.diff !== undefined ? { diff: f.diff } : {}),
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
        await new Promise(resolve => process.stdout.once('drain', resolve));
      }
    }

    if (values.summary) {
      const conditional = findings.filter(f => f.status === 'conditional').length;
      const info = {
        findings: findings.length,
        conditional,
        gaps: report.coverage.gaps.length,
        ...(report.diff ? { diff: report.diff } : {}),
      };
      process.stderr.write(`${JSON.stringify(info)}\n`);
    }
  } catch (err) {
    process.stderr.write(`smella: ${err.message}\n`);
    process.exitCode = 1;
  } finally {
    report?.dispose();
  }
}

await main();
