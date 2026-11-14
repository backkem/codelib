<h1 align="center">codelib</h1>

<p align="center">
  <b>Give your agent a programming language, not a hundred tools.</b><br>
  Sandboxed JavaScript for agents, in Rust, on WebAssembly.
</p>

---

Agents are better at writing a small program than at making forty tool calls
in a row. codelib is the part that makes running that program safe: the
script executes inside a WebAssembly sandbox and can touch **only what you
hand it**.

- 🧱 **Sealed by default.** No filesystem, no sockets, no environment. Every
  run gets a fresh engine instance.
- 🔑 **Secrets stay outside.** `fetch` goes through an allowlist, and tokens
  are attached on the host. The script never sees them.
- 🧩 **Your API, typed.** Expose Rust handlers over your own state; the
  JavaScript client and the docs are generated from them.
- ⏱️ **Hard limits.** Wall-clock deadline, memory cap, optional fuel. A
  runaway script is interrupted, not trusted.
- 📦 **Batteries included.** The JavaScript engine ships inside the crate.
  No Node, no build step.

## Quick start

```toml
[dependencies]
codelib = { git = "https://github.com/backkem/codelib" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

```rust
use codelib::{Egress, EnvBearer, Rule, Sandbox};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sandbox = Sandbox::new()?;

    let outcome = sandbox
        .script(r#"
            const me = await (await fetch("https://api.github.com/user")).json();
            console.log("hello", me.login);
            return me.public_repos;
        "#)
        .egress(
            Egress::builder()
                .allow(Rule::host("api.github.com").read_only().credential(EnvBearer::new("GITHUB_TOKEN")))
                .build(),
        )
        .run()
        .await;

    print!("{}", outcome.console.stdout);   // what the script logged
    println!("{}", outcome.result?);        // what it returned, as JSON
    Ok(())
}
```

The first start on a machine compiles the engine to native code, which takes
a little while. It is cached on disk; after that a sandbox starts in
milliseconds.

## Your own API

A `Router` answers the script's `host.call(...)` with typed Rust handlers.
State goes into a run and comes back out, whatever the script did.

```rust
let mut rpc = Router::<Notebook>::new();
rpc.method("notes.add", |book: &mut Notebook, add: Add| {
    book.notes.push(add.text);
    Ok(book.notes.len())
});

let client = codegen::js_client(rpc.methods(), &JsOptions::new("api"));
let sandbox = Sandbox::<Notebook>::builder().prelude("api.js", client).host_calls(rpc).build()?;

let outcome = sandbox.script(r#"return api.notes.add({ text: "ship it" })"#).run_with(notebook).await;
let notebook = outcome.state;   // handed back
```

## What's in the box

| Crate | What it does |
|---|---|
| [`codelib`](crates/codelib) | The sandbox: embedded engine, `Sandbox`, `Script`, `Service`. Start here. |
| [`codelib-http`](crates/codelib-http) | Egress policy behind `fetch`: allow rules, credential injection, virtual hosts, redirects, private-IP blocking, middleware. |
| [`codelib-rpc`](crates/codelib-rpc) | Method router behind `host.call`: typed handlers, guards, self-description, generated JS/TypeScript/Markdown. |
| [`codelib-sandbox`](crates/codelib-sandbox) | The wasmtime host kit underneath: compile cache, deadlines, limits. Hosts any component and any WIT world, WASI 0.2 or (feature `p3`) 0.3. |
| [`codelib-guest`](crates/codelib-guest) | The prebuilt engine component (StarlingMonkey), pinned by sha256. |

Need a different WIT world, or no JavaScript at all? Use `codelib-sandbox`
directly and keep the cache, limits and egress.

## Examples

```sh
# A tiny "code mode" CLI: allowlisted fetch with host-side credentials
cargo run --example minictl -- -e 'return 6 * 7'
cargo run --example minictl -- --allow api.github.com=GITHUB_TOKEN --read-only \
    -e 'return (await github.get("/user")).login'

# Host calls over your own state, with a generated client
cargo run --example notes

# The host kit on its own, with a hand-written component
cargo run --example custom_world
```

## Under the hood

Scripts run on [StarlingMonkey](https://github.com/bytecodealliance/StarlingMonkey)
(SpiderMonkey compiled to WebAssembly), hosted by
[wasmtime](https://wasmtime.dev) 50 against WASI 0.2.12. The world is small:
see [`wit/codelib.wit`](wit/codelib.wit).

With the `p3` feature, the host kit also links WASI 0.3.0, so your own
components can use async exports and `wasi:http` 0.3 under the same limits
and egress policy; the JavaScript engine itself stays on 0.2.

Rebuilding the engine component needs Node 20.6 or newer: `sh guest/build.sh`. Day to
day you never do; the built component is checked in.

## License

MIT
