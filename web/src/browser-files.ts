// Which files of the built package a page or Worker can load.
//
// A Worker cannot resolve a bare `codelib-web/...` specifier, so embedders that start Workers
// from their own files copy the browser-side modules next to their own code (`codelib-web
// browser-files -o <dir>`). The modules import each other by relative path, so the copy keeps the
// layout of `dist/`. This file is the one place that says which modules those are: every
// JavaScript file of `dist/` except the ones that need Node. A test checks the result against
// the built package (test/unit/browser-files.test.mjs).

import { copyFileSync, mkdirSync, readdirSync } from 'node:fs';
import { dirname, join, relative, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

/** The modules that need Node: the transpile step, the CLI, and this file. */
const NODE_ONLY = [/^transpile\//, /^cli\.js$/, /^browser-files\.js$/];

/** Whether the file at `rel` (a `/`-separated path inside `dist/`) is a browser module. */
export function isBrowserModule(rel: string): boolean {
  return rel.endsWith('.js') && !NODE_ONLY.some((pattern) => pattern.test(rel));
}

function walk(dir: string, base: string, out: string[]): void {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) walk(path, base, out);
    else out.push(relative(base, path).split(sep).join('/'));
  }
}

/** The package's `dist/` directory: where this module was loaded from. */
export function distDir(): string {
  return dirname(fileURLToPath(import.meta.url));
}

/** The browser modules under `dist` (default: the package's own), as sorted relative paths. */
export function browserModules(dist: string = distDir()): string[] {
  const all: string[] = [];
  walk(dist, dist, all);
  return all.filter(isBrowserModule).sort();
}

/** Copies the browser modules into `outDir`, keeping relative paths; returns the paths copied. */
export function copyBrowserFiles(outDir: string, dist: string = distDir()): string[] {
  const files = browserModules(dist);
  for (const file of files) {
    const target = join(outDir, ...file.split('/'));
    mkdirSync(dirname(target), { recursive: true });
    copyFileSync(join(dist, ...file.split('/')), target);
  }
  return files;
}
