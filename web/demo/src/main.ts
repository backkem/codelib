// The playground: write a program, press Run, it runs in codelib's sandbox in this tab.
//
// Everything here is what an embedder of codelib-web/script does: load the engine once, then
// for every run hand `runScript` the program, a prelude and host calls for the page's own API,
// an egress policy, and a deadline.

import { hasJspi, loadEngine, runScript, type Engine, type ScriptEgress } from 'codelib-web/script';

import { DRAW_PRELUDE, drawApi } from './draw.js';
import { EXAMPLES } from './examples.js';

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const editor = $<HTMLTextAreaElement>('code');
const picker = $<HTMLSelectElement>('example');
const runButton = $<HTMLButtonElement>('run');
const stopButton = $<HTMLButtonElement>('stop');
const status = $<HTMLElement>('status');
const valueOut = $<HTMLElement>('value');
const consoleOut = $<HTMLElement>('console');
const elapsed = $<HTMLElement>('elapsed');
const canvas = $<HTMLCanvasElement>('canvas');
const loading = $<HTMLElement>('loading');
const progress = $<HTMLProgressElement>('progress');
const policyList = $<HTMLElement>('policy');

const TIMEOUT_MS = 5000;

// A fixed fake token: the point is where it lives (in this page, outside the sandbox), not
// that it is secret.
const DEMO_TOKEN = 'demo-token-not-a-secret-k7Qz';

const encoder = new TextEncoder();
async function fingerprint(text: string): Promise<string> {
  const digest = new Uint8Array(await crypto.subtle.digest('SHA-256', encoder.encode(text)));
  return [...digest.slice(0, 4)].map((b) => b.toString(16).padStart(2, '0')).join('');
}

// The page's egress policy: one real host, read-only, and one virtual host answered right here.
const EGRESS: ScriptEgress = {
  allow: [
    { host: 'api.github.com', readOnly: true },
    { host: 'vault.internal', headers: { Authorization: `Bearer ${DEMO_TOKEN}` } },
  ],
  virtualHosts: {
    'vault.internal': async (request) => {
      const auth = request.headers.get('authorization') ?? '';
      const authorized = auth === `Bearer ${DEMO_TOKEN}`;
      const body = {
        authorized,
        identity: authorized ? 'the playground page' : null,
        // Enough to see which token arrived, never the token itself.
        tokenFingerprint: auth ? `sha256:${await fingerprint(auth)}…` : null,
        tokenEndsWith: auth ? `…${auth.slice(-4)}` : null,
        secrets: authorized ? ['the cake is in the fridge'] : [],
      };
      return new Response(JSON.stringify(body), { status: authorized ? 200 : 401, headers: { 'content-type': 'application/json' } });
    },
  },
};

function showPolicy(): void {
  const mask = (v: string) => (v.length <= 12 ? '••••' : `${v.slice(0, 7)}••••${v.slice(-2)}`);
  const items = (EGRESS.allow ?? []).map((rule) => {
    const parts = [rule.readOnly ? 'GET/HEAD only' : 'any method'];
    if (rule.host in (EGRESS.virtualHosts ?? {})) parts.unshift('virtual host, answered by this page');
    for (const [k, v] of Object.entries(rule.headers ?? {})) parts.push(`attaches ${k}: ${mask(v)}`);
    return `<li><code>${rule.host}</code> <span>${parts.join(' · ')}</span></li>`;
  });
  items.push('<li><code>*</code> <span>everything else: denied</span></li>');
  policyList.innerHTML = items.join('');
}

const draw = drawApi(canvas);
let engine: Engine | null = null;
let current: AbortController | null = null;

function setStatus(text: string, kind: 'info' | 'ok' | 'error' = 'info'): void {
  status.textContent = text;
  status.dataset.kind = kind;
}

function pick(id: string): void {
  const example = EXAMPLES.find((e) => e.id === id) ?? EXAMPLES[0]!;
  picker.value = example.id;
  editor.value = example.code;
}

async function run(): Promise<void> {
  if (!engine || current) return;
  const controller = new AbortController();
  current = controller;
  runButton.disabled = true;
  stopButton.disabled = false;
  valueOut.textContent = '';
  consoleOut.textContent = '';
  elapsed.textContent = '';
  setStatus('running…');
  const outcome = await runScript({
    engine,
    program: { name: 'playground.js', code: editor.value },
    preludes: [{ name: 'draw.js', code: DRAW_PRELUDE }],
    hostCalls: draw.hostCalls,
    egress: EGRESS,
    timeoutMs: TIMEOUT_MS,
    signal: controller.signal,
    onConsole: (_stream, text) => {
      consoleOut.textContent += text;
    },
  });
  current = null;
  runButton.disabled = false;
  stopButton.disabled = true;
  // The final console is the captured one (stdout, then stderr).
  consoleOut.textContent = outcome.console.stdout + outcome.console.stderr || '(no output)';
  elapsed.textContent = `${outcome.ms} ms`;
  if (outcome.ok) {
    valueOut.textContent = JSON.stringify(outcome.value, null, 2);
    setStatus('done', 'ok');
  } else {
    const e = outcome.error;
    valueOut.textContent = e.kind === 'script' ? `${e.name}: ${e.message}${e.stack ? `\n\n${e.stack}` : ''}` : e.message;
    setStatus(
      e.kind === 'timeout' ? `stopped by the ${TIMEOUT_MS / 1000} s deadline` : e.kind === 'stopped' ? 'stopped' : e.kind === 'script' ? 'the program threw' : 'the engine failed',
      'error',
    );
  }
}

function stop(): void {
  current?.abort();
}

async function start(): Promise<void> {
  for (const e of EXAMPLES) picker.add(new Option(e.title, e.id));
  pick(new URLSearchParams(location.search).get('example') ?? 'value');
  showPolicy();
  picker.addEventListener('change', () => pick(picker.value));
  runButton.addEventListener('click', () => void run());
  stopButton.addEventListener('click', stop);
  editor.addEventListener('keydown', (event) => {
    if (event.key === 'Enter' && (event.ctrlKey || event.metaKey)) {
      event.preventDefault();
      void run();
    } else if (event.key === 'Tab' && !event.shiftKey) {
      event.preventDefault();
      editor.setRangeText('  ', editor.selectionStart, editor.selectionEnd, 'end');
    }
  });

  if (!hasJspi()) {
    loading.hidden = true;
    setStatus('This browser cannot run the sandbox: it needs WebAssembly JSPI, which Chromium-based browsers (Chrome, Edge 137+) have.', 'error');
    return;
  }
  try {
    engine = await loadEngine(new URL('./engine/', location.href), {
      onProgress: (loaded, total) => {
        progress.max = total;
        progress.value = loaded;
      },
    });
  } catch (error) {
    loading.hidden = true;
    setStatus(`The engine failed to load: ${error instanceof Error ? error.message : String(error)}`, 'error');
    return;
  }
  loading.hidden = true;
  runButton.disabled = false;
  setStatus('ready: press Run (or Ctrl/Cmd+Enter)');
  document.body.dataset.ready = 'true';
}

void start();
