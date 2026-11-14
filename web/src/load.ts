// Instantiating a transpiled component.
//
// `codelib-web transpile` writes `<stem>.js` (an `instantiate(getCoreModule, imports)` function)
// beside the component's core modules. This loader imports that module, compiles the core
// modules (or takes precompiled ones, so a page can compile once and hand the modules to every
// Worker), and smooths over jco's naming:
//
// - imports are keyed by unversioned interface name, with every version of an interface merged
//   into one object; an import object keyed `wasi:io/poll@0.2.12` is folded into `wasi:io/poll`.
// - exports are camel-cased (`app-http-get` becomes `appHttpGet`) and interface exports appear
//   under their short name (`wasi:cli/run@0.3.0` as `run`); `exportFn` finds a function by its
//   WIT name.

import { requireJspi } from './jspi.js';
import { install, until } from './loop.js';

export { JSPI_MISSING, hasJspi, requireJspi } from './jspi.js';

export type Imports = Record<string, Record<string, unknown>>;

export interface LoadOptions {
  /** Precompiled core modules by file name (`<stem>.core.wasm`, ...), used before fetching. */
  modules?: Record<string, WebAssembly.Module>;
  /** How to compile a core module that is not in `modules`. Default: fetch it next to the JS. */
  compile?: (url: URL) => Promise<WebAssembly.Module>;
}

/** A component instance: jco's export object, and a way to call exports by WIT name. */
export interface Instance {
  /** jco's export object, as returned by its `instantiate`. */
  exports: Record<string, unknown>;
  /** Finds an exported function by WIT name (`run`, `wasi:cli/run@0.3.0#run`, `app-get`). */
  exportFn(name: string): (...args: unknown[]) => unknown;
  /**
   * Calls an export and drives the event loop until it finishes (`until`). Use this for every
   * export that may suspend.
   */
  call<T = unknown>(name: string, ...args: unknown[]): Promise<T>;
}

/** Strips the `@version` from an interface name: `wasi:io/poll@0.2.12` is `wasi:io/poll`. */
export function unversioned(name: string): string {
  const at = name.indexOf('@');
  return at < 0 ? name : name.slice(0, at);
}

/** `kebab-case` (WIT) to `camelCase` (jco). */
export function camel(name: string): string {
  return name.replace(/-([a-z0-9])/g, (_, c: string) => c.toUpperCase());
}

/** Folds versioned keys into jco's unversioned ones, merging objects for one interface. */
export function normalizeImports(imports: Imports): Imports {
  const out: Imports = {};
  for (const [key, value] of Object.entries(imports)) {
    const k = unversioned(key);
    out[k] = { ...(out[k] ?? {}), ...value };
  }
  return out;
}

/** Finds an export by WIT name in jco's export object. */
export function findExport(exports: Record<string, unknown>, name: string): ((...args: unknown[]) => unknown) | undefined {
  const hash = name.indexOf('#');
  const pick = (obj: unknown, key: string) =>
    obj !== null && typeof obj === 'object' ? (obj as Record<string, unknown>)[key] : undefined;
  if (hash < 0) {
    const f = pick(exports, name) ?? pick(exports, camel(name));
    return typeof f === 'function' ? (f as (...args: unknown[]) => unknown) : undefined;
  }
  const iface = name.slice(0, hash);
  const func = name.slice(hash + 1);
  const short = camel(unversioned(iface).split('/').pop() ?? '');
  for (const key of [iface, unversioned(iface), short]) {
    const obj = pick(exports, key);
    const f = pick(obj, func) ?? pick(obj, camel(func));
    if (typeof f === 'function') return f as (...args: unknown[]) => unknown;
  }
  return undefined;
}

interface TranspiledModule {
  instantiate(
    getCoreModule: (path: string) => Promise<WebAssembly.Module>,
    imports: Imports,
  ): Promise<Record<string, unknown>>;
}

/**
 * Instantiates the transpiled component whose JS module is at `moduleUrl`, with `imports`.
 * Checks for JSPI and installs the event loop first.
 */
export async function instantiate(moduleUrl: string | URL, imports: Imports, options: LoadOptions = {}): Promise<Instance> {
  requireJspi();
  install();
  const url = new URL(moduleUrl, (globalThis as { location?: { href: string } }).location?.href);
  const mod = (await import(/* @vite-ignore */ url.href)) as TranspiledModule;
  if (typeof mod.instantiate !== 'function') {
    throw new Error(`${url.href} is not a component transpiled for instantiation (no instantiate export)`);
  }
  const compile = options.compile ?? ((u: URL) => WebAssembly.compileStreaming(fetch(u)));
  const getCoreModule = (path: string) => {
    const pre = options.modules?.[path];
    return pre ? Promise.resolve(pre) : compile(new URL(path, url));
  };
  const exports = await mod.instantiate(getCoreModule, normalizeImports(imports));
  const exportFn = (name: string) => {
    const f = findExport(exports, name);
    if (!f) throw new Error(`the component has no export ${name} (exports: ${Object.keys(exports).join(', ')})`);
    return f;
  };
  return {
    exports,
    exportFn,
    call<T>(name: string, ...args: unknown[]) {
      const f = exportFn(name);
      return until(new Promise<T>((resolve) => resolve(f(...args) as T)));
    },
  };
}

/** Compiles the core modules of a transpiled component once, for `LoadOptions.modules`. */
export async function compileModules(baseUrl: string | URL, files: string[]): Promise<Record<string, WebAssembly.Module>> {
  const base = new URL(baseUrl, (globalThis as { location?: { href: string } }).location?.href);
  const entries = await Promise.all(
    files.map(async (f) => [f, await WebAssembly.compileStreaming(fetch(new URL(f, base)))] as const),
  );
  return Object.fromEntries(entries);
}
