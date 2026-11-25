// Run source-backed behavioral checks against clean, pinned upstream checkouts.
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { cp, mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { analyze } from '../index.js';

const base = resolve(process.env.SMELLA_CORPUS_DIR || '.');
const projects = {
  transformers: ['huggingface/transformers.js', '890e398f3fe99e52e855ffc4f2c81fccbe762dd5'],
  nx: ['nrwl/nx', '3272c313f283c9cf4e2af868a2b3617fad280f40'],
  react: ['facebook/react', '92bb0678c52133a47b3095f6e54da883ea0d6cff'],
};
const records = [];
const config = (runtime = [], test = [], publicEntries = []) => ({
  entries: { runtime, test, tooling: [] },
  publicEntries,
});
async function snapshot(root, profile, check) {
  const report = await analyze({ root, ...profile });
  try {
    return await check(report);
  } finally {
    report.dispose();
  }
}
function checkout(key) {
  const root = join(base, key);
  const git = (...args) => execFileSync('git', args, { cwd: root, encoding: 'utf8' }).trim();
  assert.equal(git('rev-parse', 'HEAD'), projects[key][1], key + ' revision');
  assert.equal(git('status', '--porcelain'), '', key + ' checkout must match its recorded revision');
  return root;
}
async function anchor(root, file, needle) {
  const text = await readFile(join(root, file), 'utf8');
  const at = text.indexOf(needle);
  assert.ok(at >= 0, 'missing source evidence in ' + file + ': ' + needle);
  return { file, line: text.slice(0, at).split('\n').length };
}
function observe(e) {
  return {
    file: e.file,
    name: e.bindingName ?? e.exportName,
    kind: e.kind,
    status: e.status,
    usage: e.usage,
    tests: e.tests.map((t) => t.file),
    paths: e.witnesses
      .filter((w) => w.fact === 'contract' || w.fact === 'value')
      .map((w) => ({
        role: w.role,
        fact: w.fact,
        files: [...new Set(w.path.map((s) => s.file))],
      })),
  };
}
function through(path, expected) {
  let i = 0;
  for (const file of path) if (file === expected[i]) i++;
  return i === expected.length;
}

async function transformers() {
  const root = checkout('transformers'),
    dir = 'packages/transformers/';
  const file = dir + 'src/pipelines.js',
    barrel = dir + 'src/transformers.js';
  const test = dir + 'tests/progress_callbacks.test.js';
  const evidence = await Promise.all([
    anchor(root, test, '=> pipeline("text-generation", model_id'),
    anchor(root, barrel, "export * from './pipelines.js'"),
    anchor(root, file, 'export async function pipeline('),
  ]);
  const ask = (r) => observe(r.explain({ file, exportName: 'pipeline' }));
  const testOnly = await snapshot(root, config([], [test]), ask);
  const both = await snapshot(root, config([], [test], [barrel]), ask);
  const contractOnly = await snapshot(root, config([], [], [barrel]), ask);
  assert.equal(testOnly.kind, 'test-only-export');
  assert.deepEqual(testOnly.tests, [test]);
  assert.ok(testOnly.paths.some((w) => w.role === 'test' && through(w.files, [test, barrel, file])));
  assert.deepEqual(both.usage.value, ['test']);
  assert.equal(both.usage.contract, true);
  assert.equal(both.kind, null);
  assert.deepEqual(contractOnly.usage.value, []);
  assert.equal(contractOnly.usage.contract, true);
  records.push({
    repo: 'transformers',
    title: 'A pipeline can have both a test caller and a public contract',
    evidence,
    observations: { testOnly, both, contractOnly },
    description:
      'The upstream progress callback test imports pipeline through the public barrel. Adding the barrel as a public surface must preserve the test witness and suppress the test-only finding. Removing the test root must leave the public contract intact.',
  });
  const runner = dir + 'tests/pipelines.test.js',
    loader = dir + 'tests/test_utils.js';
  const registration = dir + 'tests/pipelines/test_pipelines_image_feature_extraction.js';
  const dynamic = await snapshot(root, config([], [runner]), (r) => {
    const gap = r.coverage.gaps.find(
      (g) => g.code === 'computed-import' && g.file === loader && g.affectedFiles === null,
    );
    assert.ok(gap, 'the upstream directory-driven suite needs a project-wide loader gap');
    const suite = observe(r.explain({ file: registration, exportName: 'default' }));
    assert.equal(suite.status, 'conditional');
    return { gap: gap.code, file: gap.file, suite };
  });
  records.push({
    repo: 'transformers',
    title: 'A directory-driven test loader keeps an uncertain suite conditional',
    evidence: await Promise.all([
      anchor(root, runner, 'collect_and_execute_pipeline_tests('),
      anchor(root, loader, 'const pipeline_types = fs.readdirSync'),
      anchor(root, registration, 'export default () =>'),
    ]),
    observations: { dynamic },
    description:
      'The upstream runner enumerates filenames, imports a computed path and calls each module’s default registration. A missing static path to that suite must carry the loader uncertainty.',
  });
}
async function nx() {
  const root = checkout('nx'),
    dir = 'packages/devkit/',
    file = dir + 'src/utils/names.ts',
    test = dir + 'src/utils/names.spec.ts',
    barrel = dir + 'index.ts';
  const evidence = await Promise.all([
    anchor(root, test, "names('foo-bar')"),
    anchor(root, file, 'className: toClassName(name)'),
    anchor(root, dir + 'public-api.ts', 'export { names }'),
    anchor(root, barrel, "export * from './public-api'"),
  ]);
  const ask = (r) => observe(r.explain({ file, bindingName: 'toClassName' }));
  const internal = await snapshot(root, config([], [test]), ask);
  const exposed = await snapshot(root, config([], [test], [barrel]), ask);
  assert.equal(internal.kind, 'test-only-binding');
  assert.deepEqual(internal.tests, [test]);
  assert.equal(exposed.usage.contract, true);
  assert.deepEqual(exposed.usage.value, ['test']);
  assert.equal(exposed.kind, null);
  assert.ok(
    exposed.paths.some(
      (w) => w.fact === 'contract' && through(w.files, [barrel, dir + 'public-api.ts', file]),
    ),
  );
  records.push({
    repo: 'nx',
    title: 'A public devkit export retains its private implementation',
    evidence,
    observations: { internal, exposed },
    description:
      'The spec calls names, which calls the private toClassName helper. Only the public-entry setting changes between runs. The retained helper must acquire a contract witness through both upstream barrel files.',
  });
}
async function react() {
  const root = checkout('react'),
    dir = 'fixtures/ssr2/src/',
    main = dir + 'index.js',
    app = dir + 'App.js',
    file = dir + 'Comments.js';
  const evidence = await Promise.all([
    anchor(root, main, 'hydrateRoot(document, <App'),
    anchor(root, app, "lazy(() => import('./Comments'"),
    anchor(root, app, '<Comments />'),
    anchor(root, file, 'export default function Comments'),
  ]);
  const ask = (r) => observe(r.explain({ file, exportName: 'default' }));
  const upstream = await snapshot(root, config([main]), ask);
  assert.deepEqual(upstream.usage.value, ['runtime']);
  assert.ok(upstream.paths.some((w) => w.role === 'runtime' && through(w.files, [main, app, file])));
  const temp = await mkdtemp(join(tmpdir(), 'smella-ssr2-control-'));
  try {
    // Preserve all the SSR fixture source and the React package identity in both controls.
    for (const relative of ['package.json', 'packages/react', dir]) {
      await mkdir(dirname(join(temp, relative)), { recursive: true });
      await cp(join(root, relative), join(temp, relative), { recursive: true });
    }
    const query = (r) => ({
      comments: ask(r),
      sidebar: observe(r.explain({ file: dir + 'Sidebar.js', exportName: 'default' })),
    });
    const before = await snapshot(temp, config([main]), query);
    const source = await readFile(join(temp, app), 'utf8');
    assert.equal(source.split('<Comments />').length, 2);
    await writeFile(join(temp, app), source.replace('<Comments />', '{null}'));
    const after = await snapshot(temp, config([main]), query);
    assert.deepEqual(before.comments.usage.value, ['runtime']);
    assert.deepEqual(after.comments.usage.value, []);
    assert.deepEqual(after.comments.usage.evaluation, []);
    assert.deepEqual(before.sidebar.usage.value, ['runtime']);
    assert.deepEqual(after.sidebar.usage.value, ['runtime']);
    records.push({
      repo: 'react',
      title: 'Removing one JSX render releases only its lazy component',
      evidence,
      observations: { upstream, before, after },
      description:
        'In a disposable copy of the upstream streaming fixture, replace only <Comments /> with {null}. Its lazy declaration remains. Comments must lose runtime value and evaluation, while Sidebar stays runtime-reachable.',
    });
  } finally {
    await rm(temp, { recursive: true, force: true });
  }
}
for (const verify of [transformers, nx, react]) await verify();
console.log(JSON.stringify({ projects, records }, null, 2));
