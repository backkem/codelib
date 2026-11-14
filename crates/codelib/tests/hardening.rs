//! Hardening tests against the embedded JavaScript engine: deadlines,
//! isolation, out-of-memory and fuel accounting, panics in host code,
//! hostile programs, and the streaming `Service`.

use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use codelib::{
    BoxFuture, Egress, Failure, HostCalls, Limits, Rule, RunError, Sandbox, Service, host_fn,
};
use http_body::Frame;
use http_body_util::BodyExt as _;
use serde_json::{Value, json};
use tokio::net::TcpListener;

static SANDBOX: LazyLock<Sandbox> = LazyLock::new(|| Sandbox::new().expect("the sandbox builds"));

/// Host calls over a `Vec<String>` journal: every call is recorded, then
/// `sleep` never answers (in practice), `panic` panics, anything else echoes.
struct Journal;

impl HostCalls<Vec<String>> for Journal {
    fn call<'a>(
        &'a self,
        state: &'a mut Vec<String>,
        method: &'a str,
        args: &'a str,
    ) -> BoxFuture<'a, Result<String, Failure>> {
        Box::pin(async move {
            state.push(format!("{method}:{args}"));
            match method {
                "sleep" => {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    Ok("null".into())
                }
                "panic" => panic!("the journal is on fire"),
                _ => Ok(args.to_string()),
            }
        })
    }
}

static JOURNAL: LazyLock<Sandbox<Vec<String>>> = LazyLock::new(|| {
    Sandbox::<Vec<String>>::builder()
        .host_calls(Journal)
        .build()
        .expect("the sandbox builds")
});

/// Generous cap for collecting response bodies in tests.
const MAX_BODY: usize = 64 * 1024 * 1024;

fn get(path: &str) -> http::Request<codelib::egress::Body> {
    http::Request::builder()
        .uri(format!("http://service.local{path}"))
        .body(codelib::egress::empty_body())
        .unwrap()
}

async fn serve(sandbox: &Sandbox, code: &str, limits: Option<Limits>) -> Service {
    let mut script = sandbox.script(code);
    if let Some(limits) = limits {
        script = script.limits(limits);
    }
    match script.serve().await {
        Ok(service) => service,
        Err(outcome) => panic!("serve failed: {:?}", outcome.result),
    }
}

async fn text(service: &Service, request: http::Request<codelib::egress::Body>) -> String {
    let response = service.handle_buffered(request, MAX_BODY).await.unwrap();
    String::from_utf8(response.into_body().to_vec()).unwrap()
}

/// A request body that sends one chunk and then never ends.
struct Stalled {
    sent: bool,
}

impl http_body::Body for Stalled {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        if self.sent {
            Poll::Pending
        } else {
            self.sent = true;
            Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(b"x")))))
        }
    }
}

const HOG: &str =
    "const hoard = []; while (true) { hoard.push(new Uint8Array(1024 * 1024).fill(1)); }";

fn oom_limits() -> Limits {
    Limits::default()
        .memory(96 * 1024 * 1024)
        .timeout(Duration::from_secs(20))
}

// ===========================================================================
// Service: requests the host or the client get wrong
// ===========================================================================

/// Requests without a usable authority are normalised (URI authority, else
/// a valid Host header, else `localhost`) instead of ending the service.
#[tokio::test(flavor = "multi_thread")]
async fn malformed_requests_do_not_end_the_service() {
    let service = serve(
        &SANDBOX,
        "host.serve((request) => new Response(request.url))",
        None,
    )
    .await;

    let no_authority = http::Request::builder()
        .uri("/hello?x=1")
        .body(codelib::egress::empty_body())
        .unwrap();
    assert_eq!(
        text(&service, no_authority).await,
        "http://localhost/hello?x=1"
    );

    let host_header = http::Request::builder()
        .uri("/a")
        .header("host", "example.org:8080")
        .body(codelib::egress::empty_body())
        .unwrap();
    assert_eq!(
        text(&service, host_header).await,
        "http://example.org:8080/a"
    );

    for bad in [&b"caf\xe9.example"[..], b"bad host", b""] {
        let request = http::Request::builder()
            .method("POST")
            .uri("/b")
            .header("host", http::HeaderValue::from_bytes(bad).unwrap())
            .body(codelib::egress::full_body("payload"))
            .unwrap();
        assert_eq!(text(&service, request).await, "http://localhost/b");
    }

    assert_eq!(
        text(&service, get("/after")).await,
        "http://service.local/after"
    );
    assert!(service.finish().await.result.is_ok());
}

/// GET/HEAD bodies are discarded without being read, so a GET whose body
/// stalls costs nothing, and a warm instance keeps answering many GETs that
/// carry bodies.
#[tokio::test(flavor = "multi_thread")]
async fn get_bodies_are_discarded_not_read() {
    let service = serve(
        &SANDBOX,
        "let n = 0; host.serve((request) => new Response(`${request.method} ${++n}`))",
        Some(Limits::default().timeout(Duration::from_secs(1))),
    )
    .await;

    let started = Instant::now();
    let stalled = http::Request::builder()
        .uri("http://service.local/")
        .body(Stalled { sent: false })
        .unwrap();
    let answer = service.handle_buffered(stalled, MAX_BODY).await.unwrap();
    assert_eq!(answer.body(), "GET 1");
    assert!(started.elapsed() < Duration::from_millis(900));

    for n in 2..=60 {
        let method = if n % 2 == 0 { "GET" } else { "HEAD" };
        let request = http::Request::builder()
            .method(method)
            .uri("http://service.local/")
            .body(codelib::egress::full_body(vec![b'z'; 64 * 1024]))
            .unwrap();
        let answer = service.handle_buffered(request, MAX_BODY).await.unwrap();
        assert_eq!(answer.status(), 200);
        if method == "GET" {
            assert_eq!(answer.body(), &format!("GET {n}"));
        }
    }
    let stalled = http::Request::builder()
        .uri("http://service.local/")
        .body(Stalled { sent: false })
        .unwrap();
    assert_eq!(
        service
            .handle_buffered(stalled, MAX_BODY)
            .await
            .unwrap()
            .body(),
        "GET 61"
    );
}

/// POST handlers that ignore their body, read part of it or all of it leave
/// the instance working.
#[tokio::test(flavor = "multi_thread")]
async fn unread_post_bodies_leave_the_instance_working() {
    let service = serve(
        &SANDBOX,
        r#"host.serve(async (request) => {
            const path = new URL(request.url).pathname;
            if (path === "/ignore") return new Response("ignored");
            if (path === "/part") {
                const reader = request.body.getReader();
                const { value } = await reader.read();
                await reader.cancel();
                return new Response(`part ${value.length > 0}`);
            }
            return new Response(`all ${(await request.text()).length}`);
        })"#,
        None,
    )
    .await;
    let post = |path: &str| {
        http::Request::builder()
            .method("POST")
            .uri(format!("http://service.local{path}"))
            .body(codelib::egress::full_body(vec![b'p'; 100_000]))
            .unwrap()
    };
    for _ in 0..20 {
        assert_eq!(text(&service, post("/ignore")).await, "ignored");
        assert_eq!(text(&service, post("/part")).await, "part true");
        assert_eq!(text(&service, post("/all")).await, "all 100000");
    }
}

// ===========================================================================
// Service: failures inside the handler
// ===========================================================================

/// A handler that hits the memory cap and throws is a 500 like any other
/// throwing handler; the service carries on, and the stats record the hit.
#[tokio::test(flavor = "multi_thread")]
async fn service_handler_out_of_memory_is_a_500() {
    let service = serve(
        &SANDBOX,
        &format!(
            "host.serve((request) => {{ if (request.url.endsWith('/hog')) {{ {HOG} }} return new Response('fine'); }})"
        ),
        Some(oom_limits()),
    )
    .await;
    let answer = service
        .handle_buffered(get("/hog"), MAX_BODY)
        .await
        .unwrap();
    assert_eq!(answer.status(), 500);
    assert_eq!(text(&service, get("/next")).await, "fine");
    let outcome = service.finish().await;
    assert!(outcome.result.is_ok(), "{:?}", outcome.result);
    assert!(outcome.stats.memory_limit_hit);
}

/// The memory flag is per request: after one request recovered from a
/// refused allocation, a later timeout is reported as a timeout.
#[tokio::test(flavor = "multi_thread")]
async fn service_later_timeout_is_not_reported_as_out_of_memory() {
    let service = serve(
        &SANDBOX,
        &format!(
            r#"host.serve((request) => {{
                if (request.url.endsWith("/spin")) {{ while (true) {{}} }}
                try {{ {HOG} }} catch {{ return new Response("recovered"); }}
            }})"#
        ),
        Some(
            Limits::default()
                .memory(96 * 1024 * 1024)
                .timeout(Duration::from_secs(3)),
        ),
    )
    .await;
    assert_eq!(text(&service, get("/hog")).await, "recovered");
    let second = service.handle(get("/spin")).await;
    assert!(
        matches!(second, Err(RunError::Timeout(_))),
        "{:?}",
        second.map(|r| r.status())
    );
}

/// A response whose body fails half-way ends with an error for the reader
/// (`handle`) and is an error for `handle_buffered`; the service carries on.
#[tokio::test(flavor = "multi_thread")]
async fn truncated_response_is_an_error_not_a_short_success() {
    let service = serve(
        &SANDBOX,
        r#"host.serve((request) => {
            if (request.url.endsWith("/ok")) return new Response("whole");
            return new Response(new ReadableStream({
                start(controller) { controller.enqueue(new TextEncoder().encode("partial")); },
                pull(controller) { controller.error(new Error("upstream broke")); },
            }));
        })"#,
        None,
    )
    .await;

    let buffered = service.handle_buffered(get("/broken"), MAX_BODY).await;
    assert!(
        buffered.is_err(),
        "{:?}",
        buffered.map(|r| (r.status(), r.into_body()))
    );

    let streamed = service.handle(get("/broken")).await.unwrap();
    assert_eq!(streamed.status(), 200, "the head was already sent");
    assert!(streamed.into_body().collect().await.is_err());

    assert_eq!(text(&service, get("/ok")).await, "whole");
}

/// The error that ends a service is what later requests and `finish`
/// report; the state and console are still handed back.
#[tokio::test(flavor = "multi_thread")]
async fn finish_of_a_dead_service_reports_what_ended_it() {
    let service = serve(
        &SANDBOX,
        "host.serve(() => { console.log('spinning'); while (true) {} }); return 'ready'",
        Some(Limits::default().timeout(Duration::from_millis(500))),
    )
    .await;
    let answer = service.handle(get("/")).await;
    assert!(matches!(answer, Err(RunError::Timeout(_))));
    let later = service.handle(get("/")).await;
    assert!(
        matches!(later, Err(RunError::Timeout(_))),
        "{:?}",
        later.map(|r| r.status())
    );
    let outcome = service.finish().await;
    assert!(
        matches!(outcome.result, Err(RunError::Timeout(_))),
        "{:?}",
        outcome.result
    );
    assert!(outcome.console.stdout.contains("spinning"));
}

/// A caller that gives up on a request (client disconnect, its own timeout)
/// does not end the service: the request is answered and discarded, and the
/// next one is served.
#[tokio::test(flavor = "multi_thread")]
async fn abandoned_requests_do_not_end_the_service() {
    let service = serve(
        &SANDBOX,
        r#"host.serve(async (request) => {
            if (request.url.endsWith("/slow")) await new Promise((resolve) => setTimeout(resolve, 1000));
            return new Response("done");
        })"#,
        None,
    )
    .await;
    let abandoned =
        tokio::time::timeout(Duration::from_millis(200), service.handle(get("/slow"))).await;
    assert!(abandoned.is_err(), "the caller gave up");

    let next = tokio::time::timeout(Duration::from_secs(10), text(&service, get("/fast")))
        .await
        .expect("served once the abandoned request is done");
    assert_eq!(next, "done");
}

/// A handler that never responds ends the service cleanly and promptly:
/// either the event loop runs dry, or the budget runs out.
#[tokio::test(flavor = "multi_thread")]
async fn handler_that_never_responds_ends_cleanly() {
    let limits = Limits::default().timeout(Duration::from_secs(1));

    let dry = serve(
        &SANDBOX,
        "host.serve(() => new Promise(() => {}))",
        Some(limits.clone()),
    )
    .await;
    let answer = tokio::time::timeout(Duration::from_secs(10), dry.handle(get("/")))
        .await
        .expect("returns");
    assert!(
        matches!(answer, Err(RunError::Sandbox(_))),
        "{:?}",
        answer.map(|r| r.status())
    );
    assert!(dry.finish().await.result.is_err());

    let waiting = serve(
        &SANDBOX,
        "host.serve(() => new Promise((resolve) => setTimeout(resolve, 60_000)))",
        Some(limits),
    )
    .await;
    let answer = tokio::time::timeout(Duration::from_secs(10), waiting.handle(get("/")))
        .await
        .expect("returns");
    assert!(
        matches!(answer, Err(RunError::Timeout(_))),
        "{:?}",
        answer.map(|r| r.status())
    );
    assert!(waiting.handle(get("/")).await.is_err());
    assert!(matches!(
        waiting.finish().await.result,
        Err(RunError::Timeout(_))
    ));
}

/// Handlers that misbehave without trapping yield 500s and the service
/// carries on.
#[tokio::test(flavor = "multi_thread")]
async fn misbehaving_handlers_yield_500() {
    let service = serve(
        &SANDBOX,
        r#"host.serve((request) => {
            const path = new URL(request.url).pathname;
            if (path === "/string") return "not a response";
            if (path === "/null") return null;
            if (path === "/reject") return Promise.reject(42);
            if (path === "/hostile") throw { toJSON() { throw 1; }, toString() { throw 2; } };
            if (path === "/status") return new Response("x", { status: 299 });
            return new Response("fine");
        })"#,
        None,
    )
    .await;
    for path in ["/string", "/null", "/reject", "/hostile"] {
        let answer = service.handle_buffered(get(path), MAX_BODY).await.unwrap();
        assert_eq!(answer.status(), 500, "{path}");
    }
    let status = service
        .handle_buffered(get("/status"), MAX_BODY)
        .await
        .unwrap();
    assert_eq!(status.status(), 299);
    assert_eq!(text(&service, get("/ok")).await, "fine");

    let twice = serve(
        &SANDBOX,
        r#"addEventListener("fetch", (event) => {
            event.respondWith(new Response("first"));
            event.respondWith(new Response("second"));
        })"#,
        None,
    )
    .await;
    for _ in 0..2 {
        let answer = twice.handle_buffered(get("/"), MAX_BODY).await.unwrap();
        assert_eq!(answer.status(), 500);
    }
}

// ===========================================================================
// Service: streaming and sharing
// ===========================================================================

/// `handle` returns once the head is set; the first chunk is readable while
/// the handler is still producing the rest.
#[tokio::test(flavor = "multi_thread")]
async fn responses_stream_while_the_handler_runs() {
    let service = serve(
        &SANDBOX,
        r#"host.serve(() => {
            const encoder = new TextEncoder();
            let step = 0;
            return new Response(new ReadableStream({
                async pull(controller) {
                    if (step === 0) controller.enqueue(encoder.encode("first;"));
                    else if (step === 1) {
                        await new Promise((resolve) => setTimeout(resolve, 300));
                        controller.enqueue(encoder.encode("second"));
                    } else controller.close();
                    step += 1;
                },
            }));
        })"#,
        None,
    )
    .await;

    let started = Instant::now();
    let response = service.handle(get("/")).await.unwrap();
    let mut body = response.into_body();
    let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
    let first_at = started.elapsed();
    assert_eq!(first, "first;");
    assert!(
        first_at < Duration::from_millis(250),
        "the first chunk took {first_at:?}"
    );

    let rest = body.collect().await.unwrap().to_bytes();
    assert_eq!(rest, "second");
    assert!(started.elapsed() >= Duration::from_millis(300));
}

/// A reader that drops the body early does not break later requests.
#[tokio::test(flavor = "multi_thread")]
async fn dropping_a_response_body_early_is_harmless() {
    let service = serve(
        &SANDBOX,
        r#"host.serve((request) => {
            if (request.url.endsWith("/small")) return new Response("small");
            let sent = 0;
            return new Response(new ReadableStream({
                pull(controller) {
                    if (sent >= 8 * 1024 * 1024) { controller.close(); return; }
                    controller.enqueue(new Uint8Array(64 * 1024).fill(1));
                    sent += 64 * 1024;
                },
            }));
        })"#,
        None,
    )
    .await;
    for _ in 0..5 {
        let mut body = service.handle(get("/big")).await.unwrap().into_body();
        let _ = body.frame().await;
        drop(body);
        assert_eq!(text(&service, get("/small")).await, "small");
    }
}

/// `handle_buffered` refuses a body over its cap; the service carries on.
#[tokio::test(flavor = "multi_thread")]
async fn handle_buffered_caps_the_body() {
    let service = serve(
        &SANDBOX,
        "host.serve((request) => new Response('y'.repeat(request.url.endsWith('/big') ? 100_000 : 10)))",
        None,
    )
    .await;
    let over = service.handle_buffered(get("/big"), 50_000).await;
    assert!(
        matches!(&over, Err(RunError::Sandbox(m)) if m.contains("50000")),
        "{:?}",
        over.map(|r| r.status())
    );
    let under = service
        .handle_buffered(get("/small"), 50_000)
        .await
        .unwrap();
    assert_eq!(under.body().len(), 10);
}

/// Large bodies in both directions, streamed.
#[tokio::test(flavor = "multi_thread")]
async fn large_bodies_both_ways() {
    let service = serve(
        &SANDBOX,
        r#"host.serve(async (request) => {
            if (request.method === "POST") {
                const body = new Uint8Array(await request.arrayBuffer());
                let sum = 0; for (const b of body) sum = (sum + b) % 65521;
                return Response.json({ length: body.length, sum });
            }
            let sent = 0;
            return new Response(new ReadableStream({
                pull(controller) {
                    if (sent >= 8 * 1024 * 1024) { controller.close(); return; }
                    controller.enqueue(new Uint8Array(64 * 1024).fill(7));
                    sent += 64 * 1024;
                },
            }));
        })"#,
        None,
    )
    .await;

    let upload: Vec<u8> = (0..3 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    let sum = upload.iter().fold(0u32, |s, &b| (s + b as u32) % 65521);
    let post = http::Request::builder()
        .method("POST")
        .uri("http://service.local/upload")
        .body(codelib::egress::full_body(upload.clone()))
        .unwrap();
    let answer = service.handle_buffered(post, MAX_BODY).await.unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(answer.body()).unwrap(),
        json!({ "length": upload.len(), "sum": sum })
    );

    let download = service.handle(get("/download")).await.unwrap();
    let body = download.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body.len(), 8 * 1024 * 1024);
    assert!(body.iter().all(|&b| b == 7));
}

/// One `Arc<Service>` used from many tasks at once answers every request.
#[tokio::test(flavor = "multi_thread")]
async fn a_shared_service_answers_concurrent_callers() {
    let service = Arc::new(
        serve(
            &SANDBOX,
            r#"let count = 0;
            host.serve(async (request) => {
                count += 1;
                await new Promise((resolve) => setTimeout(resolve, 5));
                return new Response(new URL(request.url).pathname.slice(1));
            })"#,
            None,
        )
        .await,
    );
    let mut tasks = Vec::new();
    for n in 0..24 {
        let service = service.clone();
        tasks.push(tokio::spawn(async move {
            let response = service
                .handle_buffered(get(&format!("/{n}")), MAX_BODY)
                .await
                .unwrap();
            assert_eq!(response.body(), &n.to_string());
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    let service = Arc::try_unwrap(service).ok().expect("the only handle left");
    assert!(service.finish().await.result.is_ok());
}

/// `with_state` and `console` reach into the running service between
/// requests; `finish` hands the state back.
#[tokio::test(flavor = "multi_thread")]
async fn state_and_console_between_requests() {
    let service = JOURNAL
        .script(
            r#"host.serve(() => {
                console.log("handled");
                return new Response(String(host.call("note", 1)));
            })"#,
        )
        .serve_with(vec!["start".into()])
        .await
        .unwrap_or_else(|outcome| panic!("{:?}", outcome.result));
    let response = service.handle(get("/")).await.unwrap();
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        "1"
    );
    let seen = service.with_state(|state| {
        state.push("between".into());
        state.len()
    });
    assert_eq!(seen.await.unwrap(), 3);
    assert!(service.console().await.unwrap().stdout.contains("handled"));
    let outcome = service.finish().await;
    assert_eq!(outcome.state, ["start", "note:1", "between"]);
}

// ===========================================================================
// Runs: memory, fuel, panics
// ===========================================================================

/// A program that recovers from a refused allocation and returns a value is
/// a success; the stats record that the cap was hit.
#[tokio::test(flavor = "multi_thread")]
async fn caught_out_of_memory_is_a_success_with_the_hit_recorded() {
    let outcome = SANDBOX
        .script(format!(
            "try {{ {HOG} }} catch (e) {{ return 'recovered: ' + String(e); }}"
        ))
        .limits(oom_limits())
        .run()
        .await;
    assert!(outcome.result.is_ok(), "{:?}", outcome.result);
    assert!(outcome.stats.memory_limit_hit);

    let clean = SANDBOX.run("return 1").await;
    assert!(!clean.stats.memory_limit_hit);
}

/// A fuel budget on a sandbox built without fuel metering is refused, not
/// silently ignored; with metering on, the budget is enforced.
#[tokio::test(flavor = "multi_thread")]
async fn per_run_fuel_budget_is_enforced_or_refused() {
    let fuel = Limits::default()
        .fuel(50_000_000)
        .timeout(Duration::from_secs(20));

    let refused = JOURNAL
        .script("while (true) {}")
        .limits(fuel.clone())
        .run_with(vec!["mine".into()])
        .await;
    assert!(
        matches!(&refused.result, Err(RunError::Sandbox(m)) if m.contains("fuel")),
        "{:?}",
        refused.result
    );
    assert_eq!(refused.state, ["mine"]);

    let metered = Sandbox::<()>::builder()
        .fuel_metering(true)
        .build()
        .expect("the sandbox builds");
    let outcome = metered.script("while (true) {}").limits(fuel).run().await;
    assert!(
        matches!(outcome.result, Err(RunError::OutOfFuel)),
        "{:?}",
        outcome.result
    );
    assert!(outcome.stats.fuel_used.unwrap() >= 50_000_000);
    assert_eq!(metered.run("return 2").await.result.unwrap(), json!(2));
}

/// A panic in a host call handler (async or synchronous) or in the emit sink
/// ends the run with a sandbox error, and the state is handed back.
#[tokio::test(flavor = "multi_thread")]
async fn panics_in_host_code_hand_the_state_back() {
    let outcome = JOURNAL
        .script("host.call('panic', 1); return 'unreachable'")
        .run_with(vec!["before".to_string()])
        .await;
    assert!(
        matches!(&outcome.result, Err(RunError::Sandbox(m)) if m == "a host call panicked: the journal is on fire"),
        "{:?}",
        outcome.result
    );
    assert_eq!(outcome.state, ["before", "panic:1"]);

    let sync = Sandbox::<u32>::builder()
        .host_calls(host_fn(|count: &mut u32, _, _| {
            *count += 1;
            panic!("synchronous handler panicked")
        }))
        .build()
        .unwrap();
    let outcome = sync.script("host.call('x')").run_with(41).await;
    assert!(
        matches!(&outcome.result, Err(RunError::Sandbox(m)) if m.contains("synchronous handler panicked")),
        "{:?}",
        outcome.result
    );
    assert_eq!(outcome.state, 42);

    let outcome = JOURNAL
        .script("host.emit('x', 1); return 'unreachable'")
        .on_emit(|_, _| panic!("the sink broke"))
        .run_with(vec!["kept".to_string()])
        .await;
    assert!(
        matches!(&outcome.result, Err(RunError::Sandbox(m)) if m.contains("the sink broke")),
        "{:?}",
        outcome.result
    );
    assert_eq!(outcome.state, ["kept"]);
}

/// `run_blocking` inside a tokio runtime returns an error with the state
/// instead of panicking.
#[tokio::test(flavor = "multi_thread")]
async fn run_blocking_inside_a_runtime_is_an_error() {
    let outcome = JOURNAL
        .script("return 1")
        .run_blocking_with(vec!["mine".into()]);
    assert!(
        matches!(&outcome.result, Err(RunError::Sandbox(m)) if m.contains("run_blocking")),
        "{:?}",
        outcome.result
    );
    assert_eq!(outcome.state, ["mine"]);
}

/// `timeout` applies on top of `limits`, in either order.
#[tokio::test(flavor = "multi_thread")]
async fn timeout_and_limits_compose_in_either_order() {
    LazyLock::force(&SANDBOX);
    let limits = Limits::default().timeout(Duration::from_secs(60));
    for script in [
        SANDBOX
            .script("while (true) {}")
            .timeout(Duration::from_millis(300))
            .limits(limits.clone()),
        SANDBOX
            .script("while (true) {}")
            .limits(limits.clone())
            .timeout(Duration::from_millis(300)),
    ] {
        let started = Instant::now();
        let outcome = script.run().await;
        assert!(matches!(
            outcome.result,
            Err(RunError::Timeout(t)) if t == Duration::from_millis(300)
        ));
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}

// ===========================================================================
// Runs: hostile programs and odd values
// ===========================================================================

/// Whatever a program throws comes back as a script error: error reporting
/// cannot be made to throw.
#[tokio::test(flavor = "multi_thread")]
async fn hostile_thrown_values_are_still_script_errors() {
    let programs = [
        "throw { toJSON() { throw 1; }, toString() { throw 2; } };",
        "const { proxy, revoke } = Proxy.revocable({}, {}); revoke(); throw proxy;",
        "const e = new Error('x'); Object.defineProperty(e, 'message', { get() { throw 3; } }); throw e;",
        "const e = new Error('x'); Object.defineProperty(e, 'name', { get() { throw 4; } }); throw e;",
        "const e = new Error('x'); Object.defineProperty(e, 'stack', { get() { throw 5; } }); throw e;",
        "Object.defineProperty(Error, Symbol.hasInstance, { value() { throw 6; } }); throw new Error('x');",
        "globalThis.String = () => { throw 7; }; JSON.stringify = () => { throw 8; }; throw { a: 1 };",
        "throw Object.create(null);",
        "throw Symbol('s');",
        "throw 10n;",
    ];
    let mut wrong = Vec::new();
    for program in programs {
        let outcome = SANDBOX.run(program).await;
        if !matches!(outcome.result, Err(RunError::Script(_))) {
            wrong.push(format!("{program} => {:?}", outcome.result));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// Source names cannot inject code: line terminators and other control
/// characters in a name are neutralised.
#[tokio::test(flavor = "multi_thread")]
async fn source_names_cannot_inject_code() {
    for separator in ["\u{2028}", "\u{2029}", "\r", "\n", "\u{0b}", "\u{0c}"] {
        let outcome = SANDBOX
            .script("globalThis.ran = true")
            .name(format!("report.js{separator}return 'injected'"))
            .run()
            .await;
        assert_eq!(
            outcome.result.ok(),
            Some(Value::Null),
            "separator {separator:?}"
        );
    }
}

/// A return value nested more deeply than the host's JSON parser accepts is
/// a sandbox error that says so; such an emitted item arrives as its raw
/// JSON text.
#[tokio::test(flavor = "multi_thread")]
async fn deeply_nested_values() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let outcome = SANDBOX
        .script("let v = 0; for (let i = 0; i < 200; i++) v = [v]; host.emit('deep', v); return v;")
        .on_emit(move |_, item| sink.lock().unwrap().push(item))
        .run()
        .await;
    assert!(
        matches!(&outcome.result, Err(RunError::Sandbox(m)) if m.contains("nested")),
        "{:?}",
        outcome.result
    );
    let expected = format!("{}0{}", "[".repeat(200), "]".repeat(200));
    assert_eq!(seen.lock().unwrap().as_slice(), [Value::String(expected)]);
}

/// `host.call` / `host.emit` with arguments JSON cannot represent: the
/// script gets a catchable error (or `null`), the run is not lost.
#[tokio::test(flavor = "multi_thread")]
async fn host_call_argument_edge_cases() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let outcome = JOURNAL
        .script(
            r#"
            const results = {};
            const cyclic = {}; cyclic.self = cyclic;
            const cases = {
                undef: undefined, fn: () => 1, sym: Symbol("s"), big: 1n, cyclic,
                nan: NaN, large: "z".repeat(4 * 1024 * 1024),
            };
            for (const [name, args] of Object.entries(cases)) {
                try {
                    const back = host.call("echo", args);
                    results[name] = typeof back === "string" ? back.length : back;
                } catch (e) { results[name] = "threw " + e.name; }
            }
            for (const [name, item] of Object.entries({ fn: () => 1, sym: Symbol("s"), big: 1n })) {
                try { host.emit(name, item); } catch (e) { results["emit-" + name] = "threw " + e.name; }
            }
            return results;
            "#,
        )
        .on_emit(move |kind, item| sink.lock().unwrap().push((kind.to_string(), item)))
        .run_with(Vec::new())
        .await;
    let results = outcome
        .result
        .unwrap_or_else(|e| panic!("the run was lost: {e:?}"));
    assert_eq!(results["undef"], Value::Null);
    assert_eq!(results["nan"], Value::Null);
    assert_eq!(results["large"], json!(4 * 1024 * 1024));
    assert_eq!(results["big"], json!("threw TypeError"));
    assert_eq!(results["cyclic"], json!("threw TypeError"));
    // `JSON.stringify` gives `undefined` for these; the bindings refuse it
    // with a catchable TypeError rather than trapping.
    for name in ["fn", "sym", "emit-fn", "emit-sym", "emit-big"] {
        assert_eq!(results[name], json!("threw TypeError"), "{name}");
    }
    assert!(seen.lock().unwrap().is_empty());
    assert_eq!(
        outcome.state.len(),
        3,
        "undef, nan and large reached the host"
    );
}

/// A script tampering with the globals the wrapper uses can only change its
/// own result, never the limits or the classification of a timeout.
#[tokio::test(flavor = "multi_thread")]
async fn tampered_globals_do_not_fool_limits() {
    let outcome = SANDBOX
        .script(
            r#"
            JSON.stringify = () => '"forged"';
            Object.prototype.then = undefined;
            Promise.prototype.then = function () { return this; };
            Error.prototype.toString = () => { throw 1; };
            while (true) {}
            "#,
        )
        .timeout(Duration::from_millis(500))
        .run()
        .await;
    assert!(
        matches!(outcome.result, Err(RunError::Timeout(_))),
        "{:?}",
        outcome.result
    );

    let outcome = SANDBOX
        .script(format!("JSON.stringify = () => '\"fine\"'; {HOG}"))
        .limits(oom_limits())
        .run()
        .await;
    assert!(
        matches!(outcome.result, Err(RunError::OutOfMemory(_))),
        "{:?}",
        outcome.result
    );
}

// ===========================================================================
// Runs: deadlines, cancellation, isolation, console
// ===========================================================================

/// Dropping a run's future mid-run (the caller's own timeout) neither panics
/// nor affects later runs. (The state is necessarily lost with the future.)
#[tokio::test(flavor = "multi_thread")]
async fn dropped_run_futures_are_harmless() {
    LazyLock::force(&JOURNAL);
    for program in [
        "host.call('sleep', null)",
        "while (true) {}",
        "await new Promise((r) => setTimeout(r, 5000))",
    ] {
        let dropped = tokio::time::timeout(
            Duration::from_millis(300),
            JOURNAL.script(program).run_with(Vec::new()),
        )
        .await;
        assert!(dropped.is_err(), "{program}");
    }
    let outcome = JOURNAL
        .script("return host.call('echo', 5)")
        .run_with(Vec::new())
        .await;
    assert_eq!(outcome.result.unwrap(), json!(5));
}

/// Blocked in a host call that does not answer: the run times out promptly
/// and the state comes back.
#[tokio::test(flavor = "multi_thread")]
async fn timeout_while_blocked_in_a_host_call_hands_state_back() {
    LazyLock::force(&JOURNAL);
    let started = Instant::now();
    let outcome = JOURNAL
        .script("host.call('sleep', null); return 'late'")
        .timeout(Duration::from_millis(500))
        .run_with(vec!["before".into()])
        .await;
    assert!(
        matches!(outcome.result, Err(RunError::Timeout(_))),
        "{:?}",
        outcome.result
    );
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(outcome.state, ["before", "sleep:null"]);
}

/// Blocked in a `fetch` whose server never answers: the run times out
/// promptly.
#[tokio::test(flavor = "multi_thread")]
async fn timeout_while_blocked_in_fetch() {
    LazyLock::force(&SANDBOX);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket); // accept, never answer
        }
    });
    let egress = Egress::builder()
        .allow(Rule::host("127.0.0.1").https_only(false))
        .deny_private_ips(false)
        .build();
    let started = Instant::now();
    let outcome = SANDBOX
        .script("await fetch(`http://${host.input}/`); return 'late'")
        .input(&address)
        .egress(egress)
        .timeout(Duration::from_millis(800))
        .run()
        .await;
    assert!(
        matches!(outcome.result, Err(RunError::Timeout(_))),
        "{:?}",
        outcome.result
    );
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
}

/// A timed-out run gives its `max_concurrent` slot back.
#[tokio::test(flavor = "multi_thread")]
async fn timeout_frees_the_concurrency_slot() {
    let sandbox = Sandbox::<()>::builder()
        .max_concurrent(1)
        .build()
        .expect("the sandbox builds");
    let outcome = sandbox
        .script("while (true) {}")
        .timeout(Duration::from_millis(300))
        .run()
        .await;
    assert!(matches!(outcome.result, Err(RunError::Timeout(_))));
    let next = tokio::time::timeout(Duration::from_secs(10), sandbox.run("return 1"))
        .await
        .expect("the slot is free again");
    assert_eq!(next.result.unwrap(), json!(1));
}

/// `run_blocking` (a current-thread runtime) interrupts a spinning program.
#[test]
fn run_blocking_times_out() {
    LazyLock::force(&SANDBOX);
    let started = Instant::now();
    let outcome = SANDBOX
        .script("while (true) {}")
        .timeout(Duration::from_millis(400))
        .run_blocking();
    assert!(matches!(outcome.result, Err(RunError::Timeout(_))));
    assert!(started.elapsed() < Duration::from_secs(5));
}

/// Overlapping runs see neither each other's globals, state, nor emit sink.
#[tokio::test(flavor = "multi_thread")]
async fn overlapping_runs_are_isolated() {
    let mut runs = Vec::new();
    for n in 0..6 {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let run = tokio::spawn(
            JOURNAL
                .script(
                    r#"
                    const me = host.input;
                    if (globalThis.owner !== undefined) throw new Error("saw " + globalThis.owner);
                    globalThis.owner = me;
                    for (let i = 0; i < 5; i++) {
                        host.emit("tick", me);
                        host.call("note", me);
                        await new Promise((resolve) => setTimeout(resolve, 20));
                    }
                    return globalThis.owner;
                    "#,
                )
                .input(n)
                .on_emit(move |_, item| sink.lock().unwrap().push(item))
                .run_with(vec![format!("run {n}")]),
        );
        runs.push((n, seen, run));
    }
    for (n, seen, run) in runs {
        let outcome = run.await.unwrap();
        assert_eq!(outcome.result.unwrap(), json!(n));
        assert_eq!(*seen.lock().unwrap(), vec![json!(n); 5]);
        assert_eq!(outcome.state.len(), 6);
        assert!(outcome.state[1..].iter().all(|s| *s == format!("note:{n}")));
    }
}

/// Console text is complete and in order up to the cap, and an exact prefix
/// beyond it.
#[tokio::test(flavor = "multi_thread")]
async fn console_is_complete_up_to_the_cap() {
    let program = "for (let i = 0; i < 5000; i++) console.log('line ' + i); return 'done'";
    let expected: String = (0..5000).map(|i| format!("line {i}\n")).collect();

    let outcome = SANDBOX.run(program).await;
    assert_eq!(outcome.result.unwrap(), json!("done"));
    assert_eq!(outcome.console.stdout, expected);
    assert!(!outcome.console.truncated);

    let outcome = SANDBOX
        .script(program)
        .limits(Limits::default().console(1000))
        .run()
        .await;
    assert_eq!(outcome.result.unwrap(), json!("done"));
    assert!(outcome.console.truncated);
    assert_eq!(outcome.console.stdout, expected[..1000]);
}
