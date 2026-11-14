// Assembles the playground in demo/dist (run `npm run build:demo`, which compiles demo/src
// afterwards):
//
//   demo/dist/index.html, style.css   the page
//   demo/dist/kit/                    the kit's browser modules (dist/, without the transpile step)
//   demo/dist/engine/                 codelib's guest, transpiled with the kit (preset `script`)
//
// Every URL in the page is relative, so the site works under any base path (GitHub Pages serves
// it from /codelib/).

import { cpSync, mkdirSync, readdirSync, rmSync, statSync } from 'node:fs';
import { dirname, join, relative } from 'node:path';
import { fileURLToPath } from 'node:url';

import { transpile } from '../dist/transpile/index.js';

const web = join(dirname(fileURLToPath(import.meta.url)), '..');
const demo = join(web, 'demo');
const out = join(demo, 'dist');
const guest = join(web, '../crates/codelib-guest/codelib-guest.wasm');

rmSync(out, { recursive: true, force: true });
mkdirSync(out, { recursive: true });
for (const f of ['index.html', 'style.css']) cpSync(join(demo, f), join(out, f));

// The kit's browser modules: every .js under dist/ except the Node-only transpile step and CLI.
const dist = join(web, 'dist');
const walk = (dir) => readdirSync(dir).flatMap((n) => (statSync(join(dir, n)).isDirectory() ? walk(join(dir, n)) : [join(dir, n)]));
let files = 0;
for (const file of walk(dist)) {
  const rel = relative(dist, file);
  if (!rel.endsWith('.js') || rel.startsWith('transpile') || rel === 'cli.js') continue;
  mkdirSync(dirname(join(out, 'kit', rel)), { recursive: true });
  cpSync(file, join(out, 'kit', rel));
  files++;
}

const t0 = performance.now();
const manifest = await transpile(guest, { outDir: join(out, 'engine'), name: 'codelib-guest', preset: 'script' });
rmSync(join(out, 'engine/interfaces'), { recursive: true, force: true });
rmSync(join(out, 'engine/codelib-guest.d.ts'), { force: true });
console.log(`demo: ${files} kit modules, engine transpiled in ${Math.round(performance.now() - t0)} ms (${manifest.coreModules.length} core modules) -> ${relative(web, out)}`);
