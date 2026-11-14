// The browser tests, run by run.mjs in headless Chromium. Each test is an async function that
// throws on failure and may return details (numbers worth printing).

import { hasJspi, runInWorker } from '/dist/index.js';

const BUILD = '/test/browser/build';
const tests = new Map();
const test = (name, fn) => tests.set(name, fn);

function check(cond, message) {
  if (!cond) throw new Error(message);
}

/** Runs the clock-p3 fixture (or another WASI 0.3 command) in a Worker. */
function runFixture({ module = `${BUILD}/clock-p3/clock-p3.js`, args, seed, timeoutMs, trace } = {}) {
  const streamed = { stdout: '', stderr: '' };
  const run = runInWorker(new URL('./fixture-worker.js', import.meta.url), {
    init: { module, args, seed, trace },
    timeoutMs,
    startTimeoutMs: 60_000,
    onMessage: ({ stream, chunk }) => (streamed[stream] += chunk),
  });
  return run.outcome.then((outcome) => ({ outcome, streamed }));
}

function ok(outcome) {
  if (outcome.status !== 'ok') throw new Error(`run ${outcome.status}: ${JSON.stringify(outcome.error ?? outcome)}`);
  return outcome.value;
}

test('jspi: the browser has JSPI', async () => {
  check(hasJspi(), 'no JSPI');
});

test('p3 virtual: one seed, the same bytes', async () => {
  const args = ['clock-p3', 'probe', '10000'];
  const a = ok((await runFixture({ args, seed: 42 })).outcome);
  const b = ok((await runFixture({ args, seed: 42 })).outcome);
  const c = ok((await runFixture({ args, seed: 43 })).outcome);
  check(a.failed === null, `run failed: ${a.failed}`);
  check(a.stdout.length > 100, `short output: ${JSON.stringify(a.stdout)}`);
  check(a.stdout === b.stdout && a.stderr === b.stderr, `two runs with seed 42 differ:\n${a.stdout}\n---\n${b.stdout}`);
  check(a.stdout !== c.stdout, 'seeds 42 and 43 print the same bytes');
  check(a.stdout.includes('t0=0 wall=1767225600.000000000'), `virtual clocks start at 0 and 2026-01-01: ${a.stdout}`);
  check(a.stdout.includes('args=clock-p3,probe,10000'), `args: ${a.stdout}`);
  return { stdout: a.stdout.trim() };
});

test('p3 virtual: a 10 s sleep takes no wall-clock time', async () => {
  const t0 = performance.now();
  const r = ok((await runFixture({ args: ['clock-p3', 'probe', '10000'], seed: 7 })).outcome);
  const wall = performance.now() - t0;
  check(r.stdout.includes('fast=+10000000 slow=+30000000'), `concurrent waits at their exact virtual instants: ${r.stdout}`);
  check(r.stdout.includes('slept=10000000000'), `slept exactly 10 virtual seconds: ${r.stdout}`);
  check(r.runMs < 2000, `the run took ${r.runMs} ms of wall-clock time`);
  return { runMs: Math.round(r.runMs), wallMsIncludingLoad: Math.round(wall), instantiateMs: Math.round(r.instantiateMs) };
});

test('p3 real world: real clocks and timers', async () => {
  const r = ok((await runFixture({ args: ['clock-p3', 'probe', '200'] })).outcome);
  check(r.failed === null, `run failed: ${r.failed}`);
  const slept = Number(/slept=(\d+)/.exec(r.stdout)?.[1]);
  check(slept >= 200e6 && slept < 5e9, `slept ${slept} ns for a 200 ms sleep`);
  check(r.runMs >= 190, `a 200 ms sleep took ${r.runMs} ms`);
  const wall = Number(/wall=(\d+)\./.exec(r.stdout)?.[1]);
  check(Math.abs(wall - Date.now() / 1000) < 60, `wall clock ${wall} is not now`);
  const a = /random=(0x[0-9a-f]+)/.exec(r.stdout)?.[1];
  const r2 = ok((await runFixture({ args: ['clock-p3', 'probe', '1'] })).outcome);
  const b = /random=(0x[0-9a-f]+)/.exec(r2.stdout)?.[1];
  check(a !== undefined && a !== b, `real randomness repeats: ${a} ${b}`);
  return { slept, runMs: Math.round(r.runMs) };
});

test('p3 stdio: stdout and stderr are captured separately', async () => {
  const { outcome, streamed } = await runFixture({ args: ['clock-p3', 'probe', '1'], seed: 1 });
  const r = ok(outcome);
  check(r.stderr === 'done\n', `stderr: ${JSON.stringify(r.stderr)}`);
  check(r.stdout.startsWith('t0=') && !r.stdout.includes('done'), `stdout: ${JSON.stringify(r.stdout)}`);
  check(streamed.stdout === r.stdout && streamed.stderr === r.stderr, 'streamed output differs from the result');
  const f = ok((await runFixture({ args: ['clock-p3', 'fail'], seed: 1 })).outcome);
  check(f.failed !== null, 'a run that returns Err did not fail');
  check(f.stderr === 'failing on purpose\n', `stderr of the failing run: ${JSON.stringify(f.stderr)}`);
  return { failed: f.failed };
});

test('p3 deadline: a spinning component is terminated', async () => {
  const t0 = performance.now();
  const { outcome, streamed } = await runFixture({ args: ['clock-p3', 'spin'], seed: 1, timeoutMs: 500 });
  const wall = performance.now() - t0;
  check(outcome.status === 'timeout' && outcome.phase === 'run', `outcome: ${JSON.stringify(outcome)}`);
  check(streamed.stdout === 'spinning\n', `the output before the spin arrives: ${JSON.stringify(streamed.stdout)}`);
  check(wall < 10_000, `took ${wall} ms`);
  return { outcomeMs: outcome.ms };
});

test('pair-p3: a composition (async and sync guest-to-guest calls) runs patched', async () => {
  // The patched code paths run: a deferred async start (the second `work` finds the callee
  // busy) and a sync call's exit. This shows the patches keep these paths working; it does
  // not reproduce the defects they fix, which need a busy composition (docs/jco-patches.md).
  const trace = ['continuing started subtask', '[_symmetricSyncGuestCallExit()] exiting'];
  const r = ok((await runFixture({ module: `${BUILD}/pair-p3/pair-p3.js`, seed: 3, trace })).outcome);
  check(r.failed === null, `run failed: ${r.failed}`);
  check(r.stdout === 'work=2,4 ping=6\n', `stdout: ${JSON.stringify(r.stdout)}`);
  check(r.traced[trace[0]] >= 1, `no deferred start: ${JSON.stringify(r.traced)}`);
  check(r.traced[trace[1]] >= 1, `no sync guest-to-guest call: ${JSON.stringify(r.traced)}`);
  const real = ok((await runFixture({ module: `${BUILD}/pair-p3/pair-p3.js` })).outcome);
  check(real.stdout === 'work=2,4 ping=6\n', `real world stdout: ${JSON.stringify(real.stdout)}`);
});

export const names = () => [...tests.keys()];
export async function run(name) {
  const fn = tests.get(name);
  if (!fn) throw new Error(`no test ${name}`);
  return (await fn()) ?? null;
}
export function register(name, fn) {
  tests.set(name, fn);
}
