// The playground, end to end in headless Chromium: serve the built site (demo/dist, from
// `npm run build:demo`) under /codelib/ as GitHub Pages does, wait for the engine, run the
// default example and a drawing example, stop an endless loop, and run the credentials example.
//
//   node test/demo/run.mjs

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { chromium } from 'playwright-core';

import { serve } from '../../scripts/serve.mjs';

const web = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');
const site = path.join(web, 'demo/dist');
if (!fs.existsSync(path.join(site, 'main.js'))) {
  console.error('demo/dist is not built: run `npm run build:demo` first');
  process.exit(2);
}

const server = await serve(site, { port: 0 });
const url = `http://127.0.0.1:${server.address().port}/codelib/`;
const executablePath = process.env.CHROMIUM ?? (fs.existsSync('/usr/bin/chromium') ? '/usr/bin/chromium' : undefined);
const browser = await chromium.launch({ headless: true, ...(executablePath ? { executablePath } : {}) });
const page = await browser.newPage({ viewport: { width: 1200, height: 900 } });
const errors = [];
page.on('pageerror', (e) => errors.push(e.message));
const external = [];
page.on('request', (r) => {
  if (!r.url().startsWith(url) && !r.url().startsWith('data:')) external.push(r.url());
});

const steps = [];
async function step(name, fn) {
  const t0 = performance.now();
  try {
    const detail = await fn();
    steps.push({ name, ok: true, ms: Math.round(performance.now() - t0), detail });
    console.log(`[pass] ${name} (${Math.round(performance.now() - t0)} ms)${detail ? ` ${JSON.stringify(detail)}` : ''}`);
  } catch (e) {
    steps.push({ name, ok: false, error: e.message });
    console.log(`[FAIL] ${name}: ${e.message.split('\n').slice(0, 6).join('\n    ')}`);
  }
}

const status = () => page.textContent('#status');
async function runExample(id, until = /^done$/) {
  await page.selectOption('#example', id);
  await page.click('#run');
  await page.waitForFunction((re) => new RegExp(re).test(document.getElementById('status').textContent), until.source, { timeout: 30_000 });
}

await step('loads and compiles the engine', async () => {
  const t0 = performance.now();
  await page.goto(url);
  await page.waitForSelector('body[data-ready="true"]', { timeout: 60_000 });
  return { engineReadyMs: Math.round(performance.now() - t0), status: await status() };
});

await step('runs the default example', async () => {
  if ((await page.inputValue('#example')) !== 'value') throw new Error('the default example is not "value"');
  await page.focus('#code');
  await page.keyboard.press('Control+Enter');
  await page.waitForFunction(() => document.getElementById('status').textContent === 'done', null, { timeout: 30_000 });
  const value = await page.textContent('#value');
  if (!value.includes('"answer": 42')) throw new Error(`result: ${value}`);
  return { elapsed: await page.textContent('#elapsed') };
});

await step('a drawing example paints the canvas', async () => {
  await runExample('spirograph');
  const painted = await page.evaluate(() => {
    const c = document.getElementById('canvas');
    const data = c.getContext('2d').getImageData(0, 0, c.width, c.height).data;
    let n = 0;
    for (let i = 3; i < data.length; i += 4) if (data[i] > 0) n++;
    return n / (c.width * c.height);
  });
  if (painted < 0.5) throw new Error(`only ${(painted * 100).toFixed(1)}% of the canvas is painted`);
  return { painted: `${Math.round(painted * 100)}%`, value: (await page.textContent('#value')).trim(), elapsed: await page.textContent('#elapsed') };
});

await step('Stop ends an endless loop', async () => {
  await page.selectOption('#example', 'loop');
  await page.click('#run');
  await page.waitForFunction(() => document.getElementById('console').textContent.includes('spinning'), null, { timeout: 10_000 });
  await page.click('#stop');
  await page.waitForFunction(() => document.getElementById('status').textContent === 'stopped', null, { timeout: 5000 });
  if (await page.isDisabled('#run')) throw new Error('Run stays disabled after Stop');
  return { elapsed: await page.textContent('#elapsed') };
});

await step('credentials stay outside the sandbox', async () => {
  await runExample('credentials');
  const value = JSON.parse(await page.textContent('#value'));
  if (value.tokenSeen !== false || value.plain.authorized !== true || value.forged.authorized !== true) throw new Error(JSON.stringify(value));
  const policy = await page.textContent('#policy');
  if (policy.includes('not-a-secret')) throw new Error('the policy panel shows the token');
  return { tokenEndsWith: value.plain.tokenEndsWith };
});

await step('the page made no external requests', async () => {
  if (external.length > 0) throw new Error(external.join(', '));
});

await browser.close();
server.close();
if (errors.length > 0) console.log(`page errors:\n  ${errors.join('\n  ')}`);
const failed = steps.filter((s) => !s.ok).length;
fs.mkdirSync(path.join(web, 'test-results'), { recursive: true });
fs.writeFileSync(path.join(web, 'test-results/demo.json'), `${JSON.stringify({ steps, errors }, null, 2)}\n`);
console.log(failed === 0 ? `demo test: all ${steps.length} steps passed` : `demo test: ${failed} of ${steps.length} steps failed`);
process.exit(failed === 0 && errors.length === 0 ? 0 : 1);
