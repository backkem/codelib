// The browser modules a Worker can load: the rule in src/browser-files.ts against the built dist/.

import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, readdirSync, rmSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, relative, resolve, sep } from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

import { browserModules, copyBrowserFiles, isBrowserModule } from '../../dist/browser-files.js';

const dist = resolve(dirname(fileURLToPath(import.meta.url)), '../../dist');

function files(dir, base = dir) {
  const out = [];
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) out.push(...files(path, base));
    else out.push(relative(base, path).split(sep).join('/'));
  }
  return out.sort();
}

function withCopy(fn) {
  const out = mkdtempSync(join(tmpdir(), 'codelib-web-browser-'));
  try {
    return fn(out, copyBrowserFiles(out, dist));
  } finally {
    rmSync(out, { recursive: true, force: true });
  }
}

// `import ... from './x.js'`, `export ... from './x.js'`, `import './x.js'` and `import('./x.js')`.
function specifiers(source) {
  const out = [];
  for (const m of source.matchAll(/(?:\bfrom\s*|\bimport\s*\(?\s*)(['"])([^'"]+)\1/g)) out.push(m[2]);
  return out;
}

test('the package entry points a page or Worker imports are all browser modules', () => {
  const modules = browserModules(dist);
  for (const m of ['index.js', 'loop.js', 'worlds.js', 'load.js', 'jspi.js', 'wasi/index.js', 'worker/index.js', 'script/index.js', 'script/worker.js']) {
    assert.ok(modules.includes(m), `${m} is missing from ${modules.join(', ')}`);
  }
});

test('no Node-only module is copied, and nothing in the copy needs Node', () => {
  const modules = browserModules(dist);
  for (const m of modules) {
    assert.ok(!m.startsWith('transpile/') && m !== 'cli.js' && m !== 'browser-files.js', `${m} needs Node`);
    const source = readFileSync(join(dist, m), 'utf8');
    for (const s of specifiers(source)) assert.ok(!s.startsWith('node:'), `${m} imports ${s}`);
  }
  assert.equal(isBrowserModule('cli.js'), false);
  assert.equal(isBrowserModule('transpile/patches.js'), false);
  assert.equal(isBrowserModule('loop.d.ts'), false);
  assert.equal(isBrowserModule('loop.js.map'), false);
});

test('every JavaScript file of dist is either copied or on the Node-only list', () => {
  const js = files(dist).filter((f) => f.endsWith('.js'));
  const copied = new Set(browserModules(dist));
  const nodeOnly = js.filter((f) => !copied.has(f));
  assert.deepEqual(nodeOnly.filter((f) => !f.startsWith('transpile/')).sort(), ['browser-files.js', 'cli.js']);
});

test('the copy has the relative layout, and every relative import inside it resolves inside it', () => {
  withCopy((out, copied) => {
    assert.deepEqual(files(out), copied);
    assert.deepEqual(copied, browserModules(dist));
    for (const file of copied) {
      assert.ok(statSync(join(out, ...file.split('/'))).isFile());
      for (const s of specifiers(readFileSync(join(out, ...file.split('/')), 'utf8'))) {
        if (!s.startsWith('.')) continue;
        const target = relative(out, resolve(out, dirname(file), s)).split(sep).join('/');
        assert.ok(copied.includes(target), `${file} imports ${s}, which is not in the copy`);
      }
    }
  });
});
