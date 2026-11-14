// Running a component in a dedicated Worker, with a wall-clock deadline.
//
// A browser cannot cap a component's memory or meter its instructions, and a component that
// computes forever blocks the thread it runs on. So the component runs in its own Worker, and
// the page-side handle enforces the deadline by terminating that Worker: whatever the component
// was doing, it stops, and the run is reported as a timeout. That is the whole limit; see the
// README for what the browser does not enforce compared with codelib-sandbox.
//
// Page side: `runInWorker(scriptUrl, options)` starts the Worker, hands it `init`, answers its
// requests (`onRequest`, for host functions that live in the page) and resolves to an outcome.
// Worker side: `serveRun(run)` receives `init`, calls `run(init, context)` and reports.
//
// Two budgets: `startTimeoutMs` covers loading and instantiating (from the Worker's creation
// until it calls `context.running()`); `timeoutMs` covers the run from then on. A Worker that
// never calls `running()` is only under the start budget.

/** An error as it crosses the Worker boundary. */
export interface ErrorInfo {
  name: string;
  message: string;
  stack?: string;
  /** Extra fields a run wants to carry (a script error's payload). */
  data?: unknown;
}

export type RunOutcome<R> =
  | { status: 'ok'; value: R; ms: number }
  | { status: 'error'; error: ErrorInfo; ms: number }
  | { status: 'timeout'; phase: 'start' | 'run'; limitMs: number; ms: number }
  | { status: 'stopped'; ms: number };

export interface RunOptions<I> {
  init: I;
  transfer?: Transferable[];
  /** Wall-clock budget for the run, in ms. Default: none. */
  timeoutMs?: number;
  /** Wall-clock budget for loading and instantiating, in ms. Default: none. */
  startTimeoutMs?: number;
  /** Answers `context.request(payload)` calls from the Worker. */
  onRequest?: (payload: unknown) => unknown;
  /** Receives `context.post(payload)` messages from the Worker. */
  onMessage?: (payload: unknown) => void;
  /** Aborting stops the run (outcome `stopped`). */
  signal?: AbortSignal;
}

export interface WorkerRun<R> {
  outcome: Promise<RunOutcome<R>>;
  /** Terminates the Worker now; the outcome is `stopped` (unless it already finished). */
  stop(): void;
  /** Sends a message the Worker receives through `context.onMessage`. */
  post(payload: unknown, transfer?: Transferable[]): void;
}

type ToWorker =
  | { cw: 'start'; init: unknown }
  | { cw: 'reply'; id: number; ok: true; value: unknown }
  | { cw: 'reply'; id: number; ok: false; error: ErrorInfo }
  | { cw: 'message'; payload: unknown };

type FromWorker =
  | { cw: 'running' }
  | { cw: 'request'; id: number; payload: unknown }
  | { cw: 'message'; payload: unknown }
  | { cw: 'done'; ok: true; value: unknown }
  | { cw: 'done'; ok: false; error: ErrorInfo };

export function errorInfo(error: unknown): ErrorInfo {
  if (error instanceof Error) {
    return { name: error.name, message: error.message, ...(error.stack ? { stack: error.stack } : {}) };
  }
  return { name: 'Error', message: typeof error === 'string' ? error : JSON.stringify(error) ?? String(error) };
}

/** Starts `scriptUrl` (a module Worker that calls `serveRun`) and runs it once. */
export function runInWorker<I, R>(scriptUrl: string | URL, options: RunOptions<I>): WorkerRun<R> {
  const started = performance.now();
  const ms = () => Math.round(performance.now() - started);
  const worker = new Worker(scriptUrl, { type: 'module' });
  let finished = false;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let settle!: (outcome: RunOutcome<R>) => void;
  const outcome = new Promise<RunOutcome<R>>((resolve) => (settle = resolve));

  const finish = (o: RunOutcome<R>) => {
    if (finished) return;
    finished = true;
    if (timer !== undefined) clearTimeout(timer);
    worker.terminate();
    options.signal?.removeEventListener('abort', stop);
    settle(o);
  };
  const arm = (limit: number | undefined, phase: 'start' | 'run') => {
    if (timer !== undefined) clearTimeout(timer);
    timer = undefined;
    if (limit !== undefined) timer = setTimeout(() => finish({ status: 'timeout', phase, limitMs: limit, ms: ms() }), limit);
  };
  const stop = () => finish({ status: 'stopped', ms: ms() });

  worker.onmessage = (event: MessageEvent<FromWorker>) => {
    const m = event.data;
    if (finished || m === null || typeof m !== 'object') return;
    if (m.cw === 'running') arm(options.timeoutMs, 'run');
    else if (m.cw === 'message') options.onMessage?.(m.payload);
    else if (m.cw === 'request') {
      void (async () => {
        let reply: ToWorker;
        try {
          if (!options.onRequest) throw new Error('this run answers no requests');
          reply = { cw: 'reply', id: m.id, ok: true, value: await options.onRequest(m.payload) };
        } catch (error) {
          reply = { cw: 'reply', id: m.id, ok: false, error: errorInfo(error) };
        }
        if (!finished) worker.postMessage(reply);
      })();
    } else if (m.cw === 'done') {
      finish(m.ok ? { status: 'ok', value: m.value as R, ms: ms() } : { status: 'error', error: m.error, ms: ms() });
    }
  };
  worker.onerror = (event: ErrorEvent) => {
    event.preventDefault();
    finish({ status: 'error', error: { name: 'WorkerError', message: event.message || 'the Worker failed to load or crashed' }, ms: ms() });
  };

  if (options.signal?.aborted) {
    queueMicrotask(stop);
  } else {
    options.signal?.addEventListener('abort', stop);
    arm(options.startTimeoutMs, 'start');
    const start: ToWorker = { cw: 'start', init: options.init };
    worker.postMessage(start, options.transfer ?? []);
  }
  return {
    outcome,
    stop,
    post(payload, transfer) {
      if (!finished) worker.postMessage({ cw: 'message', payload } satisfies ToWorker, transfer ?? []);
    },
  };
}

/** What a Worker's run function gets. */
export interface RunContext {
  /** Starts the run's deadline: loading is done, the component is about to run. */
  running(): void;
  /** Asks the page (`onRequest`) and waits for its answer. */
  request<T = unknown>(payload: unknown): Promise<T>;
  /** Sends a message to the page (`onMessage`). */
  post(payload: unknown, transfer?: Transferable[]): void;
  /** Receives messages the page sends with `WorkerRun.post`. */
  onMessage(handler: (payload: unknown) => void): void;
}

/**
 * Worker side: waits for `init`, calls `run(init, context)` once, and reports its value (which
 * must be structured-cloneable) or its error. Call it at the top level of the Worker module.
 */
export function serveRun<I, R>(run: (init: I, context: RunContext) => Promise<R>): void {
  const scope = globalThis as unknown as {
    postMessage(message: FromWorker, transfer?: Transferable[]): void;
    onmessage: ((event: MessageEvent<ToWorker>) => void) | null;
  };
  const replies = new Map<number, { resolve(v: unknown): void; reject(e: unknown): void }>();
  const handlers: ((payload: unknown) => void)[] = [];
  let nextId = 1;
  let started = false;
  const context: RunContext = {
    running: () => scope.postMessage({ cw: 'running' }),
    request: <T>(payload: unknown) =>
      new Promise<T>((resolve, reject) => {
        const id = nextId++;
        replies.set(id, { resolve: resolve as (v: unknown) => void, reject });
        scope.postMessage({ cw: 'request', id, payload });
      }),
    post: (payload, transfer) => scope.postMessage({ cw: 'message', payload }, transfer),
    onMessage: (handler) => handlers.push(handler),
  };
  scope.onmessage = (event) => {
    const m = event.data;
    if (m.cw === 'reply') {
      const r = replies.get(m.id);
      replies.delete(m.id);
      if (!r) return;
      if (m.ok) r.resolve(m.value);
      else r.reject(Object.assign(new Error(m.error.message), { name: m.error.name }));
    } else if (m.cw === 'message') {
      for (const h of handlers) h(m.payload);
    } else if (m.cw === 'start' && !started) {
      started = true;
      run(m.init as I, context).then(
        (value) => scope.postMessage({ cw: 'done', ok: true, value }),
        (error: unknown) => {
          const info = errorInfo(error);
          const data = (error as { data?: unknown } | null)?.data;
          scope.postMessage({ cw: 'done', ok: false, error: data === undefined ? info : { ...info, data } });
        },
      );
    }
  };
}
