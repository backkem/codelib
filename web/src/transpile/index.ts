// The transpile step (Node only): a component in, a directory of patched JS and core modules
// out, with the pinned jco.
//
// It runs jco's own `transpile` with fixed options (async instantiation, no WASI shim, JSPI),
// patches the generated module (patches.ts) and writes a manifest beside it. It refuses any jco
// other than the pinned one: the patches match the code one jco version generates, and a jco
// upgrade has to re-check every one of them (docs/jco-patches.md).

import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

import { PATCHED_MARKER, applyPatches, type Applied } from './patches.js';

export { PATCHES, PatchError, applyPatches, type Applied, type Patch } from './patches.js';

/** The jco the patches are written for. */
export const JCO_VERSION = '1.42.0';
/** The code generator inside it (`@bytecodealliance/jco-transpile`), which writes the output. */
export const JCO_TRANSPILE_VERSION = '0.24.0';

/**
 * WASI 0.2 functions that may have to wait: transpiled as JSPI async imports, so the kit's WASI
 * can return a promise from them (a real-time sleep, an HTTP response). Ignored by jco when a
 * component does not import them.
 */
export const WASI_ASYNC_IMPORTS: readonly string[] = [
  'wasi:io/poll#poll',
  'wasi:io/poll#[method]pollable.block',
  'wasi:io/streams#[method]input-stream.blocking-read',
  'wasi:io/streams#[method]input-stream.blocking-skip',
  'wasi:io/streams#[method]output-stream.blocking-splice',
];

/** Presets: what hosting a particular world needs on top of the WASI defaults. */
export const PRESETS: Record<string, { asyncImports: string[]; asyncExports: string[] }> = {
  // codelib's JavaScript guest (world codelib:runtime/script): `host.call` waits for the page's
  // answer, so the import is async and the exports that run programs may suspend.
  script: {
    asyncImports: ['codelib:runtime/host#call'],
    asyncExports: ['run', 'serve'],
  },
};

export interface TranspileOptions {
  /** Output directory; replaced if it exists. */
  outDir: string;
  /** File stem of the output (`<name>.js`, `<name>.core.wasm`, ...). */
  name: string;
  /** More JSPI async imports (`iface#func`, as jco's `--async-imports`). */
  asyncImports?: string[];
  /** JSPI async exports (as jco's `--async-exports`). */
  asyncExports?: string[];
  /** A preset from `PRESETS`. */
  preset?: string;
  /** Leave out `WASI_ASYNC_IMPORTS`. */
  noWasiAsync?: boolean;
  /** jco's package directory. Default: the one installed with this package ($CODELIB_WEB_JCO overrides). */
  jcoDir?: string;
}

export interface Manifest {
  name: string;
  kit: string;
  jco: string;
  jcoTranspile: string;
  /** sha256 of the component. */
  component: string;
  /** The JS module to import. */
  module: string;
  /** Core modules, for precompiling (`compileModules`). */
  coreModules: string[];
  asyncImports: string[];
  asyncExports: string[];
  patches: Applied[];
}

export class JcoVersionError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'JcoVersionError';
  }
}

function readJson(path: string): { name?: string; version?: string } {
  return JSON.parse(readFileSync(path, 'utf8')) as { name?: string; version?: string };
}

/** The package root above `file` whose package.json is named `name`. */
function packageRoot(file: string, name: string): string {
  for (let dir = dirname(file); dir !== dirname(dir); dir = dirname(dir)) {
    const pj = join(dir, 'package.json');
    if (existsSync(pj) && readJson(pj).name === name) return dir;
  }
  throw new JcoVersionError(`cannot find the ${name} package above ${file}`);
}

export interface JcoInstall {
  dir: string;
  version: string;
  transpileVersion: string;
}

/** Finds jco and its code generator, without checking their versions. */
export function locateJco(jcoDir?: string): JcoInstall {
  const dir = jcoDir ?? process.env.CODELIB_WEB_JCO ?? packageRoot(createRequire(import.meta.url).resolve('@bytecodealliance/jco'), '@bytecodealliance/jco');
  const version = readJson(join(dir, 'package.json')).version ?? '?';
  const candidates = [join(dir, 'node_modules/@bytecodealliance/jco-transpile/package.json'), join(dir, '../jco-transpile/package.json')];
  const found = candidates.find((p) => existsSync(p));
  const transpileVersion = found ? (readJson(found).version ?? '?') : 'missing';
  return { dir, version, transpileVersion };
}

/** Throws `JcoVersionError` unless `jco` is the pinned jco with the pinned code generator. */
export function checkJco(jco: JcoInstall): void {
  if (jco.version !== JCO_VERSION || jco.transpileVersion !== JCO_TRANSPILE_VERSION) {
    throw new JcoVersionError(
      `codelib-web's patches are written for jco ${JCO_VERSION} (jco-transpile ${JCO_TRANSPILE_VERSION}), ` +
        `found jco ${jco.version} (jco-transpile ${jco.transpileVersion}) at ${jco.dir}. ` +
        'A different jco needs every patch re-checked: see docs/jco-patches.md.',
    );
  }
}

/** This package's version. */
export function kitVersion(): string {
  const here = dirname(fileURLToPath(import.meta.url));
  return readJson(resolve(here, '../../package.json')).version ?? '?';
}

interface JcoApi {
  transpile(
    component: Uint8Array,
    opts: Record<string, unknown>,
  ): Promise<{ files: Record<string, Uint8Array>; imports: string[]; exports: [string, string][] }>;
}

/** Transpiles `component` (bytes or a path) into `options.outDir`. Returns the manifest. */
export async function transpile(component: Uint8Array | string, options: TranspileOptions): Promise<Manifest> {
  const jco = locateJco(options.jcoDir);
  checkJco(jco);
  const bytes = typeof component === 'string' ? new Uint8Array(readFileSync(component)) : component;
  const preset = options.preset === undefined ? undefined : PRESETS[options.preset];
  if (options.preset !== undefined && preset === undefined) {
    throw new Error(`unknown preset ${options.preset} (known: ${Object.keys(PRESETS).join(', ')})`);
  }
  const asyncImports = [...new Set([...(options.noWasiAsync ? [] : WASI_ASYNC_IMPORTS), ...(preset?.asyncImports ?? []), ...(options.asyncImports ?? [])])];
  const asyncExports = [...new Set([...(preset?.asyncExports ?? []), ...(options.asyncExports ?? [])])];

  const api = (await import(pathToFileURL(join(jco.dir, 'dist/api.js')).href)) as JcoApi;
  const { files } = await api.transpile(bytes, {
    name: options.name,
    instantiation: 'async',
    wasiShim: false,
    noTypescript: false,
    quiet: true,
    asyncMode: 'jspi',
    asyncImports,
    asyncExports,
  });

  const moduleName = `${options.name}.js`;
  const js = files[moduleName];
  if (js === undefined) throw new Error(`jco produced no ${moduleName} (files: ${Object.keys(files).join(', ')})`);
  const patched = applyPatches(new TextDecoder().decode(js));

  const manifest: Manifest = {
    name: options.name,
    kit: kitVersion(),
    jco: jco.version,
    jcoTranspile: jco.transpileVersion,
    component: createHash('sha256').update(bytes).digest('hex'),
    module: moduleName,
    coreModules: Object.keys(files).filter((f) => f.endsWith('.wasm')).sort(),
    asyncImports,
    asyncExports,
    patches: patched.applied,
  };

  rmSync(options.outDir, { recursive: true, force: true });
  for (const [file, data] of Object.entries(files)) {
    const out = join(options.outDir, file);
    mkdirSync(dirname(out), { recursive: true });
    if (file === moduleName) {
      writeFileSync(out, `${PATCHED_MARKER} ${manifest.kit} for jco ${jco.version}; see docs/jco-patches.md */\n${patched.js}`);
    } else {
      writeFileSync(out, data);
    }
  }
  writeFileSync(join(options.outDir, `${options.name}.manifest.json`), `${JSON.stringify(manifest, null, 2)}\n`);
  return manifest;
}
