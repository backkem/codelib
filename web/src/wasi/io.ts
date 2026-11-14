// wasi:io@0.2 (poll, streams, error) for one import object.
//
// jco checks resource arguments against the classes it was given, so every resource class here
// is created per import object (`createIo`) and shared by the interfaces that use it (cli stdio,
// http bodies). Errors follow jco's conventions: a WIT `result` error is a thrown value (a plain
// object such as `{ tag: 'closed' }` for `stream-error`).
//
// Waiting: when nothing a component polls is ready, `poll` and `pollable.block` must wait. With
// a world that can move its clock (`advanceTo`, the virtual world) and only clock deadlines
// outstanding, they move virtual time and return at once. Otherwise they return a promise,
// which works only when the component was transpiled with these functions as JSPI async
// imports (the transpile step does that by default).

import { touch } from '../loop.js';
import type { World } from '../worlds.js';

/** The `stream-error` a closed or finished stream reports. */
export const CLOSED = Object.freeze({ tag: 'closed' as const });

export interface PollableLike {
  ready(): boolean;
  block(): void | Promise<void>;
}

export interface IoErrorLike {
  readonly message: string;
  readonly httpCode: unknown;
  toDebugString(): string;
}

export interface InputStreamLike {
  read(len: bigint): Uint8Array;
  blockingRead(len: bigint): Uint8Array | Promise<Uint8Array>;
  skip(len: bigint): bigint;
  blockingSkip(len: bigint): bigint | Promise<bigint>;
  subscribe(): PollableLike;
}

export interface OutputStreamLike {
  checkWrite(): bigint;
  write(bytes: Uint8Array): void;
  blockingWriteAndFlush(bytes: Uint8Array): void;
  flush(): void;
  blockingFlush(): void;
  subscribe(): PollableLike;
  writeZeroes(len: bigint): void;
  blockingWriteZeroesAndFlush(len: bigint): void;
  splice(src: InputStreamLike, len: bigint): bigint;
  blockingSplice(src: InputStreamLike, len: bigint): bigint | Promise<bigint>;
}

/**
 * Where an input stream's bytes come from. `pull` returns the next chunk, `undefined` at the
 * end, or `null` when no data is available yet; then `wait` resolves when there may be.
 */
export interface ByteSource {
  pull(): Uint8Array | undefined | null;
  wait(): Promise<void>;
}

/** Where an output stream's bytes go. Throwing `CLOSED` ends the stream for the writer. */
export type ByteSink = (bytes: Uint8Array) => void;

/** A source with nothing in it. */
export const EMPTY: ByteSource = { pull: () => undefined, wait: () => Promise.resolve() };

/** A source over bytes that are all there already (a buffered body). */
export function bytesSource(bytes: Uint8Array): ByteSource {
  let rest: Uint8Array | undefined = bytes;
  return {
    pull() {
      const out = rest;
      rest = undefined;
      return out;
    },
    wait: () => Promise.resolve(),
  };
}

/**
 * A source over a read function that may answer later (the world's stdin): a promise counts as
 * no data until it settles.
 */
export function pullSource(read: () => Uint8Array | undefined | Promise<Uint8Array | undefined>): ByteSource {
  let inFlight: Promise<void> | null = null;
  let ready: { value: Uint8Array | undefined } | null = null;
  let ended = false;
  return {
    pull() {
      if (ended) return undefined;
      if (ready !== null) {
        const { value } = ready;
        ready = null;
        if (value === undefined) ended = true;
        return value;
      }
      if (inFlight !== null) return null;
      const r = read();
      if (r instanceof Promise) {
        inFlight = r.then(
          (value) => {
            ready = { value };
            inFlight = null;
          },
          () => {
            ready = { value: undefined };
            inFlight = null;
          },
        );
        return null;
      }
      if (r === undefined) ended = true;
      return r;
    },
    wait: () => inFlight ?? Promise.resolve(),
  };
}

type Readiness =
  | { kind: 'ready' }
  | { kind: 'deadline'; at: bigint }
  | { kind: 'promise'; isReady: () => boolean; wait: () => Promise<void> };

export function createIo(world: World): Io {
  const PRIVATE = Symbol('pollable');

  class Pollable implements PollableLike {
    readonly #r: Readiness;
    /** Built by the kit only; a component gets pollables from `subscribe` functions. */
    constructor(key: symbol, readiness: Readiness) {
      if (key !== PRIVATE) throw new TypeError('pollables are created by the host');
      this.#r = readiness;
    }
    static always(): Pollable {
      return new Pollable(PRIVATE, { kind: 'ready' });
    }
    static deadline(at: bigint): Pollable {
      return new Pollable(PRIVATE, { kind: 'deadline', at });
    }
    static when(isReady: () => boolean, wait: () => Promise<void>): Pollable {
      return new Pollable(PRIVATE, { kind: 'promise', isReady, wait });
    }
    ready(): boolean {
      const r = this.#r;
      if (r.kind === 'ready') return true;
      if (r.kind === 'deadline') return world.now() >= r.at;
      return r.isReady();
    }
    /** The deadline, if this pollable waits on the monotonic clock. */
    deadline(): bigint | undefined {
      return this.#r.kind === 'deadline' ? this.#r.at : undefined;
    }
    /** Resolves when this pollable may have become ready. */
    whenReady(): Promise<void> {
      const r = this.#r;
      if (r.kind === 'ready') return Promise.resolve();
      if (r.kind === 'deadline') {
        return new Promise((resolve) =>
          world.timer(r.at, () => {
            touch();
            resolve();
          }),
        );
      }
      return r.wait();
    }
    block(): void | Promise<void> {
      if (this.ready()) return;
      const at = this.deadline();
      if (at !== undefined && world.advanceTo) {
        world.advanceTo(at);
        return;
      }
      return (async () => {
        while (!this.ready()) await this.whenReady();
      })();
    }
  }

  function readyIndices(list: Pollable[]): number[] {
    const out: number[] = [];
    list.forEach((p, i) => {
      if (p.ready()) out.push(i);
    });
    return out;
  }

  function poll(list: Pollable[]): Uint32Array | Promise<Uint32Array> {
    if (list.length === 0) throw new TypeError('wasi:io/poll: poll called with an empty list');
    const now = readyIndices(list);
    if (now.length > 0) return Uint32Array.from(now);
    // Only clock deadlines outstanding, and a clock we may move: move it to the earliest.
    const deadlines = list.map((p) => p.deadline());
    if (world.advanceTo && deadlines.every((d) => d !== undefined)) {
      let earliest = deadlines[0]!;
      for (const d of deadlines) if (d! < earliest) earliest = d!;
      world.advanceTo(earliest);
      return Uint32Array.from(readyIndices(list));
    }
    return (async () => {
      for (;;) {
        await Promise.race(list.map((p) => p.whenReady()));
        const ready = readyIndices(list);
        if (ready.length > 0) return Uint32Array.from(ready);
      }
    })();
  }

  class IoError implements IoErrorLike {
    constructor(
      readonly message: string,
      /** The `wasi:http` error code behind this error, for `http-error-code`. */
      readonly httpCode: unknown = undefined,
    ) {}
    toDebugString(): string {
      return this.message;
    }
  }

  class InputStream implements InputStreamLike {
    #source: ByteSource;
    #chunk: Uint8Array | null = null;
    #ended = false;
    constructor(source: ByteSource) {
      this.#source = source;
    }
    // The next bytes: a chunk, an empty array when none are available yet; throws at the end.
    #take(len: bigint): Uint8Array {
      const max = Number(len > 0x7fffffffn ? 0x7fffffffn : len);
      if (this.#chunk === null || this.#chunk.length === 0) {
        if (this.#ended) throw CLOSED;
        const next = this.#source.pull();
        if (next === undefined) {
          this.#ended = true;
          throw CLOSED;
        }
        if (next === null) return new Uint8Array(0);
        this.#chunk = next;
      }
      const chunk = this.#chunk!;
      const out = chunk.subarray(0, max);
      this.#chunk = chunk.subarray(out.length);
      return out.slice();
    }
    read(len: bigint): Uint8Array {
      return this.#take(len);
    }
    blockingRead(len: bigint): Uint8Array | Promise<Uint8Array> {
      const out = this.#take(len);
      if (out.length > 0 || len === 0n) return out;
      return (async () => {
        for (;;) {
          await this.#source.wait();
          const more = this.#take(len);
          if (more.length > 0) return more;
        }
      })();
    }
    skip(len: bigint): bigint {
      return BigInt(this.#take(len).length);
    }
    blockingSkip(len: bigint): bigint | Promise<bigint> {
      const r = this.blockingRead(len);
      return r instanceof Promise ? r.then((b) => BigInt(b.length)) : BigInt(r.length);
    }
    subscribe(): Pollable {
      return Pollable.when(
        () => this.#ended || (this.#chunk !== null && this.#chunk.length > 0) || this.#peek(),
        () => this.#source.wait(),
      );
    }
    // Pulls ahead so readiness is known; keeps what it pulled.
    #peek(): boolean {
      const next = this.#source.pull();
      if (next === null) return false;
      if (next === undefined) this.#ended = true;
      else this.#chunk = next;
      return true;
    }
  }

  class OutputStream implements OutputStreamLike {
    #sink: ByteSink;
    constructor(sink: ByteSink) {
      this.#sink = sink;
    }
    checkWrite(): bigint {
      return 65536n;
    }
    write(bytes: Uint8Array): void {
      touch();
      // jco may hand us a view into guest memory; the sink keeps a copy.
      this.#sink(bytes.slice());
    }
    blockingWriteAndFlush(bytes: Uint8Array): void {
      this.write(bytes);
    }
    flush(): void {}
    blockingFlush(): void {}
    subscribe(): Pollable {
      return Pollable.always();
    }
    writeZeroes(len: bigint): void {
      this.write(new Uint8Array(Number(len)));
    }
    blockingWriteZeroesAndFlush(len: bigint): void {
      this.writeZeroes(len);
    }
    splice(src: InputStream, len: bigint): bigint {
      const bytes = src.read(len);
      this.write(bytes);
      return BigInt(bytes.length);
    }
    blockingSplice(src: InputStream, len: bigint): bigint | Promise<bigint> {
      const r = src.blockingRead(len);
      const done = (bytes: Uint8Array) => {
        this.write(bytes);
        return BigInt(bytes.length);
      };
      return r instanceof Promise ? r.then(done) : done(r);
    }
  }

  return {
    Pollable,
    IoError,
    InputStream,
    OutputStream,
    poll,
    imports: {
      'wasi:io/poll': { Pollable, poll },
      'wasi:io/error': { Error: IoError },
      'wasi:io/streams': { InputStream, OutputStream },
    },
  };
}

/** A pollable as the kit's own code sees it. */
export interface HostPollable extends PollableLike {
  deadline(): bigint | undefined;
  whenReady(): Promise<void>;
}

/** The io resources of one import object, for the interfaces built on them. */
export interface Io {
  Pollable: {
    always(): HostPollable;
    deadline(at: bigint): HostPollable;
    when(isReady: () => boolean, wait: () => Promise<void>): HostPollable;
  };
  IoError: new (message: string, httpCode?: unknown) => IoErrorLike;
  InputStream: new (source: ByteSource) => InputStreamLike;
  OutputStream: new (sink: ByteSink) => OutputStreamLike;
  poll(list: PollableLike[]): Uint32Array | Promise<Uint32Array>;
  imports: Record<string, Record<string, unknown>>;
}
