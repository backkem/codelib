// Runs the browser tests in headless Chromium.
//
//   node test/browser/run.mjs [--no-transpile] [NAME-SUBSTRING...]
//
// 1. Transpiles the fixtures and codelib's guest component with the kit (dist/, so run
//    `npm run build` first) into test/browser/build/.
// 2. Serves web/ over HTTP, plus a few /api/ endpoints the HTTP tests talk to.
// 3. Opens test/browser/index.html and runs every test (or those whose name contains one of the
//    arguments), printing pass/fail and timings; writes test-results/browser.json.
//
// Chromium: $CHROMIUM, else /usr/bin/chromium if it exists, else Playwright's own build
// (`npx playwright-core install chromium-headless-shell`).

import fs from 'node:fs';
import http from 'node:http';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { chromium } from 'playwright-core';

import { transpile } from '../../dist/transpile/index.js';

const web = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');
const repo = path.resolve(web, '..');
const build = path.join(web, 'test/browser/build');
const results = path.join(web, 'test-results');
const args = process.argv.slice(2);
const noTranspile = args.includes('--no-transpile');
const filters = args.filter((a) => !a.startsWith('--'));

const timings = {};

// 1. Transpile.
const components = [
  { name: 'clock-p3', wasm: path.join(web, 'test/fixtures/clock-p3.wasm') },
  { name: 'pair-p3', wasm: path.join(web, 'test/fixtures/pair-p3.wasm') },
  { name: 'codelib-guest', wasm: path.join(repo, 'crates/codelib-guest/codelib-guest.wasm'), preset: 'script' },
];
if (!noTranspile) {
  for (const c of components) {
    const t0 = performance.now();
    const manifest = await transpile(c.wasm, { outDir: path.join(build, c.name), name: c.name, preset: c.preset });
    const ms = Math.round(performance.now() - t0);
    timings[`transpile ${c.name}`] = ms;
    console.log(`transpiled ${c.name} in ${ms} ms: ${manifest.patches.map((p) => `${p.name}x${p.count}`).join(' ')}`);
  }
}

// 2. Serve web/ and the test API.
const TYPES = { '.html': 'text/html', '.js': 'text/javascript', '.mjs': 'text/javascript', '.wasm': 'application/wasm', '.json': 'application/json', '.css': 'text/css', '.map': 'application/json' };
const received = [];
const server = http.createServer((req, res) => {
  const url = new URL(req.url, 'http://x');
  if (url.pathname.startsWith('/api/')) return api(url, req, res);
  const file = path.join(web, decodeURIComponent(url.pathname));
  if (!file.startsWith(web) || !fs.existsSync(file) || !fs.statSync(file).isFile()) {
    res.writeHead(404).end();
    return;
  }
  res.writeHead(200, { 'content-type': TYPES[path.extname(file)] ?? 'application/octet-stream' });
  fs.createReadStream(file).pipe(res);
});

// /api/echo records what it received (method, headers, body) and answers without repeating any
// header back; /api/received hands the record to the page (outside the sandbox).
function api(url, req, res) {
  const chunks = [];
  req.on('data', (c) => chunks.push(c));
  req.on('end', () => {
    if (url.pathname === '/api/echo') {
      received.push({ method: req.method, path: url.pathname, headers: req.headers, body: Buffer.concat(chunks).toString('utf8') });
      res.writeHead(200, { 'content-type': 'application/json', 'x-served-by': 'echo' });
      res.end(JSON.stringify({ ok: true, method: req.method, bodyBytes: Buffer.concat(chunks).length }));
    } else if (url.pathname === '/api/received') {
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(JSON.stringify(received.splice(0)));
    } else {
      res.writeHead(404).end();
    }
  });
}
await new Promise((r) => server.listen(0, '127.0.0.1', r));
const base = `http://127.0.0.1:${server.address().port}`;

// 3. Run.
const executablePath = process.env.CHROMIUM ?? (fs.existsSync('/usr/bin/chromium') ? '/usr/bin/chromium' : undefined);
const browser = await chromium.launch({ headless: true, ...(executablePath ? { executablePath } : {}) });
const page = await browser.newPage();
const pageErrors = [];
page.on('pageerror', (e) => pageErrors.push(e.message));
page.on('console', (m) => {
  if (m.type() === 'error') pageErrors.push(m.text());
});
await page.goto(`${base}/test/browser/index.html`);
await page.waitForFunction(() => window.ready === true, null, { timeout: 30_000 });
console.log(`chromium ${browser.version()}${executablePath ? ` (${executablePath})` : ''}`);

const names = (await page.evaluate(() => window.testNames())).filter((n) => filters.length === 0 || filters.some((f) => n.includes(f)));
const report = [];
let failures = 0;
for (const name of names) {
  const t0 = performance.now();
  let status = 'pass';
  let detail = null;
  try {
    detail = await page.evaluate((n) => window.runTest(n), name);
  } catch (e) {
    status = 'FAIL';
    failures++;
    detail = e.message.split('\n').slice(0, 12).join('\n    ');
  }
  const ms = Math.round(performance.now() - t0);
  report.push({ name, status, ms, detail });
  console.log(`[${status}] ${name} (${ms} ms)`);
  if (detail !== null) console.log(`    ${typeof detail === 'string' ? detail : JSON.stringify(detail)}`);
}
await browser.close();
server.close();

fs.mkdirSync(results, { recursive: true });
fs.writeFileSync(path.join(results, 'browser.json'), `${JSON.stringify({ chromium: browser.version(), timings, tests: report, pageErrors }, null, 2)}\n`);
if (pageErrors.length > 0) console.log(`page errors:\n  ${pageErrors.slice(0, 20).join('\n  ')}`);
console.log(failures === 0 ? `browser tests: all ${names.length} passed` : `browser tests: ${failures} of ${names.length} failed`);
process.exit(failures === 0 && names.length > 0 ? 0 : 1);
