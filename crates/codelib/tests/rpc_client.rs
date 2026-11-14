//! Runs generated `codelib-rpc` clients in the real JS engine, with adversarial method names.

use codelib::rpc::codegen::{self, JsOptions};
use codelib::{Failure, Router, Sandbox};
use serde_json::{Value, json};

const NAMES: &[&str] = &[
    "plain",
    "a\"b'c\u{5c}d",
    "line\nbreak",
    "cr\rlf",
    "</script><script>alert(1)</script>",
    "__proto__",
    "__proto__.x",
    "x.__proto__",
    "constructor",
    "prototype",
    "toString",
    "hasOwnProperty",
    "delete",
    "class",
    "1abc",
    "\u{e9}.\u{fc}n\u{ef}",
    "a..b",
    ".lead",
    "trail.",
    "files",
    "files.read",
    "files.sub.deep",
    "hyb",
    "hyb.name",
    "hyb.length",
    "hyb.prototype",
    "hyb.call",
    "hyb.__proto__",
    "u\u{2028}\u{2029}sep",
    "*/ evil() /*",
    "new",
    "get",
    "set",
    "async",
    "static",
    "eval",
    "arguments",
    "host",
    "globalThis",
    "api",
    "\u{1F600}",
    "a b",
    "a-b.c-d",
];

fn router() -> Router<()> {
    let mut r = Router::<()>::new();
    for name in NAMES {
        let n = name.to_string();
        r.method(name, move |_: &mut (), _: ()| Ok(n.clone()))
            .summary("sum */ evil(); /* \u{2028} \r\n x*\n/ y <!-- -->");
    }
    r
}

/// How the generated client reaches `name`: split on dots unless a segment is empty.
fn path(name: &str) -> Vec<String> {
    let segs: Vec<&str> = name.split('.').collect();
    if segs.iter().any(|s| s.is_empty()) {
        vec![name.to_owned()]
    } else {
        segs.iter().map(|s| s.to_string()).collect()
    }
}

fn walk_script(global: &str) -> String {
    let cases: Vec<Value> = NAMES.iter().map(|n| json!([n, path(n)])).collect();
    format!(
        r#"
        const cases = {cases};
        const root = globalThis[{global}];
        const bad = [];
        for (const [name, path] of cases) {{
            let node = root;
            for (const seg of path) {{
                if (node === null || (typeof node !== 'object' && typeof node !== 'function')
                    || !Object.prototype.hasOwnProperty.call(node, seg)) {{ node = undefined; break; }}
                node = node[seg];
            }}
            if (typeof node !== 'function') {{ bad.push([name, 'not a function']); continue; }}
            try {{
                const got = node();
                if (got !== name) bad.push([name, 'returned ' + got]);
            }} catch (e) {{ bad.push([name, 'threw ' + e]); }}
        }}
        return bad;
        "#,
        cases = serde_json::to_string(&cases).unwrap(),
        global = serde_json::to_string(global).unwrap(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn every_adversarial_name_is_callable_through_the_generated_client() {
    let r = router();
    let client = codegen::js_client(r.methods(), &JsOptions::new("api"));
    let sandbox = Sandbox::builder()
        .prelude("api.js", client)
        .host_calls(r)
        .build()
        .unwrap();
    let outcome = sandbox.run(walk_script("api")).await;
    assert_eq!(outcome.result.unwrap(), json!([]));
}

#[tokio::test(flavor = "multi_thread")]
async fn odd_global_names_work() {
    for global in [
        "my-api",
        "a\"b",
        "constructor",
        "delete",
        "\u{e9}",
        "u\u{2028}",
    ] {
        let r = router();
        let client = codegen::js_client(r.methods(), &JsOptions::new(global));
        let sandbox = Sandbox::builder()
            .prelude("api.js", client)
            .host_calls(r)
            .build()
            .unwrap();
        let outcome = sandbox.run(walk_script(global)).await;
        assert_eq!(outcome.result.unwrap(), json!([]), "global {global:?}");
    }
}

/// `globalThis.__proto__ = {...}` does not define a global named `__proto__`; it swaps the
/// global object's prototype.
#[tokio::test(flavor = "multi_thread")]
async fn global_named_proto_defines_an_own_property() {
    let mut r = Router::<()>::new();
    r.method("a", |_: &mut (), _: ()| Ok(1));
    let client = codegen::js_client(r.methods(), &JsOptions::new("__proto__"));
    let sandbox = Sandbox::builder()
        .prelude("api.js", client)
        .host_calls(r)
        .build()
        .unwrap();
    let outcome = sandbox
        .run(r#"return Object.prototype.hasOwnProperty.call(globalThis, "__proto__");"#)
        .await;
    assert_eq!(
        outcome.result.unwrap(),
        json!(true),
        "the global must be an own property"
    );
}

/// A global that shadows the `host` import breaks every generated call.
#[tokio::test(flavor = "multi_thread")]
async fn global_named_host_breaks_the_client() {
    let mut r = Router::<()>::new();
    r.method("a", |_: &mut (), _: ()| Ok(1));
    let client = codegen::js_client(r.methods(), &JsOptions::new("host"));
    let sandbox = Sandbox::builder()
        .prelude("api.js", client)
        .host_calls(r)
        .build()
        .unwrap();
    let outcome = sandbox.run("return host.a();").await;
    eprintln!("global=host: {:?}", outcome.result);
}

#[tokio::test(flavor = "multi_thread")]
async fn hostile_method_names_and_args_from_script_are_failures_not_crashes() {
    let mut r = Router::<()>::new();
    r.method("v", |_: &mut (), _: Value| Ok(1));
    for i in 0..50 {
        r.method(&format!("ns{i}.methodNumber{i}"), |_: &mut (), _: ()| Ok(1));
    }
    let sandbox = Sandbox::builder().host_calls(r).build().unwrap();
    let outcome = sandbox
        .run(
            r#"
            const out = [];
            const names = ["", ".", "\u0000", "\u{1F600}".repeat(1000), "x".repeat(100000), "methods\u0000", "__proto__", "constructor"];
            for (const n of names) {
                try { host.call(n, null); out.push("ok"); } catch (e) { out.push(e.code); }
            }
            let deep = [];
            for (let i = 0; i < 100000; i++) deep = [deep];
            try { host.call("v", deep); out.push("ok"); } catch (e) { out.push(e.code || String(e).slice(0, 40)); }
            try { host.call("v", 1n); } catch (e) { out.push("bigint:" + (e.code || e.name)); }
            const cyc = {}; cyc.a = cyc;
            try { host.call("v", cyc); } catch (e) { out.push("cyclic:" + (e.code || e.name)); }
            try { host.call("v", undefined); out.push("undef-ok"); } catch (e) { out.push("undef:" + (e.code || e.name)); }
            try { host.call("v"); out.push("noarg-ok"); } catch (e) { out.push("noarg:" + (e.code || e.name)); }
            return out;
            "#,
        )
        .await;
    eprintln!("{:?}", outcome.result);
    let v = outcome.result.unwrap();
    let v = v.as_array().unwrap();
    for item in &v[..8] {
        assert_eq!(item, "unknown-method", "{v:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn hint_and_failure_survive_the_boundary() {
    let mut r = Router::<()>::new();
    r.method("add", |_: &mut (), _: ()| -> Result<(), Failure> { Ok(()) });
    let sandbox = Sandbox::builder().host_calls(r).build().unwrap();
    let outcome = sandbox
        .run(
            r#"try { host.call("ad\n`", null) } catch (e) { return [e.code, e.message, e.hint]; }"#,
        )
        .await;
    eprintln!("{:?}", outcome.result);
    assert_eq!(outcome.result.unwrap()[0], json!("unknown-method"));
}
