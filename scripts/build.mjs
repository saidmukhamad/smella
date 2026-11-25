import { spawnSync } from 'node:child_process';
import { copyFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const cwd = fileURLToPath(new URL('..', import.meta.url));
const release = process.argv.includes('--release');
const build = spawnSync('cargo', ['build', '--locked', '--message-format=json-render-diagnostics', ...(release ? ['--release'] : [])], {
  cwd, encoding: 'utf8', stdio: ['ignore', 'pipe', 'inherit'],
});
if (build.error) throw build.error;
if (build.status !== 0) process.exit(build.status ?? 1);
const artifact = build.stdout.trim().split('\n').map(line => JSON.parse(line))
  .findLast(record => record.reason === 'compiler-artifact' && record.target.name === 'smella' && record.target.crate_types.includes('cdylib'));
const library = artifact?.filenames.find(path => /\.(so|dylib|dll)$/.test(path));
if (!library) throw new Error('Cargo did not return a native library');
copyFileSync(library, new URL('../smella.node', import.meta.url));
if (release && process.platform === 'linux' && process.arch === 'x64' && process.report?.getReport().header.glibcVersionRuntime) {
  copyFileSync(library, new URL('../npm/linux-x64-gnu/smella.node', import.meta.url));
}
