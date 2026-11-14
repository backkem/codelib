#!/usr/bin/env node
// codelib-web transpile <component.wasm> -o <dir> --name <stem> [options]
// codelib-web browser-files -o <dir>

import { basename } from 'node:path';

import { copyBrowserFiles } from './browser-files.js';
import { JCO_TRANSPILE_VERSION, JCO_VERSION, PRESETS, kitVersion, transpile } from './transpile/index.js';

const USAGE = `usage: codelib-web transpile <component.wasm> -o <dir> [--name <stem>] [options]

Transpiles a WebAssembly component with the pinned jco (${JCO_VERSION}) and patches the output
for codelib-web's event loop. Writes <stem>.js, the core modules and <stem>.manifest.json.

options:
  --name <stem>             output file stem (default: the component's file name)
  --preset <name>           async imports/exports for a known world (${Object.keys(PRESETS).join(', ')})
  --async-imports <spec>    a JSPI async import, e.g. 'my:pkg/iface#func' (repeatable)
  --async-exports <spec>    a JSPI async export, e.g. 'run' (repeatable)
  --no-wasi-async           do not make WASI 0.2's blocking functions async imports
  -q, --quiet               print nothing on success

codelib-web browser-files -o <dir>
                            copies the browser-side modules of the package (loop, worlds, wasi,
                            load, jspi, worker, script, index) into <dir>, keeping their relative
                            paths, for a Worker that cannot resolve codelib-web/... specifiers

codelib-web version         prints the kit's and the pinned jco's versions`;

function fail(message: string): never {
  console.error(`codelib-web: ${message}`);
  process.exit(2);
}

function browserFiles(args: string[]): void {
  let outDir: string | undefined;
  for (let i = 0; i < args.length; i++) {
    const a = args[i]!;
    if (a === '-o' || a === '--out-dir') outDir = args[++i] ?? fail(`${a} needs a value`);
    else fail(`unexpected argument ${a}

${USAGE}`);
  }
  if (outDir === undefined) fail('no output directory given (-o <dir>)');
  const files = copyBrowserFiles(outDir);
  console.log(`codelib-web: copied ${files.length} browser modules to ${outDir}`);
  for (const f of files) console.log(`  ${f}`);
}

async function main(argv: string[]): Promise<void> {
  const [command, ...rest] = argv;
  if (command === 'version' || command === '--version') {
    console.log(`codelib-web ${kitVersion()} (jco ${JCO_VERSION}, jco-transpile ${JCO_TRANSPILE_VERSION})`);
    return;
  }
  if (command === 'browser-files') {
    browserFiles(rest);
    return;
  }
  if (command !== 'transpile') {
    console.error(USAGE);
    process.exit(command === undefined || command === '-h' || command === '--help' ? 0 : 2);
  }
  let input: string | undefined;
  let outDir: string | undefined;
  let name: string | undefined;
  let preset: string | undefined;
  let quiet = false;
  let noWasiAsync = false;
  const asyncImports: string[] = [];
  const asyncExports: string[] = [];
  for (let i = 0; i < rest.length; i++) {
    const a = rest[i]!;
    const value = () => rest[++i] ?? fail(`${a} needs a value`);
    if (a === '-o' || a === '--out-dir') outDir = value();
    else if (a === '--name') name = value();
    else if (a === '--preset') preset = value();
    else if (a === '--async-imports') asyncImports.push(value());
    else if (a === '--async-exports') asyncExports.push(value());
    else if (a === '--no-wasi-async') noWasiAsync = true;
    else if (a === '-q' || a === '--quiet') quiet = true;
    else if (a.startsWith('-')) fail(`unknown option ${a}\n\n${USAGE}`);
    else if (input === undefined) input = a;
    else fail(`unexpected argument ${a}`);
  }
  if (input === undefined) fail(`no component given\n\n${USAGE}`);
  if (outDir === undefined) fail('no output directory given (-o <dir>)');
  name ??= basename(input).replace(/\.wasm$/, '');
  const t0 = performance.now();
  const manifest = await transpile(input, { outDir, name, preset, asyncImports, asyncExports, noWasiAsync });
  if (!quiet) {
    const ms = Math.round(performance.now() - t0);
    console.log(`codelib-web: ${input} -> ${outDir}/${manifest.module} (${manifest.coreModules.length} core modules, ${ms} ms)`);
    console.log(`  patches: ${manifest.patches.map((p) => `${p.name}x${p.count}`).join(' ') || '(none)'}`);
  }
}

main(process.argv.slice(2)).catch((error: unknown) => {
  console.error(`codelib-web: ${error instanceof Error ? error.message : String(error)}`);
  process.exit(1);
});
