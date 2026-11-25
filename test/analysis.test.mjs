import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { analyze, createConfig } from 'smella';

const contexts = fileURLToPath(new URL('./fixtures/contexts', import.meta.url));
const app = createConfig({ entries: { runtime: ['app.ts'], test: [], tooling: [] }, publicEntries: [] });

async function fixture(t, files) {
  const root = await mkdtemp(join(tmpdir(), 'smella-test-'));
  t.after(() => rm(root, { recursive: true, force: true }));
  for (const [file, text] of Object.entries(files)) {
    const path = join(root, file);
    await mkdir(dirname(path), { recursive: true });
    await writeFile(path, text);
  }
  return root;
}
async function inspect(t, root = contexts, config = {}) {
  const report = await analyze({ root, ...config });
  t.after(() => report.dispose());
  return report;
}
const subject = (r, file, exportName) => r.explain({ file, exportName });

test('AC01 direct test imports retain production source only for tests', async t => {
  const r = await inspect(t);
  assert.equal(r.coverage.status, 'complete');
  const f = r.findings({ kind: 'test-only-export' }).find(f => f.exportName === 'directLegacy');
  assert.deepEqual(f.usage.value, ['test']);
  assert.equal(r.explain(f.id).witnesses.find(w => w.fact === 'value').path[0].file, 'tests/service.test.ts');
});

test('AC02 named aliases, star barrels and helpers appear in the witness', async t => {
  const r = await inspect(t);
  const e = subject(r, 'src/service.ts', 'resetFixture');
  assert.equal(e.kind, 'test-only-export');
  const path = e.witnesses.find(w => w.fact === 'value').path;
  assert.deepEqual([...new Set(path.map(s => s.file))], ['tests/service.test.ts','tests/fixture.ts','src/index.ts','src/barrel.ts','src/service.ts']);
  assert.ok(path.some(step => step.name === 'clear'));
  assert.ok(path.some(step => step.name === 'reset'));
  assert.equal(subject(r,'src/service.ts','legacySave').kind, 'unused-export');
});

test('AC03 three bindings in one evaluated module have independent usage', async t => {
  const r = await inspect(t);
  assert.deepEqual(subject(r,'src/service.ts','save').usage.value, ['runtime']);
  assert.equal(subject(r,'src/service.ts','resetFixture').kind, 'test-only-export');
  assert.equal(subject(r,'src/service.ts','legacySave').kind, 'unused-export');
  assert.equal(subject(r,'src/service.ts','lintSchema').kind,'tooling-only-export');
  assert.equal(subject(r,'src/orphan.ts').kind,'unreferenced-module');
});

test('AC04 public API retains its implementation through a barrel', async t => {
  const r = await inspect(t);
  for (const [file, name] of [['sdk/client.ts','createClient'], ['sdk/internal.ts','internalHelper']]) {
    const e = subject(r,file,name);
    assert.equal(e.kind,null);
    assert.equal(e.usage.contract,true);
    assert.deepEqual(e.usage.value,['test']);
    assert.equal(e.witnesses.find(w => w.fact === 'contract').path[0].file,'sdk/index.ts');
  }
});

test('AC05 type-only imports and typeof do not create runtime evaluation', async t => {
  const r = await inspect(t);
  assert.deepEqual(subject(r,'src/types.ts','Options').usage, {value:[],type:['runtime'],contract:false,evaluation:['test']});
  const e = subject(r,'src/types.ts','factory');
  assert.equal(e.kind,null);
  assert.deepEqual(e.usage,{ value:['test'],type:['runtime'],contract:false,evaluation:['test'] });
});

test('AC06 side effects survive unread bindings, including ordinary and bare imports', async t => {
  const r = await inspect(t);
  const e = subject(r,'src/install.ts','registration');
  assert.equal(e.kind,'unused-export');
  assert.deepEqual(e.usage.evaluation,['runtime']);
  assert.deepEqual(e.witnesses.map(w => w.fact), ['evaluation']);
  assert.deepEqual(subject(r,'src/registry.ts','registerHandler').usage.value,['runtime']);
  assert.equal(subject(r,'src/ordinary.ts','unread').kind,'unused-export');
  assert.deepEqual(subject(r,'src/ordinary.ts').usage.evaluation,['runtime']);
});

test('AC07 reachable computed import qualifies negatives while literal import resolves', async t => {
  const source = 'const prefix = "🦦"; await import("./known.js"); await import(prefix);';
  const root = await fixture(t, { 'app.ts':source,'known.ts':'export const k = 1;', 'unknown.ts':'export const u = 2;' });
  const r = await inspect(t,root,app);
  assert.equal(r.coverage.status,'partial');
  const [gap] = r.coverage.gaps;
  assert.equal(gap.code,'computed-import');
  assert.equal(gap.affectedFiles,null);
  assert.equal(Buffer.from(source).subarray(gap.location.start,gap.location.end).toString(),'import(prefix)');
  assert.deepEqual(subject(r,'known.ts').usage.evaluation,['runtime']);
  const e = subject(r,'unknown.ts','u');
  assert.equal(e.kind,'unused-export');
  assert.deepEqual(e.usage.value,[]);
  assert.deepEqual(e.gapIds,[gap.id]);
  assert.equal(e.status,'conditional');
});

test('dead function bodies and shadowing cannot retain unrelated bindings', async t => {
  const root = await fixture(t, {
    'app.ts':'import { work } from "./lib"; work(1);',
    'lib.ts':'import { helper } from "./helper"; export function work(helper: number) { return helper; } export const dead = () => { helper(); import(path); };',
    'helper.ts':'export function helper() {}',
  });
  const r = await inspect(t,root,app);
  assert.equal(r.coverage.status,'complete');
  assert.equal(subject(r,'helper.ts','helper').kind,'unused-export');
});

test('namespace properties select individual exports, escapes qualify only possible dependencies', async t => {
  const root = await fixture(t,{
    'app.ts':'import * as api from "./lib"; api.used(); consume(api);',
    'lib.ts':'import { dep } from "./dep"; export function used() {} export function unknown() { return dep(); }',
    'dep.ts':'export function dep() {}','other.ts':'export function unrelated() {}',
  });
  const r = await inspect(t,root,app);
  assert.deepEqual(subject(r,'lib.ts','used').usage.value,['runtime']);
  const [gap] = r.coverage.gaps;
  assert.equal(gap.code,'namespace-escape');
  assert.deepEqual(gap.affectedFiles,['dep.ts','lib.ts']);
  assert.deepEqual(subject(r,'dep.ts','dep').gapIds,[gap.id]);
  assert.equal(subject(r,'other.ts','unrelated').status,'complete');
});

test('cycles converge with deterministic findings in reversed file creation order', async t => {
  const files = {'app.ts':'import { b } from "./a"; b();','a.ts':'export * from "./b"; export function a() {}','b.ts':'export * from "./a"; import { a } from "./a"; export function b() { a(); } export const never = 1;'};
  const first = await inspect(t,await fixture(t,files),app);
  const second = await inspect(t,await fixture(t,Object.fromEntries(Object.entries(files).reverse())),app);
  assert.deepEqual(first.coverage,second.coverage);
  assert.equal(first.coverage.status,'complete');
  assert.deepEqual(first.findings(),second.findings());
  assert.deepEqual(subject(first,'a.ts','a').usage.value,['runtime']);
  assert.equal(subject(first,'b.ts','never').kind,'unused-export');
});

test('parse errors, unknown imports and unmatched roots preserve positive evidence', async t => {
  const root = await fixture(t, {'app.ts':'import { ok } from "./lib"; import "./missing"; ok();','lib.ts':'export const ok = () => {}; export const maybe = 1;', 'bad.ts':'export const =;'});
  const r = await inspect(t,root,{ entries:{runtime:['app.ts','absent.ts']} });
  assert.deepEqual([...new Set(r.coverage.gaps.map(g => g.code))].sort(),['invalid-root','parse-error','unresolved-import']);
  assert.deepEqual(subject(r,'lib.ts','ok').usage.value,['runtime']);
  assert.equal(subject(r,'lib.ts','maybe').status,'conditional');
  await assert.rejects(analyze({root,entries:{runtime:[]}}),{code:'ANALYSIS_FAILED'});
  await assert.rejects(analyze({root:join(root,'absent')}),{code:'ANALYSIS_FAILED'});
  await assert.rejects(analyze({root,entries:{runtime:['bad.ts']}}),{code:'ANALYSIS_FAILED'});
});

test('discovery reports provenance, empty categories disable discovery, and config stays inert', async t => {
  const root = await fixture(t,{
    'package.json':JSON.stringify({bin:{app:'./app.ts'},exports:{'.':{import:'./lib.ts'}}}),
    'app.ts':'import "./lib";','lib.ts':'export const api = 1;', 'app.test.ts':'import {api} from "./lib"; void api;',
    'vite.config.ts':'throw new Error("must never execute");',
  });
  const r = await inspect(t,root);
  assert.deepEqual(r.roots.map(r => r.role),['test','runtime','public','tooling']);
  assert.ok(r.roots.every(r => r.origin === 'discovered' && r.input));
  const overridden = await inspect(t,root,{entries:{test:[], tooling:[]}, publicEntries:[]});
  assert.deepEqual(overridden.roots.map(r => r.role),['runtime']);
  assert.equal(subject(overridden,'lib.ts','api').kind,'unused-export');
});

test('root tsconfig paths and uninstalled workspace exports respect the ESM profile', async t => {
  const root = await fixture(t,{
    'package.json':JSON.stringify({workspaces:['packages/*']}),
    'tsconfig.json':JSON.stringify({compilerOptions:{baseUrl:'.',paths:{'@lib/*':['src/*']}}}),
    'app.ts':'import { x } from "@lib/utils"; import { y } from "@example/pkg/sub"; x(); y();',
    'src/utils.ts':'export function x() {}',
    'packages/pkg/package.json':JSON.stringify({name:'@example/pkg',exports:{'./sub':{require:'./wrong.ts',import:'./sub.ts'}}}),
    'packages/pkg/sub.ts':'export function y() {}','packages/pkg/wrong.ts':'export function y() {}',
  });
  const r = await inspect(t,root,app);
  assert.deepEqual(r.coverage.gaps,[]);
  assert.deepEqual(subject(r,'src/utils.ts','x').usage.value,['runtime']);
  assert.deepEqual(subject(r,'packages/pkg/sub.ts','y').usage.value,['runtime']);
  assert.equal(subject(r,'packages/pkg/wrong.ts','y').kind,'unused-export');
});

test('removing the last runtime reference or changing the public surface reclassifies an export', async t => {
  const root = await fixture(t,{'app.ts':'import { x } from "./lib"; x();','x.test.ts':'import { x as renamed } from "./lib"; renamed();','lib.ts':'export const x = 1;'});
  const config = {entries:{runtime:['app.ts'],test:['*.test.ts']}};
  const first = await inspect(t,root,config);
  assert.deepEqual(subject(first,'lib.ts','x').usage.value,['runtime','test']);
  await writeFile(join(root,'app.ts'),'import "./lib";');
  assert.equal(subject(await inspect(t,root,config),'lib.ts','x').kind,'test-only-export');
  const exposed = await inspect(t,root,{...config,publicEntries:['lib.ts']});
  assert.equal(subject(exposed,'lib.ts','x').usage.contract,true);
  assert.equal(subject(first,'lib.ts','x').kind,null);
});

test('snapshot queries are isolated; disposing twice preserves returned data', async () => {
  const r = await analyze({root:contexts});
  const records = r.findings();
  const roots = r.roots;
  const count = records.length;
  records.pop();
  assert.equal(r.findings().length,count);
  assert.throws(() => r.explain('missing'),{code:'UNKNOWN_SUBJECT'});
  r.dispose(); r.dispose();
  assert.ok(roots.length && records.length);
  for (const query of [()=>r.findings(),()=>r.explain(records[0].id),()=>r.roots,()=>r.coverage]) {
    assert.throws(query,{code:'REPORT_DISPOSED'});
  }
});

test('type-only chains through a barrel retain the defining module without evaluating it', async t => {
  const root = await fixture(t,{'app.ts':'import {type T} from "./index"; type Result = T;', 'index.ts':'export type {T} from "./types";', 'types.ts':'export interface T { id: number }'});
  const r = await inspect(t,root,app);
  assert.equal(r.coverage.status,'complete');
  for (const file of ['index.ts','types.ts']) {
    const e = subject(r,file);
    assert.equal(e.kind,null);
    assert.deepEqual(e.usage.evaluation,[]);
    assert.deepEqual(e.usage.type,['runtime']);
  }
  assert.deepEqual(subject(r,'types.ts','T').usage.type,['runtime']);
});

test('star diamonds share a binding and conflicting star declarations are excluded', async t => {
  const files = {
    'app.ts':'import { x } from "./index"; x();', 'index.ts':'export * from "./a"; export * from "./b";',
    'a.ts':'export {x} from "./lib"; export const y = 1;', 'b.ts':'export * from "./lib"; export const y = 2;',
    'lib.ts':'export function x() {}',
  };
  const r = await inspect(t,await fixture(t,files),app);
  assert.deepEqual(r.coverage.gaps,[]);
  assert.deepEqual(subject(r,'lib.ts','x').usage.value,['runtime']);
  assert.throws(()=>subject(r,'index.ts','y'),{code:'UNKNOWN_SUBJECT'});
  assert.equal(subject(r,'a.ts','y').kind,'unused-export');
  files['app.ts']='import { y } from "./index"; void y;';
  const ambiguous = await inspect(t,await fixture(t,files),app);
  assert.equal(ambiguous.coverage.gaps[0].code,'missing-export');
  assert.deepEqual(subject(ambiguous,'a.ts','y').usage.value,[]);
});

test('JSX component references and ambient declarations retain source identities', async t => {
  const root = await fixture(t,{
    'app.tsx':'import Component from "./view"; import type { Props } from "./types"; const props: Props = {}; void <Component {...props} />;',
    'view.jsx':'export default function View() { return <div/>; }',
    'types.d.ts':'export interface Props { x?: string }',
  });
  const r = await inspect(t,root,{entries:{runtime:['app.tsx']},publicEntries:[]});
  assert.deepEqual(r.coverage.gaps,[]);
  assert.deepEqual(subject(r,'view.jsx','default').usage.value,['runtime']);
  assert.deepEqual(subject(r,'types.d.ts','Props').usage.type,['runtime']);
});

test('discovery preserves declared public types and diagnoses ambiguous application scripts', async t => {
  const root = await fixture(t,{
    'package.json':JSON.stringify({scripts:{start:'node run.js'},exports:{'.':{types:'./api.d.ts',import:'./api.js'}}}),
    'api.js':'export const x = 1;', 'api.d.ts':'export declare const x: number;', 'run.js':'import { x } from "./api.js"; void x;',
  });
  const discovered = await inspect(t,root);
  assert.deepEqual(discovered.roots.map(r => r.file),['api.d.ts','api.js']);
  assert.equal(discovered.coverage.gaps[0].code,'ambiguous-entry');
  const configured = await inspect(t,root,{entries:{runtime:['run.js']}});
  assert.equal(configured.coverage.status,'complete');
  assert.equal(subject(configured,'api.d.ts','x').usage.contract,true);
});

test('large partial reports share immutable gap lists across subjects', async t => {
  const unresolved = Array.from({length:80},(_,i)=>`import "./missing${i}";`).join('\n');
  const candidates = Array.from({length:80},(_,i)=>`export const x${i} = 1;`).join('\n');
  const r = await inspect(t,await fixture(t,{'app.ts':unresolved,'a.ts':candidates,'b.ts':candidates}),app);
  const findings = r.findings();
  const ids = findings[0].gapIds;
  assert.equal(ids.length,80);
  assert.equal(findings.length,162);
  assert.ok(findings.every(f => f.gapIds === ids && f.status === 'conditional'));
  assert.ok(Object.isFrozen(ids));
});
