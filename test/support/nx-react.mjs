// Install inert external modules only in the temporary fixture. Smella parses project
// source and stops at the installed package boundary, so rendering is unnecessary.
import { cp, mkdir, writeFile } from 'node:fs/promises';
import { join } from 'node:path';

export async function prepareNxReact(root) {
  await cp(new URL('../fixtures/nx-react',import.meta.url),root,{recursive:true});
  for (const [name,manifest] of [
    ['react',{ main:'./index.js' }],
    ['react-dom',{ exports:{ './client':'./index.js' } }],
  ]) {
    const dir = join(root,'node_modules',name);
    await mkdir(dir,{recursive:true});
    await writeFile(join(dir,'package.json'),JSON.stringify({ name,...manifest }));
    await writeFile(join(dir,'index.js'),'throw new Error("analysis executed fixture code");');
  }
}
