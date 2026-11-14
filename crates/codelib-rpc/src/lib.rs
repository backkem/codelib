//! A transport-agnostic method registry and dispatcher for host calls.
//!
//! Sandboxed scripts reach host capabilities through one generic call,
//! `host.call(method, args)`. A [`Router`] is what answers it: a table of typed handlers
//! (arguments and results go through serde), with guards and observers around every call, and
//! metadata from which it describes itself ([`Router::methods`], the built-in `methods` method)
//! and generates bindings ([`codegen`]: a JS client prelude, a Markdown reference, TypeScript
//! declarations).
//!
//! The crate does no I/O and needs no runtime: no threads, clocks, file system or executor.
//! The same router compiles to `wasm32-unknown-unknown` and can be served from a wasmtime host
//! import, a browser or a network transport; the embedder drives the futures.
//!
//! # Example
//!
//! ```
//! use codelib_rpc::codegen::{self, JsOptions};
//! use codelib_rpc::{Failure, Router};
//! use serde::Deserialize;
//!
//! #[derive(Default)]
//! struct Counter {
//!     value: i64,
//!     read_only: bool,
//! }
//!
//! #[derive(Deserialize)]
//! struct Add {
//!     by: i64,
//! }
//!
//! async fn reset(counter: &mut Counter, _: ()) -> Result<i64, Failure> {
//!     counter.value = 0;
//!     Ok(0)
//! }
//!
//! let mut router = Router::<Counter>::new();
//! router
//!     .method("add", |c: &mut Counter, a: Add| {
//!         c.value += a.by;
//!         Ok(c.value)
//!     })
//!     .summary("Adds to the counter; returns the new value.")
//!     .tag("write")
//!     .args("{by}")
//!     .returns("number");
//! router.method("get", |c: &mut Counter, _: ()| Ok(c.value)).summary("The current value.");
//! router.method_async("reset", reset).summary("Back to zero.").tag("write");
//! // Grants live in the state; a guard checks them against the method's tags.
//! router.guard(|method, state: &Counter| {
//!     if method.has_tag("write") && state.read_only {
//!         return Err(Failure::denied(format!("{} writes; this session is read-only", method.name)));
//!     }
//!     Ok(())
//! });
//!
//! let mut state = Counter::default();
//! assert_eq!(router.call_json_sync(&mut state, "add", r#"{"by": 2}"#).unwrap(), "2");
//! assert_eq!(router.call_json_sync(&mut state, "get", "").unwrap(), "2");
//! assert_eq!(pollster::block_on(router.call_json(&mut state, "reset", "null")).unwrap(), "0");
//!
//! state.read_only = true;
//! let err = router.call_json_sync(&mut state, "add", r#"{"by": 1}"#).unwrap_err();
//! assert_eq!(err.code, "denied");
//!
//! let err = router.call_json_sync(&mut state, "ad", "{}").unwrap_err();
//! assert_eq!(err.code, "unknown-method");
//! assert!(err.hint.unwrap().contains("`add`"));
//!
//! // A prelude for the sandbox: `counter.add({by: 1})` calls `host.call("add", {by: 1})`.
//! let js = codegen::js_client(router.methods(), &JsOptions::new("counter"));
//! assert!(js.contains(r#"add(args) { return host.call("add", args); },"#));
//! ```
//!
//! # Dispatch rules
//!
//! - **Unknown method**: [`codes::UNKNOWN_METHOD`], with near-miss names in the hint (only for
//!   names up to 64 bytes). The message echoes at most 128 bytes of the name.
//! - **Arguments** are decoded into the handler's argument type with the router's [`Codec`];
//!   a decode error is [`codes::BAD_ARGS`] carrying the serde message, cut at 512 bytes (and the documented
//!   argument shape as the hint, when set).
//! - **Empty-args rule**: when the arguments carry no value — empty input, the codec's null,
//!   or an empty map (for JSON: `""`, whitespace, `null`, `{}`) — and decoding them as-is
//!   fails, they are decoded as unit/`null` and then as an empty map. So a `()` handler accepts
//!   `""`, `null` and `{}`; an `Option<T>` handler gets `None`; a struct whose fields are all
//!   optional (`Option` or `#[serde(default)]`) gets its defaults. A struct with required fields
//!   still fails, with serde's "missing field" message.
//! - **Results** are encoded with the codec; an encode error is [`codes::INTERNAL`].
//! - **Built-in `methods`** returns the list of [`MethodInfo`] (encoded with the codec; its
//!   arguments are ignored). Registering a method named `methods` replaces it. Registering any
//!   other name twice panics: that is a programming error at startup.
//! - **Guards** ([`Router::guard`]) run in registration order after the method is resolved and
//!   before the arguments are decoded; the first `Err` is the call's result. **Observers**
//!   ([`Router::observe`]) run after the call, also for denied calls and argument errors.
//! - **Nesting** ([`Router::nest`], [`Router::merge`]): a mounted router keeps its metadata (with
//!   the prefix) and its guards and observers, which apply to its own methods, inside the
//!   mounting router's: outer guards, inner guards, handler, inner observers, outer observers.
//! - **Sync and async**: [`Router::call`] runs every method; [`Router::call_sync`] runs
//!   synchronous ones and fails an async one with [`codes::ASYNC_ONLY`].
//!
//! # Features
//!
//! - `schema`: JSON Schemas from `schemars` (`MethodBuilder::schema` and friends), used by
//!   [`codegen::typescript`] and [`codegen::markdown`].
//! - `cbor`: the `Cbor` codec (`ciborium`).

#![warn(missing_docs)]

mod codec;
pub mod codegen;
mod failure;
mod info;
mod router;

#[cfg(feature = "cbor")]
pub use codec::Cbor;
pub use codec::{Codec, Json};
pub use failure::{Failure, codes};
#[cfg(feature = "schema")]
pub use info::schema_value;
pub use info::{CallRecord, MethodBuilder, MethodInfo};
pub use router::{AsyncHandler, BoxFuture, CallResult, METHODS, Router};
