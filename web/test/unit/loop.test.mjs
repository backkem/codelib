// The event loop, without a component: what jco's patched timers do under it.

import assert from 'node:assert/strict';
import { test } from 'node:test';

import { GLOBAL_NAME, IdleError, external, install, loopState, onIdle, settle, touch, until } from '../../dist/loop.js';

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
