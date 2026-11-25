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

export function createConfig(config) { return config; }

export async function analyze({ root = process.cwd(), ...options } = {}) {
  native ??= load();
  let snapshot;
  try {
    snapshot = await native.analyzeNative(resolve(root), JSON.stringify(options));
  } catch (cause) {
    throw new AnalysisError('ANALYSIS_FAILED', cause.message, cause);
  }
  const { roots, coverage } = freeze(JSON.parse(snapshot.metadata()));
  function live() {
    if (!snapshot) throw new AnalysisError('REPORT_DISPOSED', 'This analysis report has been disposed');
    return snapshot;
  }
  return Object.freeze({
    get roots() { live(); return roots; },
    get coverage() { live(); return coverage; },
    findings({ kind } = {}) { return JSON.parse(live().findings(kind)); },
    explain(subject) {
      const current = live();
      try { return JSON.parse(current.explain(JSON.stringify(subject))); }
      catch (cause) { throw new AnalysisError('UNKNOWN_SUBJECT', cause.message, cause); }
    },
    dispose() { snapshot?.dispose(); snapshot = undefined; },
  });
}
