// codelib-web: a browser host kit for WebAssembly components transpiled with jco.
//
// The pieces, also importable on their own:
//   codelib-web/transpile  the transpile step (Node; also the `codelib-web` CLI)
//   codelib-web/loop       the event loop the patched jco output runs on
//   codelib-web/wasi       virtual WASI 0.2/0.3 imports from a World, HTTP handlers, egress policy
//   codelib-web/worlds     the World interface, realWorld(), virtualWorld({ seed })
//   codelib-web/load       instantiate(), JSPI check, jco naming helpers
//   codelib-web/worker     runInWorker() with a wall-clock deadline, serveRun()
//   codelib-web/script     runScript(): codelib's JavaScript guest in a Worker
//
// This entry re-exports the browser-side pieces (not the transpile step, which needs Node).

export { GLOBAL_NAME, IdleError, external, install, loopState, onIdle, settle, touch, until, type LoopEnv } from './loop.js';
export { JSPI_MISSING, hasJspi, requireJspi } from './jspi.js';
export { camel, compileModules, findExport, instantiate, normalizeImports, unversioned, type Imports, type Instance, type LoadOptions } from './load.js';
export { realWorld, virtualWorld, type RandomStream, type VirtualWorld, type VirtualWorldOptions, type World, type WorldOptions } from './worlds.js';
export { HttpError, WasiExit, denyAll, wasiImports, type HttpErrorCode, type HttpHandler, type HttpRequest, type HttpResponse, type WasiOptions } from './wasi/index.js';
export { egress, type EgressOptions, type EgressPolicy, type Rule } from './wasi/egress.js';
export { errorInfo, runInWorker, serveRun, type ErrorInfo, type RunContext, type RunOptions, type RunOutcome, type WorkerRun } from './worker/index.js';
