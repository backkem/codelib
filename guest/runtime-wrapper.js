// The codelib guest: the JavaScript that is componentized (together with
// StarlingMonkey) into `codelib-guest.wasm`, implementing the
// `codelib:runtime/script` world from ../wit/codelib.wit.
//
// It evaluates programs that arrive as text. This module itself may use ES
// imports because it is componentized at build time; the programs it runs may
// not.
import { call as hostCall, emit as hostEmit } from "codelib:runtime/host@0.1.0";
import {
    Fields,
    IncomingBody,
    OutgoingBody,
    OutgoingResponse,
    ResponseOutparam,
} from "wasi:http/types@0.2.12";

// Intrinsics captured before any program runs: a program may replace the
// globals, and error reporting and evaluation must not depend on what it
// left behind. `eval` called through this binding is an indirect eval: it
// runs in the global scope.
const indirectEval = eval;
const NativeError = Error;
const NativeSyntaxError = SyntaxError;
const defineProperty = Object.defineProperty;
const NativeString = String;
const jsonStringify = JSON.stringify;

// `uncurry(f)(thisArg, ...args)` is `f.call(thisArg, ...args)`, without
// looking `call` up at the time of use.
const uncurry = Function.prototype.bind.bind(Function.prototype.call);
const strIndexOf = uncurry(String.prototype.indexOf);
const strSlice = uncurry(String.prototype.slice);
const strCharCodeAt = uncurry(String.prototype.charCodeAt);
const strLastIndexOf = uncurry(String.prototype.lastIndexOf);

// Chunk size for reading bodies in. Reads of this size are fine for the host.
const CHUNK = 8192;

// Writes go through blocking-write-and-flush, which hosts cap at 4096 bytes
// per call (larger buffers trap the instance), whatever the stream's
// check-write budget says.
const WRITE_CHUNK = 4096;

// ---------------------------------------------------------------------------
// The `host` global
// ---------------------------------------------------------------------------

/// Thrown by `host.call` when the host answers with a failure.
class HostError extends Error {
    constructor(failure) {
        super(failure.message);
        this.name = "HostError";
        this.code = failure.code;
        this.hint = failure.hint ?? null;
        let detail = null;
        if (failure.detail !== undefined && failure.detail !== null) {
            try {
                detail = JSON.parse(failure.detail);
            } catch {
                detail = failure.detail;
            }
        }
        this.detail = detail;
    }
}

// Exactly one request handler per instance.
let handler = null;
let registrations = 0;

function registerHandler(fn) {
    if (typeof fn !== "function") {
        throw new TypeError("host.serve: the handler must be a function");
    }
    registrations += 1;
    if (registrations > 1) {
        // Silently keeping the last registration would discard a handler the
        // program believes is live.
        throw new Error("a request handler is already registered; only one is supported");
    }
    handler = fn;
}

let input = null;

// The host import, with a failure turned into a `HostError`.
function invokeHost(method, argsText) {
    try {
        return hostCall(method, argsText);
    } catch (error) {
        const failure = error && error.payload;
        if (failure && typeof failure.code === "string") {
            throw new HostError(failure);
        }
        throw error;
    }
}

const host = Object.freeze({
    /// Call a host method with JSON-serializable arguments; returns the parsed
    /// result. Synchronous: `await` is harmless but not needed.
    call(method, args) {
        const out = invokeHost(String(method), args === undefined ? "null" : JSON.stringify(args));
        return out === "" ? undefined : JSON.parse(out);
    },

    /// Like `call`, but `text` goes to the host as it is and the host's result
    /// text comes back as it is: no JSON on either side.
    callRaw(method, text) {
        if (typeof text !== "string") {
            throw new TypeError("host.callRaw: the argument must be a string");
        }
        return invokeHost(String(method), text);
    },

    /// Send a progress item to the embedder immediately.
    emit(kind, item) {
        hostEmit(String(kind), JSON.stringify(item === undefined ? null : item));
    },

    /// Register the request handler for a `serve` run:
    /// `host.serve(async (request) => new Response("hi"))`.
    serve(fnOrOptions) {
        const fn = typeof fnOrOptions === "function" ? fnOrOptions : fnOrOptions?.fetch;
        registerHandler(fn);
    },

    /// The JSON input the embedder passed to this run.
    get input() {
        return input;
    },

    HostError,
});

let installed = false;

function install() {
    if (installed) return;
    installed = true;

    Object.defineProperty(globalThis, "host", {
        value: host,
        writable: false,
        configurable: false,
        enumerable: true,
    });

    // `addEventListener("fetch", ...)` compatibility. Deliberately a shim over
    // our own dispatch rather than the engine's fetch event: there is no
    // singleton event object, so one instance can answer many requests.
    const nativeAddEventListener = globalThis.addEventListener;
    globalThis.addEventListener = function (type, listener, ...rest) {
        if (type !== "fetch") {
            if (typeof nativeAddEventListener !== "function") {
                throw new Error(`addEventListener("${type}") is not available here`);
            }
            return nativeAddEventListener.call(globalThis, type, listener, ...rest);
        }
        if (typeof listener !== "function") {
            throw new TypeError('addEventListener("fetch") requires a function');
        }
        registerHandler(async (request) => {
            let responded = false;
            let promise = null;
            const event = {
                type: "fetch",
                request,
                respondWith(response) {
                    if (responded) {
                        throw new Error("respondWith can only be called once per request");
                    }
                    responded = true;
                    promise = Promise.resolve(response);
                },
                waitUntil() {
                    // Accepting and ignoring it would silently drop background
                    // work the program expects to complete.
                    throw new Error(
                        "waitUntil is not supported: nothing runs between requests. " +
                            "Await the work inside the handler instead."
                    );
                },
            };
            listener(event);
            if (!responded) {
                throw new Error("the fetch listener returned without calling respondWith");
            }
            return await promise;
        });
    };
}

// ---------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------

// The name ends up in a `//# sourceURL=` comment after the code, so it must
// not contain anything JavaScript treats as a line end (or other control
// characters): the rest of the name would be evaluated as code.
function sourceName(name) {
    const clean = String(name ?? "")
        .replace(/[\u0000-\u001f\u007f\u2028\u2029]/g, " ")
        .trim();
    return clean === "" ? "script.js" : clean;
}

// Each source is its own async arrow function body: top-level `await` and
// `return` work, `this` is `globalThis`, and one prelude's `const` cannot
// collide with another's. Sources share state through `globalThis` only.
//
// The body starts on the same line as the prefix, so line numbers in stack
// traces are the program's own; only columns on line 1 are shifted, by the
// prefix length (see `unshiftColumn`).
const EVAL_PREFIX = "(async () => {";

async function evaluate(source) {
    // The function is called from here rather than from the evaluated text,
    // so the call is a frame of this file (hidden from stacks) and not a
    // phantom frame on a line after the program's last.
    const name = sourceName(source.name);
    let body;
    try {
        body = indirectEval(`${EVAL_PREFIX}${source.code}\n})\n//# sourceURL=${name}`);
    } catch (error) {
        throw nameSyntaxError(error, name);
    }
    return await body();
}

// The engine does not apply `sourceURL` to code that fails to parse, so a
// syntax error carries no stack and points at this file. Give it a one-frame
// stack in the source's own name (with the column as the engine counted it:
// `cleanStack` takes the prefix off). Never throws.
function nameSyntaxError(error, name) {
    try {
        if (!(error instanceof NativeSyntaxError)) return error;
        const line = safeGet(error, "lineNumber");
        const column = safeGet(error, "columnNumber");
        if (typeof line !== "number" || typeof column !== "number") return error;
        const named = new NativeSyntaxError(safeString(safeGet(error, "message"), ""));
        defineProperty(named, "stack", {
            value: `@${name}:${NativeString(line)}:${NativeString(column)}`,
            writable: true,
            configurable: true,
        });
        return named;
    } catch {
        return error;
    }
}

function toJson(value) {
    const text = JSON.stringify(value);
    return text === undefined ? "null" : text;
}

function parseInput(text) {
    if (text === undefined || text === null || text === "") return null;
    return JSON.parse(text);
}

const UNPRINTABLE = "a value that cannot be converted to text was thrown";

// `String(value)`, or `fallback` if that throws or yields something else.
function safeString(value, fallback) {
    try {
        if (typeof value === "string") return value;
        const text = NativeString(value);
        return typeof text === "string" ? text : fallback;
    } catch {
        return fallback;
    }
}

// Read a property without letting a getter or proxy trap throw.
function safeGet(object, key) {
    try {
        return object[key];
    } catch {
        return undefined;
    }
}

function safeInstanceOf(value, type) {
    try {
        return value instanceof type;
    } catch {
        return false;
    }
}

// The number in `text[from, to)` if it is all ASCII digits, else -1.
function parseDigits(text, from, to) {
    if (to <= from || to - from > 9) return -1;
    let n = 0;
    for (let i = from; i < to; i++) {
        const d = strCharCodeAt(text, i) - 48;
        if (d < 0 || d > 9) return -1;
        n = n * 10 + d;
    }
    return n;
}

// One stack line (`function@source:line:column`) with the wrapper prefix
// taken off the column if the frame is on line 1 of a source. A line that
// does not look like that is returned as it is. Never throws.
function unshiftColumn(line) {
    const colon = strLastIndexOf(line, ":");
    if (colon < 0) return line;
    const column = parseDigits(line, colon + 1, line.length);
    const lineColon = strLastIndexOf(line, ":", colon - 1);
    if (column <= EVAL_PREFIX.length || lineColon < 0) return line;
    if (parseDigits(line, lineColon + 1, colon) !== 1) return line;
    return strSlice(line, 0, colon + 1) + NativeString(column - EVAL_PREFIX.length);
}

// The stack text a script sees: frames of this file and of the engine's
// glue are dropped (they are not the program's), and columns on line 1 are
// the program's own. Never throws; yields `stack` unchanged if it cannot
// make sense of it.
function cleanStack(stack) {
    try {
        let out = "";
        let start = 0;
        while (start <= stack.length) {
            let end = strIndexOf(stack, "\n", start);
            if (end < 0) end = stack.length;
            const line = strSlice(stack, start, end);
            start = end + 1;
            if (strIndexOf(line, "runtime-wrapper.js") >= 0) continue;
            if (strIndexOf(line, "initializer.js") >= 0) continue;
            out += (out === "" ? "" : "\n") + unshiftColumn(line);
        }
        return out;
    } catch {
        return stack;
    }
}

// The `script-error` record. A plain object on purpose: the component glue
// lowers a thrown value into the error arm of the result as-is. The thrown
// value is the program's, so nothing here may throw: a throw would escape as
// something that is not a `script-error` and trap the instance.
function asScriptError(error) {
    try {
        if (safeInstanceOf(error, NativeError)) {
            const name = safeString(safeGet(error, "name") || "Error", "Error");
            let message = safeString(safeGet(error, "message"), "");
            if (safeInstanceOf(error, HostError)) {
                message = `${safeString(safeGet(error, "code"), "")}: ${message}`;
            }
            let stack = safeGet(error, "stack");
            if (typeof stack === "string" && stack !== "") {
                stack = cleanStack(stack);
            }
            return {
                name,
                message,
                stack: typeof stack === "string" && stack !== "" ? stack : undefined,
            };
        }
        let message;
        if (typeof error === "string") {
            message = error;
        } else {
            try {
                message = jsonStringify(error);
            } catch {
                message = undefined;
            }
            if (typeof message !== "string") message = safeString(error, UNPRINTABLE);
        }
        return { name: "Error", message, stack: undefined };
    } catch {
        return { name: "Error", message: UNPRINTABLE, stack: undefined };
    }
}

async function evaluateAll(preludes, program, inputText) {
    install();
    input = parseInput(inputText);
    for (const prelude of preludes) {
        await evaluate(prelude);
    }
    return await evaluate(program);
}

// WIT export: run
export async function run(preludes, program, inputText) {
    try {
        const value = await evaluateAll(preludes, program, inputText);
        // `run` has no dispatch path; accepting a registration would leave the
        // program believing it will serve.
        if (registrations > 0) {
            throw new Error(
                "this program registered a request handler, but `run` never serves; " +
                    "use the serve entry point instead"
            );
        }
        return toJson(value);
    } catch (error) {
        throw asScriptError(error);
    }
}

// WIT export: serve
export async function serve(preludes, program, inputText) {
    try {
        const value = await evaluateAll(preludes, program, inputText);
        if (registrations === 0) {
            throw new Error(
                "serve: the program registered no request handler; call " +
                    'host.serve(fn) or addEventListener("fetch", ...)'
            );
        }
        return toJson(value);
    } catch (error) {
        throw asScriptError(error);
    }
}

// ---------------------------------------------------------------------------
// wasi:http/incoming-handler: our own dispatch
// ---------------------------------------------------------------------------

// wasi:http scheme is a variant; `other` carries the name.
function schemeToString(scheme) {
    const tag = typeof scheme === "string" ? scheme : scheme && scheme.tag;
    if (tag === "other" && typeof scheme.val === "string" && scheme.val !== "") {
        return scheme.val.toLowerCase();
    }
    return typeof tag === "string" && tag.toUpperCase() === "HTTPS" ? "https" : "http";
}

function methodToString(method) {
    // wasi:http method is a variant; `other` carries the verb.
    if (typeof method === "string") return method.toUpperCase();
    if (method && typeof method === "object") {
        if (method.tag === "other" && typeof method.val === "string") {
            return method.val.toUpperCase();
        }
        if (typeof method.tag === "string") return method.tag.toUpperCase();
    }
    return "GET";
}

// Close an incoming body: the child input-stream must be dropped before the
// parent body is finished, or WASI traps rather than throwing.
function closeIncoming(stream, body) {
    dispose(stream);
    if (body) {
        try { IncomingBody.finish(body); } catch { /* already finished */ }
    }
}

// Drop a resource handle now rather than whenever the engine collects it.
function dispose(resource) {
    if (!resource) return;
    try { resource[Symbol.dispose]?.(); } catch { /* already gone */ }
}

// Discard a request body without reading it: dropping the body resource
// releases the host side, so a body that is large, slow or never ends costs
// the handler nothing.
function discardBody(incoming) {
    let body;
    try {
        body = incoming.consume();
    } catch {
        return; // No body, or already consumed.
    }
    dispose(body);
}

// Build a JS Request from the WASI incoming-request, streaming the body rather
// than buffering it.
function toRequest(incoming) {
    const method = methodToString(incoming.method());
    const pathWithQuery = incoming.pathWithQuery() ?? "/";
    const authority = incoming.authority() ?? "localhost";

    const headers = new Headers();
    for (const [name, value] of incoming.headers().entries()) {
        try {
            headers.append(name, new TextDecoder().decode(value));
        } catch {
            // Skip headers that are not valid for the Fetch API rather than
            // failing the whole request.
        }
    }

    const url = `${schemeToString(incoming.scheme())}://${authority}${pathWithQuery}`;

    // GET/HEAD bodies are not passed on; discard rather than read them.
    if (method === "GET" || method === "HEAD") {
        discardBody(incoming);
        return new Request(url, { method, headers });
    }

    const body = incoming.consume();
    const stream = body.stream();
    let closed = false;
    const readable = new ReadableStream({
        pull(controller) {
            try {
                const chunk = stream.blockingRead(BigInt(CHUNK));
                if (chunk.length === 0) return;
                controller.enqueue(chunk);
            } catch {
                // blockingRead throws at end-of-stream; that is the normal exit.
                if (!closed) { closed = true; closeIncoming(stream, body); }
                controller.close();
            }
        },
        cancel() {
            // A handler that stopped reading.
            if (!closed) { closed = true; closeIncoming(stream, body); }
        },
    });

    try {
        return new Request(url, { method, headers, body: readable, duplex: "half" });
    } catch (error) {
        closed = true;
        closeIncoming(stream, body);
        throw error;
    }
}

function toFields(headers) {
    const fields = new Fields();
    const encoder = new TextEncoder();
    for (const [name, value] of headers.entries()) {
        try {
            fields.set(name, [encoder.encode(value)]);
        } catch {
            // Forbidden/immutable header names are dropped rather than fatal.
        }
    }
    return fields;
}

// Write a JS Response out through the response-outparam, streaming the body.
async function writeResponse(responseOut, response, mark) {
    const outgoing = new OutgoingResponse(toFields(response.headers));
    outgoing.setStatusCode(response.status);
    const outBody = outgoing.body();

    // Set the outparam before writing: that is what releases the status and
    // headers to the client, so a streamed body starts flowing immediately.
    mark();
    ResponseOutparam.set(responseOut, { tag: "ok", val: outgoing });

    const stream = outBody.write();
    let complete = false;
    try {
        if (response.body) {
            const reader = response.body.getReader();
            for (;;) {
                const { done, value } = await reader.read();
                if (done) break;
                if (!value || value.length === 0) continue;
                // Bounded slices: blocking-write-and-flush traps on buffers above
                // WRITE_CHUNK.
                for (let off = 0; off < value.length; off += WRITE_CHUNK) {
                    stream.blockingWriteAndFlush(value.subarray(off, off + WRITE_CHUNK));
                }
            }
        }
        complete = true;
    } finally {
        // The child stream goes first: the body cannot be finished or
        // dropped while it is alive.
        dispose(stream);
        if (complete) {
            try { OutgoingBody.finish(outBody, undefined); } catch { /* already finished */ }
        } else {
            // Dropping the body without finishing it aborts it: the host sees
            // the body end with an error, not a short but complete response.
            dispose(outBody);
        }
    }
}

// Exporting a symbol with this exact name replaces the engine's built-in
// fetch-event dispatch with ours.
export const incomingHandler = {
    async handle(incoming, responseOut) {
        let outparamSet = false;
        const mark = () => { outparamSet = true; };
        try {
            const request = toRequest(incoming);
            if (!handler) {
                throw new Error("no request handler registered");
            }
            const response = await handler(request);
            if (!(response instanceof Response)) {
                throw new TypeError("the request handler must return a Response");
            }
            await writeResponse(responseOut, response, mark);
        } catch (error) {
            // Report handler failures as a 500 instead of trapping the
            // instance: one bad request must not end the whole session.
            const failure = asScriptError(error);
            const message = `${failure.name}: ${failure.message}`;
            try {
                console.error(`serve: handler failed: ${message}`);
            } catch {
                // The program replaced `console`; the 500 still goes out.
            }
            if (outparamSet) {
                // Headers are already on the wire; the client sees a truncated
                // body. Setting the outparam again would trap the instance.
                return;
            }
            try {
                const fields = new Fields();
                fields.set("content-type", [new TextEncoder().encode("text/plain")]);
                const outgoing = new OutgoingResponse(fields);
                outgoing.setStatusCode(500);
                const outBody = outgoing.body();
                ResponseOutparam.set(responseOut, { tag: "ok", val: outgoing });
                const stream = outBody.write();
                stream.blockingWriteAndFlush(new TextEncoder().encode(message));
                stream[Symbol.dispose]?.();
                OutgoingBody.finish(outBody, undefined);
            } catch {
                // The outparam was already set, or the client is gone.
            }
        }
    },
};
