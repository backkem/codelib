# codelib-web

The browser twin of [`codelib-sandbox`](../crates/codelib-sandbox): a host kit that runs
WebAssembly components, transpiled with [jco](https://github.com/bytecodealliance/jco), in a
browser Worker. It owns jco's event loop, serves virtual WASI 0.2 and 0.3 from a world you choose,
and stops a run that takes too long. On top of it, `codelib-web/script` runs codelib's own
JavaScript sandbox in the page.

**Try it in your browser:** <https://backkem.github.io/codelib/> (a playground built on this
package; [`demo/`](demo)).

Chromium-based browsers only: jco's async runtime and the kit's blocking waits need JSPI
(`WebAssembly.Suspending`, `WebAssembly.promising`).

## The pieces

| Import | What |
|---|---|
| `codelib-web transpile` (CLI), `codelib-web/transpile` | Component in, a directory of patched JS and core modules out, with the pinned jco 1.42.0. Fails on any other jco and when a patch no longer applies ([docs/jco-patches.md](docs/jco-patches.md)). |
| `codelib-web/loop` | The event loop the patched output runs on (`globalThis.__codelibWeb`): `install()`, `settle()`, `touch()`, `until(promise)`, `external(promise)`, `onIdle(handler)`. |
| `codelib-web/wasi` | `wasiImports(world, { http })`: WASI 0.2.x and 0.3.0 clocks, random, cli (stdio, environment, exit, terminals), `wasi:config/store`, io streams for stdio, an empty filesystem (no preopens) and the client side of `wasi:http` over a handler that denies everything by default. `egress(policy)` builds a handler from an allowlist. No sockets. |
| `codelib-web/worlds` | The `World` interface the WASI imports read from, and two built-in worlds (below). |
| `codelib-web/load` | `instantiate(moduleUrl, imports)`: loads a transpiled component, maps jco's naming quirks (unversioned, merged interface names; camel-cased exports) and calls exports by WIT name (`instance.call('wasi:cli/run@0.3.0#run')`). `hasJspi()` / `requireJspi()` with a clear message. |
| `codelib-web/worker` | `runInWorker(url, { init, timeoutMs, onRequest })` on the page and `serveRun(fn)` in the Worker: one run per Worker, ended by terminating the Worker when its wall-clock deadline passes. |
| `codelib-web/script` | `runScript({ program, preludes, input, hostCalls, egress, timeoutMs, engine })`: codelib's JavaScript guest (`crates/codelib-guest`) in a Worker. |
| `codelib-web` | Everything above except the transpile step. |

```js
import { instantiate, realWorld, serveRun, wasiImports } from 'codelib-web';

// in a module Worker started with runInWorker()
serveRun(async (init, ctx) => {
  const world = realWorld({ args: ['app'], stdout: (bytes) => ctx.post(bytes) });
  const app = await instantiate(init.module, wasiImports(world));
  ctx.running(); // the deadline starts here
  await app.call('wasi:cli/run@0.3.0#run');
});
```

## Two worlds

`realWorld()` reads `performance.now()` and `Date`, draws from `crypto.getRandomValues` and sleeps
on real timers: the default, as natively.

`virtualWorld({ seed })` is the browser's `WasiConfig::deterministic`: a monotonic clock that
starts at 0 and only moves when the component is idle, jumping to its next timer; a wall clock at
2026-01-01T00:00:00Z plus that; both random streams seeded. Two runs with one seed print the same
bytes, and a 10-second sleep takes milliseconds. It is repeatable, not identical to a native
run: the order of work inside one instant follows jco, not wasmtime.

An embedder with its own clock (a simulator) implements `World` itself: `now`, `wallNow`,
`timer(at, wake)`, `randomU64`, `randomFill`, `insecureSeed`, `stdout`, `stderr`, and optionally
`stdin`, `config`, `args`, `env`.

## Running codelib's sandbox in the page

```sh
npx codelib-web transpile crates/codelib-guest/codelib-guest.wasm --preset script -o public/engine --name codelib-guest
```

```js
import { jsonHostCalls, loadEngine, runScript } from 'codelib-web/script';

const engine = await loadEngine('/engine/');          // ~13 MB, compiled once per page
const outcome = await runScript({
  engine,
  program: 'console.log("hi"); return host.call("notes.add", { text: "ship it" })',
  hostCalls: jsonHostCalls({ 'notes.add': ({ text }) => notes.push(text) }),
  egress: { allow: [{ host: 'api.github.com', readOnly: true }] },
  timeoutMs: 5000,
});
// { ok: true, value: 1, console: { stdout: 'hi\n', stderr: '', truncated: false }, ms, timings }
```

`host.call`, `host.callRaw` and `host.emit` reach the page while the engine waits (JSPI). `fetch`
goes through a small egress policy: deny by default; allow rules by host (`*.suffix` for
subdomains), optionally read-only, with headers the Worker attaches (they replace the script's
own, so a credential never enters the sandbox); virtual hosts answered by the page. A refusal is a
catchable `fetch` error plus a `[codelib] fetch denied: ...` line on stderr, as natively. There is
no `serve` and no state handed back.

## What the browser does not enforce

A run gets a wall-clock deadline and nothing else: when it passes, the Worker is terminated,
whatever the component was doing. There is no memory cap and no fuel, and a component reaches
only what its imports give it (the kit gives no sockets and no files). The egress policy is a
small subset of codelib-http's: the browser's CORS rules still apply, attached headers are held
by the page (outside the sandbox, but in the page), there is no private-IP blocking or path
scope, redirects are not followed, and bodies are buffered.

| | codelib-sandbox (native) | codelib-web (browser) |
|---|---|---|
| Deadline | per call, epoch interrupt plus wall clock | per run, Worker terminated |
| Memory, fuel | capped, metered | not enforced |
| Egress | codelib-http policy | allowlist, attached headers, virtual hosts |

## Tests

Everything runs on Node 24 with Chromium:

```sh
npm ci
npx playwright-core install chromium-headless-shell   # or set CHROMIUM=/path/to/chromium
npm test        # build, unit tests (Node), browser tests and the playground (headless Chromium)
```

The unit tests check the patches against jco's real output, the version pin, the loop, the
worlds and the egress decisions. The browser tests run a WASI 0.3 fixture
(`test/fixtures/clock-p3`, rebuilt by `test/fixtures/build.sh`) in both worlds, a composed
fixture (`pair-p3`), codelib's guest (values, `await`, console, `host.call`, errors, deadline) and
the egress policy against a local server and a virtual host. The playground test loads the built
site under `/codelib/`, runs examples and checks the canvas, Stop and the credential example.

## The playground

[`demo/`](demo) is the page behind the "try it" link: an editor, examples, and a small drawing API
(`draw.line`, `circle`, `text`, `clear`, `size`) that the page exposes to scripts through
`host.call`, plus an egress policy with one real host and one virtual host that gets a credential
the script never sees. `.github/workflows/pages.yml` builds and deploys it. Locally:

```sh
npm run build && npm run build:demo && npm run serve:demo   # http://127.0.0.1:8080/codelib/
```

## Using it before it is on npm

The package is built and packed into one tarball, and npm installs that:

```sh
cd web && npm ci && npm pack     # builds dist/, writes codelib-web-<version>.tgz
```

- From a release: pushing a tag `web-v<version>` runs `.github/workflows/web-release.yml`, which
  tests the package and attaches the tarball to that GitHub release. A consumer then runs
  `npm install https://github.com/backkem/codelib/releases/download/web-v0.1.0/codelib-web-0.1.0.tgz`.
- Vendored: copy the tarball into the consuming repository and
  `npm install ./vendor/codelib-web-0.1.0.tgz` (`"codelib-web": "file:vendor/codelib-web-0.1.0.tgz"`).

Either way the consumer's lockfile records the tarball's integrity hash, no build runs on install,
and `dist/` stays out of git. npm cannot install a package from a subdirectory of a git
repository, which rules out a plain git dependency.
