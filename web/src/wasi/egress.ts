// A small egress policy: an `HttpHandler` (http.ts) from an allowlist and virtual hosts.
//
// The model is codelib-http's (the native egress engine) in miniature:
//
// - deny by default: a request goes out only if an allow rule matches its host (exact, or
//   `*.suffix` for subdomains of `suffix` but not `suffix` itself; case-insensitive). The first
//   matching rule decides. Rules require https unless `httpsOnly: false`.
// - a rule may be read-only (GET and HEAD only) and may attach headers. Attached headers
//   replace whatever the component set under the same names, so a component can neither
//   pre-set nor shadow a credential, and never sees its value.
// - virtual hosts are answered by a function, never by the network. They are matched first, by
//   exact host, and are reachable over http or https without a rule (registering one is the
//   grant). A rule for the same host still applies its read-only limit and headers, so a page
//   can inject a credential into its own virtual host.
// - URLs with user info, and TRACE, TRACK and CONNECT, are always refused.
//
// A refusal fails the request with `HTTP-request-denied` (a JavaScript guest's `fetch` rejects)
// and logs `[codelib] fetch denied: METHOD scheme://host[:port]/path (reason)`, without the
// query string, as the native kit writes to the captured console.
//
// What this does not do, unlike the native kit: no private-IP blocking, no path scopes, no
// redirect re-checks. Redirects are not followed at all (`redirect: 'error'`): following one
// would carry an attached header to a URL the policy never saw. The browser's own rules (CORS,
// forbidden headers) still apply on top. Bodies are buffered.

import { HttpError, type HttpHandler, type HttpResponse } from './http.js';

export interface Rule {
  /** `api.example.com`, or `*.example.com` for its subdomains. */
  host: string;
  /** Only GET and HEAD. */
  readOnly?: boolean;
  /** Attached to every request this rule allows, replacing the component's own. */
  headers?: Record<string, string>;
  /** Require https (default true). */
  httpsOnly?: boolean;
}

/** A request as a virtual host sees it: everything resolved, body included. */
export interface VirtualRequest {
  method: string;
  url: string;
  headers: [string, string][];
  body?: Uint8Array;
}

export type VirtualHost = (request: VirtualRequest) => HttpResponse | Promise<HttpResponse>;

export interface EgressPolicy {
  allow?: Rule[];
  virtualHosts?: Record<string, VirtualHost>;
}

export interface EgressOptions {
  /** Where denial notes go (a line without the newline). Default: `console.warn`. */
  log?: (line: string) => void;
  /** The fetch used for allowed real requests. Default: `globalThis.fetch`. */
  fetch?: typeof fetch;
}

const REFUSED_METHODS = new Set(['TRACE', 'TRACK', 'CONNECT']);
const READ_ONLY = new Set(['GET', 'HEAD']);

/** Whether `pattern` (a rule's host) matches `host` (lowercase). */
export function hostMatches(pattern: string, host: string): boolean {
  const p = pattern.toLowerCase().replace(/\.$/, '');
  const h = host.toLowerCase().replace(/\.$/, '');
  if (p.startsWith('*.')) {
    const suffix = p.slice(1); // ".example.com"
    return h.endsWith(suffix) && h.length > suffix.length;
  }
  return p === h;
}

/** How a request is named in notes: no query, no user info. */
function describe(method: string, url: URL | null, raw: string): string {
  if (url === null) return `${method} ${raw.split('?')[0]}`;
  return `${method} ${url.protocol}//${url.host}${url.pathname}`;
}

export function egress(policy: EgressPolicy, options: EgressOptions = {}): HttpHandler {
  const rules = policy.allow ?? [];
  const virtual = new Map(Object.entries(policy.virtualHosts ?? {}).map(([h, f]) => [h.toLowerCase(), f]));
  const log = options.log ?? ((line: string) => console.warn(line));
  const doFetch = options.fetch ?? ((input: RequestInfo | URL, init?: RequestInit) => globalThis.fetch(input, init));

  return (request) => {
    const method = request.method.toUpperCase();
    let url: URL | null = null;
    try {
      url = new URL(request.url);
    } catch {
      // described below
    }
    const deny = (reason: string): never => {
      log(`[codelib] fetch denied: ${describe(method, url, request.url)} (${reason})`);
      throw new HttpError({ tag: 'HTTP-request-denied' }, reason);
    };
    if (url === null) return deny('invalid URL');
    if (url.username !== '' || url.password !== '') return deny('user info in the URL');
    if (REFUSED_METHODS.has(method)) return deny(`${method} is never allowed`);
    if (url.protocol !== 'https:' && url.protocol !== 'http:') return deny(`scheme ${url.protocol} is not allowed`);

    const host = url.hostname.toLowerCase();
    const rule = rules.find((r) => hostMatches(r.host, host));
    const vhost = virtual.get(host);
    if (!vhost) {
      if (!rule) return deny('no rule allows this host');
      if ((rule.httpsOnly ?? true) && url.protocol !== 'https:') return deny('only https is allowed');
    }
    if (rule?.readOnly && !READ_ONLY.has(method)) return deny(`the rule for ${rule.host} is read-only`);

    // The component's headers, with the rule's replacing any of the same name.
    const attached = Object.entries(rule?.headers ?? {}).map(([k, v]) => [k.toLowerCase(), v] as [string, string]);
    const names = new Set(attached.map(([k]) => k));
    const headers: [string, string][] = [...request.headers.filter(([k]) => !names.has(k.toLowerCase())), ...attached];

    return (async (): Promise<HttpResponse> => {
      const body = await request.body;
      if (vhost) {
        return await vhost({ method, url: url.href, headers, ...(body !== undefined ? { body } : {}) });
      }
      let response: Response;
      try {
        response = await doFetch(url.href, {
          method,
          headers,
          ...(body !== undefined && body.length > 0 && !READ_ONLY.has(method) ? { body: body as BodyInit } : {}),
          redirect: 'error',
          credentials: 'omit',
          cache: 'no-store',
        });
      } catch (error) {
        const why = error instanceof Error ? error.message : String(error);
        log(`[codelib] fetch failed: ${describe(method, url, request.url)} (${why}; CORS, network, or a redirect, which is not followed)`);
        throw new HttpError({ tag: 'destination-unavailable' }, why);
      }
      const bytes = new Uint8Array(await response.arrayBuffer());
      return { status: response.status, headers: [...response.headers], body: bytes };
    })();
  };
}
