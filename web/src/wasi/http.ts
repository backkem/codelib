// wasi:http@0.2 (types, outgoing-handler) over a request handler the embedder supplies.
//
// The kit decides nothing about the network: every request a component sends becomes one call
// of an `HttpHandler`. The default handler denies everything, as the native host kit does
// without an egress policy, so a JavaScript guest's `fetch` rejects with a catchable error.
// `egress()` (egress.ts) builds a handler from an allowlist; an embedder may write its own.
//
// Bodies are buffered: the handler sees the whole request body once the component has
// finished writing it, and the response body is handed to the component in one piece. Fine for
// API calls; not for streaming. The server side of wasi:http (incoming-handler) is not served
// in the browser; its resource types exist only so components that import them instantiate.

import { external } from '../loop.js';
import { CLOSED, EMPTY, bytesSource, type InputStreamLike, type Io, type IoErrorLike, type OutputStreamLike } from './io.js';

/** A request a component sent, as the handler sees it. Header names are lowercase. */
export interface HttpRequest {
  method: string;
  url: string;
  headers: [string, string][];
  /** The request body, once the component has finished writing it; `undefined` if none. */
  body: Promise<Uint8Array | undefined>;
}

export interface HttpResponse {
  status: number;
  headers: [string, string][];
  body?: Uint8Array;
}

/** A `wasi:http/types#error-code` value, as jco represents variants. */
export interface HttpErrorCode {
  tag: string;
  val?: unknown;
}

/**
 * Thrown (or rejected) by a handler to fail a request with a specific `error-code`; any other
 * error becomes `internal-error` with its message. Thrown synchronously, the component's
 * `handle` call fails at once; otherwise the response future fails.
 */
export class HttpError extends Error {
  /** jco reads a thrown value's `payload` as the error arm of a WIT `result`. */
  readonly payload: HttpErrorCode;
  constructor(code: HttpErrorCode, message?: string) {
    super(message ?? code.tag);
    this.name = 'HttpError';
    this.payload = code;
  }
}

/**
 * Handles one request. May throw `HttpError` synchronously (refused before anything is sent),
 * or return a promise of the response.
 */
export type HttpHandler = (request: HttpRequest) => HttpResponse | Promise<HttpResponse>;

/** The default: every request is refused with `HTTP-request-denied`. */
export const denyAll: HttpHandler = () => {
  throw new HttpError({ tag: 'HTTP-request-denied' }, 'outgoing HTTP is not allowed');
};

const METHODS = ['get', 'head', 'post', 'put', 'delete', 'connect', 'options', 'trace', 'patch'];

function methodName(m: HttpErrorCode | undefined): string {
  if (!m) return 'GET';
  return m.tag === 'other' ? String(m.val) : m.tag.toUpperCase();
}

function methodVariant(name: string): HttpErrorCode {
  const lower = name.toLowerCase();
  return METHODS.includes(lower) ? { tag: lower } : { tag: 'other', val: name };
}

function schemeName(s: HttpErrorCode | undefined): string {
  if (!s) return 'https';
  if (s.tag === 'other') return String(s.val).toLowerCase();
  return s.tag.toLowerCase();
}

// Header bytes are carried as Latin-1 strings, which maps every byte to one character and back.
function latin1(bytes: Uint8Array): string {
  let s = '';
  for (const b of bytes) s += String.fromCharCode(b);
  return s;
}
function fromLatin1(s: string): Uint8Array {
  const out = new Uint8Array(s.length);
  for (let i = 0; i < s.length; i++) out[i] = s.charCodeAt(i) & 0xff;
  return out;
}

const TOKEN = /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/;
const INVALID = Object.freeze({ tag: 'invalid-syntax' as const });
const IMMUTABLE = Object.freeze({ tag: 'immutable' as const });

function checkName(name: string): string {
  if (!TOKEN.test(name)) throw INVALID;
  return name.toLowerCase();
}
function checkValue(value: Uint8Array): Uint8Array {
  for (const b of value) if (b === 0 || b === 10 || b === 13) throw INVALID;
  return value.slice();
}

export function createHttp(io: Io, handler: HttpHandler = denyAll): { imports: Record<string, Record<string, unknown>> } {
  const { Pollable, IoError, InputStream, OutputStream } = io;

  class Fields {
    #entries: [string, Uint8Array][] = [];
    #immutable = false;
    constructor() {}
    static fromList(entries: [string, Uint8Array][]): Fields {
      const f = new Fields();
      for (const [name, value] of entries) f.#entries.push([checkName(name), checkValue(value)]);
      return f;
    }
    /** Host side: a frozen copy of `entries`. */
    static frozen(entries: [string, string][]): Fields {
      const f = new Fields();
      for (const [name, value] of entries) f.#entries.push([name.toLowerCase(), fromLatin1(value)]);
      f.#immutable = true;
      return f;
    }
    freeze(): this {
      this.#immutable = true;
      return this;
    }
    get(name: string): Uint8Array[] {
      const n = name.toLowerCase();
      return this.#entries.filter(([k]) => k === n).map(([, v]) => v.slice());
    }
    has(name: string): boolean {
      const n = name.toLowerCase();
      return this.#entries.some(([k]) => k === n);
    }
    set(name: string, values: Uint8Array[]): void {
      if (this.#immutable) throw IMMUTABLE;
      const n = checkName(name);
      const vs = values.map(checkValue);
      this.#entries = this.#entries.filter(([k]) => k !== n);
      for (const v of vs) this.#entries.push([n, v]);
    }
    delete(name: string): void {
      if (this.#immutable) throw IMMUTABLE;
      const n = checkName(name);
      this.#entries = this.#entries.filter(([k]) => k !== n);
    }
    append(name: string, value: Uint8Array): void {
      if (this.#immutable) throw IMMUTABLE;
      this.#entries.push([checkName(name), checkValue(value)]);
    }
    entries(): [string, Uint8Array][] {
      return this.#entries.map(([k, v]) => [k, v.slice()]);
    }
    clone(): Fields {
      const f = new Fields();
      f.#entries = this.entries();
      return f;
    }
    /** Host side: the entries as Latin-1 strings. */
    pairs(): [string, string][] {
      return this.#entries.map(([k, v]) => [k, latin1(v)]);
    }
  }

  class OutgoingBody {
    #chunks: Uint8Array[] = [];
    #stream = false;
    #done = false;
    #resolve!: (body: Uint8Array) => void;
    #reject!: (error: unknown) => void;
    readonly finished: Promise<Uint8Array>;
    constructor() {
      this.finished = new Promise((resolve, reject) => {
        this.#resolve = resolve;
        this.#reject = reject;
      });
      this.finished.catch(() => undefined);
    }
    write(): OutputStreamLike {
      if (this.#stream) throw undefined;
      this.#stream = true;
      return new OutputStream((bytes) => {
        if (this.#done) throw CLOSED;
        this.#chunks.push(bytes);
      });
    }
    static finish(body: OutgoingBody, _trailers: Fields | undefined): void {
      if (body.#done) return;
      body.#done = true;
      const total = body.#chunks.reduce((n, c) => n + c.length, 0);
      const out = new Uint8Array(total);
      let at = 0;
      for (const c of body.#chunks) {
        out.set(c, at);
        at += c.length;
      }
      body.#resolve(out);
    }
    /** A body dropped without `finish` is aborted. */
    [Symbol.dispose](): void {
      if (this.#done) return;
      this.#done = true;
      this.#reject(new HttpError({ tag: 'HTTP-request-body-size', val: undefined }, 'request body aborted'));
    }
  }

  class OutgoingRequest {
    #headers: Fields;
    #method: HttpErrorCode = { tag: 'get' };
    #path: string | undefined = undefined;
    #scheme: HttpErrorCode | undefined = undefined;
    #authority: string | undefined = undefined;
    #body: OutgoingBody | null = null;
    #bodyTaken = false;
    constructor(headers: Fields) {
      this.#headers = headers.freeze();
    }
    body(): OutgoingBody {
      if (this.#bodyTaken) throw undefined;
      this.#bodyTaken = true;
      this.#body = new OutgoingBody();
      return this.#body;
    }
    method(): HttpErrorCode {
      return this.#method;
    }
    setMethod(m: HttpErrorCode): void {
      if (m.tag === 'other' && !TOKEN.test(String(m.val))) throw undefined;
      this.#method = m;
    }
    pathWithQuery(): string | undefined {
      return this.#path;
    }
    setPathWithQuery(p: string | undefined): void {
      this.#path = p;
    }
    scheme(): HttpErrorCode | undefined {
      return this.#scheme;
    }
    setScheme(s: HttpErrorCode | undefined): void {
      this.#scheme = s;
    }
    authority(): string | undefined {
      return this.#authority;
    }
    setAuthority(a: string | undefined): void {
      this.#authority = a;
    }
    headers(): Fields {
      return this.#headers;
    }
    /** Host side: the request as the handler sees it. */
    toRequest(): HttpRequest {
      const path = this.#path ?? '/';
      return {
        method: methodName(this.#method),
        url: `${schemeName(this.#scheme)}://${this.#authority ?? ''}${path.startsWith('/') ? path : `/${path}`}`,
        headers: this.#headers.pairs(),
        body: this.#body === null ? Promise.resolve(undefined) : this.#body.finished,
      };
    }
  }

  class RequestOptions {
    #connect: bigint | undefined;
    #firstByte: bigint | undefined;
    #between: bigint | undefined;
    connectTimeout() {
      return this.#connect;
    }
    setConnectTimeout(d: bigint | undefined) {
      this.#connect = d;
    }
    firstByteTimeout() {
      return this.#firstByte;
    }
    setFirstByteTimeout(d: bigint | undefined) {
      this.#firstByte = d;
    }
    betweenBytesTimeout() {
      return this.#between;
    }
    setBetweenBytesTimeout(d: bigint | undefined) {
      this.#between = d;
    }
  }

  class IncomingBody {
    #bytes: Uint8Array;
    #streamed = false;
    constructor(bytes: Uint8Array) {
      this.#bytes = bytes;
    }
    stream(): InputStreamLike {
      if (this.#streamed) throw undefined;
      this.#streamed = true;
      return new InputStream(this.#bytes.length > 0 ? bytesSource(this.#bytes) : EMPTY);
    }
    static finish(_body: IncomingBody): FutureTrailers {
      return new FutureTrailers();
    }
  }

  class FutureTrailers {
    #taken = false;
    subscribe() {
      return Pollable.always();
    }
    get() {
      if (this.#taken) return { tag: 'err', val: undefined };
      this.#taken = true;
      return { tag: 'ok', val: { tag: 'ok', val: undefined } };
    }
  }

  class IncomingResponse {
    #status: number;
    #headers: Fields;
    #body: Uint8Array;
    #consumed = false;
    constructor(response: HttpResponse) {
      this.#status = response.status;
      this.#headers = Fields.frozen(response.headers);
      this.#body = response.body ?? new Uint8Array(0);
    }
    status(): number {
      return this.#status;
    }
    headers(): Fields {
      return this.#headers;
    }
    consume(): IncomingBody {
      if (this.#consumed) throw undefined;
      this.#consumed = true;
      return new IncomingBody(this.#body);
    }
  }

  type Outcome = { ok: true; response: IncomingResponse } | { ok: false; code: HttpErrorCode };

  class FutureIncomingResponse {
    #outcome: Outcome | null = null;
    #taken = false;
    readonly #settled: Promise<void>;
    constructor(pending: Promise<HttpResponse>) {
      this.#settled = pending.then(
        (response) => {
          this.#outcome = { ok: true, response: new IncomingResponse(response) };
        },
        (error: unknown) => {
          this.#outcome = { ok: false, code: errorCode(error) };
        },
      );
    }
    subscribe() {
      return Pollable.when(
        () => this.#outcome !== null,
        () => this.#settled,
      );
    }
    get() {
      if (this.#outcome === null) return undefined;
      if (this.#taken) return { tag: 'err', val: undefined };
      this.#taken = true;
      const o = this.#outcome;
      return { tag: 'ok', val: o.ok ? { tag: 'ok', val: o.response } : { tag: 'err', val: o.code } };
    }
  }

  // The server side is not served in the browser.
  const unsupported = (what: string) => {
    throw new Error(`wasi:http ${what} is not supported by codelib-web (no incoming requests in the browser)`);
  };
  class IncomingRequest {
    constructor() {
      unsupported('incoming-request');
    }
  }
  class OutgoingResponse {
    constructor() {
      unsupported('outgoing-response');
    }
  }
  class ResponseOutparam {
    constructor() {
      unsupported('response-outparam');
    }
    static set() {
      unsupported('response-outparam.set');
    }
  }

  function errorCode(error: unknown): HttpErrorCode {
    if (error instanceof HttpError) return error.payload;
    const message = error instanceof Error ? error.message : String(error);
    return { tag: 'internal-error', val: message };
  }

  function handle(request: OutgoingRequest, _options: RequestOptions | undefined): FutureIncomingResponse {
    // A synchronous throw (a policy refusal) fails `handle` itself: jco turns the thrown
    // `HttpError`'s payload into the `error-code` arm.
    const answer = handler(request.toRequest());
    return new FutureIncomingResponse(external(Promise.resolve(answer)));
  }

  return {
    imports: {
      'wasi:http/types': {
        Fields,
        IncomingRequest,
        OutgoingRequest,
        RequestOptions,
        OutgoingBody,
        IncomingBody,
        FutureTrailers,
        IncomingResponse,
        OutgoingResponse,
        ResponseOutparam,
        FutureIncomingResponse,
        httpErrorCode: (err: IoErrorLike) => err.httpCode as HttpErrorCode | undefined,
      },
      'wasi:http/outgoing-handler': { handle },
    },
  };
}
