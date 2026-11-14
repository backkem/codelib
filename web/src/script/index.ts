// codelib-web/script: run a JavaScript program in codelib's sandbox, in the browser.
//
// The browser counterpart of `codelib::Sandbox::script(...).run()`, in miniature: the program
// runs in codelib's standard guest (StarlingMonkey compiled to WebAssembly, world
// codelib:runtime/script) in a dedicated Worker, and gets three things from the page:
//
// - `host.call(method, args)` / `host.callRaw` / `host.emit`, answered by `hostCalls` (an async
//   JavaScript function of the page) while the guest waits;
// - `fetch`, through a small egress policy that denies everything by default;
// - `console`, captured and returned with the result.
//
// A run that exceeds `timeoutMs` is ended by terminating its Worker. There is no `serve` and no
// state handed back: the native crate does more (see the README).
//
// The engine is the guest component transpiled with the kit:
//   codelib-web transpile crates/codelib-guest/codelib-guest.wasm --preset script -o <dir> --name codelib-guest
// `loadEngine(<dir URL>)` compiles it once per page; every run instantiates it afresh.

import type { Rule, VirtualRequest } from '../wasi/egress.js';
import type { HttpResponse } from '../wasi/http.js';
import { runInWorker } from '../worker/index.js';
import type { CallReply, Failure, ScriptInit, ScriptMessage, ScriptRequest, ScriptResult, Source } from './protocol.js';

export type { Rule } from '../wasi/egress.js';
export type { Failure, Source } from './protocol.js';
export { hasJspi, requireJspi, JSPI_MISSING } from '../jspi.js';

/**
 * Answers `host.call(method, args)` and `host.callRaw(method, text)`: `args` is the text the
 * program sent (JSON for `host.call`), the return value is the text it gets back (parsed as JSON
 * by `host.call`; an empty string is `undefined`). Throw `HostFailure` to make the call throw
 * `host.HostError` in the program; any other error becomes the failure code `internal`.
 */
export type HostCalls = (method: string, args: string) => string | Promise<string>;

/** A failure `host.call` reports to the program (`host.HostError` with this code). */
export class HostFailure extends Error {
  readonly code: string;
  readonly hint: string | undefined;
  readonly detail: unknown;
  constructor(code: string, message: string, options: { hint?: string; detail?: unknown } = {}) {
    super(message);
    this.name = 'HostFailure';
    this.code = code;
    this.hint = options.hint;
    this.detail = options.detail;
  }
}

/**
 * `HostCalls` from a table of handlers that take and return JSON values, like codelib's native
 * `Router`: an unknown method fails with `unknown-method`, arguments that are not JSON with
 * `bad-args`.
 */
export function jsonHostCalls(handlers: Record<string, (args: unknown) => unknown>): HostCalls {
  return async (method, argsText) => {
    const handler = Object.hasOwn(handlers, method) ? handlers[method] : undefined;
    if (!handler) throw new HostFailure('unknown-method', `no host method ${method}`, { hint: `known: ${Object.keys(handlers).join(', ')}` });
    let args: unknown;
    try {
      args = JSON.parse(argsText);
    } catch {
      throw new HostFailure('bad-args', `the arguments of ${method} are not JSON`);
    }
    const result = await handler(args);
    return result === undefined ? '' : JSON.stringify(result);
  };
}

/** An in-page virtual host: Fetch API in, Fetch API out. */
export type VirtualHostHandler = (request: Request) => Response | Promise<Response>;

export interface ScriptEgress {
  /** Hosts the program may reach (first match wins); headers are attached by the Worker. */
  allow?: Rule[];
  /** Hosts answered by the page, never by the network, matched before the rules. */
  virtualHosts?: Record<string, VirtualHostHandler>;
}

/** A compiled engine: the transpiled guest, ready to instantiate in any number of Workers. */
export interface Engine {
  module: string;
  modules: Record<string, WebAssembly.Module>;
}

export interface LoadEngineOptions {
  /** File stem the guest was transpiled under. Default: `codelib-guest`. */
  name?: string;
  /** Download progress of the core modules, in bytes. */
  onProgress?: (loaded: number, total: number) => void;
}

/**
 * Downloads and compiles the transpiled guest at `base` (the directory `codelib-web transpile`
 * wrote) once, for every later `runScript`.
 */
export async function loadEngine(base: string | URL, options: LoadEngineOptions = {}): Promise<Engine> {
  const name = options.name ?? 'codelib-guest';
  const dir = new URL(String(base).endsWith('/') ? String(base) : `${String(base)}/`, globalThis.location?.href);
  const manifestUrl = new URL(`${name}.manifest.json`, dir);
  const res = await fetch(manifestUrl);
  if (!res.ok) throw new Error(`${manifestUrl.href}: HTTP ${res.status} (transpile the guest with codelib-web first)`);
  const manifest = (await res.json()) as { module: string; coreModules: string[] };

  // Sizes first, for one progress total over all modules.
  const responses = await Promise.all(
    manifest.coreModules.map(async (file) => {
      const r = await fetch(new URL(file, dir));
      if (!r.ok) throw new Error(`${file}: HTTP ${r.status}`);
      return [file, r] as const;
    }),
  );
  const total = responses.reduce((n, [, r]) => n + Number(r.headers.get('content-length') ?? 0), 0);
  let loaded = 0;
  const modules: Record<string, WebAssembly.Module> = {};
  await Promise.all(
    responses.map(async ([file, r]) => {
      const reader = r.body!.getReader();
      const chunks: Uint8Array[] = [];
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        chunks.push(value);
        loaded += value.length;
        options.onProgress?.(loaded, Math.max(total, loaded));
      }
      const bytes = new Uint8Array(chunks.reduce((n, c) => n + c.length, 0));
      let at = 0;
      for (const c of chunks) {
        bytes.set(c, at);
        at += c.length;
      }
      modules[file] = await WebAssembly.compile(bytes);
    }),
  );
  return { module: new URL(manifest.module, dir).href, modules };
}

export interface ScriptOptions {
  /** The program: the body of an async function (top-level `await` and `return` work). */
  program: string | Source;
  /** Evaluated first, in order, each in its own scope; they share state through `globalThis`. */
  preludes?: (string | Source)[];
  /** Visible to the program as `host.input`; must be JSON-serializable. */
  input?: unknown;
  hostCalls?: HostCalls;
  /** `host.emit(kind, item)`, as it happens; `item` is parsed JSON. */
  onEmit?: (kind: string, item: unknown) => void;
  /** Console output as it happens. */
  onConsole?: (stream: 'stdout' | 'stderr', text: string) => void;
  /** Wall-clock budget for the run, in ms (default 30 000, as the native default). */
  timeoutMs?: number;
  /** Budget for starting the Worker and instantiating the engine, in ms (default 120 000). */
  startTimeoutMs?: number;
  /** Outgoing HTTP. Default: every request denied. */
  egress?: ScriptEgress;
  /** Run in a virtual world: seeded randomness, a virtual clock (2026-01-01 at the start). */
  seed?: number | bigint;
  /** The engine (`loadEngine`), or the URL of the transpiled guest's directory. */
  engine: Engine | string | URL;
  /** Aborting ends the run at once (outcome `stopped`). */
  signal?: AbortSignal;
  /** The script Worker's module. Default: the one next to this module. */
  workerUrl?: string | URL;
}

export interface ScriptConsole {
  stdout: string;
  stderr: string;
  /** Output past 1 MiB per stream was dropped. */
  truncated: boolean;
}

export interface ScriptError {
  /** `script`: the program threw. `timeout`, `stopped`: the run was ended. `engine`: the engine failed. */
  kind: 'script' | 'timeout' | 'stopped' | 'engine';
  name: string;
  message: string;
  stack?: string;
}

export type ScriptOutcome =
  | { ok: true; value: unknown; console: ScriptConsole; ms: number; timings: { instantiateMs: number; runMs: number } }
  | { ok: false; error: ScriptError; console: ScriptConsole; ms: number };

const CONSOLE_LIMIT = 1024 * 1024;

const asSource = (s: string | Source, name: string): Source => (typeof s === 'string' ? { name, code: s } : s);

function failure(error: unknown): Failure {
  if (error instanceof HostFailure) {
    return {
      code: error.code,
      message: error.message,
      ...(error.hint !== undefined ? { hint: error.hint } : {}),
      ...(error.detail !== undefined ? { detail: JSON.stringify(error.detail) } : {}),
    };
  }
  return { code: 'internal', message: error instanceof Error ? error.message : String(error) };
}

async function answerVhost(handler: VirtualHostHandler, req: VirtualRequest): Promise<HttpResponse> {
  const request = new Request(req.url, {
    method: req.method,
    headers: req.headers,
    ...(req.body !== undefined && req.method !== 'GET' && req.method !== 'HEAD' ? { body: req.body as BodyInit } : {}),
  });
  const response = await handler(request);
  return { status: response.status, headers: [...response.headers], body: new Uint8Array(await response.arrayBuffer()) };
}

/** Runs `program` once, in a fresh engine instance in its own Worker. */
export async function runScript(options: ScriptOptions): Promise<ScriptOutcome> {
  const engine = typeof options.engine === 'string' || options.engine instanceof URL ? await loadEngine(options.engine) : options.engine;
  const console: ScriptConsole = { stdout: '', stderr: '', truncated: false };
  const virtualHosts = Object.fromEntries(Object.entries(options.egress?.virtualHosts ?? {}).map(([h, f]) => [h.toLowerCase(), f]));
  const init: ScriptInit = {
    module: engine.module,
    modules: engine.modules,
    preludes: (options.preludes ?? []).map((p, i) => asSource(p, `prelude-${i + 1}.js`)),
    program: asSource(options.program, 'script.js'),
    input: JSON.stringify(options.input ?? null),
    ...(options.seed !== undefined ? { seed: BigInt.asUintN(64, BigInt(options.seed)).toString() } : {}),
    allow: options.egress?.allow ?? [],
    virtualHosts: Object.keys(virtualHosts),
  };
  const timeoutMs = options.timeoutMs ?? 30_000;

  const run = runInWorker<ScriptInit, ScriptResult>(options.workerUrl ?? new URL('./worker.js', import.meta.url), {
    init,
    timeoutMs,
    startTimeoutMs: options.startTimeoutMs ?? 120_000,
    ...(options.signal ? { signal: options.signal } : {}),
    onMessage(payload) {
      const m = payload as ScriptMessage;
      if (m.kind === 'console') {
        const room = CONSOLE_LIMIT - console[m.stream].length;
        if (m.text.length > room) console.truncated = true;
        console[m.stream] += m.text.slice(0, Math.max(0, room));
        options.onConsole?.(m.stream, m.text);
      } else if (m.kind === 'emit') {
        let item: unknown = m.item;
        try {
          item = JSON.parse(m.item);
        } catch {
          // passed on as text
        }
        options.onEmit?.(m.emitKind, item);
      }
    },
    async onRequest(payload) {
      const r = payload as ScriptRequest;
      if (r.kind === 'call') {
        if (!options.hostCalls) return { err: { code: 'unknown-method', message: `no host method ${r.method}` } } satisfies CallReply;
        try {
          return { ok: await options.hostCalls(r.method, r.args) } satisfies CallReply;
        } catch (error) {
          return { err: failure(error) } satisfies CallReply;
        }
      }
      const handler = virtualHosts[new URL(r.request.url).hostname.toLowerCase()];
      if (!handler) throw new Error(`no virtual host for ${r.request.url}`);
      return answerVhost(handler, r.request);
    },
  });

  const outcome = await run.outcome;
  switch (outcome.status) {
    case 'ok':
      return {
        ok: true,
        value: JSON.parse(outcome.value.value),
        console,
        ms: outcome.ms,
        timings: { instantiateMs: outcome.value.instantiateMs, runMs: outcome.value.runMs },
      };
    case 'error': {
      const data = outcome.error.data as { name: string; message: string; stack?: string } | undefined;
      const error: ScriptError = data
        ? { kind: 'script', ...data }
        : { kind: 'engine', name: outcome.error.name, message: outcome.error.message, ...(outcome.error.stack ? { stack: outcome.error.stack } : {}) };
      return { ok: false, error, console, ms: outcome.ms };
    }
    case 'timeout':
      return {
        ok: false,
        error: {
          kind: 'timeout',
          name: 'TimeoutError',
          message: outcome.phase === 'run' ? `the script ran longer than ${outcome.limitMs} ms and was stopped` : `the engine did not start within ${outcome.limitMs} ms`,
        },
        console,
        ms: outcome.ms,
      };
    case 'stopped':
      return { ok: false, error: { kind: 'stopped', name: 'AbortError', message: 'the run was stopped' }, console, ms: outcome.ms };
  }
}
