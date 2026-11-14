# jco patches

`codelib-web transpile` runs **`@bytecodealliance/jco` 1.42.0** (its code generator,
`@bytecodealliance/jco-transpile`, at **0.24.0**) and rewrites the generated JavaScript. The
rewrites live in one table, `PATCHES` in [`src/transpile/patches.ts`](../src/transpile/patches.ts).
This file says, for each one, what it changes, which jco defect or gap it works around, how to
tell when it can go, and which test in this package covers it.

## How the patches fail loudly

- The transpile step refuses any jco other than 1.42.0 with jco-transpile 0.24.0
  (`JcoVersionError`): the patches match the text one generator version writes.
- Each patch has a `requiredIf` text. When the generated module contains it, the patch must apply,
  or the step fails with `PatchError` naming the patch. A multi-line patch must apply exactly once.
- After patching, no bare `setTimeout(`, `clearTimeout(`, `setInterval(`, `clearInterval(`,
  `Math.random(`, `Date.now(` or `performance.now(` may be left in the module. A jco change that
  adds a new timer or a new source of randomness stops the step instead of running on real time.
- Patched output starts with a `/* patched by codelib-web ... */` line; patching it again fails.

## Moving the pin

1. Bump `@bytecodealliance/jco` and `@bytecodealliance/jco-transpile` in `package.json` (the
   latter is a direct, exact dependency so that a consumer's install resolves jco's caret range to
   the pinned generator), and `JCO_VERSION` / `JCO_TRANSPILE_VERSION` in
   `src/transpile/index.ts`.
2. Run the tests. Every patch whose anchor moved fails by name; read the new output around it and
   decide, per entry below, whether the defect is still there. For the register patches (E) the
   answer comes from the downstream embedder's composed-component scenarios, not from this package's tests.
3. Update this file: an entry per patch, including the ones you drop and why.

jco 1.42.0 embeds its component-model runtime in every output (functions prefixed
`__jcoEmbeddedRuntime$`). Its `--cm-runtime-es-module` option imports the runtime from a shared
module instead; patching that one module would replace patching every output, but the option is
marked experimental and the kit does not use it yet.

## The table

| Patch | Status in 1.42.0 | Upstream defect (see below) | Covered by a test here |
|---|---|---|---|
| `timers` | kept | A: embedder cannot drive the loop | yes |
| `clear-timeouts` | **new** | A | unit (patch text) |
| `intervals` | kept | A | unit (patch text) |
| `clear-intervals` | kept | A | unit (patch text) |
| `no-idle-deadlock-trap` | ported | B: trap on embedder idle | yes |
| `deterministic-mode` | ported | C: scheduling randomised on purpose | partly |
| `no-coin-flips` | ported | C | unit (scan) |
| `no-waitable-shuffle` | kept, now guards dead code | C | unit (scan) |
| `deferred-start-task` | **dropped** | D: deferred async start without a current task | n/a (fixed upstream; a downstream scenario passes without it) |
| `prepare-saves-registers` | ported | E: current-task register left on the wrong task | required (see E); gate lives with a downstream embedder |
| `deferred-start-restores-registers` | ported | E | required (see E); gate lives with a downstream embedder |
| `sync-call-saves-callee-register` | ported | E | required (see E); gate lives with a downstream embedder |
| `sync-call-stacks-callee-register` | ported | E | required (see E); gate lives with a downstream embedder |
| `sync-call-exit-restores-callee-register` | ported | E | required (see E); gate lives with a downstream embedder |
| `sync-call-exit-restores-callee-register-2` | ported | E | required (see E); gate lives with a downstream embedder |

The unit tests check that each patch applies to the transpiled composition
`test/fixtures/pair-p3.wasm`, and the browser test `pair-p3` runs that composition through the
patched code path (a deferred async start and a sync guest-to-guest call, confirmed from jco's own
debug log) and checks its output. That shows the patches apply and do no harm; it does not show
that they are needed, because this package's own fixtures do not reproduce the defect. The
evidence that they are needed is in section E, and the scenarios that gate them live with a
downstream embedder.

## Defects and gaps behind the patches

### A. An embedder cannot drive jco's event loop

jco's component-model async runtime schedules itself with timers: a tick loop that resumes
suspended tasks (`runTickLoop`: 10 ms when idle, 0 ms after a resume), 0 ms deferred task starts,
a 0 ms deadlock check, `setTimeout(resolve)` yields in stream code, and `setInterval` readiness
polls when the host reads or writes a future. On real timers, an embedder can only wait for
wall-clock quiet to guess that a component is idle, and a virtual clock can never move.

- **`timers`**: every `setTimeout(` becomes `globalThis.__codelibWeb.setTimeout(` (the loop in
  `src/loop.ts`: 0 ms is work, more is an idle poll).
- **`clear-timeouts`** (new in 1.42.0): every `clearTimeout(` becomes
  `globalThis.__codelibWeb.clearTimeout(`. jco 1.42.0 stops its tick loop and its deadlock check
  with `clearTimeout` (`ComponentAsyncState#stop`, `stopInstance`); without this patch it would
  hand a loop id to the real `clearTimeout`, which cancels nothing, or an unrelated real timer
  that happens to have the same number.
- **`intervals`**, **`clear-intervals`**: `setInterval(` / `clearInterval(` likewise (the host's
  future reads and writes poll for the guest's side with an interval).

Upstream: not filed. jco has no API to run its loop to idle. Drop these when jco lets an embedder
supply the scheduler (or exposes "run until idle"), and replace them with that API.

### B. jco traps on embedder-idle

`checkForDeadlock` runs on a 0 ms timer whenever a task suspends or the tick loop finds nothing to
resume. In 1.42.0 it no longer traps while a host import's promise is pending (an improvement over
earlier versions, which trapped as soon as no host import was pending), but it still traps
"deadlock detected" when a root task waits on guest work and no host operation is in flight,
although an embedder may call an export later that provides that work (wasmtime treats this as
idle). Under the kit's loop it also costs more than it checks: armed on every idle tick, its 0 ms
timer is new work on every pass, so `settle()` never sees an idle pass. Before this patch was
ported, every virtual-world test failed with "the component did not settle".

- **`no-idle-deadlock-trap`**: `checkForDeadlock` returns at once. The embedder detects real
  deadlocks: `until()` fails with `IdleError` when the component is idle, no idle handler (virtual
  clock) can make progress, and the awaited call is still pending.

Drop it when jco's deadlock check can be turned off or delegated to the embedder, and no longer
schedules timers while idle. Test: every virtual-world browser test (they hang without it).

### C. Async scheduling is randomised on purpose

The generated module starts its runtime with `_jcoInitializeAsyncDeterminism("random")`; in that
mode a task whose wait is already satisfied flips a coin (`Math.random()`) to decide whether to
suspend anyway. A `"deterministic"` mode exists in the generator, but the CLI and API have no
option for it (1.42.0's `jco transpile --help` lists none).

- **`deterministic-mode`**: `"random"` becomes `"deterministic"`. Covered in part: every test runs
  with it, and the repeatability tests (one seed, the same bytes) pass; no fixture here shows a
  difference without it, because the fixtures' output depends on virtual time, not on the order
  of work inside one instant.
- **`no-coin-flips`**: the coin flip returns `false`. In deterministic mode it is never called;
  the patch keeps the forbidden-call scan strict and keeps scheduling repeatable should the mode
  check move.
- **`no-waitable-shuffle`**: `WaitableSet#shuffleWaitables` sorts by `Math.random()`. In 1.42.0
  nothing calls it any more (it was live in earlier versions); the patch rewrites the dead code
  so the forbidden-call scan can stay strict. Drop it when jco removes the method.

Upstream: not filed (ask for a deterministic option). Drop all three when the option exists and
the transpile step passes it.

### D. A deferred async guest-to-guest call started without a current task (dropped)

When an async-lowered call found the callee instance's exclusive lock held, `asyncStartCall`
deferred the start to a timer, and that deferred path ran the fused start adapter outside any
current-task register: its `context.get` threw, the callee was left unable to leave, and every
later callback of that instance trapped "cannot leave component instance".

- **`deferred-start-task`**: **dropped**. jco 1.42.0's deferred path calls
  `withCurrentTaskMeta({ taskID: preparedTask.id(), ... fn: startSubtask })`, which is exactly
  what this patch did. The `pair-p3` test runs a deferred start (confirmed from jco's debug log)
  without it. A downstream scenario that needed this patch on jco 1.37.0 passes without it on
  1.42.0.

### E. The current-task register is left on the wrong task

jco keeps one "current task" register per component instance (`registers[componentIdx]`, plus
`registers.current`); `context.get`/`context.set` and import attribution read it. A callback sets
it when it starts, and nothing sets it again when the callback's slice resumes after a JSPI
suspension. Under JSPI a callback is suspended at almost every import (V8 suspends at every
`WebAssembly.Suspending` call, even one whose JavaScript returns a plain value). Meanwhile sibling
instances may call into the suspended one, and two kinds of entry leave its register behind:

(a) `prepareCall` makes an async call's new task the callee's current task. When the callee is
busy, `asyncStartCall` defers the start, and the register stays on a task that has not started.
The suspended slice then resumes under it: its `context.set` writes into the unstarted task, whose
own start later finds a context already set and panics (wit-bindgen's
`task_state::get().is_null()` assertion).

- **`prepare-saves-registers`**: `prepareCall` saves the callee's and the global register on the
  new task before replacing them.
- **`deferred-start-restores-registers`**: when the start is deferred, the saved registers are put
  back; the deferred start sets its own when it runs.

(b) `symmetricSyncGuestCallExit` gives the caller its register back but leaves the callee's on the
sync call's finished task. A slice of the callee that was suspended during the call resumes under
a dead task: its `context.set` goes nowhere and its waker state is lost, so it never wakes again.

- **`sync-call-saves-callee-register`**: the enter path saves the callee's register,
- **`sync-call-stacks-callee-register`**: keeps it on the sync call stack,
- **`sync-call-exit-restores-callee-register`** and **`-2`**: the exit path takes it off the stack
  and restores it before restoring the caller's (the caller's wins when caller and callee are one
  component).

1.42.0 restructured these functions (registers now carry a thread id; `withCurrentTaskMetaAsync`
clears a register when an async callee finishes), but neither entry path restores what it
replaced, and a resumed slice still does not re-establish its register, so both patches are
ported to the new text.

**The defect is still present in 1.42.0 and the six patches are required.** Downstream,
composed components re-transpiled with this kit's options but with these patches left out fail
under load in scenarios where a component is suspended inside a callback while sibling
components call into it (clients that half-close with their request while a load balancer checks
its backends). Leaving out `prepare-saves-registers` and `deferred-start-restores-registers`
breaks both of two such scenarios; leaving out the four `sync-call-*` patches breaks one of the
two; with all six applied both pass. These scenarios live with a downstream embedder, not here,
and they are the gate for these patches whenever the jco pin moves: this package's own fixtures
do not reproduce the defect, so a green run here says nothing about it.

Upstream: not filed. A robust fix belongs in jco: re-establish the
register when a suspended slice resumes, or keep it per task stack. Drop these patches when jco
does that; to check, run a composition whose callback-based component is suspended inside an
import while siblings make async and sync calls into it (the defect showed as a component that
stopped answering after its first request under load).

## Other jco behaviour an embedder should know (no patch)

Checked against 1.42.0 where marked; otherwise carried over from earlier versions, unverified.

- **Import and export naming** (checked): the import object is keyed by unversioned interface
  name, and the 0.2 and 0.3 versions of an interface share one object (`wasi:cli/stdout` holds
  both `getStdout` and `writeViaStream`). Exports are camel-cased; interface exports appear under
  their short name. `normalizeImports` and `findExport` (`codelib-web/load`) handle both.
- **JSPI everywhere** (checked): every async lift and blocking builtin goes through
  `WebAssembly.promising` / `WebAssembly.Suspending`.
- **A host's async import is never told about cancellation**: a guest that drops a pending
  `wait-for` leaves the host's timer queued; it fires later and resolves a promise nobody awaits.
- **An async export does not run to its first suspension when called**: the order of work inside
  one instant follows jco, not wasmtime, so a browser run is repeatable but not byte-identical to
  a native one.
- **Module-level state is shared** between instances of one transpiled module: keep one world
  per Worker.
- **Host-provided streams are read one element at a time by default** (checked):
  `stream.read()` without a count reads one byte of a `stream<u8>`; the kit's WASI reads with a
  count of 64 KiB.
- **Output size**: earlier generators could not emit a file of 2^28 bytes or more (each output
  file crosses the generator component's boundary as a `list<u8>`); not re-checked with 0.24.0.
