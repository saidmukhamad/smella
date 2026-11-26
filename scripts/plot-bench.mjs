// Gnuplot is a development dependency for rendering; analysis needs only the addon.
import { execFileSync } from 'node:child_process';
import { readdir, readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
const repo = fileURLToPath(new URL('..',import.meta.url));
const folder = new URL('../benchmarks/history/',import.meta.url);
const targets = JSON.parse(await readFile(new URL('../benchmarks/targets.json',import.meta.url),'utf8'));
const files = await readdir(folder).catch(error => {
  if (error.code === 'ENOENT') return [];
  throw error;
});
const entries = await Promise.all(files.filter(f=>f.endsWith('.json')).sort().map(async f=>
  JSON.parse(await readFile(new URL(f,folder),'utf8'))));
if (!entries.length) throw new Error('Run bun run bench:record to create the first local benchmark.');
const latest = entries.at(-1).machine;
const comparable = entries.filter(e=>['cpu','arch','platform','os','node','rust'].every(k=>e.machine[k]===latest[k]));
const data = [
  ['Resident analysis + findings','chain.residentMs'],
  ['Cold process + findings','chain.coldProcessMs'],
  ['Findings conversion','chain.findingsConversionMs'],
  ['Deep workspace + caller evidence','workspace.totalMs'],
  ['Nx app + React route explanations','nxReact.totalMs'],
];
function quote(s) { return "'"+s.replaceAll("'","''").replaceAll('\n',' ')+"'"; }
const labels = comparable.map((e,i)=>quote(e.revision.slice(0,7))+' '+(i+1)).join(', ');
const program = [
  "set terminal pngcairo size 1560,1220 font 'DejaVu Sans,15' noenhanced",
  "set output 'benchmarks/history/progress.png'",
  "set multiplot layout 3,2 rowsfirst title 'Smella benchmark history  ·  milliseconds per run' font ',23'",
  "set border 3 lc rgb '#73838f'",
  "set grid ytics back lc rgb '#e0e6eb'",
  "set tics nomirror scale 0.4",
  "set lmargin 8; set rmargin 3; set bmargin 4",
  "set style line 1 lc rgb '#12615e' lw 3 pt 7 ps 1.3",
  "set style line 2 lc rgb '#5281a6' lw 2 pt 5 ps 1.1",
  "set style line 3 lc rgb '#b65324' lw 2 dt 2",
  "set key horizontal top center samplen 1.5 spacing 1 font ',12'",
  "set xtics ("+labels+") font ',12'",
  'set xrange [0.75:'+(comparable.length+.25)+']',
];
for (const [title,key] of data) {
  const limit = targets.limits.find(t=>t.metric === key+'.p95').max;
  const rows = comparable.map((e,i)=>{
    const values = key.split('.').reduce((v,k)=>v?.[k],e);
    return [i+1,values?.p50 ?? 'NaN',values?.p95 ?? 'NaN'];
  });
  const high = Math.max(limit,...rows.map(r=>Number.isFinite(r[2]) ? r[2] : 0));
  program.push('$series << EOD',...rows.map(r=>r.join(' ')),'EOD',
    'set title '+quote(title)+" font ',18' offset 0,0.25",'set yrange [0:'+(high*1.32)+']',
    "plot $series using 1:3 with linespoints ls 1 title 'p95', $series using 1:2 with linespoints ls 2 title 'p50', "+limit+" with lines ls 3 title 'p95 target "+limit+" ms'");
}
program.push('unset multiplot','unset output');
execFileSync('gnuplot',[],{cwd:repo,input:program.join('\n')+'\n',stdio:['pipe','inherit','inherit']});
