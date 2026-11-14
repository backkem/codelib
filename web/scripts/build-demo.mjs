// Assembles the playground in demo/dist (run `npm run build:demo`, which compiles demo/src
// afterwards):
//
//   demo/dist/index.html, style.css   the page
//   demo/dist/kit/                    the kit's browser modules (dist/, without the transpile step)
//   demo/dist/engine/                 codelib's guest, transpiled with the kit (preset `script`)
//
// Every URL in the page is relative, so the site works under any base path (GitHub Pages serves
// it from /codelib/).

import { cpSync, mkdirSync, rmSync } from 'node:fs';
import { dirname, join, relative } from 'node:path';
import { fileURLToPath } from 'node:url';

import { copyBrowserFiles } from '../dist/browser-files.js';
import { transpile } from '../dist/transpile/index.js';

const web = join(dirname(fileURLToPath(import.meta.url)), '..');
const demo = join(web, 'demo');
const out = join(demo, 'dist');
const guest = join(web, '../crates/codelib-guest/codelib-guest.wasm');

rmSync(out, { recursive: true, force: true });
mkdirSync(out, { recursive: true });
for (const f of ['index.html', 'style.css']) cpSync(join(demo, f), join(out, f));

// The kit's browser modules, as `codelib-web browser-files` copies them.
const files = copyBrowserFiles(join(out, 'kit')).length;

const t0 = performance.now();
const manifest = await transpile(guest, { outDir: join(out, 'engine'), name: 'codelib-guest', preset: 'script' });
rmSync(join(out, 'engine/interfaces'), { recursive: true, force: true });
rmSync(join(out, 'engine/codelib-guest.d.ts'), { force: true });
console.log(`demo: ${files} kit modules, engine transpiled in ${Math.round(performance.now() - t0)} ms (${manifest.coreModules.length} core modules) -> ${relative(web, out)}`);
