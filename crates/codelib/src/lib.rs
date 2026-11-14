//! Run agent-written JavaScript in a sandbox, and decide exactly what it can
//! reach.
//!
//! A script runs inside a WebAssembly component (a JavaScript engine hosted
//! by wasmtime) with no ambient authority: no filesystem, no sockets, no
//! environment. It gets three things, each under the embedder's control:
//!
//! - **`fetch`**, through an [`Egress`] policy: an allowlist of hosts, with
//!   credentials attached on the host side so secrets never enter the
//!   sandbox.
//! - **`host.call(method, args)`**, answered by your own [`Router`] of typed
//!   Rust handlers working on your own state.
//! - **`console`**, captured and returned with the result.
//!
//! ```no_run
//! use codelib::{Egress, EnvBearer, Rule, Sandbox};
//!
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! let sandbox = Sandbox::new()?;
//!
//! let outcome = sandbox
//!     .script(r#"
//!         const response = await fetch("https://api.github.com/user");
//!         const user = await response.json();
//!         console.log("hello", user.login);
//!         return user.public_repos;
//!     "#)
//!     .egress(
//!         Egress::builder()
//!             .allow(Rule::host("api.github.com").read_only().credential(EnvBearer::new("GITHUB_TOKEN")))
//!             .build(),
//!     )
//!     .run()
//!     .await;
//!
//! println!("{}", outcome.console.stdout);
//! println!("{}", outcome.result?);
//! # Ok(()) }
//! ```
//!
//! # Pieces
//!
//! | Crate | What it is |
//! |---|---|
//! | `codelib` | This crate: the standard world, the embedded engine, [`Sandbox`]. |
//! | `codelib-sandbox` ([`sandbox`]) | The world-agnostic host kit underneath: compile cache, deadlines, limits. Use it directly to host a different WIT world, also a WASI 0.3 one (feature `p3`). |
//! | `codelib-http` ([`egress`]) | The egress policy engine behind `fetch`. |
//! | `codelib-rpc` ([`rpc`]) | The method router behind `host.call`. |
//! | `codelib_guest` | The prebuilt engine component (feature `embedded-guest`, on by default). |
//!
//! # What a script sees
//!
//! Standard web APIs (`fetch`, `URL`, `TextEncoder`, `crypto`, streams,
//! timers), plus one global:
//!
//! ```js
//! host.call("notes.add", { text: "hi" })   // -> the handler's result; throws host.HostError
//! host.callRaw("notes.import", csvText)    // -> the handler's result text; no JSON on either side
//! host.emit("progress", { done: 3 })       // -> Script::on_emit, as it happens
//! host.input                               // -> the JSON passed with Script::input
//! host.serve(async (request) => new Response("ok"))   // for Script::serve
//! ```
//!
//! `host.callRaw(method, text)` passes the string `text` to the handler as
//! it is and returns the handler's result string as it is (an empty string
//! for an empty result), for payloads that are not JSON or that are too
//! large to be worth encoding. Failures throw `host.HostError`, as with
//! `host.call`.
//!
//! A program is the body of an async function: top-level `await` and
//! `return` work, `import` does not. Preludes are evaluated first, each in
//! its own scope, and share state with the program through `globalThis`.

mod bindings;
mod builder;
mod host;
mod outcome;
mod script;
mod service;

pub use builder::{Sandbox, SandboxBuilder};
pub use host::{BoxFuture, HostCalls, host_fn};
pub use outcome::{Location, Outcome, RunError, ScriptError};
pub use script::Script;
pub use service::Service;

pub use codelib_sandbox::{Clocks, Console, Error, Limits, Random, Stats, WasiConfig};

pub use codelib_http::{Credential, CredentialProvider, Egress, EnvBearer, Redirects, Rule};
pub use codelib_rpc::{Failure, Router};

/// The egress policy engine behind `fetch` (the `codelib-http` crate).
pub use codelib_http as egress;
/// The method router behind `host.call` (the `codelib-rpc` crate).
pub use codelib_rpc as rpc;
/// The world-agnostic host kit (the `codelib-sandbox` crate).
pub use codelib_sandbox as sandbox;
