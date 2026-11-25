// Generate a fixed graph and verify every reported subject before comparing timings.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtemp, writeFile, rm } from 'node:fs/promises';
import { tmpdir, cpus, release } from 'node:os';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { performance } from 'node:perf_hooks';
import { analyze } from '../index.js';

const count = 512;
const samples = 20;
const expected = Array.from({ length: count }, (_, i) => `m${i}.ts#dead`).sort();
const config = { entries: { runtime: ['app.ts'], test: [], tooling: [] }, publicEntries: [] };

async function once(root) {
  const start = performance.now();
  const report = await analyze({ root, ...config });
  try {
    const ready = performance.now();
    const findings = report.findings();
    const converted = performance.now();
    assert.equal(report.coverage.status, 'complete');
    assert.equal(report.coverage.files.length, count + 1);
    assert.ok(findings.every(f => f.kind === 'unused-export' && f.status === 'complete'));
    assert.deepEqual(findings.map(f => f.file + '#' + f.exportName).sort(), expected);
    return { analysis: ready - start, conversion: converted - ready, total: converted - start, rssKiB: process.resourceUsage().maxRSS };
  } finally { report.dispose(); }
}

function run(command, args, cwd) {
  const start = performance.now();
  const result = spawnSync(command, args, { cwd, encoding: 'utf8', maxBuffer: 8 * 1024 ** 2 });
  const ms = performance.now() - start;
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(result.stderr || result.stdout);
  return { ms, output: result.stdout.trim() };
}

function stats(numbers) {
  const sorted = numbers.toSorted((a,b) => a-b);
  const quantile = q => Number(sorted[Math.ceil(q * sorted.length) - 1].toFixed(2));
  return { p50: quantile(.5), p95: quantile(.95) };
}

async function bench() {
  const root = await mkdtemp(join(tmpdir(), 'smella-bench-'));
  try {
    await writeFile(join(root,'package.json'),JSON.stringify({name:'smella-synthetic',private:true,type:'module'}));
    await writeFile(join(root,'knip.json'),JSON.stringify({entry:['app.ts'],project:['*.ts']}));
    await writeFile(join(root,'app.ts'),`import { live } from './m${count-1}.js'; live();\n`);
    for (let i = 0; i < count; i++) {
      const source = (i ? `import { live as previous } from './m${i-1}.js';\n` : '') +
        `export function live() { return ${i ? 'previous()' : '1'}; }\nexport const dead = ${i};\n`;
      await writeFile(join(root,`m${i}.ts`),source);
    }
    const cold = [], resident = [], startup = [], rss = [], knipTimes = [];
    const knip = process.env.KNIP_BIN && resolve(process.env.KNIP_BIN);
    const script = fileURLToPath(import.meta.url);
    for (let i = 0; i < samples; i++) {
      if (knip && i % 2 === 0) compareKnip();
      startup.push(run(process.execPath,['--input-type=module','-e',`await import(${JSON.stringify(new URL('../index.js',import.meta.url).href)})`],root).ms);
      const { ms, output } = run(process.execPath,[script,'--once',root],root);
      cold.push(ms);
      rss.push(JSON.parse(output).rssKiB);
      if (knip && i % 2 === 1) compareKnip();
    }
    function compareKnip() {
      const result = run(knip,['--config','knip.json','--include','files,exports','--reporter','json','--no-exit-code'],root);
      const report = JSON.parse(result.output);
      assert.ok(report.issues.every(issue => issue.files.length === 0));
      const subjects = report.issues.flatMap(issue => issue.exports.map(e => issue.file + '#' + e.name)).sort();
      assert.deepEqual(subjects, expected, 'Results differ; discard the comparison');
      knipTimes.push(result.ms);
    }
    await once(root);
    for (let i = 0; i < samples; i++) resident.push(await once(root));
    const repo = fileURLToPath(new URL('..',import.meta.url));
    console.log(JSON.stringify({
      node:process.version, rust:run('rustc',['--version'],repo).output, cpu:cpus()[0].model, os:release(),
      revision:run('git',['rev-parse','--short','HEAD'],repo).output,
      files: count + 1, findings: count, samples,
      startupMs:stats(startup),coldProcessMs:stats(cold),coldPeakRSSKiB:Math.max(...rss),
      residentMs:stats(resident.map(s => s.total)),analysisMs:stats(resident.map(s => s.analysis)),
      findingsConversionMs:stats(resident.map(s => s.conversion)),
      ...(knip ? {knip:run(knip,['--version'],root).output, knipColdProcessMs:stats(knipTimes)} : {})
    },null,2));
  } finally { await rm(root,{recursive:true,force:true}); }
}

if (process.argv[2] === '--once') console.log(JSON.stringify(await once(process.argv[3])));
else await bench();
