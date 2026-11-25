import { createRequire } from 'node:module';
import { existsSync } from 'node:fs';
import { resolve } from 'node:path';

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

// Every subject in a file shares its uncertainty. Repeating thousands of project-wide
// gap IDs in the native JSON for every finding can exceed the JS string size limit.
function indexGaps(gaps) {
  const global = [];
  const local = new Map();
  for (const gap of gaps) {
    // The native graph qualifies individual subjects for unrooted workspace reads.
    if (gap.code === 'unrooted-workspace-reference') continue;
    if (gap.affectedFiles === null) global.push(gap.id);
    else for (const file of gap.affectedFiles) {
      if (!local.has(file)) local.set(file, []);
      local.get(file).push(gap.id);
    }
  }
  Object.freeze(global);
  const byCombination = new Map();
  for (const [file, ids] of local) {
    const key = ids.join(',');
    if (!byCombination.has(key)) byCombination.set(key, Object.freeze(global.concat(ids)));
    local.set(file, byCombination.get(key));
  }
  const withWorkspace = new Map();
  return record => {
    let ids = local.get(record.file) ?? global;
    if (record.workspaceGap) {
      if (!withWorkspace.has(ids)) withWorkspace.set(ids, Object.freeze([...ids, record.workspaceGap]));
      ids = withWorkspace.get(ids);
      delete record.workspaceGap;
    }
    record.gapIds = ids;
    record.status = record.gapIds.length ? 'conditional' : 'complete';
    return record;
  };
}

export function createConfig(config) { return config; }

export async function analyze({ root = process.cwd(), ...options } = {}) {
  native ??= load();
  let snapshot;
  try {
    snapshot = await native.analyzeNative(resolve(root), JSON.stringify(options));
  } catch (cause) {
    throw new AnalysisError('ANALYSIS_FAILED', cause.message, cause);
  }
  let metadata = freeze(JSON.parse(snapshot.metadata()));
  let withGaps = indexGaps(metadata.coverage.gaps);
  function live() {
    if (!snapshot) throw new AnalysisError('REPORT_DISPOSED', 'This analysis report has been disposed');
    return snapshot;
  }
  return Object.freeze({
    get roots() { live(); return metadata.roots; },
    get coverage() { live(); return metadata.coverage; },
    findings({ kind } = {}) { return JSON.parse(live().findings(kind)).map(withGaps); },
    explain(subject) {
      const current = live();
      try { return withGaps(JSON.parse(current.explain(JSON.stringify(subject)))); }
      catch (cause) { throw new AnalysisError('UNKNOWN_SUBJECT', cause.message, cause); }
    },
    dispose() {
      snapshot?.dispose();
      snapshot = metadata = withGaps = undefined;
    },
  });
}
