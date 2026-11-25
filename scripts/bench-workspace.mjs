import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { analyze } from '../index.js';
import { stats } from './bench-stats.mjs';

const packages = 32;
const tests = 64;
const root = await mkdtemp(join(tmpdir(),'smella-workspace-'));
const config = { root, entries: { runtime:['app.ts'], test:['tests/*.test.ts'], tooling:[] }, publicEntries:[] };
const groups = 'repo/division/team/services';
const pkg = i => `${groups}/units/p${i}`;
const sourceOf = i => `${pkg(i)}/api.ts`;
const target = { file:sourceOf(packages-1), bindingName:'privateLeaf' };
async function manifest(dir, fields) {
  await mkdir(join(root,dir),{recursive:true});
  await writeFile(join(root,dir,'package.json'), JSON.stringify(fields));
}

async function once() {
  const start = performance.now();
  const report = await analyze(config);
  try {
    const findings = report.findings();
    const ready = performance.now();
    const leaf = report.explain(target);
    const end = performance.now();
    assert.equal(report.coverage.files.length, packages+tests+1);
    assert.deepEqual(report.coverage.gaps.map(g=>g.code),['unrooted-workspace-reference']);
    assert.ok(findings.some(f => f.file === target.file && f.bindingName === target.bindingName));
    assert.equal(leaf.kind,'test-only-binding');
    assert.equal(leaf.status,'conditional');
    assert.equal(leaf.tests.length,tests);
    assert.ok(leaf.tests.every(t=>t.facts.join() === 'value'));
    const trace = leaf.unrootedWorkspaceReads.find(r=>r.file === sourceOf(0));
    assert.deepEqual([...new Set(trace.path.map(s=>s.file))],Array.from({length:packages},(_,i)=>sourceOf(i)));
    return { analysis:ready-start, evidence:end-ready, total:end-start };
  } finally { report.dispose(); }
}

try {
  await manifest('',{name:'fixture',private:true,workspaces:['repo']});
  let dir = '';
  for (const [index, segment] of groups.split('/').entries()) {
    dir = dir ? dir+'/'+segment : segment;
    await manifest(dir,{name:'group-'+index,workspaces:[groups.split('/')[index+1] ?? 'units/*']});
  }
  for (let i=0;i<packages;i++) {
    await manifest(pkg(i),{name:`@fixture/p${i}`,exports:i ? './api.ts' : './missing.js'});
    const source = i === packages-1 ? 'function privateLeaf() { return 1; } export function api() { return privateLeaf(); }'
      : `import {api as next} from "@fixture/p${i+1}"; export function api() { return next(); }`;
    await writeFile(join(root,sourceOf(i)),source);
  }
  await writeFile(join(root,'app.ts'),'void 0;');
  await mkdir(join(root,'tests'));
  for (let i=0;i<tests;i++) {
    await writeFile(join(root,'tests',i+'.test.ts'),`import {api} from "@fixture/p${packages-1}"; api();`);
  }
  await once();
  const observations = [];
  for (let i=0;i<20;i++) observations.push(await once());
  console.log(JSON.stringify({
    fixture:'workspace-v1', packages, testRoots:tests, files:packages+tests+1, samples:observations.length,
    analysisAndFindingsMs:stats(observations.map(s=>s.analysis)),
    explanationMs:stats(observations.map(s=>s.evidence)), totalMs:stats(observations.map(s=>s.total))
  }));
} finally { await rm(root,{recursive:true,force:true}); }
