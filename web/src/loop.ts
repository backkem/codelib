// The event loop jco's generated code runs on.
//
// jco's component-model async runtime schedules itself with timers: a 10 ms tick loop that
// resumes suspended tasks, 0 ms re-ticks, deferred task starts, a deadlock check, and
// `setInterval` readiness polls for host-side futures. On real timers an embedder cannot tell
// when the component is idle (it can only wait for wall-clock quiet), and virtual time cannot
// move. The transpile step rewrites every timer call in jco's output to this module's
// environment on `globalThis.__codelibWeb` (see docs/jco-patches.md), so the embedder owns them:
//
// - `setTimeout(fn)` / `setTimeout(fn, 0)` is *work*: it runs at the next settle pass.
// - `setTimeout(fn, ms > 0)` is an *idle poll* (the tick loop with nothing to resume): it runs
//   once per pass when there is no work.
// - `setInterval(fn)` is a readiness poll; it runs on every idle pass until cleared.
//
// `settle()` runs work until none is left, then gives the idle polls one pass. If that changed
// nothing (no host import was called, no work appeared, no `external` work is in flight), the
// component is idle. `until(promise)` drives the loop until a call into the component finishes;
// when the component is idle and the call is still pending, the idle handlers (a virtual clock
// jumping to its next timer) get a chance to make progress. Nothing here waits for wall-clock
// time, except for `patient` work (below), which it waits for only when nothing else can move.

type Fn = () => void;

/** The timer functions jco's patched output calls, on `globalThis.__codelibWeb`. */
export interface LoopEnv {
  setTimeout(fn: Fn, ms?: number): number;
  clearTimeout(id: number | undefined | null): void;
  setInterval(fn: Fn, ms?: number): number;
  clearInterval(id: number | undefined | null): void;
}

/** Name of the global the patched jco output calls into. */
export const GLOBAL_NAME = '__codelibWeb';

const work = new Map<number, Fn>();
const idle = new Map<number, Fn>();
const intervals = new Map<number, Fn>();
const pending = new Set<Promise<unknown>>();
const patients = new Set<Promise<unknown>>();
const idleHandlers = new Set<() => boolean>();
let nextId = 1;
let activity = 0;

/** Counts something the outside world can see: a host import call, a call completing. */
export function touch(): void {
  activity++;
}

const env: LoopEnv = {
  setTimeout(fn, ms) {
    const id = nextId++;
    (ms !== undefined && ms > 0 ? idle : work).set(id, fn);
    return id;
  },
  clearTimeout(id) {
    if (id == null) return;
    work.delete(id);
    idle.delete(id);
  },
  setInterval(fn) {
    const id = nextId++;
    intervals.set(id, fn);
    return id;
  },
  clearInterval(id) {
    if (id != null && intervals.delete(id)) activity++;
  },
};

/**
 * Installs the loop's environment on `globalThis.__codelibWeb`. Must run before a transpiled
 * component's code runs its first timer; `instantiate` (load.ts) calls it for you. Idempotent.
 */
export function install(): void {
  const g = globalThis as unknown as Record<string, unknown>;
  if (g[GLOBAL_NAME] !== env) g[GLOBAL_NAME] = env;
}

// A macrotask without timer clamping: every microtask (promise continuation, JSPI resumption)
// runs before it. Created on first use, and unref'd under Node so it does not keep a process
// (a unit test) alive.
let channel: MessageChannel | null = null;
const waiting: Fn[] = [];
function macrotask(): Promise<void> {
  if (channel === null) {
    channel = new MessageChannel();
    channel.port1.onmessage = () => waiting.shift()?.();
    (channel.port1 as unknown as { unref?: () => void }).unref?.();
  }
  const port = channel.port2;
  return new Promise((resolve) => {
    waiting.push(resolve);
    port.postMessage(0);
  });
}

// Runs what was queued when the pass began, in order. An entry cleared by an earlier callback
// of the same pass does not run; entries queued meanwhile wait for the next pass.
function runQueue(queue: Map<number, Fn>): void {
  for (const id of [...queue.keys()]) {
    const fn = queue.get(id);
    if (fn === undefined) continue;
    queue.delete(id);
    runOne(fn);
  }
}

function runOne(fn: Fn): void {
  try {
    fn();
  } catch (error) {
    // A throwing timer callback does not stop the others, as with real timers.
    queueMicrotask(() => {
      throw error;
    });
  }
}

/**
 * Lets the component run until it is idle: no queued work, idle polls changed nothing, and no
 * `external` work is in flight. Resolves to the number of passes it took. Throws if it does not
 * settle within `maxPasses` (a component that keeps itself busy).
 */
export function settle(maxPasses = 1_000_000): Promise<number> {
  return settlePasses(maxPasses, true);
}

// `waitForPatient` is for the public `settle`: with only `patient` work left it lets the idle
// handlers move things along, then waits for the patient promise. `until` passes false because
// it must come back as soon as its call is done, however long the patient promise stays pending.
async function settlePasses(maxPasses: number, waitForPatient: boolean): Promise<number> {
  for (let pass = 1; pass <= maxPasses; pass++) {
    await macrotask();
    if (work.size > 0) {
      runQueue(work);
      continue;
    }
    const before = activity;
    runQueue(idle);
    for (const fn of [...intervals.values()]) runOne(fn);
    await macrotask();
    if (work.size > 0 || activity !== before) continue;
    if (pending.size > 0) {
      // Real-world work in flight (a fetch, a real timer): wait for it in wall time.
      await Promise.race(pending);
      continue;
    }
    if (waitForPatient && patients.size > 0) {
      // Nothing else can move: let a virtual clock jump first, and wait in wall time only when
      // it cannot.
      if (!runIdleHandlers()) await Promise.race(patients);
      continue;
    }
    return pass;
  }
  throw new Error(`the component did not settle within ${maxPasses} passes (is it busy-looping?)`);
}

/**
 * Registers a handler that is asked to make progress when the component is idle and a call is
 * still pending (a virtual clock firing its next timer). It returns whether it did anything.
 * Returns a function that unregisters it.
 */
export function onIdle(handler: () => boolean): () => void {
  idleHandlers.add(handler);
  return () => {
    idleHandlers.delete(handler);
  };
}

function runIdleHandlers(): boolean {
  for (const handler of [...idleHandlers]) {
    if (handler()) return true;
  }
  return false;
}

/** Thrown by `until` when the component is idle and the awaited call can never finish. */
export class IdleError extends Error {
  constructor() {
    super('the component is idle and the call is still waiting: nothing can ever finish it (a deadlock)');
    this.name = 'IdleError';
  }
}

/**
 * Waits for `call`, a promise from a call into a transpiled component (an async export, or any
 * export jco wrapped for JSPI), while driving the loop as `settle()` does: the call's progress
 * may depend on queued work that only runs here. When the component goes idle and `call` is
 * still pending, the idle handlers get a chance (virtual time moves to the next timer); if none
 * of them can do anything, fails with `IdleError`.
 */
export async function until<T>(call: Promise<T>): Promise<T> {
  let done = false;
  const tracked = call.then(
    (value) => {
      done = true;
      return value;
    },
    (error: unknown) => {
      done = true;
      throw error;
    },
  );
  tracked.catch(() => undefined);
  for (let idleRounds = 0; ; ) {
    await macrotask();
    if (done) return tracked;
    await settlePasses(1_000_000, false);
    await macrotask();
    if (done) return tracked;
    if (runIdleHandlers()) {
      idleRounds = 0;
      continue;
    }
    if (patients.size > 0) {
      // Only patient work can move things now (an interactive guest's pending stdin read).
      await Promise.race([...patients, tracked.catch(() => undefined)]);
      idleRounds = 0;
      continue;
    }
    if (++idleRounds >= 3) throw new IdleError();
  }
}

/**
 * Marks `work` as real-world work the component waits on (a `fetch`, a real timer): `settle()`
 * does not report idle while it is in flight, so virtual time stands still until it is done.
 * Returns `work`.
 */
export function external<T>(work: Promise<T>): Promise<T> {
  const tracked: Promise<unknown> = work.then(
    () => undefined,
    () => undefined,
  );
  pending.add(tracked);
  void tracked.then(() => {
    pending.delete(tracked);
    activity++;
  });
  return work;
}

/**
 * Marks `work` as something to wait for in wall time only when nothing else can make progress.
 * Unlike `external`, a pending patient promise does not stop the idle handlers: a virtual clock
 * still jumps to its next timer, and `until` does not fail with `IdleError`. When no queued
 * work, no `external` work and no idle handler can move things along, `settle` and `until` wait
 * for the first pending patient promise to settle (a rejection counts as settling) and go on.
 *
 * This is for input that arrives from outside at an unknown time, such as an interactive
 * guest's stdin read, which is always pending under a virtual clock: timers the guest is
 * waiting on fire first, and the loop waits for the input only when no timer is left. Wrap
 * a promise you want waited for like that, as `external` wraps one that must hold time still.
 * Returns `work`.
 */
export function patient<T>(work: Promise<T>): Promise<T> {
  const tracked: Promise<unknown> = work.then(
    () => undefined,
    () => undefined,
  );
  patients.add(tracked);
  void tracked.then(() => {
    patients.delete(tracked);
    activity++;
  });
  return work;
}

/** Counters, for tests and diagnostics. */
export function loopState(): { work: number; idle: number; intervals: number; external: number; patient: number; activity: number } {
  return { work: work.size, idle: idle.size, intervals: intervals.size, external: pending.size, patient: patients.size, activity };
}
