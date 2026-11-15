// Virtual WASI for one component instance, built from a `World` (worlds.ts).
//
// Covers WASI 0.2.x and 0.3.0: clocks, random, cli (stdio, environment, exit, terminals),
// wasi:config/store, wasi:io for stdio and bodies, an empty filesystem (no preopens, so no
// descriptor ever exists) and wasi:http's client side over a handler that denies everything
// unless the embedder supplies one. No sockets. Nothing here reads a clock, draws entropy or
// touches the network itself.
//
// jco keys its import object by unversioned interface name and merges the 0.2 and 0.3 versions
// of an interface into one object (wasi:clocks/monotonic-clock has `subscribe-duration` from 0.2
// and `wait-for` from 0.3), so each object below serves both versions.

import { patient, touch } from '../loop.js';
import type { World } from '../worlds.js';
import { createHttp, denyAll, type HttpHandler } from './http.js';
import { EMPTY, createIo, pullSource } from './io.js';

export { CLOSED, bytesSource, createIo, pullSource, type ByteSink, type ByteSource, type Io } from './io.js';
export { HttpError, createHttp, denyAll, type HttpErrorCode, type HttpHandler, type HttpRequest, type HttpResponse } from './http.js';
export { egress, type EgressOptions, type EgressPolicy, type Rule, type VirtualHost } from './egress.js';

/** Thrown by `wasi:cli/exit`: the component asked to end with `code`. It unwinds the guest. */
export class WasiExit extends Error {
  constructor(readonly code: number) {
    super(`the component exited with code ${code}`);
    this.name = 'WasiExit';
  }
}

export interface WasiOptions {
  /** Answers the component's outgoing HTTP requests. Default: `denyAll`. */
  http?: HttpHandler;
}

const NS = 1_000_000_000n;

/** The import object (unversioned interface names, as jco expects) for one instance. */
export function wasiImports(world: World, options: WasiOptions = {}): Record<string, Record<string, unknown>> {
  const io = createIo(world);
  const { Pollable, InputStream, OutputStream } = io;
  const http = createHttp(io, options.http ?? denyAll);

  const stdout = new OutputStream((b) => world.stdout(b));
  const stderr = new OutputStream((b) => world.stderr(b));
  // Input that is not there yet is waited for `patient`ly: under a virtual clock an interactive
  // guest always has a read pending, and its timers must still fire before `until` waits for it.
  // `settle()` does not wait for it, so an embedder that owns time is not held by a pending read.
  const nextStdin = () => {
    const next = world.stdin!();
    return next instanceof Promise ? patient(next) : next;
  };
  const stdinSource = world.stdin ? pullSource(nextStdin) : EMPTY;
  const stdin = new InputStream(stdinSource);

  const sleepUntil = (at: bigint): Promise<void> =>
    new Promise((resolve) =>
      world.timer(at, () => {
        touch();
        resolve();
      }),
    );

  const wall = () => {
    const ns = world.wallNow();
    return { seconds: ns / NS, nanoseconds: Number(ns % NS) };
  };

  // WASI 0.3: drain a stream<u8> the component hands over, until it closes it.
  const drain = async (stream: { read(n: number): Promise<{ value?: Uint8Array; done: boolean }>; return?(): unknown }, sink: (b: Uint8Array) => void) => {
    try {
      for (;;) {
        const { value, done } = await stream.read(65536);
        if (done) break;
        if (value && value.length > 0) {
          touch();
          sink(value.slice());
        }
      }
    } finally {
      await stream.return?.();
    }
    return { tag: 'ok', val: undefined };
  };

  // WASI 0.3 stdin: a stream of the world's stdin chunks and a future for its end.
  const readViaStream = () => {
    let end!: (v: unknown) => void;
    const done = new Promise((resolve) => (end = resolve));
    async function* chunks() {
      try {
        if (!world.stdin) return;
        for (;;) {
          const chunk = await nextStdin();
          if (chunk === undefined) return;
          yield chunk;
        }
      } finally {
        end({ tag: 'ok', val: undefined });
      }
    }
    return [chunks(), done];
  };

  const exit = (code: number): never => {
    throw new WasiExit(code);
  };

  class TerminalInput {}
  class TerminalOutput {}
  // No preopens: no descriptor is ever handed out, so none of its methods is reachable.
  class Descriptor {}
  class DirectoryEntryStream {}

  const config = world.config;
  return {
    ...io.imports,
    ...http.imports,
    'wasi:clocks/monotonic-clock': {
      now: () => world.now(),
      resolution: () => 1n,
      getResolution: () => 1n,
      subscribeInstant: (at: bigint) => Pollable.deadline(at),
      subscribeDuration: (d: bigint) => Pollable.deadline(world.now() + d),
      waitUntil: (at: bigint) => sleepUntil(at),
      waitFor: (d: bigint) => sleepUntil(world.now() + d),
    },
    'wasi:clocks/wall-clock': {
      now: wall,
      resolution: () => ({ seconds: 0n, nanoseconds: 1 }),
    },
    'wasi:clocks/system-clock': {
      now: wall,
      getResolution: () => 1n,
    },
    'wasi:clocks/types': {},
    'wasi:clocks/timezone': {
      ianaId: () => 'UTC',
      utcOffset: () => 0n,
      toDebugString: () => 'UTC',
    },
    'wasi:random/random': {
      getRandomU64: () => world.randomU64('secure'),
      getRandomBytes: (len: bigint) => {
        const out = new Uint8Array(Number(len));
        world.randomFill('secure', out);
        return out;
      },
    },
    'wasi:random/insecure': {
      getInsecureRandomU64: () => world.randomU64('insecure'),
      getInsecureRandomBytes: (len: bigint) => {
        const out = new Uint8Array(Number(len));
        world.randomFill('insecure', out);
        return out;
      },
    },
    'wasi:random/insecure-seed': {
      insecureSeed: () => world.insecureSeed(),
      getInsecureSeed: () => world.insecureSeed(),
    },
    'wasi:cli/environment': {
      getEnvironment: () => (world.env ?? []).map(([k, v]) => [k, v]),
      getArguments: () => [...(world.args ?? [])],
      initialCwd: () => world.cwd,
      getInitialCwd: () => world.cwd,
    },
    'wasi:cli/exit': {
      exit: (status: { tag: string }) => exit(status?.tag === 'ok' ? 0 : 1),
      exitWithCode: (code: number) => exit(code),
    },
    'wasi:cli/types': {},
    'wasi:cli/stdin': { getStdin: () => stdin, readViaStream },
    'wasi:cli/stdout': {
      getStdout: () => stdout,
      writeViaStream: (s: Parameters<typeof drain>[0]) => drain(s, (b) => world.stdout(b)),
    },
    'wasi:cli/stderr': {
      getStderr: () => stderr,
      writeViaStream: (s: Parameters<typeof drain>[0]) => drain(s, (b) => world.stderr(b)),
    },
    'wasi:cli/terminal-input': { TerminalInput },
    'wasi:cli/terminal-output': { TerminalOutput },
    'wasi:cli/terminal-stdin': { getTerminalStdin: () => undefined },
    'wasi:cli/terminal-stdout': { getTerminalStdout: () => undefined },
    'wasi:cli/terminal-stderr': { getTerminalStderr: () => undefined },
    'wasi:config/store': {
      get: (key: string) => config?.get(key),
      getAll: () => config?.all() ?? [],
    },
    'wasi:filesystem/types': {
      Descriptor,
      DirectoryEntryStream,
      filesystemErrorCode: () => undefined,
    },
    'wasi:filesystem/preopens': { getDirectories: () => [] },
  };
}
