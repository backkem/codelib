// Build-time fix-ups for the JavaScript glue that componentize-js generates
// for the guest's component imports. Loaded with `node --import` before
// `jco componentize` (see package.json), it wraps the splicer that produces
// the glue so the guest is built from the patched text.
//
// Why: in componentize-js 0.23.0 every import call lowers its arguments
// (strings, lists) with the engine's `$realloc`, which records each
// allocation in a list that is only released when the *export* call returns
// (`post_call`). The glue never frees import arguments, so the list grows by
// one entry per argument for the whole run, and every `$dealloc` the glue
// does afterwards (result strings, return areas) scans it linearly
// (`std::remove` over a `std::vector`). Per-call cost therefore grows with
// the number of earlier calls, and the argument bytes stay allocated until
// the run ends.
//
// Fix: in each import glue function, allocations made while lowering the
// arguments are tracked locally and freed with `$dealloc` right after the
// core import returns. The canonical ABI lets the caller do that: a
// synchronous import has copied its arguments by the time it returns.
// `$dealloc` also removes the pointer from the engine's list, so the list
// stays short and post_call has nothing left to free twice.
//
// Second, smaller fix: `_utf8AllocateAndEncode` sizes strings with a
// per-character JavaScript loop (about 0.1 us per character in the
// interpreter); a native `TextEncoder.encode` does the same job.
//
// The patch is tied to one componentize-js version and fails the build on
// any other, or if the glue no longer has the shape it expects: an upgrade
// must re-check whether upstream still leaks import arguments (if it frees
// them itself, this patch would free them twice).

import { readFileSync } from "node:fs";
import { Parser } from "acorn";
import { ancestor } from "acorn-walk";
import { splicer } from "./node_modules/@bytecodealliance/componentize-js/lib/spidermonkey-embedding-splicer.js";

const PATCHED_VERSION = "0.23.0";
const { version } = JSON.parse(
    readFileSync(new URL("./node_modules/@bytecodealliance/componentize-js/package.json", import.meta.url), "utf8"),
);
if (version !== PATCHED_VERSION) {
    throw new Error(
        `patch-bindings: written for componentize-js ${PATCHED_VERSION}, found ${version}; ` +
            "check whether its import glue still leaks arguments, then update or drop this patch",
    );
}

// Appended to the glue. Each patched glue function gets its own list, so if
// lowering throws, its allocations are simply left to post_call, as before.
const HELPERS = `

// --- codelib: free import arguments after the call (see guest/patch-bindings.mjs) ---
function __codelibTrackLowering(list) {
  return function (ptr, oldLen, align, newLen) {
    const out = $realloc(ptr, oldLen, align, newLen);
    if (ptr !== 0) {
      const at = list.indexOf(ptr);
      if (at !== -1) list.splice(at, 1);
    }
    if (out !== 0) list.push(out);
    return out;
  };
}
function __codelibFreeLowered(list) {
  for (let i = list.length - 1; i >= 0; i--) $dealloc(list[i]);
  list.length = 0;
}
`;

const UTF8_ENCODE = `function _utf8AllocateAndEncode(s, realloc, memory) {
  if (typeof s !== 'string') {
    throw new TypeError('expected a string, received [' + typeof s + ']');
  }
  if (s.length === 0) { return { ptr: 1, len: 0 }; }
  // codelib: native encoding instead of a per-character loop; the
  // allocation is still exact, as older adapters require.
  const bytes = TEXT_ENCODER_UTF8.encode(s);
  const len = bytes.length;
  const ptr = realloc(0, 0, 1, len);
  new Uint8Array(memory.buffer, ptr, len).set(bytes);
  return { ptr, len };
}`;

const isFunction = (node) =>
    node.type === "FunctionDeclaration" ||
    node.type === "FunctionExpression" ||
    node.type === "ArrowFunctionExpression";

/// Returns the patched glue and the names of the imports whose argument
/// allocations are now freed after the call.
export function patchBindings(source) {
    const ast = Parser.parse(source, { ecmaVersion: "latest", sourceType: "script" });

    // Per function: its core import calls and its `$realloc` references.
    const fns = new Map();
    const entry = (fn) => {
        if (!fns.has(fn)) fns.set(fn, { calls: [], reallocs: [] });
        return fns.get(fn);
    };
    let encoder = null;

    ancestor(ast, {
        CallExpression(node, _state, ancestors) {
            if (node.callee.type !== "Identifier" || !node.callee.name.startsWith("$import_")) return;
            const fn = ancestors.findLast(isFunction);
            const statement = ancestors.findLast((a) => a.type.endsWith("Statement"));
            if (!fn) throw new Error(`core import ${node.callee.name} called outside a function`);
            entry(fn).calls.push({ node, statement });
        },
        Identifier(node, _state, ancestors) {
            if (node.name !== "$realloc") return;
            const fn = ancestors.findLast(isFunction);
            if (fn) entry(fn).reallocs.push(node);
        },
        FunctionDeclaration(node) {
            if (node.id?.name === "_utf8AllocateAndEncode") encoder = node;
        },
    });

    const edits = []; // { at, remove, insert }
    const patched = [];
    for (const [fn, { calls, reallocs }] of fns) {
        if (calls.length === 0 || reallocs.length === 0) continue;
        if (calls.length !== 1) {
            throw new Error(`patch-bindings: a glue function makes ${calls.length} core import calls`);
        }
        const [{ node: call, statement }] = calls;
        const before = reallocs.filter((r) => r.start < call.start);
        if (before.length !== reallocs.length) {
            // Allocations after the call would be results the glue hands on.
            throw new Error(`patch-bindings: ${call.callee.name} glue allocates after the call`);
        }
        if (statement?.type !== "ExpressionStatement") {
            throw new Error(`patch-bindings: unexpected statement around ${call.callee.name}`);
        }
        if (fn.body.type !== "BlockStatement") {
            throw new Error(`patch-bindings: ${call.callee.name} glue has no block body`);
        }
        edits.push({
            at: fn.body.start + 1,
            remove: 0,
            insert: "\n  const __codelibLowered = [], __codelibAlloc = __codelibTrackLowering(__codelibLowered);",
        });
        for (const r of before) {
            edits.push({ at: r.start, remove: r.end - r.start, insert: "__codelibAlloc" });
        }
        edits.push({ at: statement.end, remove: 0, insert: "\n  __codelibFreeLowered(__codelibLowered);" });
        patched.push(call.callee.name);
    }

    for (const required of ["$import_codelib_runtime_host_0_1_0$call", "$import_codelib_runtime_host_0_1_0$emit"]) {
        if (!patched.includes(required)) {
            throw new Error(`patch-bindings: did not find the argument lowering of ${required}`);
        }
    }

    if (encoder) {
        edits.push({ at: encoder.start, remove: encoder.end - encoder.start, insert: UTF8_ENCODE });
    } else {
        throw new Error("patch-bindings: _utf8AllocateAndEncode not found");
    }

    // Apply back to front so earlier offsets stay valid.
    edits.sort((a, b) => b.at - a.at);
    let out = source;
    for (const { at, remove, insert } of edits) {
        out = out.slice(0, at) + insert + out.slice(at + remove);
    }
    out += HELPERS;

    // The result must still parse.
    Parser.parse(out, { ecmaVersion: "latest", sourceType: "script" });
    return { source: out, patched };
}

const splice = splicer.spliceBindings;
splicer.spliceBindings = function (...args) {
    const result = splice.apply(this, args);
    const { source, patched } = patchBindings(result.jsBindings);
    result.jsBindings = source;
    if (process.env.CODELIB_PATCH_VERBOSE) {
        console.error(`patch-bindings: import arguments freed after the call in ${patched.length} glue functions`);
    }
    return result;
};
