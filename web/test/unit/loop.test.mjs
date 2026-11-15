// The event loop, without a component: what jco's patched timers do under it.

import assert from 'node:assert/strict';
import { test } from 'node:test';

import { GLOBAL_NAME, IdleError, external, install, loopState, onIdle, patient, settle, touch, until } from '../../dist/loop.js';
import { virtualWorld } from '../../dist/worlds.js';

const quiet = { stdout: () => {}, stderr: () => {} };

install();
const env = globalThis[GLOBAL_NAME];

test('install puts the timer environment on the neutral global', () => {
  assert.equal(GLOBAL_NAME, '__codelibWeb');
  for (const f of ['setTimeout', 'clearTimeout', 'setInterval', 'clearInterval']) assert.equal(typeof env[f], 'function');
});

test('settle runs work before idle polls, and returns when nothing changes', async () => {
  const order = [];
  env.setTimeout(() => order.push('idle'), 10);
  env.setTimeout(() => {
    order.push('work-1');
    env.setTimeout(() => order.push('work-2'));
  }, 0);
  await settle();
  assert.deepEqual(order, ['work-1', 'work-2', 'idle']);
  assert.equal(loopState().work + loopState().idle, 0);
});

test('a cleared timeout never runs', async () => {
  let ran = false;
  const id = env.setTimeout(() => (ran = true), 0);
  env.clearTimeout(id);
  env.clearTimeout(undefined);
  await settle();
  assert.equal(ran, false);
});

test('an interval polls once per idle pass; a pass where nothing changed is idle', async () => {
  let polls = 0;
  const id = env.setInterval(() => {
    polls++;
    touch(); // something visible happened, so the loop goes on
    if (polls === 3) env.clearInterval(id);
  });
  await settle();
  assert.equal(polls, 3);
  assert.equal(loopState().intervals, 0);

  let idlePolls = 0;
  const quiet = env.setInterval(() => idlePolls++);
  await settle();
  assert.equal(idlePolls, 1, 'an interval that changes nothing does not keep the loop busy');
  env.clearInterval(quiet);
});

test('settle waits for external work in flight', async () => {
  let done = false;
  external(new Promise((r) => setTimeout(r, 30))).then(() => (done = true));
  await settle();
  assert.equal(done, true);
});

test('until drives queued work that the awaited call depends on', async () => {
  const call = new Promise((resolve) => env.setTimeout(() => env.setTimeout(() => resolve('done'), 10), 0));
  assert.equal(await until(call), 'done');
});

test('until asks the idle handlers, then fails when nothing can progress', async () => {
  let wake = null;
  const call = new Promise((resolve) => (wake = resolve));
  const off = onIdle(() => {
    if (!wake) return false;
    const w = wake;
    wake = null;
    touch();
    w('woken');
    return true;
  });
  assert.equal(await until(call), 'woken');
  off();
  await assert.rejects(until(new Promise(() => {})), IdleError);
});

test('until passes a rejection through', async () => {
  await assert.rejects(until(Promise.reject(new Error('boom'))), /boom/);
});

test('patient: a pending virtual timer fires first, and virtual time advances', async () => {
  const world = virtualWorld({ seed: 1, ...quiet });
  try {
    const order = [];
    let input;
    const stdin = patient(new Promise((resolve) => (input = resolve)));
    world.timer(5_000_000_000n, () => {
      touch();
      order.push(['timer', world.now()]);
      // The input arrives in wall time, only once the timer is out of the way.
      setTimeout(() => input('line'), 20);
    });
    const call = stdin.then((line) => {
      order.push(['input', line]);
      return line;
    });
    assert.equal(await until(call), 'line');
    assert.deepEqual(order, [['timer', 5_000_000_000n], ['input', 'line']]);
    assert.equal(world.now(), 5_000_000_000n);
    assert.equal(loopState().patient, 0);
  } finally {
    world.dispose();
  }
});

test('patient: with nothing else pending, until waits for it and no IdleError is raised', async () => {
  let input;
  const stdin = patient(new Promise((resolve) => (input = resolve)));
  setTimeout(() => input('typed'), 30);
  assert.equal(await until(stdin), 'typed');
  assert.equal(loopState().patient, 0);
});

test('patient: settle does not wait for it, so an embedder that owns time gets control back', async () => {
  // A simulator calls settle() between instants; a keystroke is one of its own events and can
  // only arrive after settle() has returned. A pending stdin read must not hold settle().
  let keystroke;
  let read = null;
  void patient(new Promise((resolve) => (keystroke = resolve))).then((line) => (read = line));
  let handlerRan = false;
  const off = onIdle(() => (handlerRan = true));
  try {
    await settle();
    assert.equal(read, null, 'settle came back with the read still pending');
    assert.equal(loopState().patient, 1);
    assert.equal(handlerRan, false, 'settle does not run the idle handlers either');

    // The embedder moves on, delivers the keystroke, and settles again.
    keystroke('k');
    await settle();
    assert.equal(read, 'k');
    assert.equal(loopState().patient, 0);
  } finally {
    off();
  }
});

test('patient: settle leaves a virtual clock alone while a read is pending', async () => {
  const world = virtualWorld({ seed: 3, ...quiet });
  let release;
  try {
    let fired = false;
    world.timer(1_000_000_000n, () => (fired = true));
    patient(new Promise((resolve) => (release = resolve)));
    await settle();
    assert.equal(fired, false);
    assert.equal(world.now(), 0n);
  } finally {
    release();
    await settle();
    world.dispose();
  }
});

test('patient: until returns when its call is done even though a patient promise is still pending', async () => {
  let release;
  patient(new Promise((resolve) => (release = resolve)));
  assert.equal(await until(Promise.resolve('done')), 'done');
  assert.equal(await until(new Promise((resolve) => env.setTimeout(() => resolve('queued'), 0))), 'queued');
  assert.equal(loopState().patient, 1);
  release();
  await settle();
});

test('external keeps holding virtual time still while it is pending', async () => {
  const world = virtualWorld({ seed: 2, ...quiet });
  try {
    let fired = false;
    world.timer(1_000_000_000n, () => (fired = true));
    let done = false;
    external(new Promise((r) => setTimeout(r, 30))).then(() => (done = true));
    await settle();
    assert.equal(done, true);
    assert.equal(fired, false, 'settle does not move the virtual clock');
    assert.equal(world.now(), 0n);
    await assert.rejects(until(new Promise(() => {})), IdleError, 'nothing patient is pending, so a stuck call is still a deadlock');
  } finally {
    world.dispose();
  }
});

test('patient: a rejection does not break the loop', async () => {
  const failing = patient(Promise.reject(new Error('input failed')));
  await assert.rejects(failing, /input failed/);
  assert.equal(await until(Promise.resolve(7)), 7);
  await settle();

  let reject;
  const late = patient(new Promise((_, r) => (reject = r)));
  setTimeout(() => reject(new Error('later')), 20);
  await assert.rejects(until(late), /later/);
  assert.equal(loopState().patient, 0);
});
