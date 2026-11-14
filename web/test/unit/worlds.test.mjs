// The built-in worlds and the WASI imports built on them, without a component.

import assert from 'node:assert/strict';
import { test } from 'node:test';

import { Prng } from '../../dist/prng.js';
import { WasiExit, wasiImports } from '../../dist/wasi/index.js';
import { realWorld, virtualWorld } from '../../dist/worlds.js';

const quiet = { stdout: () => {}, stderr: () => {} };

test('one seed, one sequence', () => {
  const a = new Prng(42n);
  const b = new Prng(42);
  const xs = Array.from({ length: 4 }, () => a.nextU64());
  assert.deepEqual(xs, Array.from({ length: 4 }, () => b.nextU64()));
  assert.notDeepEqual(xs, Array.from({ length: 4 }, () => new Prng(43n).nextU64()));
});

test('a virtual world is repeatable and starts at 2026-01-01', () => {
  const draw = () => {
    const w = virtualWorld({ seed: 9, ...quiet });
    const bytes = new Uint8Array(13);
    w.randomFill('secure', bytes);
    const out = [w.randomU64('secure'), w.randomU64('insecure'), [...bytes], w.insecureSeed(), w.now(), w.wallNow()];
    w.dispose();
    return out;
  };
  const a = draw();
  assert.deepEqual(a, draw());
  assert.equal(a[4], 0n);
  assert.equal(a[5], 1_767_225_600_000_000_000n);
  assert.notEqual(a[0], a[1], 'the two streams are seeded differently');
});

test('virtual timers fire in order when the world advances', () => {
  const w = virtualWorld({ seed: 1, ...quiet });
  const fired = [];
  w.timer(30n, () => fired.push(['b', w.now()]));
  w.timer(10n, () => fired.push(['a', w.now()]));
  w.timer(30n, () => fired.push(['c', w.now()]));
  assert.equal(w.advance(), true);
  assert.deepEqual(fired, [['a', 10n]]);
  assert.equal(w.advance(), true);
  assert.deepEqual(fired, [['a', 10n], ['b', 30n], ['c', 30n]]);
  assert.equal(w.advance(), false);
  w.dispose();
});

test('the real world reads real clocks and entropy', async () => {
  const w = realWorld(quiet);
  const t0 = w.now();
  assert.ok(Math.abs(Number(w.wallNow() / 1_000_000n) - Date.now()) < 1000);
  assert.notEqual(w.randomU64('secure'), w.randomU64('secure'));
  const big = new Uint8Array(100_000);
  w.randomFill('secure', big);
  assert.ok(big.some((b) => b !== 0));
  await new Promise((r) => w.timer(t0 + 20_000_000n, r));
  assert.ok(w.now() - t0 >= 20_000_000n);
});

test('WASI imports serve 0.2 and 0.3 from one object per interface', async () => {
  const out = [];
  const w = virtualWorld({ seed: 1, stdout: (b) => out.push(new TextDecoder().decode(b)), stderr: () => {}, args: ['prog', 'x'], env: { A: '1' }, config: { b: '2', a: '1' } });
  const wasi = wasiImports(w);
  const mono = wasi['wasi:clocks/monotonic-clock'];
  assert.equal(typeof mono.subscribeDuration, 'function'); // 0.2
  assert.equal(typeof mono.waitFor, 'function'); // 0.3
  assert.deepEqual(wasi['wasi:cli/environment'].getArguments(), ['prog', 'x']);
  assert.deepEqual(wasi['wasi:cli/environment'].getEnvironment(), [['A', '1']]);
  assert.deepEqual(wasi['wasi:config/store'].getAll(), [['a', '1'], ['b', '2']]);
  assert.equal(wasi['wasi:config/store'].get('b'), '2');
  assert.deepEqual(wasi['wasi:clocks/wall-clock'].now(), { seconds: 1_767_225_600n, nanoseconds: 0 });
  assert.deepEqual(wasi['wasi:filesystem/preopens'].getDirectories(), []);
  wasi['wasi:cli/stdout'].getStdout().blockingWriteAndFlush(new TextEncoder().encode('hi'));
  assert.deepEqual(out, ['hi']);
  assert.throws(() => wasi['wasi:cli/exit'].exitWithCode(3), (e) => e instanceof WasiExit && e.code === 3);

  // A 0.2 blocking wait on the virtual clock moves the clock instead of waiting.
  const p = mono.subscribeDuration(5_000_000_000n);
  assert.equal(p.ready(), false);
  const ready = wasi['wasi:io/poll'].poll([p]);
  assert.deepEqual([...ready], [0]);
  assert.equal(w.now(), 5_000_000_000n);
  w.dispose();
});

test('outgoing HTTP is denied by default', () => {
  const wasi = wasiImports(virtualWorld({ seed: 1, ...quiet }));
  const { Fields, OutgoingRequest } = wasi['wasi:http/types'];
  const req = new OutgoingRequest(new Fields());
  req.setAuthority('example.com');
  assert.throws(() => wasi['wasi:http/outgoing-handler'].handle(req, undefined), (e) => e.payload?.tag === 'HTTP-request-denied');
});
