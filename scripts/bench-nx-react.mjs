// Exercise a React route, per-test attribution and an Nx caller without an entry.
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { analyze } from '../dist/index.mjs';
import { prepareNxReact } from '../test/support/nx-react.mjs';
import { stats } from './bench-stats.mjs';

const root = await mkdtemp(join(tmpdir(),'smella-nx-react-'));
const app = 'products/retail/apps/storefront/src';
const ui = 'shared/design/ui';
const test = ui+'/tests/preview.spec.tsx';

async function once() {
  const start = performance.now();
  const r = await analyze({root});
  try {
    const findings = r.findings();
    const preview = r.explain({file:ui+'/src/panels.tsx',exportName:'PreviewPanel'});
    const helper = r.explain({file:'shared/domain/pricing/src/pricing.ts',bindingName:'discount'});
    const duration = performance.now()-start;
    assert.equal(preview.kind,'test-only-export');
    assert.equal(preview.status,'complete');
    assert.deepEqual(preview.tests,[{file:test,facts:['value']}]);
    assert.equal(helper.kind,'test-only-binding');
    assert.ok(helper.unrootedWorkspaceReads.some(w=>w.file==='unshipped/apps/legacy/src/checkout.ts'));
    assert.equal(helper.status,'conditional');
    assert.ok(findings.some(f=>f.file===ui+'/src/Archive.tsx' && f.kind==='unreferenced-module'));
    assert.deepEqual(r.roots.filter(r=>r.role==='runtime').map(r=>r.file),[app+'/client.tsx',app+'/worker.ts']);
    assert.deepEqual(r.explain({file:ui+'/src/Catalog.tsx',exportName:'default'}).usage.value,['runtime']);
    return duration;
  } finally { r.dispose(); }
}

async function compareKnip() {
  // Give Knip identical source roots and path mappings so that a missing entry
  // cannot account for the difference in test provenance.
  await writeFile(join(root,'tsconfig.json'),JSON.stringify({extends:'./tsconfig.base.json'}));
  await writeFile(join(root,'knip.json'),JSON.stringify({
    entry:[app+'/client.tsx!',app+'/worker.ts!',test], project:['**/*.{ts,tsx}!'],
  }));
  const bin = resolve(process.env.KNIP_BIN);
  function issues(args) {
    const output = execFileSync(bin,['--reporter','json','--include','exports,files','--no-exit-code',...args],{cwd:root,encoding:'utf8'});
    const {issues} = JSON.parse(output);
    const files = issues.filter(i=>i.files.length).map(i=>i.file).sort();
    return {files, exports:issues.flatMap(i=>i.exports.map(e=>i.file+'#'+e.name)).sort()};
  }
  const version = execFileSync(bin,['--version'],{encoding:'utf8'}).trim();
  return {version, default:issues([]), production:issues(['--production'])};
}

try {
  await prepareNxReact(root);
  await once();
  const times = [];
  for (let i=0; i<20; i++) times.push(await once());
  const knip = process.env.KNIP_BIN ? await compareKnip() : undefined;
  console.log(JSON.stringify({fixture:'nx-react-v1',samples:times.length,totalMs:stats(times),knip}));
} finally { await rm(root,{recursive:true,force:true}); }
