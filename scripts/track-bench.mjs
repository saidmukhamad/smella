import { execFileSync } from 'node:child_process';
import { mkdir, mkdtemp, readFile, rm, stat, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { gunzipSync } from 'node:zlib';

function readTar(tgzBytes) {
  const raw = gunzipSync(tgzBytes);
  const files = [];
  let unpackedSize = 0;
  for (let off = 0; off + 512 <= raw.length; ) {
    const header = raw.subarray(off, off + 512);
    if (header.every(b => b === 0)) break;
    const name = header.subarray(0, 100).toString('utf8').replace(/\0.*$/, '');
    const prefix = header.subarray(345, 500).toString('utf8').replace(/\0.*$/, '');
    const full = (prefix ? `${prefix}/${name}` : name).replace(/^package\//, '');
    const size = parseInt(header.subarray(124, 136).toString('utf8').replace(/\0.*$/, '').trim() || '0', 8);
    files.push(full);
    unpackedSize += size;
    off += 512 + Math.ceil(size / 512) * 512;
  }
  return { files, unpackedSize };
}

const repo = fileURLToPath(new URL('..',import.meta.url));
const record = process.argv.includes('--record');
const check = process.argv.includes('--check');
function run(command,args,cwd=repo) {
  return execFileSync(command,args,{cwd,encoding:'utf8',stdio:['ignore','pipe','inherit']}).trim();
}
const targets = JSON.parse(await readFile(join(repo,'benchmarks/targets.json'),'utf8'));
const chain = JSON.parse(run(process.execPath,['scripts/bench.mjs']));
const workspace = JSON.parse(run(process.execPath,['scripts/bench-workspace.mjs']));
const nxReact = JSON.parse(run(process.execPath,['scripts/bench-nx-react.mjs']));
const output = await mkdtemp(join(tmpdir(),'smella-pack-'));
try {
  if (process.platform !== 'linux' || process.arch !== 'x64' || !process.report.getReport().header.glibcVersionRuntime) {
    throw new Error('The package footprint workload requires the supported Linux x64 glibc release build.');
  }
  const tarballs = await Promise.all(['.', 'npm/linux-x64-gnu'].map(async dir => {
    const filename = run('bun', ['pm', 'pack', '--ignore-scripts', '--destination', output, '--quiet'], join(repo, dir));
    const tgzPath = join(output, filename);
    const [{ size }, bytes] = await Promise.all([stat(tgzPath), readFile(tgzPath)]);
    const { files, unpackedSize } = readTar(bytes);
    return { size, unpackedSize, files };
  }));
  if (!tarballs[1].files.some(f => f === 'smella.node')) throw new Error('Build the release addon before measuring its package.');
  const revision = run('git',['rev-parse','HEAD']);
  const observedAt = new Date().toISOString();
  const entry = {
    schema:1, observedAt, revision,
    dirty:Boolean(run('git',['status','--porcelain','--untracked-files=normal'])),
    machine:{cpu:chain.cpu,os:chain.os,platform:process.platform,arch:process.arch,node:chain.node,rust:chain.rust},
    chain:{fixture:'chain-v1',...chain}, workspace, nxReact,
    package:{
      compressedBytes:tarballs.reduce((sum,p)=>sum+p.size,0),
      unpackedBytes:tarballs.reduce((sum,p)=>sum+p.unpackedSize,0),
    },
    targetProfile:targets.profile,
    checks:[],
  };
  for (const {metric,max,unit} of targets.limits) {
    const actual = metric.split('.').reduce((obj,k)=>obj?.[k],entry);
    entry.checks.push({metric,actual,max,unit,pass:Number.isFinite(actual) && actual <= max});
  }
  if (record) {
    const dir = join(repo,'benchmarks/history');
    await mkdir(dir,{recursive:true});
    const filename = observedAt.replaceAll(':','-')+'-'+revision.slice(0,7)+'.json';
    await writeFile(join(dir,filename),JSON.stringify(entry,null,2)+'\n',{flag:'wx'});
    console.log('Recorded benchmarks/history/' + filename);
    run(process.execPath,['scripts/plot-bench.mjs']);
    console.log('Updated benchmarks/history/progress.png');
  }
  console.table(entry.checks);
  if (check && entry.checks.some(c=>!c.pass)) process.exitCode=1;
} finally { await rm(output,{recursive:true,force:true}); }
