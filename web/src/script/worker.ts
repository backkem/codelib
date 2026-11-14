// The script Worker: codelib's JavaScript guest (world codelib:runtime/script) under the kit.
//
// One run per Worker, so every run gets a fresh engine instance (as natively) and the page can
// end a run at any point by terminating the Worker. WASI is the kit's: clocks and entropy from
// the real world (or a seeded virtual one), stdout and stderr streamed to the page, no
// filesystem, and outgoing HTTP through a small egress policy that denies by default.
// `host.call` goes to the page and waits for its answer: the import is a JSPI async import, so
// the guest is suspended meanwhile and the program sees a synchronous call.

import { external, touch } from '../loop.js';
import { instantiate } from '../load.js';
import { egress } from '../wasi/egress.js';
import { wasiImports } from '../wasi/index.js';
import { serveRun, type RunContext } from '../worker/index.js';
import { realWorld, virtualWorld } from '../worlds.js';
import type { CallReply, ScriptInit, ScriptMessage, ScriptRequest, ScriptResult, VhostReply } from './protocol.js';

/** The guest's `run` failed with a script error; `data` carries it to the page. */
class ScriptFailed extends Error {
  constructor(readonly data: { name: string; message: string; stack?: string }) {
    super(data.message);
    this.name = data.name;
  }
}

async function run(init: ScriptInit, ctx: RunContext): Promise<ScriptResult> {
  const post = (m: ScriptMessage) => ctx.post(m);
  const request = <T>(r: ScriptRequest) => ctx.request<T>(r);
  const stream = (name: 'stdout' | 'stderr') => {
    const decoder = new TextDecoder();
    return (bytes: Uint8Array) => post({ kind: 'console', stream: name, text: decoder.decode(bytes, { stream: true }) });
  };
  const stdout = stream('stdout');
  const stderr = stream('stderr');
  const options = { stdout, stderr, args: ['codelib'] };
  const world = init.seed !== undefined ? virtualWorld({ seed: BigInt(init.seed), ...options }) : realWorld(options);

  const encoder = new TextEncoder();
  const http = egress(
    {
      allow: init.allow,
      virtualHosts: Object.fromEntries(
        init.virtualHosts.map((host) => [host, (req) => request<VhostReply>({ kind: 'vhost', request: req })]),
      ),
    },
    { log: (line) => stderr(encoder.encode(`${line}\n`)) },
  );

  const imports = {
    ...wasiImports(world, { http }),
    'codelib:runtime/types': {},
    'codelib:runtime/host': {
      call: async (method: string, args: string) => {
        touch();
        // The page's answer is real-world work: the loop must wait for it, not call it idle.
        const reply = await external(request<CallReply>({ kind: 'call', method, args }));
        return 'ok' in reply ? { tag: 'ok', val: reply.ok } : { tag: 'err', val: reply.err };
      },
      emit: (kind: string, item: string) => {
        touch();
        post({ kind: 'emit', emitKind: kind, item });
      },
    },
  };

  const t0 = performance.now();
  const instance = await instantiate(init.module, imports, init.modules ? { modules: init.modules } : {});
  const t1 = performance.now();
  ctx.running();
  let value: string;
  try {
    value = await instance.call<string>('run', init.preludes, init.program, init.input);
  } catch (error) {
    // `result::err` arrives as jco's ComponentError carrying the script-error record.
    const payload = (error as { payload?: { name?: unknown; message?: unknown; stack?: unknown } } | null)?.payload;
    if (payload && typeof payload.name === 'string' && typeof payload.message === 'string') {
      throw new ScriptFailed({
        name: payload.name,
        message: payload.message,
        ...(typeof payload.stack === 'string' ? { stack: payload.stack } : {}),
      });
    }
    throw error;
  }
  return { value, instantiateMs: t1 - t0, runMs: performance.now() - t1 };
}

serveRun(run);
