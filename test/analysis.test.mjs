import test from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdtemp, mkdir, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { analyze, createConfig } from 'smella';
import { prepareNxReact } from './support/nx-react.mjs';

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

test('discovery preserves declared public types, parses node scripts, and diagnoses ambiguous scripts', async t => {
  const root = await fixture(t,{
    'package.json':JSON.stringify({scripts:{start:'node run.js'},exports:{'.':{types:'./api.d.ts',import:'./api.js'}}}),
    'api.js':'export const x = 1;', 'api.d.ts':'export declare const x: number;', 'run.js':'import { x } from "./api.js"; void x;',
  });
  const discovered = await inspect(t,root);
  assert.deepEqual(discovered.roots.map(r => r.file),['api.d.ts','api.js','run.js']);
  assert.equal(discovered.coverage.status,'complete');
  assert.equal(subject(discovered,'api.d.ts','x').usage.contract,true);
  const opaque = await fixture(t,{
    'package.json':JSON.stringify({scripts:{start:'custom-launcher --prod'},exports:{'.':{types:'./api.d.ts',import:'./api.js'}}}),
    'api.js':'export const x = 1;', 'api.d.ts':'export declare const x: number;', 'run.js':'import { x } from "./api.js"; void x;',
  });
  const amb = await inspect(t,opaque);
  assert.deepEqual(amb.roots.map(r => r.file),['api.d.ts','api.js']);
  assert.equal(amb.coverage.gaps[0].code,'ambiguous-entry');
  const configured = await inspect(t,opaque,{entries:{runtime:['run.js']}});
  assert.equal(configured.coverage.status,'complete');
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

const deep = fileURLToPath(new URL('./fixtures/deep-workspace', import.meta.url));
const units = 'products/group/units/';
const impl = units + 'three/src/impl.ts';
const cli = units + 'one/src/cli.ts';

test('unrooted workspace reads survive nested membership, local aliases and two package boundaries', async t => {
  const r = await inspect(t,deep);
  const reset = subject(r,impl,'reset');
  assert.equal(reset.kind,'test-only-export');
  assert.deepEqual(reset.usage.value,['test']);
  assert.equal(reset.status,'conditional');
  assert.deepEqual(r.coverage.gaps.map(g => g.code),['unrooted-workspace-reference']);
  assert.deepEqual(reset.gapIds,r.coverage.gaps.map(g => g.id));
  const read = reset.unrootedWorkspaceReads.find(r => r.file === cli);
  assert.equal(read.fact,'value');
  assert.deepEqual([...new Set(read.path.map(step => step.file))],[
    cli, units+'two/src/barrel.ts', units+'two/src/relay.ts', units+'three/src/index.ts',impl
  ]);
  assert.equal(read.path[0].name,'boot');
  assert.ok(read.path.some(s => s.name === 'forward'));
  assert.equal(subject(r,impl,'never').status,'complete');
  assert.deepEqual(reset.tests,[
    {file:'tests/alpha.test.ts',facts:['value']},
    {file:'tests/beta.test.ts',facts:['value']},
    {file:'tests/types.test.ts',facts:['type']}
  ]);
});

test('private test-only implementation is distinguished from public, runtime and unrooted consumers', async t => {
  const base = await inspect(t,deep);
  const finding = base.findings({kind:'test-only-binding'}).find(f => f.bindingName === 'makeSample');
  assert.equal(finding.file,impl);
  assert.equal(finding.exportName,null);
  assert.equal(finding.status,'complete');
  const local = base.explain(finding.id);
  assert.deepEqual(local,base.explain({file:impl,bindingName:'makeSample'}));
  assert.deepEqual(local.unrootedWorkspaceReads,[]);
  assert.deepEqual(local.tests.map(t => t.file),['tests/alpha.test.ts','tests/beta.test.ts']);
  assert.equal(base.explain({file:impl,bindingName:'normalize'}).status,'conditional');
  const rooted = await inspect(t,deep,{entries:{runtime:['app.ts',cli]}});
  assert.deepEqual(rooted.coverage.gaps,[]);
  const normalized = rooted.explain({file:impl,bindingName:'normalize'});
  assert.equal(normalized.kind,null);
  assert.deepEqual(normalized.usage.value,['runtime','test']);
  assert.deepEqual(normalized.unrootedWorkspaceReads,[]);
  const published = await inspect(t,deep,{publicEntries:[units+'three/src/index.ts']});
  assert.equal(published.explain({file:impl,bindingName:'makeSample'}).usage.contract,true);
  assert.deepEqual(published.findings({kind:'test-only-binding'}).filter(f=>f.file===impl),[]);
});

test('test attribution respects unused imports, type edges, cycles and per-file roles', async t => {
  const root = await fixture(t,{
    'a.test.ts':'import { work } from "./b"; work();',
    'b.test.ts':'import "./a.test"; import type { work } from "./b"; type W = typeof work;',
    'c.test.ts':'import {work} from "./b";',
    'b.ts':'export {work} from "./lib";',
    'lib.ts':'export function work() { return 1; }',
  });
  const r = await inspect(t,root,{entries:{test:['*.test.ts','a.test.ts']},publicEntries:[]});
  assert.deepEqual(subject(r,'lib.ts','work').tests,[
    {file:'a.test.ts',facts:['value']},{file:'b.test.ts',facts:['value','type']}
  ]);
  assert.deepEqual(subject(r,'lib.ts').tests.find(x=>x.file==='c.test.ts').facts,['evaluation']);
});

test('unrooted side-effect imports qualify module evaluation and initialization dependencies only', async t => {
  const root = await fixture(t,{
    'package.json':'{"workspaces":["pkgs/*"]}','app.ts':'void 0;',
    'pkgs/a/package.json':'{"name":"a"}','pkgs/a/start.ts':'import "b";',
    'pkgs/b/package.json':'{"name":"b","exports":"./init.ts"}',
    'pkgs/b/init.ts':'function register() {} register(); export const spare = 1;',
  });
  const r = await inspect(t,root,app);
  assert.equal(subject(r,'pkgs/b/init.ts').unrootedWorkspaceReads[0].fact,'evaluation');
  assert.equal(subject(r,'pkgs/b/init.ts').status,'conditional');
  const helper = r.explain({file:'pkgs/b/init.ts',bindingName:'register'});
  assert.deepEqual(helper.usage.value,[]);
  assert.equal(helper.unrootedWorkspaceReads[0].file,'pkgs/a/start.ts');
  assert.equal(subject(r,'pkgs/b/init.ts','spare').status,'complete');
});

test('parallel analysis resolves identical aliases in neighboring workspaces independently', async t => {
  const files = {'package.json':'{"workspaces":["units/*"]}', 'app.ts':'import {left} from "left"; left();'};
  for (const [name, role] of [['left','runtime'],['right','test']]) {
    const dir = 'units/'+name+'/';
    files[dir+'tsconfig.json']='{"compilerOptions":{"paths":{"~local":["./local.ts"]}}}';
    files[dir+'package.json']=JSON.stringify({name,exports:'./api.ts'});
    files[dir+'api.ts']=`import {local} from "~local"; export const ${name} = () => local();`;
    files[dir+'local.ts']=`export function local() {return "${role}";}`;
  }
  files['a.test.ts']='import {right} from "right"; right();';
  for(let i=0;i<32;i++) files['extras/'+i+'.ts']='export const filler = 0;';
  const root = await fixture(t,files);
  const reports = await Promise.all(Array.from({length:4},()=>inspect(t,root,{entries:{runtime:['app.ts']},publicEntries:[]})));
  for (const r of reports) {
    assert.deepEqual(r.coverage.gaps,[]);
    assert.deepEqual(subject(r,'units/left/local.ts','local').usage.value,['runtime']);
    assert.deepEqual(subject(r,'units/right/local.ts','local').usage.value,['test']);
    assert.deepEqual(r.findings(),reports[0].findings());
  }
});

test('pnpm workspace packages include deep members and honor excluded package globs', async t => {
  const root = await fixture(t,{
    'pnpm-workspace.yaml':"packages:\n  - 'tree/**'\n  - '!tree/ignored/**'\ncatalog:\n  tsx: '^4'\n",
    'app.ts':'import {x} from "lib"; x();',
    'tree/a/b/lib/package.json':'{"name":"lib","exports":"./src.ts"}',
    'tree/a/b/lib/src.ts':'export const x = () => 1;',
    'tree/ignored/duplicate/package.json':'{"name":"lib","exports":"./src.ts"}',
    'tree/ignored/duplicate/src.ts':'export const x = () => 2;',
  });
  const r = await inspect(t,root,app);
  assert.equal(r.coverage.status,'complete');
  assert.deepEqual(subject(r,'tree/a/b/lib/src.ts','x').usage.value,['runtime']);
  assert.equal(subject(r,'tree/ignored/duplicate/src.ts','x').kind,'unused-export');
});

test('missing workspace entry keeps positive evidence and an unresolved target stays conditional', async t => {
  const root = await fixture(t,{
    'package.json':'{"workspaces":["pkgs/*"]}', 'app.ts':'import {ok} from "./lib"; ok();',
    'lib.ts':'export const ok = () => 1;',
    'pkgs/a/package.json':'{"name":"a","main":"./missing.js"}',
    'pkgs/a/use.ts':'import {x} from "b/stale"; x();',
    'pkgs/b/package.json':'{"name":"b","exports":{"./stale":"./dist/missing.js"}}',
    'pkgs/b/stale.ts':'export const x = 1;',
  });
  const r = await inspect(t,root,{entries:{runtime:['app.ts']}});
  assert.ok(r.coverage.gaps.some(g=>g.code==='invalid-root'));
  assert.ok(r.coverage.gaps.some(g=>g.code==='unresolved-import' && g.file==='pkgs/a/use.ts'));
  assert.deepEqual(subject(r,'lib.ts','ok').usage.value,['runtime']);
  assert.equal(subject(r,'pkgs/b/stale.ts','x').status,'conditional');
  assert.deepEqual(subject(r,'pkgs/b/stale.ts','x').usage.value,[]);
});

test('unbounded loads behind unrooted cross-workspace reads qualify the affected workspace without tainting unrelated packages', async t => {
  const root = await fixture(t,{
    'package.json':'{"workspaces":["p/*"]}', 'app.ts':'void 0;', 'independent.ts':'export const maybe = 1;',
    'p/a/package.json':'{"name":"a"}', 'p/a/orphan.ts':'import {run} from "b"; run();',
    'p/b/package.json':'{"name":"b","exports":"./api.ts"}', 'p/b/api.ts':'export function run() { return import(path); }',
    'p/b/sibling.ts':'export const near = 1;',
  });
  const r = await inspect(t,root,app);
  const gap = r.coverage.gaps.find(g=>g.code==='computed-import');
  assert.ok(gap);
  assert.ok(gap.affectedFiles.includes('p/b/api.ts'));
  assert.ok(gap.affectedFiles.includes('p/b/sibling.ts'));
  assert.ok(subject(r,'p/b/sibling.ts','near').gapIds.includes(gap.id));
  assert.equal(subject(r,'p/b/sibling.ts','near').status,'conditional');
  assert.equal(subject(r,'independent.ts','maybe').status,'complete');
  assert.deepEqual(subject(r,'p/b/api.ts','run').usage.value,[]);
});

const store = 'products/retail/apps/storefront';
const ui = 'shared/design/ui';
const pricing = 'shared/domain/pricing/src/pricing.ts';
const reactPackage = {
  'node_modules/react/package.json': JSON.stringify({name:'react',main:'index.js'}),
  'node_modules/react/index.js': 'throw new Error("analysis executed project code");',
};

test('Nx default build and TSX lazy routes retain production, test and unrooted callers independently', async t => {
  const root = await fixture(t,{});
  await prepareNxReact(root);
  const r = await inspect(t,root);
  assert.deepEqual(r.roots.filter(x=>x.role === 'runtime').map(x=>[x.file,x.input]), [
    [store+'/src/client.tsx',store+'/project.json#targets.build.configurations.production.main'],
    [store+'/src/worker.ts',store+'/project.json#targets.build.configurations.production.additionalEntryPoints[0]'],
  ]);
  assert.deepEqual(r.coverage.gaps.map(g=>g.code),['unrooted-workspace-reference']);
  const catalog = subject(r,ui+'/src/Catalog.tsx','default');
  assert.deepEqual(catalog.usage.value,['runtime']);
  assert.equal(catalog.witnesses.find(w=>w.fact === 'value').path[0].file,store+'/src/client.tsx');
  const preview = subject(r,ui+'/src/panels.tsx','PreviewPanel');
  assert.equal(preview.kind,'test-only-export');
  assert.equal(preview.status,'complete');
  assert.deepEqual(preview.tests,[{file:ui+'/tests/preview.spec.tsx',facts:['value']}]);
  assert.deepEqual(subject(r,ui+'/src/panels.tsx','OtherPanel').usage.value,[]);
  assert.deepEqual(subject(r,ui+'/src/Catalog.tsx','OtherCatalog').usage.value,[]);
  assert.equal(subject(r,ui+'/src/Archive.tsx').kind,'unreferenced-module');
  assert.equal(subject(r,store+'/src/dev.tsx').kind,'unreferenced-module');
  const helper = r.explain({file:pricing,bindingName:'discount'});
  assert.equal(helper.kind,'test-only-binding');
  assert.equal(helper.status,'conditional');
  assert.ok(helper.unrootedWorkspaceReads.some(read=>read.file==='unshipped/apps/legacy/src/checkout.ts'));
  const recovered = await inspect(t,root,{entries:{runtime:[store+'/src/client.tsx','unshipped/apps/legacy/src/checkout.ts']}});
  assert.deepEqual(recovered.explain({file:pricing,bindingName:'discount'}).usage.value,['runtime','test']);
});

test('Nx merges package targets, JSONC, executor defaults and a referenced nondefault build profile', async t => {
  const root = await fixture(t,{
    'nx.json':'{ /* shared defaults */ "targetDefaults": { "@nx/esbuild:esbuild": { "options": { "main":"{projectRoot}/default.ts" } } }, }',
    'package.json':JSON.stringify({workspaces:['zones/**']}),
    'zones/app/package.json':JSON.stringify({name:'app',nx:{projectType:'application',targets:{serve:{executor:'@nx/js:node',options:{buildTarget:'engine:compile:browser'}}}}}),
    'zones/app/project.json':JSON.stringify({targets:{serve:{options:{watch:true}}}}),
    'engine/project.json':JSON.stringify({name:'engine',projectType:'library',targets:{compile:{executor:'@nx/esbuild:esbuild',configurations:{browser:{additionalEntryPoints:['{projectRoot}/[browser].tsx']}}}}}),
    'engine/default.ts':'export const used = 1;', 'engine/[browser].tsx':'void 0;',
    'zones/app/test.spec.ts':'void 0;'
  });
  const r = await inspect(t,root);
  assert.deepEqual(r.roots.filter(x=>x.role==='runtime').map(x=>[x.file,x.input]), [
    ['engine/[browser].tsx','engine/project.json#targets.compile.configurations.browser.additionalEntryPoints[0]'],
    ['engine/default.ts','nx.json#targetDefaults.@nx/esbuild:esbuild.options.main'],
  ]);
  assert.equal(r.coverage.status,'complete');
  const disabled = await inspect(t,root,{entries:{runtime:[]}});
  assert.deepEqual(disabled.roots.map(x=>x.role),['test']);
});

test('inferred Nx targets, command entries and missing sources keep findings conditional until configured', async t => {
  const root = await fixture(t, {
    'nx.json':JSON.stringify({plugins:['@nx/vite/plugin']}),
    'apps/web/project.json':JSON.stringify({name:'web',projectType:'application',targets:{
      build:{command:'exit 1'},serve:{executor:'@nx/js:node',options:{buildTarget:'missing:build'}},
      bundle:{executor:'@nx/webpack:webpack',options:{main:'dist/missing.js',webpackConfig:'{projectRoot}/tools/bundler.ts'}},
    }}),
    'apps/web/tools/bundler.ts':'throw new Error("should never run");',
    'apps/web/src/boot.ts':'export const app = 1;',
    'apps/web/src/app.spec.ts':'void 0;', '.nx/cache/ghost.spec.ts':'void 0;',
  });
  const r = await inspect(t,root);
  assert.ok(r.coverage.gaps.some(g=>g.message.includes('nx.json uses plugins')));
  assert.ok(r.coverage.gaps.some(g=>g.message.includes('unresolved or overridden buildTarget')));
  assert.ok(r.coverage.gaps.some(g=>g.code==='invalid-root' && g.message.includes('dist/missing.js')));
  assert.equal(subject(r,'apps/web/src/boot.ts','app').status,'conditional');
  assert.ok(r.roots.some(x=>x.file==='apps/web/tools/bundler.ts' && x.role==='tooling'));
  assert.ok(!r.coverage.files.includes('.nx/cache/ghost.spec.ts'));
  const explicit = await inspect(t,root,{entries:{runtime:['apps/web/src/boot.ts']}});
  assert.equal(explicit.coverage.status,'complete');
});

test('React wrappers defer callbacks by binding identity and preserve uncertainty for opaque loaders', async t => {
  const root = await fixture(t,{
    ...reactPackage,
    'app.tsx': 'import { Main } from "./views"; void <Main />;',
    'ui.test.tsx':'import { TestOnly } from "./views"; void <TestOnly />;',
    'views.tsx':`import * as R from "react";
import { lazy } from "react";
import { TestChild, MainChild } from "./children";
export const Main = R.memo(function MainView() { return <MainChild />; });
export const TestOnly = R.memo(R.forwardRef((p,ref) => <TestChild ref={ref} />));
export const Unused = lazy(() => import("./unread"));
function other(lazy) { return lazy(() => import("./opaque")); }
other(handler);
const Adapter = R.lazy(() => import("./adapted").then(m => choose(m)));
void <Adapter />;`,
    'children.tsx':'export function MainChild() {} export function TestChild() {}',
    'unread.tsx':'export default () => <div />;',
    'opaque.ts':'export const local = 1;', 'adapted.ts':'export default 1;',
  });
  const r = await inspect(t,root,{entries:{runtime:['app.tsx']}});
  assert.deepEqual(subject(r,'children.tsx','MainChild').usage.value,['runtime']);
  assert.deepEqual(subject(r,'children.tsx','TestChild').usage.value,['test']);
  assert.equal(subject(r,'unread.tsx').kind,'unreferenced-module');
  assert.deepEqual(r.coverage.gaps.map(g=>[g.code,g.affectedFiles]).sort(),[
    ['namespace-escape',['adapted.ts']],['namespace-escape',['opaque.ts']],
  ]);
});

test('React lazy supports a destructured alias and keeps public component dependencies in the contract', async t => {
  const root = await fixture(t,{
    ...reactPackage,
    'index.ts':'export { default as Dialog } from "./Dialog";',
    'Dialog.tsx':'import { lazy } from "react"; export default lazy(() => import("./views").then(({ DialogView: View }) => ({ default: View })));',
    'views.tsx':'export function DialogView() { return <dialog />; } export function Sibling() { return null; }',
    'app.ts':'void 0;',
  });
  const r = await inspect(t,root,{...app,publicEntries:['index.ts']});
  assert.equal(r.coverage.status,'complete');
  const view = subject(r,'views.tsx','DialogView');
  assert.equal(view.usage.contract,true);
  assert.deepEqual(view.usage.value,[]);
  assert.equal(subject(r,'views.tsx','Sibling').kind,'unused-export');
});

test('stylesheet imports resolve across CSS modules, literal import.meta.glob and standard asset/worker queries resolve cleanly, while dynamic glob and unknown queries stay visible', async t => {
  const root = await fixture(t,{
    'app.ts': 'import styles from "./a.module.css"; console.log(styles.title); import.meta.glob("./routes/*.tsx"); import.meta.glob(dynPattern); import "./missing.css"; import text from "./template.ts?raw"; import url from "./template.ts?url"; import InlineCss from "./a.module.css?inline"; import MyWorker from "./bg.ts?worker"; import custom from "./template.ts?custom"; const addon = require("./native.node"); console.log(text, url, InlineCss, MyWorker, custom, addon);',
    'a.module.css':'.title { color: red }', 'template.ts':'export default 42;', 'native.node':'',
    'bg.ts':'import { work } from "./bg-lib"; work();',
    'bg-lib.ts':'export function work() {} export function unused() {}',
    'routes/landing.tsx':'export function Landing() { return <div />; }',
  });
  const r = await inspect(t,root,app);
  assert.deepEqual(r.coverage.gaps.map(g=>g.code),['bundler-glob','unresolved-import','unresolved-import']);
  assert.ok(r.coverage.gaps.some(g=>g.message.includes('loader query')));
  const candidate = subject(r,'routes/landing.tsx','Landing');
  assert.deepEqual(candidate.usage.value,['runtime']);
  assert.equal(candidate.kind,null);
  assert.deepEqual(subject(r,'template.ts','default').usage.value,[]);
  assert.deepEqual(subject(r,'bg-lib.ts','work').usage.value,['runtime']);
  assert.equal(subject(r,'bg-lib.ts','unused').kind,'unused-export');
});

test('Nx respects a relocated root, literal paths and command overrides of inherited executors', async t => {
  const root = await fixture(t,{
    'nx.json':JSON.stringify({targetDefaults:{build:{executor:'@nx/esbuild:esbuild',options:{main:'wrong.ts'}}}}),
    'metadata/web/project.json':JSON.stringify({root:'nested/site',name:'web',projectType:'application',targets:{
      build:{command:'generate-entry'}, bundle:{executor:'@nx/esbuild:esbuild',options:{main:'{projectRoot}/src/../boot.ts'}},
    }}),
    'nested/site/boot.ts':'void 0;', 'wrong.ts':'void 0;',
  });
  const r = await inspect(t,root);
  assert.deepEqual(r.roots.map(r=>r.file),['nested/site/boot.ts']);
  assert.deepEqual(r.coverage.gaps.map(g=>g.code),['ambiguous-entry']);
});

test('a .js JSX entry retains a lazy component from the React workspace package through a JSX render', async t => {
  const root = await fixture(t,{
    'package.json':JSON.stringify({workspaces:['packages/*']}),
    'packages/react/package.json':JSON.stringify({name:'react',exports:{'.':'./index.js'}}),
    'packages/react/index.js':'export function lazy(load) { return load; }',
    'site/index.js':'import App from "./App"; mount(<App />);',
    'site/App.js':'import { lazy } from "react"; const Feed = lazy(() => import("./Feed")); export default function App() { return <><Feed /></>; }',
    'site/Feed.js':'export default function Feed() { return <section />; } export function Sibling() { return null; }',
  });
  const r = await inspect(t,root,{entries:{runtime:['site/index.js']},publicEntries:[]});
  assert.equal(r.coverage.status,'complete');
  assert.deepEqual(subject(r,'site/Feed.js','default').usage.value,['runtime']);
  assert.equal(subject(r,'site/Feed.js','Sibling').kind,'unused-export');
});

test('a local alias named react keeps ordinary callback semantics', async t => {
  const root = await fixture(t,{
    'tsconfig.json':JSON.stringify({compilerOptions:{paths:{react:['./other.ts']}}}),
    'other.ts':'export function lazy(cb) { return cb(); }',
    'app.ts':'import {lazy} from "react"; const unused = lazy(() => import("./Lib"));',
    'Lib.ts':'export default 1;',
  });
  const r = await inspect(t,root,app);
  assert.ok(r.coverage.gaps.some(g=>g.code==='namespace-escape'));
  assert.deepEqual(subject(r,'Lib.ts','default').usage.value,[]);
});

test('a manifest CLI retains a registered tool callback and its internal reads', async t => {
  const pkg = 'workspaces/mcp';
  const bin = pkg + '/bin/serve.mjs';
  const library = pkg + '/src/tools.mjs';
  const testFile = pkg + '/tests/tools.test.mjs';
  const registration = "registerTool('lookup', () => lookup());";
  const source = [
    "import { lookup } from '../src/tools.mjs';",
    "const tools = new Map();",
    "function registerTool(name, handler) { tools.set(name, handler); }",
    registration,
    "console.log(tools.get('lookup')?.() ?? 'unregistered');",
  ].join('\n');
  const root = await fixture(t, {
    'package.json': JSON.stringify({ workspaces: ['workspaces/*'] }),
    [pkg + '/package.json']: JSON.stringify({ name: 'mcp', bin: { mcp: 'bin/serve.mjs' } }),
    [bin]: source,
    [library]: [
      "export const result = 'registered';",
      "export function lookup() { return result; }",
      "export function orphan() {}",
    ].join('\n'),
    [testFile]: "import { lookup } from '../src/tools.mjs'; lookup();",
  });
  // Run the synthetic CLI independently of the analyzer to check both sides of the edit.
  const run = () => execFileSync(process.execPath, [join(root, bin)], { encoding: 'utf8' }).trim();
  assert.equal(run(), 'registered');
  const before = await inspect(t, root);
  assert.equal(before.coverage.status, 'complete');
  assert.ok(before.roots.some(r => r.file === bin && r.role === 'runtime' && r.input === pkg + '/package.json#bin'));
  for (const name of ['lookup', 'result']) {
    const e = subject(before, library, name);
    assert.equal(e.kind, null);
    assert.deepEqual(e.usage.value, ['runtime', 'test']);
    assert.deepEqual(e.tests.map(t => t.file), [testFile]);
    assert.equal(e.witnesses.find(w => w.role === 'runtime' && w.fact === 'value').path[0].file, bin);
  }
  assert.equal(subject(before, library, 'orphan').kind, 'unused-export');

  await writeFile(join(root, bin), source.replace(registration, ''));
  assert.equal(run(), 'unregistered');
  const after = await inspect(t, root);
  for (const name of ['lookup', 'result']) {
    assert.equal(subject(after, library, name).kind, 'test-only-export');
    assert.deepEqual(subject(after, library, name).usage.value, ['test']);
  }
  assert.equal(subject(after, library, 'orphan').kind, 'unused-export');
});

test('a missing built CLI import qualifies source findings until its source entry is supplied', async t => {
  const file = 'src/server.ts';
  const root = await fixture(t, {
    'package.json': JSON.stringify({ bin: './bin/serve.mjs' }),
    'bin/serve.mjs': "import { serve } from '../dist/server.js'; serve();",
    'bootstrap.ts': "import { serve } from './src/server.js'; serve();",
    [file]: "import { handle } from './handler.js'; export function serve() { return handle(); }",
    'src/handler.ts': "export function handle() { return 42; } export const unused = 0;",
    'server.test.ts': "import { handle } from './src/handler.js'; handle();",
  });
  const incomplete = await inspect(t, root);
  const gap = incomplete.coverage.gaps.find(g => g.code === 'unresolved-import' && g.file === 'bin/serve.mjs');
  assert.ok(gap);
  const candidate = subject(incomplete, 'src/handler.ts', 'handle');
  assert.equal(candidate.kind, 'test-only-export');
  assert.equal(candidate.status, 'conditional');
  assert.ok(candidate.gapIds.includes(gap.id));

  const recovered = await inspect(t, root, { entries: { runtime: ['bootstrap.ts'] } });
  assert.equal(subject(recovered, 'src/handler.ts', 'handle').kind, null);
  const witness = subject(recovered, 'src/handler.ts', 'handle').witnesses.find(w => w.role === 'runtime' && w.fact === 'value');
  assert.deepEqual([...new Set(witness.path.map(step => step.file))], ['bootstrap.ts', file, 'src/handler.ts']);
  assert.deepEqual(subject(recovered, 'src/handler.ts', 'handle').usage.value, ['runtime', 'test']);
  assert.equal(subject(recovered, 'src/handler.ts', 'unused').kind, 'unused-export');
});

test('a workspace manifest subpath protects its public binding through a barrel without a local caller', async t => {
  const pkg = 'packages/sdk';
  const barrel = pkg + '/api.mjs';
  const file = pkg + '/impl.mjs';
  const manifest = pkg + '/package.json';
  const root = await fixture(t, {
    'package.json': JSON.stringify({ workspaces: ['packages/*'] }),
    [manifest]: JSON.stringify({ name: 'sdk', exports: { './api': './api.mjs' } }),
    [barrel]: "export { request as getTool } from './impl.mjs';",
    [file]: "export function request() {} export function internalOnly() {}",
    'app.test.mjs': "import { request } from './packages/sdk/impl.mjs'; request();",
  });
  const before = await inspect(t, root);
  const e = subject(before, file, 'request');
  assert.equal(e.kind, null);
  assert.equal(e.usage.contract, true);
  assert.deepEqual(e.usage.value, ['test']);
  assert.equal(e.witnesses.find(w => w.fact === 'contract').path[0].file, barrel);
  assert.equal(subject(before, file, 'internalOnly').kind, 'unused-export');
  assert.ok(!before.findings({ kind: 'unreferenced-module' }).some(f => f.file === barrel));

  await writeFile(join(root, manifest), JSON.stringify({ name: 'sdk' }));
  const after = await inspect(t, root);
  assert.equal(subject(after, file, 'request').kind, 'test-only-export');
  assert.equal(subject(after, barrel).kind, 'unreferenced-module');
});

const cliPath = fileURLToPath(new URL('../dist/cli.mjs', import.meta.url));

test('workspace build outputs map back to source through tsconfig, declaration maps and source fields', async t => {
  const root = await fixture(t, {
    'package.json': JSON.stringify({ workspaces: ['packages/*'] }),
    'app.ts': [
      'import { verifyArchive, ArchiveError } from "@org/registry";',
      'import type { Config } from "@org/registry/lib/config";',
      'import { runRuntime } from "@org/runtime";',
      'import { featureFlag } from "@org/features/registry";',
      'import { fromSourceField } from "@org/srcfield";',
      'import { fromDeclMap } from "@org/dmap";',
      'verifyArchive(); new ArchiveError(); runRuntime(); featureFlag(); fromSourceField(); fromDeclMap();',
      'type AppCfg = Config; void (null as unknown as AppCfg);',
    ].join('\n'),
    'packages/registry/package.json': JSON.stringify({ name: '@org/registry', main: './lib/index.js', types: './lib/index.d.ts' }),
    'packages/registry/tsconfig.json': JSON.stringify({ compilerOptions: { outDir: './lib', rootDir: './src' } }),
    'packages/registry/src/index.ts': 'export { verifyArchive, ArchiveError } from "./archive";',
    'packages/registry/src/archive.ts': 'export function verifyArchive() {} export class ArchiveError {} export function dead() {}',
    'packages/registry/src/config.ts': 'export interface Config { id: string }',
    'packages/runtime/package.json': JSON.stringify({
      name: '@org/runtime',
      exports: { '.': { types: './dist/index.d.ts', import: './dist/index.js' } },
    }),
    'packages/runtime/tsconfig.json': JSON.stringify({ compilerOptions: { outDir: './dist', rootDir: './lib' } }),
    'packages/runtime/lib/index.ts': 'export function runRuntime() {}',
    'packages/features/package.json': JSON.stringify({
      name: '@org/features',
      exports: { './registry': { import: { types: './lib/esm/registry.d.ts', default: './lib/esm/registry.js' } } },
    }),
    'packages/features/tsconfig.json': JSON.stringify({ compilerOptions: { rootDir: 'src' } }),
    'packages/features/tsconfig.esm.json': JSON.stringify({ extends: './tsconfig.json', compilerOptions: { outDir: 'lib/esm' } }),
    'packages/features/src/registry.ts': 'export function featureFlag() {}',
    'packages/srcfield/package.json': JSON.stringify({ name: '@org/srcfield', main: './out/index.js', source: './custom/entry.ts' }),
    'packages/srcfield/custom/entry.ts': 'export function fromSourceField() {}',
    'packages/dmap/package.json': JSON.stringify({ name: '@org/dmap', main: './compiled/index.js', types: './compiled/index.d.ts' }),
    'packages/dmap/compiled/index.d.ts': 'export declare function fromDeclMap(): void;',
    'packages/dmap/compiled/index.d.ts.map': JSON.stringify({ version: 3, sources: ['../orig/impl.ts'] }),
    'packages/dmap/orig/impl.ts': 'export function fromDeclMap() {}',
  });

  const r = await inspect(t, root, app);
  assert.deepEqual(r.coverage.gaps, []);
  assert.deepEqual(subject(r, 'packages/registry/src/archive.ts', 'verifyArchive').usage.value, ['runtime']);
  assert.deepEqual(subject(r, 'packages/registry/src/archive.ts', 'ArchiveError').usage.value, ['runtime']);
  assert.equal(subject(r, 'packages/registry/src/archive.ts', 'dead').kind, 'unused-export');
  assert.deepEqual(subject(r, 'packages/registry/src/config.ts', 'Config').usage.type, ['runtime']);
  assert.deepEqual(subject(r, 'packages/runtime/lib/index.ts', 'runRuntime').usage.value, ['runtime']);
  assert.deepEqual(subject(r, 'packages/features/src/registry.ts', 'featureFlag').usage.value, ['runtime']);
  assert.deepEqual(subject(r, 'packages/srcfield/custom/entry.ts', 'fromSourceField').usage.value, ['runtime']);
  assert.deepEqual(subject(r, 'packages/dmap/orig/impl.ts', 'fromDeclMap').usage.value, ['runtime']);
});

test('namespace imports and dynamic imports resolve static members, destructuring, qualified types and spyOn', async t => {
  const root = await fixture(t, {
    'app.ts': [
      'import * as loader from "./loader";',
      'export async function run(x: loader.LoadResult) {',
      '  const a = loader.sharedErrors;',
      '  const b = loader["computedKey"];',
      '  const { destructured } = loader;',
      '  const { dynRead } = await import("./dyn");',
      '  const mod = await import("./dyn2");',
      '  return [x, a, b, destructured, dynRead(), mod.second()];',
      '}',
      'run({} as any);',
    ].join('\n'),
    'app.test.ts': 'import * as loader from "./loader"; jest.spyOn(loader, "spied");',
    'loader.ts': [
      'export interface LoadResult { ok: boolean }',
      'export const sharedErrors = [];',
      'export const computedKey = 1;',
      'export const destructured = 2;',
      'export function spied() {}',
      'export const unused = 3;',
    ].join('\n'),
    'dyn.ts': 'export function dynRead() { return 1; } export const dynUnused = 2;',
    'dyn2.ts': 'export function second() { return 2; } export const secondUnused = 3;',
  });

  const r = await inspect(t, root, { entries: { runtime: ['app.ts'], test: ['app.test.ts'], tooling: [] }, publicEntries: [] });
  assert.deepEqual(r.coverage.gaps, []);
  assert.deepEqual(subject(r, 'loader.ts', 'LoadResult').usage.type, ['runtime']);
  assert.deepEqual(subject(r, 'loader.ts', 'sharedErrors').usage.value, ['runtime']);
  assert.deepEqual(subject(r, 'loader.ts', 'computedKey').usage.value, ['runtime']);
  assert.deepEqual(subject(r, 'loader.ts', 'destructured').usage.value, ['runtime']);
  assert.equal(subject(r, 'loader.ts', 'spied').kind, 'test-only-export');
  assert.equal(subject(r, 'loader.ts', 'unused').kind, 'unused-export');
  assert.deepEqual(subject(r, 'dyn.ts', 'dynRead').usage.value, ['runtime']);
  assert.equal(subject(r, 'dyn.ts', 'dynUnused').kind, 'unused-export');
  assert.deepEqual(subject(r, 'dyn2.ts', 'second').usage.value, ['runtime']);
  assert.equal(subject(r, 'dyn2.ts', 'secondUnused').kind, 'unused-export');
});

test('CommonJS require, module.exports, exports properties and TS import types retain exact bindings', async t => {
  const root = await fixture(t, {
    'app.cjs': [
      'const { add } = require("./math.cjs");',
      'const greet = require("./greet.cjs");',
      'const util = require("./util.cjs");',
      'if (require.main === module) console.log(add(1, 2), greet(), util.fmt());',
    ].join('\n'),
    'math.cjs': 'function add(a, b) { return a + b; } function unused() {} module.exports = { add, unused };',
    'greet.cjs': 'module.exports = function greet() { return "hi"; };',
    'util.cjs': 'exports.fmt = () => "ok"; exports.dead = () => "no";',
    'types-user.ts': 'type Val = import("./types").MyVal; void (null as unknown as Val);',
    'types.ts': 'export interface MyVal { v: number } export interface UnusedVal { u: number }',
  });

  const r = await inspect(t, root, { entries: { runtime: ['app.cjs', 'types-user.ts'], test: [], tooling: [] }, publicEntries: [] });
  assert.deepEqual(r.coverage.gaps, []);
  assert.deepEqual(subject(r, 'math.cjs', 'add').usage.value, ['runtime']);
  assert.equal(subject(r, 'math.cjs', 'unused').kind, 'unused-export');
  assert.deepEqual(subject(r, 'greet.cjs', 'default').usage.value, ['runtime']);
  assert.deepEqual(subject(r, 'util.cjs', 'fmt').usage.value, ['runtime']);
  assert.equal(subject(r, 'util.cjs', 'dead').kind, 'unused-export');
  assert.deepEqual(subject(r, 'types.ts', 'MyVal').usage.type, ['runtime']);
  assert.equal(subject(r, 'types.ts', 'UnusedVal').kind, 'unused-export');
});

test('git base diff scopes findings to changed files and marks introduced vs pre-existing', async t => {
  const root = await fixture(t, {
    'app.ts': 'import { used } from "./lib"; used();',
    'lib.ts': 'export function used() {} export function oldDead() {}',
    'untouched.ts': 'export function untouchedDead() {}',
  });
  const git = (...args) => execFileSync('git', args, { cwd: root, encoding: 'utf8' });
  git('init', '-q');
  git('config', 'user.email', 'test@example.com');
  git('config', 'user.name', 'Test');
  git('add', '.');
  git('commit', '-q', '-m', 'base');

  await writeFile(join(root, 'lib.ts'), 'export function used() {} export function oldDead() {} export function newDead() {}');
  await writeFile(join(root, 'newfile.ts'), 'export const orphanExport = 1;');

  const r = await inspect(t, root, { ...app, base: 'HEAD' });
  assert.equal(r.diff.base, 'HEAD');
  assert.deepEqual(r.diff.changedFiles, ['lib.ts', 'newfile.ts']);
  assert.equal(r.diff.preExisting, 1);
  assert.equal(r.diff.introduced, 3);
  const all = r.findings();
  assert.ok(!all.some(f => f.file === 'untouched.ts'));
  assert.equal(all.find(f => f.exportName === 'oldDead')?.diff, 'pre-existing');
  assert.equal(all.find(f => f.exportName === 'newDead')?.diff, 'introduced');
  assert.deepEqual(
    r.findings({ diff: 'introduced' }).map(f => `${f.file}:${f.exportName ?? '<mod>'}`).sort(),
    ['lib.ts:newDead', 'newfile.ts:<mod>', 'newfile.ts:orphanExport'],
  );
  assert.equal(subject(r, 'lib.ts', 'newDead').diff, 'introduced');
  assert.equal(subject(r, 'lib.ts', 'oldDead').diff, 'pre-existing');
});

test('manifest plugin hosts, runtime scripts, bundler exposes, child processes and Storybook stories are discovered', async t => {
  const root = await fixture(t, {
    'package.json': JSON.stringify({
      scripts: { start: 'sh -c \'exec node -r ts-node/register server.ts "$@"\' --' },
      'plugin-host': { nodes: { demo: { file: 'nodes/demo.js' } }, plugins: { plug: 'nodes/plug.js' } },
      'custom-host': { nodes: { extra: { file: 'nodes/extra.js' } } },
    }),
    'server.ts': [
      'import { fork } from "node:child_process";',
      'import path from "node:path";',
      'const child = require.resolve("./child");',
      'fork(child);',
      'new Worker(new URL("./worker.ts", import.meta.url));',
      'fork(path.join(__dirname, "child2.ts"));',
    ].join('\n'),
    'child.ts': 'import { childHelper } from "./helpers"; childHelper();',
    'child2.ts': 'import { child2Helper } from "./helpers"; child2Helper();',
    'worker.ts': 'import { workerHelper } from "./helpers"; workerHelper();',
    'helpers.ts': 'export function childHelper() {} export function child2Helper() {} export function workerHelper() {} export function deadHelper() {}',
    'nodes/demo.js': 'const { nodeWork } = require("./node-lib.js"); module.exports = function(host) { nodeWork(host); };',
    'nodes/demo.html': '<script src="demo-ui.js"></script>',
    'nodes/demo-ui.js': 'console.log("ui");',
    'nodes/plug.js': 'module.exports = function(host) { void host; };',
    'nodes/extra.js': 'module.exports = function(host) { void host; };',
    'nodes/node-lib.js': 'exports.nodeWork = () => {};',
    'vite.config.ts': 'export default { plugins: [federation({ exposes: { "./Widget": "./src/widget.tsx" } })] };',
    'src/widget.tsx': 'export function Widget() { return null; }',
    '.storybook/main.ts': 'export default { stories: ["../src/**/*.stories.@(ts|tsx)"] };',
    'src/Widget.stories.tsx': 'import { storyHelper } from "./story-helper"; export default { title: "W" }; export const Primary = () => storyHelper();',
    'src/story-helper.ts': 'export function storyHelper() { return 1; }',
  });

  const r = await inspect(t, root);
  assert.deepEqual(r.coverage.gaps, []);
  assert.deepEqual(subject(r, 'nodes/node-lib.js', 'nodeWork').usage.value, ['runtime']);
  assert.deepEqual(subject(r, 'nodes/demo-ui.js').usage.evaluation, ['runtime']);
  assert.deepEqual(subject(r, 'src/widget.tsx', 'Widget').usage.value, ['runtime']);
  assert.deepEqual(subject(r, 'helpers.ts', 'childHelper').usage.value, ['runtime']);
  assert.deepEqual(subject(r, 'helpers.ts', 'child2Helper').usage.value, ['runtime']);
  assert.deepEqual(subject(r, 'helpers.ts', 'workerHelper').usage.value, ['runtime']);
  assert.equal(subject(r, 'helpers.ts', 'deadHelper').kind, 'unused-export');
  assert.equal(subject(r, 'src/Widget.stories.tsx', 'Primary').kind, null);
  assert.equal(subject(r, 'src/Widget.stories.tsx', 'default').kind, null);
  assert.equal(subject(r, 'src/story-helper.ts', 'storyHelper').kind, 'tooling-only-export');
});

test('static import.meta.glob, inline primaryGaps and NDJSON CLI stream compact findings', async t => {
  const root = await fixture(t, {
    'app.ts': [
      'const parts = import.meta.glob(["./parts/*.ts", "!./parts/*.ignored.ts"], { import: "default" });',
      'const docs = import.meta.glob("./docs/*.md", { query: "?raw" });',
      'void parts; void docs;',
    ].join('\n'),
    'parts/a.ts': 'export default function a() {} export const extra = 1;',
    'parts/b.ignored.ts': 'export default function b() {}',
    'orphan.ts': 'export const unused = 42;',
  });

  const r = await inspect(t, root, app);
  assert.deepEqual(r.coverage.gaps, []);
  assert.deepEqual(subject(r, 'parts/a.ts', 'default').usage.value, ['runtime']);
  assert.equal(subject(r, 'parts/a.ts', 'extra').kind, 'unused-export');
  assert.equal(subject(r, 'parts/b.ignored.ts').kind, 'unreferenced-module');
  assert.deepEqual(subject(r, 'orphan.ts', 'unused').primaryGaps, []);

  await writeFile(join(root, 'app.ts'), 'import(dynSpec); import "./parts/a";');
  const stdout = execFileSync(process.execPath, [
    cliPath,
    '--root', root,
    '--entries', 'runtime:app.ts',
    '--entries', 'test:',
    '--entries', 'tooling:',
    '--entries', 'public:',
    '--kind', 'unused-export',
  ], { encoding: 'utf8' });
  const lines = stdout.trim().split('\n').map(l => JSON.parse(l));
  assert.ok(lines.length >= 2);
  for (const item of lines) {
    assert.equal(item.kind, 'unused-export');
    assert.equal(item.status, 'conditional');
    assert.equal(item.gapIds, undefined);
    assert.equal(item.gapCount, 1);
    assert.equal(item.primaryGaps.length, 1);
    assert.equal(item.primaryGaps[0].code, 'computed-import');
  }
});

test('type-only @types fallback, tsdown outDir, tsconfig compiled-type paths, default exports and companion declarations resolve cleanly', async t => {
  const root = await fixture(t, {
    'package.json': JSON.stringify({ workspaces: ['packages/*'] }),
    'node_modules/@types/bare-lib/package.json': JSON.stringify({ name: '@types/bare-lib', types: 'index.d.ts' }),
    'node_modules/@types/bare-lib/index.d.ts': 'export interface BareReq { id: string }',
    'node_modules/ext-tool/package.json': JSON.stringify({ name: 'ext-tool', types: 'index.d.ts' }),
    'node_modules/ext-tool/index.d.ts': 'export interface ToolCfg { enabled: boolean }',
    'tsconfig.json': JSON.stringify({
      compilerOptions: {
        baseUrl: '.',
        paths: {
          'host/@/*': ['packages/host/dist/@mf-types/compiled-types/ui/*'],
        },
      },
    }),
    'ext-tool.ts': 'import type { ToolCfg } from "ext-tool"; export const cfg: ToolCfg = { enabled: true };',
    'app.ts': [
      'import type { BareReq } from "bare-lib";',
      'import { Button } from "host/@/button";',
      'import { cfg } from "./ext-tool";',
      'import type { HelperOpts } from "./packages/helper/mod.mjs";',
      'import { runHelper } from "@org/default-only";',
      'const backend = require("./packages/bundled/dist/index.cjs");',
      'const req: BareReq = { id: "1" };',
      'const opts: HelperOpts = { retries: 1 };',
      'backend.start(); Button(); runHelper(); console.log(req, opts, cfg);',
    ].join('\n'),
    'packages/host/tsconfig.mf.json': JSON.stringify({ compilerOptions: { outDir: './@mf-types', rootDir: './src' } }),
    'packages/host/src/ui/button.ts': 'export function Button() {} export function unusedButton() {}',
    'packages/bundled/tsdown.config.ts': 'export default defineConfig({ entry: ["src/index.ts"], outDir: "dist", format: "cjs" });',
    'packages/bundled/src/index.ts': 'export function start() {} export function unusedStart() {}',
    'packages/helper/mod.mjs': 'export function helperVal() { return 1; }',
    'packages/helper/mod.d.ts': 'export interface HelperOpts { retries: number } export interface UnusedOpts { x: number }',
    'packages/default-only/package.json': JSON.stringify({
      name: '@org/default-only',
      exports: { '.': { default: './src/index.ts' } },
    }),
    'packages/default-only/src/index.ts': 'export function runHelper() {}',
  });

  const r = await inspect(t, root, app);
  assert.deepEqual(r.coverage.gaps, []);
  assert.deepEqual(subject(r, 'packages/host/src/ui/button.ts', 'Button').usage.value, ['runtime']);
  assert.equal(subject(r, 'packages/host/src/ui/button.ts', 'unusedButton').kind, 'unused-export');
  assert.deepEqual(subject(r, 'packages/bundled/src/index.ts', 'start').usage.value, ['runtime']);
  assert.equal(subject(r, 'packages/bundled/src/index.ts', 'unusedStart').kind, 'unused-export');
  assert.deepEqual(subject(r, 'packages/helper/mod.d.ts', 'HelperOpts').usage.type, ['runtime']);
  assert.equal(subject(r, 'packages/helper/mod.d.ts', 'UnusedOpts').kind, 'unused-export');
  assert.deepEqual(subject(r, 'packages/default-only/src/index.ts', 'runHelper').usage.value, ['runtime']);
});

test('export type star from .d.ts, mixed value/type companion .d.ts, declaration merging and ambient declarations resolve cleanly', async t => {
  const root = await fixture(t, {
    'package.json': JSON.stringify({ workspaces: ['packages/*'], bin: { app: './app.ts' } }),
    'app.ts': [
      'import type { SharedApi } from "./lib/shared";',
      'import { runTool, type ToolReceipt } from "./tool.mjs";',
      'import { Merged } from "./merged";',
      'const x: SharedApi = { id: "1" };',
      'const y: ToolReceipt = { ok: true };',
      'console.log(runTool(), x, y, Merged);',
    ].join('\n'),
    'reexport.ts': 'export type * from "./types/shared";',
    'types/shared.d.ts': 'export type { SharedApi, UnusedShared } from "@org/types";',
    'tsconfig.types.json': JSON.stringify({ compilerOptions: { outDir: './types', rootDir: './lib' } }),
    'lib/shared.ts': 'export type * from "../types/shared";',
    'packages/types/package.json': JSON.stringify({ name: '@org/types', exports: { '.': './index.ts' } }),
    'packages/types/index.ts': 'export interface SharedApi { id: string } export interface GlobalUser { name: string } export interface UnusedShared { x: number }',
    'tool.mjs': 'export function runTool() { return 1; }',
    'tool.d.ts': 'export interface ToolReceipt { ok: boolean } export interface UnusedReceipt { no: boolean }',
    'merged.ts': 'export interface Merged { value: number } export const Merged = { value: 1 };',
    'ambient.d.ts': 'import type { GlobalUser } from "@org/types"; declare global { namespace Req { interface Context { user?: GlobalUser; } } } export {};',
  });

  const r = await inspect(t, root, { publicEntries: [] });
  assert.deepEqual(r.coverage.gaps, []);
  assert.deepEqual(subject(r, 'packages/types/index.ts', 'SharedApi').usage.type, ['runtime']);
  assert.deepEqual(subject(r, 'packages/types/index.ts', 'GlobalUser').usage.type, ['runtime']);
  assert.deepEqual(subject(r, 'packages/types/index.ts', 'GlobalUser').usage.evaluation, []);
  assert.equal(subject(r, 'packages/types/index.ts', 'UnusedShared').kind, 'unused-export');
  assert.deepEqual(subject(r, 'tool.mjs', 'runTool').usage.value, ['runtime']);
  assert.deepEqual(subject(r, 'tool.d.ts', 'ToolReceipt').usage.type, ['runtime']);
  assert.equal(subject(r, 'tool.d.ts', 'UnusedReceipt').kind, 'unused-export');
  assert.deepEqual(subject(r, 'merged.ts', 'Merged').usage.value, ['runtime']);
});

test('typeof import, Promise.all dynamic imports, Object.defineProperty exports, template-prefix globs and dynamicImports config resolve cleanly', async t => {
  const root = await fixture(t, {
    'app.ts': [
      'type WholeApi = typeof import("./api");',
      'type ReqRet = ReturnType<typeof require>;',
      'const cjs = require("./defined.cjs");',
      'export async function run(name: string, dyn: string) {',
      '  const [{ alpha }, modB] = await Promise.all([import("./modA"), import("./modB")]);',
      '  const { cmd } = await import(new URL("./cmd.mjs", import.meta.url).href);',
      '  const plugin = require(`./plugins/${name}`);',
      '  const { cfgTarget } = await import(dyn);',
      '  return [alpha(), modB.beta(), cmd(), cjs.definedFn(), plugin, cfgTarget(), null as unknown as WholeApi, null as unknown as ReqRet];',
      '}',
      'run("x", "y");',
    ].join('\n'),
    'api.ts': 'export interface ApiShape { ok: boolean }',
    'modA.ts': 'export function alpha() {} export function unusedA() {}',
    'modB.ts': 'export function beta() {} export function unusedB() {}',
    'cmd.mjs': 'export function cmd() {} export function unusedCmd() {}',
    'defined.cjs': 'function definedFn() { return 1; } function deadCjs() {} Object.defineProperty(exports, "definedFn", { enumerable: true, get: () => definedFn }); Object.defineProperty(exports, "deadCjs", { value: deadCjs });',
    'plugins/one.ts': 'export function initOne() {}',
    'plugins/two.ts': 'export function initTwo() {}',
    'configured.ts': 'export function cfgTarget() {}',
  });

  const r = await inspect(t, root, { ...app, dynamicImports: { 'app.ts': ['./configured'] } });
  assert.deepEqual(r.coverage.gaps, []);
  assert.deepEqual(subject(r, 'api.ts', 'ApiShape').usage.type, ['runtime']);
  assert.deepEqual(subject(r, 'api.ts').usage.evaluation, []);
  assert.deepEqual(subject(r, 'modA.ts', 'alpha').usage.value, ['runtime']);
  assert.equal(subject(r, 'modA.ts', 'unusedA').kind, 'unused-export');
  assert.deepEqual(subject(r, 'modB.ts', 'beta').usage.value, ['runtime']);
  assert.equal(subject(r, 'modB.ts', 'unusedB').kind, 'unused-export');
  assert.deepEqual(subject(r, 'cmd.mjs', 'cmd').usage.value, ['runtime']);
  assert.equal(subject(r, 'cmd.mjs', 'unusedCmd').kind, 'unused-export');
  assert.deepEqual(subject(r, 'defined.cjs', 'definedFn').usage.value, ['runtime']);
  assert.equal(subject(r, 'defined.cjs', 'deadCjs').kind, 'unused-export');
  assert.deepEqual(subject(r, 'plugins/one.ts').usage.evaluation, ['runtime']);
  assert.deepEqual(subject(r, 'plugins/two.ts').usage.evaluation, ['runtime']);
  assert.deepEqual(subject(r, 'configured.ts').usage.evaluation, ['runtime']);
});

test('extensionless shebang bins, nodemon and framework start scripts, and spawn execPath children are discovered cleanly', async t => {
  const root = await fixture(t, {
    'package.json': JSON.stringify({
      workspaces: ['packages/*'],
      bin: {
        'js-tool': './bin/js-tool',
        'sh-tool': './bin/sh-tool',
        'py-tool': './bin/py-tool',
      },
    }),
    'bin/js-tool': '#!/usr/bin/env node\nconst { spawn } = require("node:child_process");\nconst path = require("node:path");\nconst { cliWork } = require("../src/cli-lib.cjs");\ncliWork();\nspawn(process.execPath, ["-r", "ts-node/register", path.join(__dirname, "../src/worker.ts")]);\n',
    'bin/sh-tool': '#!/bin/sh\necho ok\n',
    'bin/py-tool': '#!/usr/bin/env python3\nprint("ok")\n',
    'src/cli-lib.cjs': 'exports.cliWork = () => 1; exports.unusedCli = () => 2;',
    'src/worker.ts': 'import { runJob } from "./job"; runJob();',
    'src/job.ts': 'export function runJob() {} export function deadJob() {}',
    'packages/dev-srv/package.json': JSON.stringify({ name: '@org/dev-srv', scripts: { start: 'nodemon -w src --exec "tsx src/server.ts"' } }),
    'packages/dev-srv/src/server.ts': 'import { serve } from "./serve"; serve();',
    'packages/dev-srv/src/serve.ts': 'export function serve() {}',
    'packages/docs/package.json': JSON.stringify({ name: '@org/docs', scripts: { start: 'next start' } }),
    'packages/docs/app/page.tsx': 'import { DocHero } from "./hero"; export default function Page() { return DocHero(); }',
    'packages/docs/app/hero.ts': 'export function DocHero() {}',
  });

  const r = await inspect(t, root, { publicEntries: [] });
  assert.deepEqual(r.coverage.gaps, []);
  assert.ok(r.roots.some(x => x.file === 'bin/js-tool' && x.role === 'runtime'));
  assert.deepEqual(subject(r, 'src/cli-lib.cjs', 'cliWork').usage.value, ['runtime']);
  assert.equal(subject(r, 'src/cli-lib.cjs', 'unusedCli').kind, 'unused-export');
  assert.deepEqual(subject(r, 'src/job.ts', 'runJob').usage.value, ['runtime']);
  assert.equal(subject(r, 'src/job.ts', 'deadJob').kind, 'unused-export');
  assert.deepEqual(subject(r, 'packages/dev-srv/src/serve.ts', 'serve').usage.value, ['runtime']);
  assert.deepEqual(subject(r, 'packages/docs/app/page.tsx').usage.evaluation, ['runtime']);
});

test('gaps in one monorepo workspace leave fully resolved findings in unrelated workspaces complete', async t => {
  const root = await fixture(t, {
    'package.json': JSON.stringify({ workspaces: ['packages/*'], bin: { app: './app.ts' } }),
    'app.ts': 'import { ok } from "@org/clean"; import { messy } from "@org/noisy"; ok(); messy();',
    'packages/clean/package.json': JSON.stringify({ name: '@org/clean', exports: { '.': './src/index.ts' } }),
    'packages/clean/src/index.ts': 'export function ok() {} export function cleanUnused() {}',
    'packages/noisy/package.json': JSON.stringify({ name: '@org/noisy', exports: { '.': './src/index.ts' } }),
    'packages/noisy/src/index.ts': [
      'import "uninstalled-external-pkg";',
      'import { missing } from "./broken";',
      'export function messy() { eval("1"); return missing; }',
      'export function noisyUnused() {}',
    ].join('\n'),
    'packages/noisy/src/broken.ts': 'export const present = 1;',
    'packages/noisy/src/index.test.ts': 'export async function loadDyn(x: string) { return import(x); } loadDyn("a");',
  });

  const r = await inspect(t, root, { publicEntries: [] });
  assert.equal(r.coverage.status, 'partial');
  assert.equal(subject(r, 'packages/clean/src/index.ts', 'cleanUnused').status, 'complete');
  assert.deepEqual(subject(r, 'packages/clean/src/index.ts', 'cleanUnused').gapIds, []);
  assert.equal(subject(r, 'packages/noisy/src/index.ts', 'noisyUnused').status, 'conditional');
  assert.equal(subject(r, 'packages/noisy/src/broken.ts', 'present').status, 'conditional');
});

test('findings carry owning workspace attribution and filter cleanly by workspace directory or package name', async t => {
  const root = await fixture(t, {
    'package.json': JSON.stringify({ name: 'mono-root', workspaces: ['packages/*'], bin: { app: './app.ts' } }),
    'app.ts': 'import { shared } from "@org/core"; shared();',
    'packages/core/package.json': JSON.stringify({ name: '@org/core', exports: { '.': './src/index.ts' } }),
    'packages/core/src/index.ts': 'export function shared() {} export function deadCore() {}',
    'packages/web/package.json': JSON.stringify({ name: '@org/web', exports: { '.': './src/index.ts' } }),
    'packages/web/src/index.ts': 'export function deadWeb() {}',
  });

  const r = await inspect(t, root, { publicEntries: [] });
  assert.ok(r.workspaces.some(w => w.dir === 'packages/core' && w.name === '@org/core'));
  assert.ok(r.workspaces.some(w => w.dir === 'packages/web' && w.name === '@org/web'));
  assert.equal(subject(r, 'packages/core/src/index.ts', 'deadCore').workspace, 'packages/core');
  assert.deepEqual(
    r.findings({ workspace: 'packages/core' }).map(f => `${f.workspace}:${f.exportName ?? '<mod>'}`),
    ['packages/core:deadCore'],
  );
  assert.deepEqual(
    r.findings({ workspace: '@org/web' }).map(f => `${f.workspace}:${f.exportName ?? '<mod>'}`),
    ['packages/web:<mod>', 'packages/web:deadWeb'],
  );
  const cliOut = execFileSync(process.execPath, [
    cliPath, '--root', root, '--entries', 'public:', '--workspace', 'packages/core',
  ], { encoding: 'utf8' });
  const rows = cliOut.trim().split('\n').map(l => JSON.parse(l));
  assert.equal(rows.length, 1);
  assert.equal(rows[0].workspace, 'packages/core');
  assert.equal(rows[0].exportName, 'deadCore');
});

test('dependency analysis detects unused, unlisted, unlisted-binary and dev-dependency-in-runtime per workspace', async t => {
  const root = await fixture(t, {
    'package.json': JSON.stringify({
      name: 'mono',
      workspaces: ['packages/*'],
      devDependencies: { typescript: '^5.0.0' },
    }),
    'node_modules/used-lib/package.json': JSON.stringify({ name: 'used-lib', main: 'index.js' }),
    'node_modules/used-lib/index.js': 'exports.ok = 1;',
    'node_modules/dev-only-lib/package.json': JSON.stringify({ name: 'dev-only-lib', main: 'index.js' }),
    'node_modules/dev-only-lib/index.js': 'exports.helper = 1;',
    'node_modules/unlisted-lib/package.json': JSON.stringify({ name: 'unlisted-lib', main: 'index.js' }),
    'node_modules/unlisted-lib/index.js': 'exports.x = 1;',
    'node_modules/test-util/package.json': JSON.stringify({ name: 'test-util', main: 'index.js' }),
    'node_modules/test-util/index.js': 'exports.check = 1;',
    'packages/svc/package.json': JSON.stringify({
      name: '@org/svc',
      bin: { svc: './src/server.ts' },
      dependencies: { 'used-lib': '^1.0.0', 'unused-pkg': '^1.0.0' },
      devDependencies: {
        '@types/used-lib': '^1.0.0',
        'dev-only-lib': '^1.0.0',
        'test-util': '^1.0.0',
      },
      scripts: {
        typecheck: 'tsc --noEmit',
        lint: 'mystery-linter src/',
      },
    }),
    'packages/svc/src/server.ts': [
      'import { ok } from "used-lib";',
      'import { helper } from "dev-only-lib";',
      'import { x } from "unlisted-lib";',
      'console.log(ok, helper, x);',
    ].join('\n'),
    'packages/svc/src/server.test.ts': 'import { check } from "test-util"; check();',
  });

  const r = await inspect(t, root, { dependencies: true, publicEntries: [] });
  const deps = r.dependencies({ workspace: 'packages/svc' });
  assert.deepEqual(
    deps.map(d => `${d.kind}:${d.dependency ?? d.binary}:${d.manifest}:${d.file}`),
    [
      'unlisted-binary:mystery-linter:packages/svc/package.json:packages/svc/package.json',
      'unused-dependency:unused-pkg:packages/svc/package.json:packages/svc/package.json',
      'dev-dependency-in-runtime:dev-only-lib:packages/svc/package.json:packages/svc/src/server.ts',
      'unlisted-dependency:unlisted-lib:packages/svc/package.json:packages/svc/src/server.ts',
    ],
  );
  // Root typescript devDependency was consumed by child workspace's `tsc --noEmit` script.
  assert.deepEqual(r.dependencies({ workspace: '.' }), []);
});

test('per-symbol suppressions require a reason and stale configuration entries are reported', async t => {
  const root = await fixture(t, {
    'app.ts': 'import { live } from "./lib"; live();',
    'lib.ts': [
      '/** @smella-ignore loaded dynamically by host plugin registry */',
      'export function hostHook() {}',
      '/** @smella-ignore */',
      'export function missingReason() {}',
      '/** @smella-ignore stale note on live export */',
      'export function live() {}',
      'export function configIgnored() {}',
      'export function genuinelyDead() {}',
    ].join('\n'),
  });

  // File-level / glob / empty-reason config suppressions are rejected.
  await assert.rejects(
    analyze({ root, ...app, suppressions: [{ file: 'lib.ts', symbol: '*', reason: 'nope' }] }),
    { code: 'ANALYSIS_FAILED' },
  );
  await assert.rejects(
    analyze({ root, ...app, suppressions: [{ file: 'lib.ts', symbol: 'configIgnored', reason: '   ' }] }),
    { code: 'ANALYSIS_FAILED' },
  );

  const r = await inspect(t, root, {
    entries: { runtime: ['app.ts', 'missing-entry.ts'], test: [], tooling: [] },
    publicEntries: [],
    exclude: ['does-not-exist/**'],
    suppressions: [
      { file: 'lib.ts', symbol: 'configIgnored', reason: 'invoked by external scheduler' },
      { file: 'lib.ts', symbol: 'nonExistent', reason: 'old hook removed' },
    ],
  });

  assert.deepEqual(
    r.findings().map(f => f.exportName),
    ['genuinelyDead', 'missingReason'],
  );
  assert.deepEqual(
    r.suppressed.map(s => `${s.source}:${s.symbol}:${s.reason}`).sort(),
    [
      'config:configIgnored:invoked by external scheduler',
      'jsdoc:hostHook:loaded dynamically by host plugin registry',
    ],
  );
  const staleKinds = r.staleConfig.map(s => `${s.kind}:${s.symbol ?? s.pattern ?? ''}`).sort();
  assert.deepEqual(staleKinds, [
    'invalid-suppression:',
    'stale-entry:missing-entry.ts',
    'stale-exclude:does-not-exist/**',
    'stale-suppression:live',
    'stale-suppression:nonExistent',
  ]);
});

test('hook modes support --staged, --diff merge-base, two-parent merge commits, --all and human witness output', async t => {
  const root = await fixture(t, {
    'package.json': JSON.stringify({ name: 'hook-demo', bin: { app: './app.ts' } }),
    'app.ts': 'import { live } from "./lib"; live();',
    'lib.ts': 'export function live() {} export function preExistingDead() {}',
    'lib.test.ts': 'import { live } from "./lib"; live();',
  });
  const git = (...args) => execFileSync('git', args, { cwd: root, encoding: 'utf8' });
  const runCli = (...args) => {
    try {
      const stdout = execFileSync(process.execPath, [cliPath, '--root', root, ...args], { encoding: 'utf8' });
      return { code: 0, stdout };
    } catch (err) {
      return { code: err.status ?? 1, stdout: err.stdout ?? '', stderr: err.stderr ?? '' };
    }
  };
  git('init', '-q', '-b', 'main');
  git('config', 'user.email', 'test@example.com');
  git('config', 'user.name', 'Test');
  git('add', '.');
  git('commit', '-q', '-m', 'base');

  // --all always exits 0 even with pre-existing findings, and renders human format.
  const allRes = runCli('--all');
  assert.equal(allRes.code, 0);
  assert.ok(allRes.stdout.includes('preExistingDead'));

  // Stage a test-only export while leaving an unstaged dead export in the working tree.
  await writeFile(join(root, 'lib.ts'), 'export function live() {} export function preExistingDead() {} export function onlyTested() {}');
  await writeFile(join(root, 'lib.test.ts'), 'import { live, onlyTested } from "./lib"; live(); onlyTested();');
  git('add', 'lib.ts', 'lib.test.ts');
  await writeFile(join(root, 'lib.ts'), 'export function live() {} export function preExistingDead() {} export function onlyTested() {} export function unstagedDead() {}');

  // --staged inspects only the index (sees onlyTested with witness path, ignores unstagedDead) and exits 1.
  const stagedRes = runCli('--staged');
  assert.equal(stagedRes.code, 1);
  assert.ok(stagedRes.stdout.includes('test-only-export'));
  assert.ok(stagedRes.stdout.includes('onlyTested'));
  assert.ok(stagedRes.stdout.includes('via lib.test.ts -> lib.ts'));
  assert.ok(!stagedRes.stdout.includes('unstagedDead'));

  // --diff against main sees the working-tree unstagedDead too and exits 1.
  const diffRes = runCli('--diff', 'main');
  assert.equal(diffRes.code, 1);
  assert.ok(diffRes.stdout.includes('unstagedDead'));

  // Commit branch A with deadA, branch B with deadB, then merge cleanly:
  // merge commit judged against both parents has 0 introduced findings.
  git('reset', '--hard', '-q', 'HEAD');
  git('checkout', '-q', '-b', 'branch-a');
  await writeFile(join(root, 'a.ts'), 'export function deadA() {}');
  git('add', 'a.ts');
  git('commit', '-q', '-m', 'branch a');

  git('checkout', '-q', 'main');
  git('checkout', '-q', '-b', 'branch-b');
  await writeFile(join(root, 'b.ts'), 'export function deadB() {}');
  git('add', 'b.ts');
  git('commit', '-q', '-m', 'branch b');

  git('merge', '-q', '--no-edit', 'branch-a');
  const cleanMerge = runCli('--diff');
  assert.equal(cleanMerge.code, 0);
  assert.ok(cleanMerge.stdout.includes('0 introduced'));

  // If a merge commit itself adds a new dead export, --diff catches it.
  await writeFile(join(root, 'b.ts'), 'export function deadB() {} export function mergeAuthoredDead() {}');
  git('add', 'b.ts');
  git('commit', '-q', '--amend', '--no-edit');
  const dirtyMerge = runCli('--diff');
  assert.equal(dirtyMerge.code, 1);
  assert.ok(dirtyMerge.stdout.includes('mergeAuthoredDead'));
  assert.ok(!dirtyMerge.stdout.includes('deadA'));
  assert.ok(!dirtyMerge.stdout.includes('deadB'));
});

test('package scripts discover tooling entries and CLI --check gates findings cleanly', async t => {
  const root = await fixture(t, {
    'package.json': JSON.stringify({
      name: 'tooling-check-demo',
      type: 'module',
      bin: { demo: './src/cli.ts' },
      scripts: { build: 'bun scripts/build.mjs --release' },
    }),
    'src/cli.ts': 'export function run() { return 1; } run();',
    'scripts/build.mjs': 'import { helper } from "./util.mjs"; helper();',
    'scripts/util.mjs': '/** @smella-ignore Shared build helper */\nexport function helper() { return 2; }\n',
  });
  const r = await inspect(t, root);
  assert.deepEqual(r.findings(), []);
  assert.ok(r.roots.some(entry => entry.role === 'tooling' && entry.file === 'scripts/build.mjs'));
  assert.equal(r.suppressed.length, 1);
  assert.equal(r.suppressed[0].kind, 'tooling-only-export');

  const runCli = (...args) => {
    try {
      return { code: 0, stdout: execFileSync(process.execPath, [cliPath, '--root', root, ...args], { encoding: 'utf8' }) };
    } catch (err) {
      return { code: err.status, stdout: err.stdout?.toString() ?? '' };
    }
  };
  assert.equal(runCli('--check').code, 0);

  await writeFile(join(root, 'src/dead.ts'), 'export const orphan = 1;');
  const failed = runCli('--check');
  assert.equal(failed.code, 1);
  assert.ok(failed.stdout.includes('orphan'));
});

test('git-ignored build outputs are skipped while workspace source mapping still resolves them', async t => {
  const root = await fixture(t, {
    '.gitignore': 'packages/*/lib/\n*.generated.ts\n',
    'package.json': JSON.stringify({ name: 'mono', private: true, workspaces: ['packages/*'] }),
    'packages/core/package.json': JSON.stringify({ name: '@mono/core', main: './lib/index.js', types: './lib/index.d.ts' }),
    'packages/core/tsconfig.json': JSON.stringify({ compilerOptions: { rootDir: 'src', outDir: 'lib' } }),
    'packages/core/src/index.ts': 'export function greet() { return "hi"; }',
    'packages/core/lib/index.js': 'exports.greet = () => "hi"; exports.compiledNoise = () => 0;',
    'packages/core/lib/index.d.ts': 'export declare function greet(): string;',
    'packages/app/package.json': JSON.stringify({ name: '@mono/app', bin: { app: './src/main.ts' }, dependencies: { '@mono/core': '*' } }),
    'packages/app/src/main.ts': 'import { greet } from "@mono/core"; console.log(greet());',
    'packages/app/src/schema.generated.ts': 'export const generatedUnused = 1;',
  });
  const r = await inspect(t, root);
  assert.equal(r.coverage.status, 'complete');
  assert.ok(!r.coverage.files.some(f => f.includes('/lib/') || f.endsWith('.generated.ts')));
  assert.deepEqual(subject(r, 'packages/core/src/index.ts', 'greet').usage.value, ['runtime']);
  assert.deepEqual(r.findings(), []);
});
