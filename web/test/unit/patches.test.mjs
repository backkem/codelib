// The transpile step: patches apply to the pinned jco's real output, and the step refuses
// output it cannot patch and any jco but the pinned one.

import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { after, test } from 'node:test';
import { fileURLToPath, pathToFileURL } from 'node:url';

import { JCO_TRANSPILE_VERSION, JCO_VERSION, JcoVersionError, PATCHES, PatchError, applyPatches, locateJco, transpile } from '../../dist/transpile/index.js';

const web = join(dirname(fileURLToPath(import.meta.url)), '../..');
const fixture = (name) => join(web, 'test/fixtures', name);
const tmp = mkdtempSync(join(tmpdir(), 'codelib-web-'));
after(() => rmSync(tmp, { recursive: true, force: true }));

/** jco's unpatched output for a component, straight from its API. */
async function rawOutput(wasm, name) {
  const jco = locateJco();
  const api = await import(pathToFileURL(join(jco.dir, 'dist/api.js')).href);
  const { files } = await api.transpile(new Uint8Array(readFileSync(wasm)), {
    name,
    instantiation: 'async',
    wasiShim: false,
    quiet: true,
    asyncMode: 'jspi',
  });
  return new TextDecoder().decode(files[`${name}.js`]);
}

const names = (applied) => applied.map((a) => a.name);

test('every patch applies to a WASI 0.3 component, and no real timer is left', async () => {
  const manifest = await transpile(fixture('clock-p3.wasm'), { outDir: join(tmp, 'clock'), name: 'clock' });
  assert.deepEqual(names(manifest.patches), ['timers', 'clear-timeouts', 'intervals', 'clear-intervals', 'no-idle-deadlock-trap', 'deterministic-mode', 'no-coin-flips', 'no-waitable-shuffle']);
  assert.equal(manifest.jco, JCO_VERSION);
  const js = readFileSync(join(tmp, 'clock/clock.js'), 'utf8');
  assert.match(js, /^\/\* patched by codelib-web/);
  assert.ok(js.includes('globalThis.__codelibWeb.setTimeout('));
  assert.ok(manifest.coreModules.includes('clock.core.wasm'));
});

test('the guest-to-guest patches apply to a composition', async () => {
  const manifest = await transpile(fixture('pair-p3.wasm'), { outDir: join(tmp, 'pair'), name: 'pair' });
  for (const p of ['prepare-saves-registers', 'deferred-start-restores-registers', 'sync-call-saves-callee-register', 'sync-call-stacks-callee-register', 'sync-call-exit-restores-callee-register', 'sync-call-exit-restores-callee-register-2']) {
    assert.ok(names(manifest.patches).includes(p), `${p} did not apply`);
  }
});

test('unpatched output whose patch anchor moved is rejected', async () => {
  const raw = await rawOutput(fixture('clock-p3.wasm'), 'clock');
  assert.doesNotThrow(() => applyPatches(raw));
  const cases = [
    ['deterministic-mode', '_jcoInitializeAsyncDeterminism("random")', '_jcoInitializeAsyncDeterminism( "random")'],
    ['no-idle-deadlock-trap', 'function __jcoEmbeddedRuntime$checkForDeadlock(context) {\n', 'function __jcoEmbeddedRuntime$checkForDeadlock(context){\n'],
    ['no-coin-flips', '$coinFlip = () => Math.random() > .5;', '$coinFlip = () => (Math.random() > .5);'],
  ];
  for (const [patch, from, to] of cases) {
    assert.ok(raw.includes(from), `fixture output lacks the anchor of ${patch}`);
    assert.throws(() => applyPatches(raw.replace(from, to)), (e) => e instanceof PatchError && e.message.includes(patch), patch);
  }
});

test('a timer or Math.random left over is rejected', async () => {
  const raw = await rawOutput(fixture('clock-p3.wasm'), 'clock');
  assert.throws(() => applyPatches(`${raw}\nqueueMicrotask(() => Date.now());`), /Date\.now/);
  const noTimerPatch = PATCHES.filter((p) => p.name !== 'timers');
  assert.throws(() => applyPatches(raw, noTimerPatch), /setTimeout/);
});

test('patched output is not patched twice', async () => {
  const manifest = await transpile(fixture('clock-p3.wasm'), { outDir: join(tmp, 'twice'), name: 'twice' });
  assert.ok(manifest);
  const js = readFileSync(join(tmp, 'twice/twice.js'), 'utf8');
  assert.throws(() => applyPatches(js), /already patched/);
});

test('call patches leave member calls and longer names alone', () => {
  const { js } = applyPatches('setTimeout(a, 0); x.setTimeout(b); mySetTimeout(c); clearTimeout(d);\n', PATCHES.filter((p) => p.kind === 'call'));
  assert.equal(js, 'globalThis.__codelibWeb.setTimeout(a, 0); x.setTimeout(b); mySetTimeout(c); globalThis.__codelibWeb.clearTimeout(d);\n');
});

function fakeJco(version, transpileVersion) {
  const dir = join(tmp, `jco-${version}-${transpileVersion}`, 'node_modules/@bytecodealliance');
  mkdirSync(join(dir, 'jco'), { recursive: true });
  mkdirSync(join(dir, 'jco-transpile'), { recursive: true });
  writeFileSync(join(dir, 'jco/package.json'), JSON.stringify({ name: '@bytecodealliance/jco', version }));
  writeFileSync(join(dir, 'jco-transpile/package.json'), JSON.stringify({ name: '@bytecodealliance/jco-transpile', version: transpileVersion }));
  return join(dir, 'jco');
}

test('any jco but the pinned one is rejected', async () => {
  for (const [v, tv] of [['1.41.0', JCO_TRANSPILE_VERSION], ['1.43.0', JCO_TRANSPILE_VERSION], [JCO_VERSION, '0.25.0']]) {
    await assert.rejects(
      transpile(fixture('clock-p3.wasm'), { outDir: join(tmp, 'never'), name: 'never', jcoDir: fakeJco(v, tv) }),
      (e) => e instanceof JcoVersionError && e.message.includes(`found jco ${v} (jco-transpile ${tv})`),
    );
  }
});

test('the CLI fails on a wrong jco and on a missing component', () => {
  const cli = join(web, 'dist/cli.js');
  const wrong = spawnSync(process.execPath, [cli, 'transpile', fixture('clock-p3.wasm'), '-o', join(tmp, 'cli'), '--name', 'x'], {
    env: { ...process.env, CODELIB_WEB_JCO: fakeJco('1.37.0', '0.18.0') },
    encoding: 'utf8',
  });
  assert.equal(wrong.status, 1);
  assert.match(wrong.stderr, /written for jco 1\.42\.0.*found jco 1\.37\.0/);
  const ok = spawnSync(process.execPath, [cli, 'transpile', fixture('clock-p3.wasm'), '-o', join(tmp, 'cli'), '--name', 'x'], { encoding: 'utf8' });
  assert.equal(ok.status, 0, ok.stderr);
  assert.match(ok.stdout, /patches: timersx6/);
  const missing = spawnSync(process.execPath, [cli, 'transpile', '-o', join(tmp, 'cli')], { encoding: 'utf8' });
  assert.equal(missing.status, 2);
});
