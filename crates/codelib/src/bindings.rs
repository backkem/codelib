//! Typed bindings for the `codelib:runtime/script` world (wit/codelib.wit).

use codelib_sandbox::{wasmtime, wasmtime_wasi, wasmtime_wasi_http};

// Re-exported under short names so the `with:` paths below resolve from
// inside the macro.
pub use wasmtime_wasi::p2::bindings::io as io_bindings;
pub use wasmtime_wasi_http::p2::bindings::http as http_bindings;

// The `with:` mappings are required, not cosmetic: the world exports
// `wasi:http/incoming-handler`, and its generated accessor must use the same
// resource types the wasi-http linker registers, or `call_handle` could not be
// handed the request and response-outparam resources the host creates.
wasmtime::component::bindgen!({
    path: "../../wit",
    wasmtime_crate: codelib_sandbox::wasmtime,
    world: "codelib:runtime/script",
    // `trappable`: a panic in a host call handler or emit sink becomes a
    // trap that ends the run, instead of unwinding through the embedder.
    imports: {
        "codelib:runtime/host": async | trappable,
        default: async,
    },
    exports: { default: async },
    with: {
        "wasi:http": http_bindings,
        "wasi:io": io_bindings,
    },
});

pub(crate) use self::codelib::runtime::host;
pub(crate) use self::codelib::runtime::types;
