// Run both Knip modes on the same implementation and test files as Smella.
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { analyze } from '../dist/index.mjs';

if (!process.env.KNIP_BIN) throw new Error('Set KNIP_BIN to the Knip executable');
const knip = resolve(process.env.KNIP_BIN);
const fixture = new URL('../test/fixtures/deep-workspace/', import.meta.url);
const root = await mkdtemp(join(tmpdir(), 'smella-test-usage-'));
try {
  // Use a flat project to isolate the classification difference from workspace resolution.
  await mkdir(join(root,'tests'));
  await writeFile(join(root,'package.json'), JSON.stringify({ name:'test-usage', private:true, type:'module' }));
  await writeFile(join(root,'impl.ts'), await readFile(new URL('products/group/units/three/src/impl.ts',fixture)));
  for (const file of ['app.ts','tests/alpha.test.ts','tests/beta.test.ts','tests/types.test.ts']) {
    const source = await readFile(new URL(file,fixture),'utf8');
    await writeFile(join(root,file),source.replace('@example/three',file === 'app.ts' ? './impl.js' : '../impl.js'));
  }
  await writeFile(join(root,'knip.json'),JSON.stringify({entry:['app.ts!','tests/*.test.ts'],project:['*.ts!','tests/*.test.ts']}));
  function knipExports(...args) {
    const output = execFileSync(knip,['--config','knip.json','--include','exports','--reporter','json','--no-exit-code',...args],{cwd:root,encoding:'utf8'});
    return JSON.parse(output).issues.flatMap(issue => issue.exports.map(e => e.name)).sort();
  }
  const standard = knipExports();
  const production = knipExports('--production');
  const r = await analyze({root,entries:{runtime:['app.ts'],test:['tests/*.test.ts'],tooling:[]},publicEntries:[]});
  try {
    assert.equal(r.coverage.status,'complete');
    const helper = r.explain({file:'impl.ts',bindingName:'makeSample'});
    assert.equal(helper.kind,'test-only-binding');
    assert.deepEqual(helper.tests.map(t => t.file),['tests/alpha.test.ts','tests/beta.test.ts']);
    assert.deepEqual(standard,['never']);
    assert.deepEqual(production,['never','reset','sample']);
    console.log(JSON.stringify({
      knip: execFileSync(knip,['--version'],{encoding:'utf8'}).trim(),
      defaultUnusedExports:standard, productionUnusedExports:production,
      smellaTestOnlyExports:r.findings({kind:'test-only-export'}).map(f=>f.exportName),
      smellaTestOnlyBindings:r.findings({kind:'test-only-binding'}).map(f=>f.bindingName),
      privateHelper:{name:helper.bindingName,tests:helper.tests},
    },null,2));
  } finally { r.dispose(); }
} finally { await rm(root,{recursive:true,force:true}); }
