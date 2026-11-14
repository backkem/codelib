// Where a component's time, entropy and stdio come from.
//
// The WASI imports (wasi/index.ts) never read a clock, draw entropy or write output themselves:
// they ask a `World`. An embedder with its own notion of time (a simulator) implements the
// interface; everyone else picks one of the two built-in worlds, mirroring the native host kit
// (codelib-sandbox): real clocks and entropy by default, or a deterministic world on request.
//
// - `realWorld()`: performance/Date clocks, `crypto.getRandomValues`, real timers.
// - `virtualWorld({ seed })`: a virtual clock that stands still while the component works and
//   jumps to the next timer when the loop reports idle, and a seeded generator. Two runs with
//   one seed see the same clocks and the same random bytes, and a virtual sleep takes no
//   wall-clock time.

import { external, onIdle } from './loop.js';
import { Prng } from './prng.js';

export type RandomStream = 'secure' | 'insecure';

/** What a component's WASI imports read from and write to. Times are nanoseconds. */
export interface World {
  /** Monotonic clock. */
  now(): bigint;
  /** Wall clock: time since the Unix epoch. */
  wallNow(): bigint;
  /** Calls `wake` once the monotonic clock has reached `at`. */
  timer(at: bigint, wake: () => void): void;
  randomU64(stream: RandomStream): bigint;
  randomFill(stream: RandomStream, out: Uint8Array): void;
  insecureSeed(): [bigint, bigint];
  stdout(bytes: Uint8Array): void;
  stderr(bytes: Uint8Array): void;
  /**
   * The next chunk of stdin, or `undefined` at its end. A WASI 0.2 read needs the chunk at once
   * (a promise counts as no data yet); a WASI 0.3 read may wait for it. A promise is waited for
   * with `patient` (loop.ts), so timers still fire under a virtual clock. Without this, stdin is
   * empty.
   */
  stdin?(): Uint8Array | undefined | Promise<Uint8Array | undefined>;
  /** `wasi:config/store`. */
  config?: { get(key: string): string | undefined; all(): [string, string][] };
  args?: string[];
  env?: [string, string][];
  cwd?: string;
  /**
   * Moves the monotonic clock forward to `at` at once, firing the timers on the way. Only a
   * virtual world can; with it, a WASI 0.2 blocking wait on the clock needs no JSPI.
   */
  advanceTo?(at: bigint): void;
}

/** What both built-in worlds take besides their clocks and entropy. */
export interface WorldOptions {
  /** Default: line-buffered to `console.log`. */
  stdout?: (bytes: Uint8Array) => void;
  /** Default: line-buffered to `console.error`. */
  stderr?: (bytes: Uint8Array) => void;
  stdin?: () => Uint8Array | undefined | Promise<Uint8Array | undefined>;
  /** Program arguments; the first is the program's name. Default: none. */
  args?: string[];
  /** Environment variables. Default: none. */
  env?: Record<string, string> | [string, string][];
  cwd?: string;
  /** `wasi:config/store` values. */
  config?: Record<string, string> | [string, string][];
}

export interface VirtualWorldOptions extends WorldOptions {
  /** Seeds both random streams and the insecure seed. */
  seed: bigint | number;
  /** The wall clock at monotonic zero, in ns since the epoch. Default: 2026-01-01T00:00:00Z. */
  wallStart?: bigint;
}

/** A virtual world, with the controls the embedder may want. */
export interface VirtualWorld extends World {
  advanceTo(at: bigint): void;
  /** Fires the earliest pending timer(s), moving the clock to it. False when none is pending. */
  advance(): boolean;
  /** Pending timers. */
  pendingTimers(): number;
  /** Stops answering the loop's idle calls (the world is no longer driven). */
  dispose(): void;
}

const NS_PER_MS = 1_000_000n;
const DEFAULT_WALL_START = 1_767_225_600n * 1_000_000_000n; // 2026-01-01T00:00:00Z, as codelib-sandbox's deterministic clock
const M64 = (1n << 64n) - 1n;

function lineWriter(write: (line: string) => void): (bytes: Uint8Array) => void {
  const decoder = new TextDecoder();
  let buffered = '';
  return (bytes) => {
    buffered += decoder.decode(bytes, { stream: true });
    let nl: number;
    while ((nl = buffered.indexOf('\n')) >= 0) {
      write(buffered.slice(0, nl));
      buffered = buffered.slice(nl + 1);
    }
  };
}

function pairs(v: Record<string, string> | [string, string][] | undefined): [string, string][] {
  if (v === undefined) return [];
  return Array.isArray(v) ? v.map(([k, x]) => [k, x]) : Object.entries(v);
}

function common(opts: WorldOptions): Pick<World, 'stdout' | 'stderr' | 'stdin' | 'config' | 'args' | 'env' | 'cwd'> {
  const config = pairs(opts.config).sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
  const byKey = new Map(config);
  return {
    stdout: opts.stdout ?? lineWriter((line) => console.log(line)),
    stderr: opts.stderr ?? lineWriter((line) => console.error(line)),
    ...(opts.stdin ? { stdin: opts.stdin } : {}),
    config: { get: (key) => byKey.get(key), all: () => config.map(([k, v]) => [k, v]) },
    args: [...(opts.args ?? [])],
    env: pairs(opts.env),
    ...(opts.cwd !== undefined ? { cwd: opts.cwd } : {}),
  };
}

function cryptoFill(out: Uint8Array): void {
  // getRandomValues refuses more than 65 536 bytes per call.
  for (let i = 0; i < out.length; i += 65536) {
    crypto.getRandomValues(out.subarray(i, Math.min(out.length, i + 65536)) as Uint8Array<ArrayBuffer>);
  }
}

function cryptoU64(): bigint {
  const b = new BigUint64Array(1);
  crypto.getRandomValues(b);
  return b[0]!;
}

/**
 * The real world: `performance.now()` for the monotonic clock, `performance.timeOrigin +
 * performance.now()` for the wall clock, `crypto.getRandomValues` for both random streams, and
 * real timers. Timers are `external` work, so the loop waits for them in wall time.
 */
export function realWorld(opts: WorldOptions = {}): World {
  let last = 0n;
  const now = (): bigint => {
    const t = BigInt(Math.floor(performance.now() * 1e6));
    if (t > last) last = t;
    return last;
  };
  return {
    ...common(opts),
    now,
    wallNow: () => BigInt(Math.floor((performance.timeOrigin + performance.now()) * 1e6)),
    timer(at, wake) {
      const ms = Number((at - now()) / NS_PER_MS) + 1;
      void external(new Promise<void>((resolve) => setTimeout(resolve, Math.max(0, ms)))).then(wake);
    },
    randomU64: () => cryptoU64(),
    randomFill: (_stream, out) => cryptoFill(out),
    insecureSeed: () => [cryptoU64(), cryptoU64()],
  };
}

interface Timer {
  at: bigint;
  seq: number;
  wake: () => void;
}

/**
 * A deterministic world. The monotonic clock starts at 0 and moves only when the loop is idle
 * and a timer is pending: it jumps to the earliest timer and fires it. The wall clock is
 * `wallStart` plus the monotonic clock. Both random streams are seeded from `seed` (the insecure
 * one with a different seed), and the insecure seed is `[seed, !seed]`.
 *
 * Registers with the loop's idle handlers; keep one world per Worker, and `dispose()` it if the
 * Worker goes on without it.
 */
export function virtualWorld(opts: VirtualWorldOptions): VirtualWorld {
  const seed = BigInt.asUintN(64, BigInt(opts.seed));
  const secure = new Prng(seed);
  const insecure = new Prng(seed ^ 0x9e3779b97f4a7c15n);
  const wallStart = opts.wallStart ?? DEFAULT_WALL_START;
  let now = 0n;
  let seq = 0;
  const timers: Timer[] = [];

  const fireDue = (): void => {
    timers.sort((a, b) => (a.at < b.at ? -1 : a.at > b.at ? 1 : a.seq - b.seq));
    while (timers.length > 0 && timers[0]!.at <= now) timers.shift()!.wake();
  };

  const world: VirtualWorld = {
    ...common(opts),
    now: () => now,
    wallNow: () => wallStart + now,
    timer(at, wake) {
      timers.push({ at, seq: seq++, wake });
      // A timer that is already due fires at the next idle pass, never synchronously.
    },
    randomU64: (stream) => (stream === 'secure' ? secure : insecure).nextU64(),
    randomFill: (stream, out) => (stream === 'secure' ? secure : insecure).fill(out),
    insecureSeed: () => [seed, ~seed & M64],
    advanceTo(at) {
      if (at > now) now = at;
      fireDue();
    },
    advance() {
      if (timers.length === 0) return false;
      let next = timers[0]!.at;
      for (const t of timers) if (t.at < next) next = t.at;
      if (next > now) now = next;
      fireDue();
      return true;
    },
    pendingTimers: () => timers.length,
    dispose: () => unregister(),
  };
  const unregister = onIdle(() => world.advance());
  return world;
}
