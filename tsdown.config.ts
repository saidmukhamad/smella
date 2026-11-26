import { defineConfig } from 'tsdown';

export default defineConfig({
  entry: {
    index: 'lib/index.ts',
    cli: 'lib/cli.ts',
  },
  format: 'esm',
  platform: 'node',
  target: 'node22',
  outDir: 'dist',
  fixedExtension: true,
  dts: true,
  clean: true,
  sourcemap: true,
  deps: {
    neverBundle: ['smella-linux-x64-gnu'],
  },
});
