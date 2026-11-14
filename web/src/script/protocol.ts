// Messages between runScript (page) and the script Worker. Internal to codelib-web/script.

import type { Rule, VirtualRequest } from '../wasi/egress.js';
import type { HttpResponse } from '../wasi/http.js';

/** A named piece of JavaScript (codelib:runtime/types#source). */
export interface Source {
  name: string;
  code: string;
}

/** codelib:runtime/host#failure. */
export interface Failure {
  code: string;
  message: string;
  hint?: string;
  /** JSON text. */
  detail?: string;
}

export interface ScriptInit {
  /** Absolute URL of the transpiled guest's JS module. */
  module: string;
  /** Precompiled core modules, by file name. */
  modules?: Record<string, WebAssembly.Module>;
  preludes: Source[];
  program: Source;
  /** JSON text. */
  input: string;
  /** Present: a virtual world with this seed (decimal). Absent: the real world. */
  seed?: string;
  allow: Rule[];
  virtualHosts: string[];
}

export type ScriptRequest =
  | { kind: 'call'; method: string; args: string }
  | { kind: 'vhost'; request: VirtualRequest };

export type CallReply = { ok: string } | { err: Failure };
export type VhostReply = HttpResponse;

export type ScriptMessage =
  | { kind: 'console'; stream: 'stdout' | 'stderr'; text: string }
  | { kind: 'emit'; emitKind: string; item: string };

export interface ScriptResult {
  /** The program's value, as JSON text. */
  value: string;
  instantiateMs: number;
  runMs: number;
}
