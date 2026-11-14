// Tests of codelib's JavaScript guest under the kit (codelib-web/script), and of the egress
// policy behind its `fetch`.

import { HostFailure, jsonHostCalls, loadEngine, runScript } from '/dist/script/index.js';
import { register as test } from './tests.js';

const ENGINE = '/test/browser/build/codelib-guest';
let engine = null;
let engineMs = null;
async function getEngine() {
  if (engine === null) {
    const t0 = performance.now();
    engine = await loadEngine(ENGINE);
    engineMs = Math.round(performance.now() - t0);
  }
  return engine;
}

function check(cond, message) {
  if (!cond) throw new Error(message);
}
const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);

async function script(options) {
  return runScript({ engine: await getEngine(), ...options });
}
function value(outcome) {
  if (!outcome.ok) throw new Error(`run failed: ${JSON.stringify(outcome.error)}\nconsole: ${JSON.stringify(outcome.console)}`);
  return outcome.value;
}

test('script: a program returns a JSON value', async () => {
  const o = await script({ program: 'return { answer: 6 * 7, list: [1, "two", null], input: host.input }', input: { x: 1 } });
  check(same(value(o), { answer: 42, list: [1, 'two', null], input: { x: 1 } }), `value: ${JSON.stringify(o.value)}`);
  const empty = await script({ program: 'const x = 1;' });
  check(value(empty) === null, 'a program without return gives null');
  return { engineLoadAndCompileMs: engineMs, instantiateMs: Math.round(o.timings.instantiateMs), runMs: Math.round(o.timings.runMs), totalMs: o.ms };
});

test('script: top-level await, real and virtual timers', async () => {
  const program = 'const t0 = Date.now(); await new Promise((r) => setTimeout(r, 300)); return Date.now() - t0;';
  const real = await script({ program });
  check(value(real) >= 290 && real.timings.runMs >= 290, `real: slept ${real.value} ms in ${real.timings.runMs} ms`);
  const virtual = await script({ program, seed: 1 });
  check(value(virtual) >= 300 && virtual.timings.runMs < 250, `virtual: slept ${virtual.value} ms in ${virtual.timings.runMs} ms`);
  return { realRunMs: Math.round(real.timings.runMs), virtualRunMs: Math.round(virtual.timings.runMs), virtualSleptMs: virtual.value };
});

test('script: one seed, the same randomness and clock', async () => {
  const program = 'return [Math.random(), crypto.getRandomValues(new Uint8Array(4)).join(","), Date.now()]';
  const a = value(await script({ program, seed: 5 }));
  const b = value(await script({ program, seed: 5 }));
  const c = value(await script({ program }));
  check(same(a, b), `seed 5 twice: ${JSON.stringify(a)} vs ${JSON.stringify(b)}`);
  check(a[2] === 1767225600000, `virtual Date.now(): ${a[2]}`);
  check(!same(a, c) && Math.abs(c[2] - Date.now()) < 60_000, `real run: ${JSON.stringify(c)}`);
});

test('script: console.log is captured', async () => {
  const lines = [];
  const o = await script({
    program: 'console.log("hello", 42); console.error("oops"); console.log({ a: 1 }); return null',
    onConsole: (stream, text) => lines.push([stream, text]),
  });
  value(o);
  check(o.console.stdout.includes('hello 42\n'), `stdout: ${JSON.stringify(o.console.stdout)}`);
  check(o.console.stderr.includes('oops\n') && !o.console.stdout.includes('oops'), `stderr: ${JSON.stringify(o.console)}`);
  check(lines.length > 0, 'no console streamed');
  return { console: o.console };
});

test('script: host.call round trip with an async host function', async () => {
  const emitted = [];
  const calls = [];
  const hostCalls = async (method, args) => {
    calls.push(method);
    if (method === 'echo.raw') return `raw:${args}`;
    if (method === 'math.add') {
      await new Promise((r) => setTimeout(r, 20));
      const { a, b } = JSON.parse(args);
      return JSON.stringify({ sum: a + b });
    }
    throw new HostFailure('unknown-method', `no ${method}`, { hint: 'try math.add', detail: { known: ['math.add'] } });
  };
  const o = await script({
    hostCalls,
    onEmit: (kind, item) => emitted.push([kind, item]),
    program: `
      const r = host.call("math.add", { a: 2, b: 3 });
      const raw = host.callRaw("echo.raw", "not json");
      host.emit("progress", { done: 1 });
      let failure;
      try { host.call("nope", {}); } catch (e) { failure = [e.name, e.code, e.hint, e.detail, e instanceof host.HostError]; }
      return { r, raw, failure };`,
  });
  check(same(value(o), { r: { sum: 5 }, raw: 'raw:not json', failure: ['HostError', 'unknown-method', 'try math.add', { known: ['math.add'] }, true] }), `value: ${JSON.stringify(o.value)}`);
  check(same(emitted, [['progress', { done: 1 }]]), `emitted: ${JSON.stringify(emitted)}`);
  check(same(calls, ['math.add', 'echo.raw', 'nope']), `calls: ${JSON.stringify(calls)}`);

  // Per-call cost: 500 calls against a trivial host function, minus a run without them.
  const fast = jsonHostCalls({ inc: (n) => n + 1 });
  const loop = await script({ hostCalls: fast, program: 'let n = 0; for (let i = 0; i < 500; i++) n = host.call("inc", n); return n' });
  check(value(loop) === 500, `loop: ${loop.value}`);
  const base = await script({ hostCalls: fast, program: 'let n = 0; for (let i = 0; i < 500; i++) n = n + 1; return n' });
  const perCallUs = Math.round(((loop.timings.runMs - base.timings.runMs) / 500) * 1000);
  return { perHostCallUs: perCallUs, loopRunMs: Math.round(loop.timings.runMs) };
});

test('script: an uncaught error is reported with name and message', async () => {
  const o = await script({ program: 'function f() { throw new TypeError("bad thing"); }\nf();' });
  check(!o.ok && o.error.kind === 'script', `outcome: ${JSON.stringify(o)}`);
  check(o.error.name === 'TypeError' && o.error.message === 'bad thing', `error: ${JSON.stringify(o.error)}`);
  check((o.error.stack ?? '').includes('script.js:1'), `stack: ${o.error.stack}`);
  const s = await script({ program: 'return )' });
  check(!s.ok && s.error.name === 'SyntaxError', `syntax error: ${JSON.stringify(s.error)}`);
  return { stack: o.error.stack };
});

test('script: fetch is denied by default and catchable', async () => {
  const o = await script({
    program: `
      try { await fetch("https://example.com/private/x?token=secret"); return "reached"; }
      catch (e) { return e.name + ": " + e.message; }`,
  });
  check(typeof value(o) === 'string' && o.value !== 'reached', `value: ${o.value}`);
  check(o.console.stderr.includes('[codelib] fetch denied: GET https://example.com/private/x (no rule allows this host)'), `stderr: ${o.console.stderr}`);
  check(!o.console.stderr.includes('secret'), 'the query string was logged');
  return { caught: o.value };
});

test('script: an endless loop is stopped by the deadline', async () => {
  const o = await script({ program: 'console.log("looping"); while (true) {}', timeoutMs: 1000 });
  check(!o.ok && o.error.kind === 'timeout', `outcome: ${JSON.stringify(o)}`);
  check(o.ms < 6000, `took ${o.ms} ms`);
  check(o.console.stdout === 'looping\n', `console before the loop: ${JSON.stringify(o.console)}`);
  const controller = new AbortController();
  setTimeout(() => controller.abort(), 300);
  const stopped = await script({ program: 'while (true) {}', signal: controller.signal });
  check(!stopped.ok && stopped.error.kind === 'stopped', `stopped: ${JSON.stringify(stopped)}`);
  return { timeoutOutcomeMs: o.ms, stoppedMs: stopped.ms };
});

// Egress.

const origin = location.origin; // the test server: http://127.0.0.1:<port>
const local = { host: '127.0.0.1', httpsOnly: false };
const received = async () => (await fetch('/api/received')).json();

test('egress: an allowed GET reaches a real server', async () => {
  await received();
  const o = await script({
    egress: { allow: [local] },
    input: { origin },
    program: 'const r = await fetch(host.input.origin + "/api/echo?x=1"); return [r.status, r.headers.get("x-served-by"), await r.json()]',
  });
  check(same(value(o), [200, 'echo', { ok: true, method: 'GET', bodyBytes: 0 }]), `value: ${JSON.stringify(o.value)}`);
  const seen = await received();
  check(seen.length === 1 && seen[0].method === 'GET', `server saw: ${JSON.stringify(seen)}`);
  const https = await script({ egress: { allow: [{ host: '127.0.0.1' }] }, input: { origin }, program: 'try { await fetch(host.input.origin + "/api/echo"); return "reached" } catch { return "denied" }' });
  check(value(https) === 'denied' && https.console.stderr.includes('(only https is allowed)'), `https-only rule: ${JSON.stringify(https)}`);
});

test('egress: a read-only rule refuses POST', async () => {
  await received();
  const o = await script({
    egress: { allow: [{ ...local, readOnly: true }] },
    input: { origin },
    program: `
      const get = (await fetch(host.input.origin + "/api/echo")).status;
      let post;
      try { await fetch(host.input.origin + "/api/echo", { method: "POST", body: "x" }); post = "sent"; } catch { post = "refused"; }
      return [get, post];`,
  });
  check(same(value(o), [200, 'refused']), `value: ${JSON.stringify(o.value)}`);
  check(o.console.stderr.includes(`fetch denied: POST ${origin}/api/echo (the rule for 127.0.0.1 is read-only)`), `stderr: ${o.console.stderr}`);
  const seen = await received();
  check(seen.length === 1 && seen[0].method === 'GET', `server saw: ${JSON.stringify(seen)}`);
  // A POST with a body goes out under a rule that allows it.
  const ok = await script({ egress: { allow: [local] }, input: { origin }, program: 'return (await (await fetch(host.input.origin + "/api/echo", { method: "POST", body: "hello" })).json())' });
  check(same(value(ok), { ok: true, method: 'POST', bodyBytes: 5 }), `post: ${JSON.stringify(ok.value)}`);
  const seenPost = await received();
  check(seenPost[0]?.body === 'hello', `posted body: ${JSON.stringify(seenPost)}`);
});

const TOKEN = 'Bearer page-held-token-7f3a';

test('egress: rule headers replace the script\'s own', async () => {
  await received();
  const o = await script({
    egress: { allow: [{ ...local, headers: { Authorization: TOKEN, 'X-Page': 'yes' } }] },
    input: { origin },
    program: `
      const r = await fetch(host.input.origin + "/api/echo", { headers: { authorization: "Bearer forged", "x-script": "1" } });
      return r.status;`,
  });
  check(value(o) === 200, `status: ${o.value}`);
  const [seen] = await received();
  check(seen.headers.authorization === TOKEN, `server saw authorization ${JSON.stringify(seen.headers.authorization)}`);
  check(seen.headers['x-page'] === 'yes' && seen.headers['x-script'] === '1', `headers: ${JSON.stringify(seen.headers)}`);
});

test('egress: a virtual host answers in-process, with an injected credential', async () => {
  let saw = null;
  const vault = (request) => {
    saw = request.headers.get('authorization');
    const authorized = saw === TOKEN;
    return new Response(JSON.stringify({ authorized, tokenEndsWith: saw?.slice(-4) ?? null }), { status: authorized ? 200 : 401, headers: { 'content-type': 'application/json' } });
  };
  const o = await script({
    egress: { allow: [{ host: 'vault.internal', headers: { authorization: TOKEN } }], virtualHosts: { 'vault.internal': vault } },
    program: `
      const plain = await (await fetch("https://vault.internal/data")).json();
      const forged = await (await fetch("https://vault.internal/data", { headers: { Authorization: "Bearer forged" } })).json();
      return { plain, forged };`,
  });
  check(same(value(o), { plain: { authorized: true, tokenEndsWith: '7f3a' }, forged: { authorized: true, tokenEndsWith: '7f3a' } }), `value: ${JSON.stringify(o.value)}`);
  check(saw === TOKEN, 'the virtual host did not get the page token');
  // Without a rule, the virtual host is reachable but gets no credential.
  const bare = await script({ egress: { virtualHosts: { 'vault.internal': vault } }, program: 'return (await fetch("http://vault.internal/x")).status' });
  check(value(bare) === 401, `bare: ${bare.value}`);
});

test('egress: the script never sees an injected header', async () => {
  await received();
  const vault = (request) => new Response(JSON.stringify({ ok: request.headers.get('authorization') === TOKEN }), { headers: { 'content-type': 'application/json' } });
  const o = await script({
    egress: {
      allow: [{ ...local, headers: { Authorization: TOKEN } }, { host: 'vault.internal', headers: { Authorization: TOKEN } }],
      virtualHosts: { 'vault.internal': vault },
    },
    input: { origin },
    program: `
      const seen = [];
      const req = new Request(host.input.origin + "/api/echo");
      const r1 = await fetch(req);
      seen.push(JSON.stringify([...r1.headers]), await r1.text(), JSON.stringify([...req.headers]));
      const r2 = await fetch("https://vault.internal/");
      seen.push(JSON.stringify([...r2.headers]), await r2.text());
      seen.push(JSON.stringify(host.input));
      for (const k of Object.getOwnPropertyNames(globalThis)) { try { seen.push(String(globalThis[k])); } catch {} }
      const text = seen.join("\\n");
      return { found: text.includes("page-held-token"), vault: JSON.parse(seen[4]) };`,
  });
  check(same(value(o), { found: false, vault: { ok: true } }), `value: ${JSON.stringify(o.value)}`);
  const [seen] = await received();
  check(seen?.headers.authorization === TOKEN, 'the server did not get the token');
  check(!o.console.stdout.includes('page-held-token') && !o.console.stderr.includes('page-held-token'), 'the token reached the console');
});
