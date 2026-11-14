// Rewrites of jco's generated JavaScript. Each one works around a defect or gap of the pinned
// jco and is documented in docs/jco-patches.md (what it changes, why, when it can go).
//
// Fail loudly: a patch whose `requiredIf` text is in the output must apply, and a multi-line
// patch must apply exactly once. After patching, no bare timer, `Math.random`, `Date.now` or
// `performance.now` call may be left in the output. So a jco upgrade that changes the generated
// code stops the transpile step instead of producing output that silently runs on real timers
// or schedules at random.

/** The global the patched output calls into (loop.ts installs it). */
export const LOOP_GLOBAL = '__codelibWeb';

/** First line of patched output; a second patch run refuses it. */
export const PATCHED_MARKER = '/* patched by codelib-web';

export interface Patch {
  /** Stable name, used in errors and in docs/jco-patches.md. */
  name: string;
  /** Text to replace. */
  from: string;
  /** Replacement. */
  to: string;
  /** The patch must apply when the output contains this text. */
  requiredIf: string;
  /**
   * `call`: `from` is a function call (`setTimeout(`) replaced wherever it is not part of a
   * longer name or a member access, any number of times. `once`: verbatim, exactly once.
   */
  kind: 'call' | 'once';
}

const L = `globalThis.${LOOP_GLOBAL}`;

export const PATCHES: readonly Patch[] = [
  // The loop is the embedder's (jco gap: no way to drive the event loop).
  { name: 'timers', kind: 'call', from: 'setTimeout(', to: `${L}.setTimeout(`, requiredIf: 'runTickLoop() {' },
  { name: 'clear-timeouts', kind: 'call', from: 'clearTimeout(', to: `${L}.clearTimeout(`, requiredIf: 'runTickLoop() {' },
  // Readiness polls of host-side futures; not every output has them (the forbidden-call scan
  // below still catches any that is left unpatched).
  { name: 'intervals', kind: 'call', from: 'setInterval(', to: `${L}.setInterval(`, requiredIf: '' },
  { name: 'clear-intervals', kind: 'call', from: 'clearInterval(', to: `${L}.clearInterval(`, requiredIf: '' },

  // An idle component is not a deadlock: the embedder may still call an export, and it detects
  // real deadlocks itself (loop.ts `until`). jco's check is also a 0 ms timer armed on every
  // idle tick, which an embedder-driven loop would see as endless work.
  {
    name: 'no-idle-deadlock-trap',
    kind: 'once',
    from: 'function __jcoEmbeddedRuntime$checkForDeadlock(context) {\n',
    to: 'function __jcoEmbeddedRuntime$checkForDeadlock(context) {\n\treturn; /* codelib-web: see docs/jco-patches.md */\n',
    requiredIf: '$checkForDeadlock(',
  },

  // Repeatable scheduling.
  {
    name: 'deterministic-mode',
    kind: 'once',
    from: '_jcoInitializeAsyncDeterminism("random")',
    to: '_jcoInitializeAsyncDeterminism("deterministic")',
    requiredIf: 'ASYNC_DETERMINISM',
  },
  {
    name: 'no-coin-flips',
    kind: 'once',
    from: '$coinFlip = () => Math.random() > .5;',
    to: '$coinFlip = () => !1;',
    requiredIf: '$coinFlip',
  },
  {
    name: 'no-waitable-shuffle',
    kind: 'once',
    from: 'sort: Math.random()',
    to: 'sort: 0',
    requiredIf: 'shuffleWaitables() {',
  },

  // The current-task register is left on the wrong task (guest-to-guest calls; compositions only).
  // (a) An async call whose start is deferred gives back the registers `prepareCall` replaced.
  {
    name: 'prepare-saves-registers',
    kind: 'once',
    from:
      '\t__jcoEmbeddedRuntime$setCurrentTaskMeta(_context.registers, {\n' +
      '\t\ttaskID: newTask.id(),\n' +
      '\t\tthreadID: newThread.id(),\n',
    to:
      '\tnewTask.__codelibPrevMeta = { callee: _context.registers[newTask.componentIdx()] ?? null, current: _context.registers.current ?? null };\n' +
      '\t__jcoEmbeddedRuntime$setCurrentTaskMeta(_context.registers, {\n' +
      '\t\ttaskID: newTask.id(),\n' +
      '\t\tthreadID: newThread.id(),\n',
    requiredIf: 'function __jcoEmbeddedRuntime$prepareCall(',
  },
  {
    name: 'deferred-start-restores-registers',
    kind: 'once',
    from: '\t} else if (enteredSynchronously === null) {\n',
    to:
      '\t} else if (enteredSynchronously === null) {\n' +
      '\t\tif (preparedTask.__codelibPrevMeta) {\n' +
      '\t\t\t_context.registers[componentIdx] = preparedTask.__codelibPrevMeta.callee;\n' +
      '\t\t\t_context.registers.current = preparedTask.__codelibPrevMeta.current;\n' +
      '\t\t}\n',
    requiredIf: 'function __jcoEmbeddedRuntime$asyncStartCall(',
  },
  // (b) A sync call's exit gives the callee its register back, before the caller's.
  {
    name: 'sync-call-saves-callee-register',
    kind: 'once',
    from: '\tlet finishEnter = () => {\n\t\t__jcoEmbeddedRuntime$setCurrentTaskMeta(_context.registers, {',
    to:
      '\tlet finishEnter = () => {\n' +
      '\t\tlet __codelibPrevCallee = _context.registers[newTask.componentIdx()] ?? null;\n' +
      '\t\t__jcoEmbeddedRuntime$setCurrentTaskMeta(_context.registers, {',
    requiredIf: 'function __jcoEmbeddedRuntime$symmetricSyncGuestCallEnter(',
  },
  {
    name: 'sync-call-stacks-callee-register',
    kind: 'once',
    from: '\t\t\tpreviousTaskMayBlock: _context.mayBlock.value\n\t\t});',
    to: '\t\t\tpreviousTaskMayBlock: _context.mayBlock.value,\n\t\t\t__codelibPrevCallee\n\t\t});',
    requiredIf: 'function __jcoEmbeddedRuntime$symmetricSyncGuestCallEnter(',
  },
  {
    name: 'sync-call-exit-restores-callee-register',
    kind: 'once',
    from:
      '\tlet { componentIdx, callerThreadID, previousTaskMayBlock } = _context.stack.pop();\n',
    to:
      '\tlet { componentIdx, callerThreadID, previousTaskMayBlock, __codelibPrevCallee } = _context.stack.pop();\n',
    requiredIf: 'function __jcoEmbeddedRuntime$symmetricSyncGuestCallExit(',
  },
  {
    name: 'sync-call-exit-restores-callee-register-2',
    kind: 'once',
    from: '\t_context.mayBlock.value = previousTaskMayBlock;\n\t__jcoEmbeddedRuntime$setCurrentTaskMeta(_context.registers, {',
    to:
      '\t_context.mayBlock.value = previousTaskMayBlock;\n' +
      '\t_context.registers[componentIdx] = __codelibPrevCallee ?? null;\n' +
      '\t__jcoEmbeddedRuntime$setCurrentTaskMeta(_context.registers, {',
    requiredIf: 'function __jcoEmbeddedRuntime$symmetricSyncGuestCallExit(',
  },
];

/** Calls that must not be left in patched output: real time and real randomness. */
export const FORBIDDEN_CALLS = [
  'setTimeout(',
  'clearTimeout(',
  'setInterval(',
  'clearInterval(',
  'Math.random(',
  'Date.now(',
  'performance.now(',
] as const;

export class PatchError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'PatchError';
  }
}

const IDENT = /[A-Za-z0-9_$.]/;

/** Positions where the call `from` occurs as a call of its own (not `x.from`, not `fooFrom`). */
function callSites(js: string, from: string): number[] {
  const out: number[] = [];
  const memberCall = from.includes('.');
  for (let i = js.indexOf(from); i >= 0; i = js.indexOf(from, i + from.length)) {
    const prev = i > 0 ? js[i - 1]! : '';
    if (!memberCall && IDENT.test(prev)) continue;
    if (memberCall && /[A-Za-z0-9_$]/.test(prev)) continue;
    out.push(i);
  }
  return out;
}

function countVerbatim(js: string, from: string): number {
  let n = 0;
  for (let i = js.indexOf(from); i >= 0; i = js.indexOf(from, i + from.length)) n++;
  return n;
}

/** Replaces `from` with `to` at `sites` (ascending). */
function replaceAt(js: string, sites: number[], from: string, to: string): string {
  let out = '';
  let last = 0;
  for (const i of sites) {
    out += js.slice(last, i) + to;
    last = i + from.length;
  }
  return out + js.slice(last);
}

export interface Applied {
  name: string;
  count: number;
}

/**
 * Applies `PATCHES` to one generated module. Throws `PatchError` when a required patch does not
 * apply, a once-patch matches more than once, the output was patched before, or a forbidden call
 * is left over.
 */
export function applyPatches(js: string, patches: readonly Patch[] = PATCHES): { js: string; applied: Applied[] } {
  if (js.startsWith(PATCHED_MARKER)) throw new PatchError('this output is already patched');
  const applied: Applied[] = [];
  for (const p of patches) {
    let count: number;
    if (p.kind === 'call') {
      const sites = callSites(js, p.from);
      count = sites.length;
      js = replaceAt(js, sites, p.from, p.to);
    } else {
      count = countVerbatim(js, p.from);
      if (count > 1) {
        throw new PatchError(`jco patch \`${p.name}\` matches ${count} places, expected one (did jco change? see docs/jco-patches.md)`);
      }
      if (count === 1) js = js.replace(p.from, () => p.to);
    }
    if (count === 0 && p.requiredIf !== '' && js.includes(p.requiredIf)) {
      throw new PatchError(`jco patch \`${p.name}\` no longer applies (did jco change? see docs/jco-patches.md)`);
    }
    if (count > 0) applied.push({ name: p.name, count });
  }
  for (const call of FORBIDDEN_CALLS) {
    const left = callSites(js, call).length;
    if (left > 0) {
      throw new PatchError(`unpatched \`${call}\` left in jco output (${left}x): it would read real time or randomness`);
    }
  }
  return { js, applied };
}
