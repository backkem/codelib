//! End-to-end tests against the embedded JavaScript engine.
//!
//! The first run on a machine compiles the engine (slow); after that it is
//! loaded from the on-disk cache.

use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use codelib::egress::{StaticCredential, json_response};
use codelib::rpc::codegen::{self, JsOptions};
use codelib::{Egress, Failure, Limits, Router, Rule, RunError, Sandbox, WasiConfig};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// One compiled engine for all the tests that need nothing special.
static SANDBOX: LazyLock<Sandbox> = LazyLock::new(|| Sandbox::new().expect("the sandbox builds"));

fn script_error(result: &Result<Value, RunError>) -> &codelib::ScriptError {
    match result {
        Err(RunError::Script(error)) => error,
        other => panic!("expected a script error, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn returns_json_values() {
    let outcome = SANDBOX.run("return [1, 2, 3].map((n) => n * 2)").await;
    assert_eq!(outcome.result.unwrap(), json!([2, 4, 6]));

    // Nothing returned is `null`; a string stays a string.
    assert_eq!(SANDBOX.run("1 + 1").await.result.unwrap(), Value::Null);
    assert_eq!(
        SANDBOX.run("return '42'").await.result.unwrap(),
        json!("42")
    );
    assert_eq!(
        SANDBOX
            .run("return { a: { b: true } }")
            .await
            .value::<Value>()
            .unwrap()["a"]["b"],
        json!(true)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn top_level_await_works() {
    let outcome = SANDBOX
        .run(
            r#"
            const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
            await wait(10);
            const values = await Promise.all([1, 2, 3].map(async (n) => { await wait(1); return n; }));
            return values.reduce((a, b) => a + b);
            "#,
        )
        .await;
    assert_eq!(outcome.result.unwrap(), json!(6));
}

#[tokio::test(flavor = "multi_thread")]
async fn console_is_captured_even_when_the_script_throws() {
    let outcome = SANDBOX
        .run(
            r#"console.log("to stdout"); console.error("to stderr"); throw new TypeError("boom");"#,
        )
        .await;

    assert!(
        outcome.console.stdout.contains("to stdout"),
        "{:?}",
        outcome.console
    );
    assert!(
        outcome.console.stderr.contains("to stderr"),
        "{:?}",
        outcome.console
    );

    let error = script_error(&outcome.result);
    assert_eq!(error.name, "TypeError");
    assert_eq!(error.message, "boom");
}

#[tokio::test(flavor = "multi_thread")]
async fn stack_traces_name_the_program() {
    let outcome = SANDBOX
        .script("function inner() { throw new Error('deep'); }\ninner();")
        .name("report.js")
        .run()
        .await;
    let error = script_error(&outcome.result);
    let stack = error.stack.as_deref().unwrap_or_default();
    assert!(
        stack.contains("report.js"),
        "stack does not name the program: {stack:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn input_is_visible_as_host_input() {
    let outcome = SANDBOX
        .script("return host.input.items.length * host.input.factor")
        .input(json!({ "items": ["a", "b", "c"], "factor": 7 }))
        .run()
        .await;
    assert_eq!(outcome.result.unwrap(), json!(21));
}

#[tokio::test(flavor = "multi_thread")]
async fn preludes_have_their_own_scope_and_share_globals() {
    // Both preludes declare the same top-level `const`; that must not clash.
    let outcome = SANDBOX
        .script("return [first.base(), second.base(), typeof BASE]")
        .prelude(
            "first.js",
            "const BASE = 'one'; globalThis.first = { base: () => BASE };",
        )
        .prelude(
            "second.js",
            "const BASE = 'two'; globalThis.second = { base: () => BASE };",
        )
        .run()
        .await;
    assert_eq!(outcome.result.unwrap(), json!(["one", "two", "undefined"]));
}

#[tokio::test(flavor = "multi_thread")]
async fn runs_do_not_see_each_other() {
    SANDBOX
        .run("globalThis.leak = 'secret'")
        .await
        .result
        .unwrap();
    assert_eq!(
        SANDBOX
            .run("return typeof globalThis.leak")
            .await
            .result
            .unwrap(),
        json!("undefined")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn many_runs_at_once() {
    let mut runs = Vec::new();
    for n in 0..16 {
        runs.push(tokio::spawn(async move {
            SANDBOX
                .script("return host.input * 2")
                .input(n)
                .run()
                .await
                .result
                .unwrap()
        }));
    }
    for (n, run) in runs.into_iter().enumerate() {
        assert_eq!(run.await.unwrap(), json!(n * 2));
    }
}

#[test]
fn run_blocking_needs_no_runtime() {
    let outcome = SANDBOX.script("return 'sync'").run_blocking();
    assert_eq!(outcome.result.unwrap(), json!("sync"));
}

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn an_endless_loop_times_out() {
    // Building the sandbox (a cold engine compile) is not what is timed.
    LazyLock::force(&SANDBOX);
    let started = Instant::now();
    let outcome = SANDBOX
        .script("console.log('started'); while (true) {}")
        .timeout(Duration::from_millis(500))
        .run()
        .await;
    assert!(
        matches!(outcome.result, Err(RunError::Timeout(_))),
        "got {:?}",
        outcome.result
    );
    assert!(started.elapsed() < Duration::from_secs(10));
    // What was logged before the interrupt survives.
    assert!(outcome.console.stdout.contains("started"));
}

#[tokio::test(flavor = "multi_thread")]
async fn waiting_forever_times_out_too() {
    // Not computing, just never resolving: the epoch interrupt alone would
    // not catch this.
    let outcome = SANDBOX
        .script("await new Promise((resolve) => setTimeout(resolve, 60_000)); return 'late'")
        .timeout(Duration::from_millis(500))
        .run()
        .await;
    assert!(
        matches!(outcome.result, Err(RunError::Timeout(_))),
        "got {:?}",
        outcome.result
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_memory_bomb_is_reported_as_out_of_memory() {
    let outcome = SANDBOX
        .script(
            "const hoard = []; while (true) { hoard.push(new Uint8Array(1024 * 1024).fill(1)); }",
        )
        .limits(
            Limits::default()
                .memory(96 * 1024 * 1024)
                .timeout(Duration::from_secs(20)),
        )
        .run()
        .await;
    assert!(
        matches!(outcome.result, Err(RunError::OutOfMemory(_))),
        "got {:?}",
        outcome.result
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn console_output_is_capped_not_fatal() {
    let outcome = SANDBOX
        .script("for (let i = 0; i < 2000; i++) console.log('x'.repeat(100)); return 'done'")
        .limits(Limits::default().console(10_000))
        .run()
        .await;
    assert_eq!(outcome.result.unwrap(), json!("done"));
    assert!(outcome.console.truncated);
    assert!(outcome.console.stdout.len() <= 10_000);
}

#[tokio::test(flavor = "multi_thread")]
async fn deterministic_mode_repeats_clock_and_randomness() {
    let sandbox = Sandbox::<()>::builder()
        .wasi(WasiConfig::deterministic(7))
        .build()
        .expect("the sandbox builds");
    let program =
        "return [Date.now(), Math.random(), crypto.getRandomValues(new Uint8Array(4)).join(',')]";
    let first = sandbox.run(program).await.result.unwrap();
    let second = sandbox.run(program).await.result.unwrap();
    assert_eq!(first, second);
}

// ---------------------------------------------------------------------------
// Host calls
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Notes {
    items: Vec<String>,
}

#[derive(Deserialize)]
struct Add {
    text: String,
}

async fn slow_count(notes: &mut Notes, _: ()) -> Result<usize, Failure> {
    tokio::time::sleep(Duration::from_millis(20)).await;
    Ok(notes.items.len())
}

fn notes_router() -> Router<Notes> {
    let mut rpc = Router::<Notes>::new();
    rpc.method("notes.add", |notes: &mut Notes, add: Add| {
        notes.items.push(add.text);
        Ok(notes.items.len())
    })
    .summary("Add a note; returns how many there are.");
    rpc.method("notes.list", |notes: &mut Notes, _: ()| {
        Ok(notes.items.clone())
    });
    rpc.method_async("notes.count", slow_count);
    rpc.method(
        "notes.fail",
        |_: &mut Notes, _: ()| -> Result<(), Failure> {
            Err(Failure::denied("not today").with_hint("try tomorrow"))
        },
    );
    rpc
}

fn notes_sandbox() -> Sandbox<Notes> {
    let rpc = notes_router();
    // The client library is generated from the router: `notes.add({...})`.
    let client = codegen::js_client(rpc.methods(), &JsOptions::new("api"));
    Sandbox::<Notes>::builder()
        .prelude("api.js", client)
        .host_calls(rpc)
        .build()
        .expect("the sandbox builds")
}

#[tokio::test(flavor = "multi_thread")]
async fn host_calls_work_on_the_state_and_hand_it_back() {
    let sandbox = notes_sandbox();
    let outcome = sandbox
        .script(
            r#"
            host.call("notes.add", { text: "first" });
            api.notes.add({ text: "second" });          // the generated client
            const count = await api.notes.count();      // an async handler; `await` is optional
            return { count, list: api.notes.list() };
            "#,
        )
        .run_with(Notes {
            items: vec!["zeroth".into()],
        })
        .await;

    assert_eq!(
        outcome.result.unwrap(),
        json!({ "count": 3, "list": ["zeroth", "first", "second"] })
    );
    assert_eq!(outcome.state.items, ["zeroth", "first", "second"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn host_failures_are_catchable_errors_with_a_code() {
    let sandbox = notes_sandbox();
    let outcome = sandbox
        .run(
            r#"
            const seen = [];
            for (const [method, args] of [["notes.fail", null], ["notes.nope", null], ["notes.add", { wrong: 1 }]]) {
                try { host.call(method, args); seen.push("ok"); }
                catch (e) { seen.push([e instanceof host.HostError, e.name, e.code, e.hint !== null]); }
            }
            return seen;
            "#,
        )
        .await;
    let seen = outcome.result.unwrap();
    assert_eq!(seen[0], json!([true, "HostError", "denied", true]));
    assert_eq!(seen[1][2], json!("unknown-method"));
    assert_eq!(seen[2][2], json!("bad-args"));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_uncaught_host_failure_keeps_the_state() {
    let sandbox = notes_sandbox();
    let outcome = sandbox
        .run(r#"api.notes.add({ text: "kept" }); api.notes.fail();"#)
        .await;
    let error = script_error(&outcome.result);
    assert_eq!(error.name, "HostError");
    assert!(error.message.starts_with("denied:"), "{}", error.message);
    assert_eq!(outcome.state.items, ["kept"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn without_host_calls_every_call_is_unknown() {
    let outcome = SANDBOX
        .run("try { host.call('anything'); } catch (e) { return e.code; }")
        .await;
    assert_eq!(outcome.result.unwrap(), json!("unknown-method"));
}

#[tokio::test(flavor = "multi_thread")]
async fn emitted_items_arrive_as_they_happen() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let outcome = SANDBOX
        .script("for (let i = 0; i < 3; i++) host.emit('progress', { step: i }); return 'done'")
        .on_emit(move |kind, item| sink.lock().unwrap().push((kind.to_string(), item)))
        .run()
        .await;
    assert_eq!(outcome.result.unwrap(), json!("done"));
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 3);
    assert_eq!(seen[2], ("progress".to_string(), json!({ "step": 2 })));
}

// ---------------------------------------------------------------------------
// fetch and egress
// ---------------------------------------------------------------------------

/// A tiny HTTP/1.1 server that answers every request with a JSON description
/// of what it received. Returns its `host:port`.
async fn echo_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 4096];
                let (head, mut body) = loop {
                    let Ok(n) = socket.read(&mut chunk).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    buffer.extend_from_slice(&chunk[..n]);
                    if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buffer[..end]).into_owned();
                        break (head, buffer[end + 4..].to_vec());
                    }
                };
                let header = |name: &str| {
                    head.lines().skip(1).find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case(name)
                            .then(|| value.trim().to_string())
                    })
                };
                let length: usize = header("content-length")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                while body.len() < length {
                    let Ok(n) = socket.read(&mut chunk).await else {
                        return;
                    };
                    if n == 0 {
                        break;
                    }
                    body.extend_from_slice(&chunk[..n]);
                }
                let mut request_line = head.lines().next().unwrap_or_default().split(' ');
                let reply = json!({
                    "method": request_line.next(),
                    "path": request_line.next(),
                    "authorization": header("authorization"),
                    "body": String::from_utf8_lossy(&body),
                })
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                    reply.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    address
}

#[tokio::test(flavor = "multi_thread")]
async fn fetch_is_denied_by_default_and_the_denial_is_catchable() {
    let outcome = SANDBOX
        .run(
            r#"
            const results = [];
            for (const init of [undefined, { method: "POST", body: "payload" }]) {
                try { await fetch("https://example.com/", init); results.push("allowed"); }
                catch (e) { results.push("denied"); }
            }
            return results;
            "#,
        )
        .await;
    assert_eq!(outcome.result.unwrap(), json!(["denied", "denied"]));
    assert_eq!(outcome.stats.http_requests, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn credentials_are_injected_on_the_host_and_override_the_script() {
    let server = echo_server().await;
    let egress = Egress::builder()
        .allow(
            Rule::host("127.0.0.1")
                .https_only(false)
                .credential(StaticCredential::bearer("the-real-token")),
        )
        .deny_private_ips(false)
        .build();

    let outcome = SANDBOX
        .script(
            r#"
            const response = await fetch(`http://${host.input}/things?x=1`, {
                method: "POST",
                headers: { authorization: "Bearer forged-by-the-script" },
                body: "hello",
            });
            return await response.json();
            "#,
        )
        .input(&server)
        .egress(egress)
        .run()
        .await;

    let seen = outcome.result.unwrap();
    assert_eq!(seen["method"], json!("POST"));
    assert_eq!(seen["path"], json!("/things?x=1"));
    assert_eq!(seen["body"], json!("hello"));
    assert_eq!(seen["authorization"], json!("Bearer the-real-token"));
}

#[tokio::test(flavor = "multi_thread")]
async fn only_allowed_hosts_and_methods_get_through() {
    let server = echo_server().await;
    let egress = Egress::builder()
        .allow(Rule::host("127.0.0.1").https_only(false).read_only())
        .deny_private_ips(false)
        .build();

    let outcome = SANDBOX
        .script(
            r#"
            const attempt = async (url, init) => {
                try { return (await fetch(url, init)).status; } catch { return "denied"; }
            };
            return [
                await attempt(`http://${host.input}/`),
                await attempt(`http://${host.input}/`, { method: "DELETE" }),
                await attempt("http://localhost:1/"),
            ];
            "#,
        )
        .input(&server)
        .egress(egress)
        .run()
        .await;
    assert_eq!(outcome.result.unwrap(), json!([200, "denied", "denied"]));
}

#[tokio::test(flavor = "multi_thread")]
async fn virtual_hosts_answer_in_process() {
    let egress = Egress::builder()
        .virtual_host(
            "files.internal",
            |request: codelib::egress::Request| async move {
                let path = request.uri().path().to_string();
                json_response(http::StatusCode::OK, json!({ "path": path }).to_string())
            },
        )
        .build();

    let outcome = SANDBOX
        .script("return await (await fetch('http://files.internal/report.csv')).json()")
        .egress(egress)
        .run()
        .await;
    assert_eq!(outcome.result.unwrap(), json!({ "path": "/report.csv" }));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unread_response_does_not_break_the_run() {
    let server = echo_server().await;
    let egress = Egress::builder()
        .allow(Rule::host("127.0.0.1").https_only(false))
        .deny_private_ips(false)
        .build();
    let outcome = SANDBOX
        .script("const response = await fetch(`http://${host.input}/`); return response.status;")
        .input(&server)
        .egress(egress)
        .run()
        .await;
    assert_eq!(outcome.result.unwrap(), json!(200));
}

#[tokio::test(flavor = "multi_thread")]
async fn large_request_bodies_are_sent_whole() {
    let server = echo_server().await;
    let egress = Egress::builder()
        .allow(Rule::host("127.0.0.1").https_only(false))
        .deny_private_ips(false)
        .build();
    let outcome = SANDBOX
        .script(
            r#"
            const response = await fetch(`http://${host.input}/upload`, { method: "POST", body: "y".repeat(300_000) });
            return (await response.json()).body.length;
            "#,
        )
        .input(&server)
        .egress(egress)
        .run()
        .await;
    assert_eq!(outcome.result.unwrap(), json!(300_000));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_denied_fetch_is_explained_in_the_console() {
    let outcome = SANDBOX
        .run("try { await fetch('https://example.com/secret?token=abc'); } catch { return 'denied'; }")
        .await;
    assert_eq!(outcome.result.unwrap(), json!("denied"));
    let stderr = &outcome.console.stderr;
    assert!(
        stderr.contains("[codelib] fetch denied: GET https://example.com/secret"),
        "{stderr:?}"
    );
    assert!(
        !stderr.contains("token=abc"),
        "the query string must not be logged: {stderr:?}"
    );
}

// ---------------------------------------------------------------------------
// serve
// ---------------------------------------------------------------------------

fn get(path: &str) -> http::Request<codelib::egress::Body> {
    http::Request::builder()
        .uri(format!("http://service.local{path}"))
        .body(codelib::egress::empty_body())
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_served_script_answers_many_requests_and_keeps_its_state() {
    let service = SANDBOX
        .script(
            r#"
            let hits = 0;
            host.serve(async (request) => {
                hits += 1;
                const url = new URL(request.url);
                if (url.pathname === "/boom") throw new Error("handler exploded");
                const body = request.method === "POST" ? await request.text() : null;
                return Response.json({ hits, path: url.pathname, body }, { status: 201, headers: { "x-served-by": "script" } });
            });
            return { ready: true };
            "#,
        )
        .serve()
        .await
        .unwrap_or_else(|outcome| panic!("serve failed: {:?}", outcome.result));

    assert_eq!(service.value(), &json!({ "ready": true }));

    let first = service.handle_buffered(get("/one"), 1 << 20).await.unwrap();
    assert_eq!(first.status(), 201);
    assert_eq!(first.headers()["x-served-by"], "script");
    assert_eq!(
        serde_json::from_slice::<Value>(first.body()).unwrap(),
        json!({ "hits": 1, "path": "/one", "body": null })
    );

    let post = http::Request::builder()
        .method("POST")
        .uri("http://service.local/two")
        .body(codelib::egress::full_body("payload"))
        .unwrap();
    let second = service.handle_buffered(post, 1 << 20).await.unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(second.body()).unwrap(),
        json!({ "hits": 2, "path": "/two", "body": "payload" })
    );

    // A throwing handler is a 500, not the end of the service.
    let failed = service
        .handle_buffered(get("/boom"), 1 << 20)
        .await
        .unwrap();
    assert_eq!(failed.status(), 500);
    let third = service
        .handle_buffered(get("/three"), 1 << 20)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(third.body()).unwrap()["hits"],
        json!(4)
    );

    let outcome = service.finish().await;
    assert!(
        outcome.console.stderr.contains("handler exploded"),
        "{:?}",
        outcome.console
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn large_responses_are_collected() {
    let service = SANDBOX
        .script("addEventListener('fetch', (event) => event.respondWith(new Response('x'.repeat(300_000))));")
        .serve()
        .await
        .unwrap_or_else(|outcome| panic!("serve failed: {:?}", outcome.result));
    let response = service.handle_buffered(get("/"), 1 << 20).await.unwrap();
    assert_eq!(response.body().len(), 300_000);
}

#[tokio::test(flavor = "multi_thread")]
async fn run_and_serve_refuse_the_wrong_kind_of_program() {
    let outcome = SANDBOX.run("host.serve(() => new Response('hi'))").await;
    assert!(
        script_error(&outcome.result)
            .message
            .contains("never serves")
    );

    let Err(outcome) = SANDBOX.script("return 1").serve().await else {
        panic!("a program without a handler must not serve");
    };
    assert!(
        script_error(&outcome.result)
            .message
            .contains("no request handler")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn awaiting_a_promise_nothing_can_settle_is_a_clear_failure() {
    // The engine exits the process when its event loop is empty; that must
    // not surface as an opaque trap backtrace.
    let outcome = SANDBOX.run("await new Promise(() => {})").await;
    match outcome.result {
        Err(RunError::Sandbox(message)) => {
            assert!(message.contains("can never happen"), "{message}")
        }
        other => panic!("expected a sandbox failure, got {other:?}"),
    }
    // The sandbox is unaffected.
    assert_eq!(SANDBOX.run("return 1").await.result.unwrap(), json!(1));
}

// ---------------------------------------------------------------------------
// Positions in stack traces
// ---------------------------------------------------------------------------

fn location_of(result: &Result<Value, RunError>) -> String {
    let location = script_error(result)
        .location()
        .unwrap_or_else(|| panic!("no location in {result:?}"));
    format!("{}:{}:{}", location.source, location.line, location.column)
}

#[tokio::test(flavor = "multi_thread")]
async fn errors_report_the_programs_own_line_and_column() {
    let outcome = SANDBOX
        .script(r#"throw new Error("x")"#)
        .name("script.js")
        .run()
        .await;
    assert_eq!(location_of(&outcome.result), "script.js:1:7");

    let outcome = SANDBOX
        .script("const a = 1;\n\n  throw new Error('x');\n")
        .name("script.js")
        .run()
        .await;
    assert_eq!(location_of(&outcome.result), "script.js:3:9");
    let stack = script_error(&outcome.result).stack.clone().unwrap();
    assert!(stack.contains("script.js:3:"), "{stack}");
}

#[tokio::test(flavor = "multi_thread")]
async fn errors_in_a_prelude_report_its_own_position() {
    let outcome = SANDBOX
        .script("return 1")
        .prelude("lib.js", r#"throw new Error("x")"#)
        .run()
        .await;
    assert_eq!(location_of(&outcome.result), "lib.js:1:7");

    let outcome = SANDBOX
        .script("return 1")
        .prelude("lib.js", "let a;\nlet b;\nthrow new Error('x')")
        .run()
        .await;
    assert!(location_of(&outcome.result).starts_with("lib.js:3:"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_nested_frame_on_line_one_is_corrected_too() {
    let outcome = SANDBOX
        .script("function f() { throw new Error('x'); } f();")
        .name("script.js")
        .run()
        .await;
    let stack = script_error(&outcome.result).stack.clone().unwrap();
    let lines: Vec<_> = stack.lines().collect();
    assert_eq!(lines[0], "f@script.js:1:22", "{stack}");
    assert_eq!(lines[1], "@script.js:1:40", "{stack}");
    assert_eq!(lines.len(), 2, "{stack}");
}

#[tokio::test(flavor = "multi_thread")]
async fn sources_have_their_own_scope_and_share_globalthis() {
    let outcome = SANDBOX
        .script(
            r#"
            const x = "program";
            var leaked = 1;
            return [x, typeof leaked, globalThis.shared, this === globalThis, await Promise.resolve(7)];
            "#,
        )
        .prelude(
            "one.js",
            "const x = 'prelude'; var leaked = 2; globalThis.shared = x; return 5;",
        )
        .prelude(
            "two.js",
            "const x = 'second'; if (globalThis.shared !== 'prelude') throw new Error('lost');",
        )
        .run()
        .await;
    assert_eq!(
        outcome.result.unwrap(),
        json!(["program", "number", "prelude", true, 7])
    );
    // A trailing line comment cannot swallow the end of the wrapper.
    let outcome = SANDBOX.run("return 3 // done").await;
    assert_eq!(outcome.result.unwrap(), json!(3));
    // `var` inside a source does not become a global.
    let outcome = SANDBOX
        .run("var inner = 1; return typeof globalThis.inner")
        .await;
    assert_eq!(outcome.result.unwrap(), json!("undefined"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_program_cannot_replace_eval_for_later_sources() {
    let outcome = SANDBOX
        .script("return 1 + 1")
        .prelude(
            "evil.js",
            "globalThis.eval = () => { throw new Error('hijacked'); }; Function.prototype.call = null;",
        )
        .run()
        .await;
    assert_eq!(outcome.result.unwrap(), json!(2));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_syntax_error_names_its_source() {
    let outcome = SANDBOX
        .script("return 1")
        .prelude("broken.js", "let = = 1;")
        .run()
        .await;
    let error = script_error(&outcome.result);
    assert_eq!(error.name, "SyntaxError");
    let described = format!("{error:?}");
    assert!(described.contains("broken.js"), "{described}");

    let outcome = SANDBOX.script("let x = ;").name("bad.js").run().await;
    let error = script_error(&outcome.result);
    assert_eq!(error.name, "SyntaxError");
    let described = format!("{error:?}");
    assert!(described.contains("bad.js"), "{described}");
}

#[tokio::test(flavor = "multi_thread")]
async fn stacks_hide_the_runtimes_own_frames() {
    let outcome = SANDBOX
        .script(
            "function outer() { inner(); }\nfunction inner() { throw new Error('deep'); }\nouter();",
        )
        .name("app.js")
        .run()
        .await;
    let stack = script_error(&outcome.result).stack.clone().unwrap();
    assert!(stack.contains("app.js:2:"), "{stack}");
    assert!(stack.contains("app.js:1:"), "{stack}");
    for internal in ["runtime-wrapper.js", "initializer.js"] {
        assert!(!stack.contains(internal), "{stack}");
    }

    let outcome = notes_sandbox()
        .script("await Promise.resolve();\napi.notes.fail();")
        .name("app.js")
        .run()
        .await;
    let error = script_error(&outcome.result);
    assert_eq!(error.name, "HostError");
    let stack = error.stack.clone().unwrap();
    assert!(stack.contains("app.js:2:"), "{stack}");
    for internal in ["runtime-wrapper.js", "initializer.js"] {
        assert!(!stack.contains(internal), "{stack}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn results_keep_the_order_of_object_keys() {
    let outcome = SANDBOX
        .run("return { zebra: 1, apple: 2, mango: { z: 1, a: 2 } }")
        .await;
    let text = serde_json::to_string(&outcome.result.unwrap()).unwrap();
    assert_eq!(text, r#"{"zebra":1,"apple":2,"mango":{"z":1,"a":2}}"#);
}

// ---------------------------------------------------------------------------
// host.callRaw
// ---------------------------------------------------------------------------

fn raw_sandbox() -> Sandbox<()> {
    Sandbox::<()>::builder()
        .host_calls(codelib::host_fn(|_: &mut (), method, args| match method {
            "echo" => Ok(args.to_string()),
            "upper" => Ok(args.to_uppercase()),
            "big" => Ok("x".repeat(1 << 20)),
            "empty" => Ok(String::new()),
            "fail" => Err(Failure::new("denied", format!("no: {args}"))),
            other => Err(Failure::unknown_method(other)),
        }))
        .build()
        .expect("the sandbox builds")
}

#[tokio::test(flavor = "multi_thread")]
async fn call_raw_passes_text_through_unchanged() {
    let outcome = raw_sandbox()
        .run(
            r#"
            const text = 'not json: {"a" é \n\t "quotes"} <>';
            return [
                host.callRaw("echo", text) === text,
                host.callRaw("upper", "abc"),
                host.callRaw("echo", "") === "",
                host.callRaw("empty", "x"),
            ];
            "#,
        )
        .await;
    assert_eq!(outcome.result.unwrap(), json!([true, "ABC", true, ""]));
}

#[tokio::test(flavor = "multi_thread")]
async fn call_raw_returns_a_large_result() {
    let outcome = raw_sandbox()
        .run("const text = host.callRaw('big', ''); return [text.length, text[0], text[text.length - 1]];")
        .await;
    assert_eq!(outcome.result.unwrap(), json!([1 << 20, "x", "x"]));
}

#[tokio::test(flavor = "multi_thread")]
async fn call_raw_failures_throw_host_error() {
    let outcome = raw_sandbox()
        .run(
            r#"
            const seen = [];
            for (const [method, arg] of [["fail", "why"], ["nope", ""]]) {
                try { host.callRaw(method, arg); }
                catch (e) { seen.push([e instanceof host.HostError, e.code, e.message]); }
            }
            try { host.callRaw("echo", { not: "a string" }); }
            catch (e) { seen.push(e.name); }
            return seen;
            "#,
        )
        .await;
    let seen = outcome.result.unwrap();
    assert_eq!(seen[0], json!([true, "denied", "no: why"]));
    assert_eq!(
        (&seen[1][0], &seen[1][1]),
        (&json!(true), &json!("unknown-method"))
    );
    assert_eq!(seen[2], json!("TypeError"));
}

// ---------------------------------------------------------------------------
// Request URLs
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn request_urls_carry_the_incoming_scheme() {
    let service = SANDBOX
        .script("host.serve((request) => new Response(request.url));")
        .serve()
        .await
        .unwrap_or_else(|outcome| panic!("serve failed: {:?}", outcome.result));
    let request = |uri: &str| {
        http::Request::builder()
            .uri(uri)
            .body(codelib::egress::empty_body())
            .unwrap()
    };
    let https = service
        .handle_buffered(request("https://secure.example/a?b=1"), 1 << 16)
        .await
        .unwrap();
    assert_eq!(https.body().as_ref(), b"https://secure.example/a?b=1");
    let plain = service
        .handle_buffered(request("http://plain.example:8080/"), 1 << 16)
        .await
        .unwrap();
    assert_eq!(plain.body().as_ref(), b"http://plain.example:8080/");
    let bare = http::Request::builder()
        .uri("/p")
        .header("host", "h.example")
        .body(codelib::egress::empty_body())
        .unwrap();
    let bare = service.handle_buffered(bare, 1 << 16).await.unwrap();
    assert_eq!(bare.body().as_ref(), b"http://h.example/p");
}
