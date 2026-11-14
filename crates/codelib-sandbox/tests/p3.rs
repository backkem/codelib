//! The kit with a real WASI 0.3 component: `fixtures/p3-guest.wasm`, built
//! from `fixtures/p3-guest` by its `build.sh`. Each test mirrors a 0.2 test
//! in `kit.rs` or `hardening.rs` where there is one, so the guarantees the
//! kit gives 0.2 guests are pinned for 0.3 guests too.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use codelib_sandbox::codelib_http::{Egress, Rule, StaticCredential, StatusCode, text_response};
use codelib_sandbox::wasmtime::component::{Accessor, Instance};
use codelib_sandbox::{
    Console, Ctx, Error, Limits, Runtime, RuntimeBuilder, Session, SessionOptions,
};
use codelib_sandbox::{WasiConfig, wasmtime};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

mod bindings {
    codelib_sandbox::wasmtime::component::bindgen!({
        path: "tests/fixtures/p3-guest/wit",
        world: "fixture",
        wasmtime_crate: codelib_sandbox::wasmtime,
        // Every export is called through an `Accessor`, inside
        // `Session::call_concurrent`.
        exports: { default: async | store },
    });
}

use bindings::{Fixture, Outgoing};

const GUEST: &[u8] = include_bytes!("fixtures/p3-guest.wasm");

/// What the `double-later` host import records.
#[derive(Default)]
struct State {
    calls: u32,
    in_flight: u32,
    max_in_flight: u32,
}

fn builder() -> RuntimeBuilder<State> {
    Runtime::<State>::builder()
        .component_bytes(GUEST)
        .no_cache()
        .link(|linker| {
            linker.root().func_wrap_concurrent(
                "double-later",
                |accessor: &Accessor<Ctx<State>>, (ms, x): (u32, u32)| {
                    Box::pin(async move {
                        accessor.with(|mut access| {
                            let state = &mut access.get().state;
                            state.calls += 1;
                            state.in_flight += 1;
                            state.max_in_flight = state.max_in_flight.max(state.in_flight);
                        });
                        // A host import that really suspends.
                        tokio::time::sleep(Duration::from_millis(ms.into())).await;
                        accessor.with(|mut access| access.get().state.in_flight -= 1);
                        Ok((x * 2,))
                    })
                },
            )
        })
}

/// Compiling the component takes a few seconds in a debug build; tests that
/// need no special runtime configuration share one.
fn shared() -> Runtime<State> {
    static RUNTIME: OnceLock<Runtime<State>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| builder().build().expect("the fixture builds"))
        .clone()
}

fn runtime(limits: Limits) -> Runtime<State> {
    builder()
        .limits(limits)
        .build()
        .expect("the fixture builds")
}

async fn start(runtime: &Runtime<State>) -> Session<State> {
    runtime.session(SessionOptions::default()).await.unwrap()
}

/// The fixture's typed exports, for use inside `call_concurrent`.
fn guest(accessor: &Accessor<Ctx<State>>, instance: &Instance) -> wasmtime::Result<Fixture> {
    accessor.with(|mut store| Fixture::new(&mut store, instance))
}

async fn spin(session: &mut Session<State>) -> Result<(), Error> {
    session
        .call_concurrent(async |accessor, instance| {
            guest(accessor, instance)?.call_spin(accessor).await
        })
        .await
}

async fn sleep(session: &mut Session<State>, ms: u64) -> Result<u64, Error> {
    session
        .call_concurrent(async move |accessor, instance| {
            guest(accessor, instance)?.call_sleep(accessor, ms).await
        })
        .await
}

async fn echo(session: &mut Session<State>, text: &str) -> Result<String, Error> {
    let text = text.to_string();
    session
        .call_concurrent(async move |accessor, instance| {
            guest(accessor, instance)?.call_echo(accessor, text).await
        })
        .await
}

async fn allocate(session: &mut Session<State>, bytes: u64, fallible: bool) -> Result<bool, Error> {
    session
        .call_concurrent(async move |accessor, instance| {
            guest(accessor, instance)?
                .call_allocate(accessor, bytes, fallible)
                .await
        })
        .await
}

async fn print(session: &mut Session<State>, out: &str, err: &str) -> Result<(), Error> {
    let (out, err) = (out.to_string(), err.to_string());
    session
        .call_concurrent(async move |accessor, instance| {
            guest(accessor, instance)?
                .call_print(accessor, out, err)
                .await
        })
        .await
}

async fn fetch(
    session: &mut Session<State>,
    request: Outgoing,
) -> Result<Result<(u16, String), String>, Error> {
    session
        .call_concurrent(async move |accessor, instance| {
            let response = guest(accessor, instance)?
                .call_fetch(accessor, request)
                .await?;
            Ok(response.map(|r| (r.status, String::from_utf8_lossy(&r.body).into_owned())))
        })
        .await
}

fn get(scheme: &str, authority: &str, path: &str) -> Outgoing {
    Outgoing {
        method: "GET".into(),
        scheme: scheme.into(),
        authority: authority.into(),
        path: path.into(),
        headers: Vec::new(),
        body: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Calls
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn values_round_trip() {
    let mut session = start(&shared()).await;
    assert_eq!(
        echo(&mut session, "hello, 0.3").await.unwrap(),
        "hello, 0.3"
    );

    let sum = session
        .call_concurrent(async |accessor, instance| {
            guest(accessor, instance)?.call_add(accessor, 40, 2).await
        })
        .await
        .unwrap();
    assert_eq!(sum, 42);
    assert!(!session.is_poisoned());
}

/// `Session::call` (no `Accessor`) runs a 0.3 component's exports too, the
/// async ones included, one at a time.
#[tokio::test(flavor = "multi_thread")]
async fn session_call_runs_async_and_sync_exports() {
    let mut session = start(&shared()).await;
    let (echoed, sum, doubled) = session
        .call(async |store, instance| {
            let echo = instance.get_typed_func::<(&str,), (String,)>(&mut *store, "echo")?;
            let add = instance.get_typed_func::<(u32, u32), (u32,)>(&mut *store, "add")?;
            let twice =
                instance.get_typed_func::<(u32, u32), (u32,)>(&mut *store, "twice-later")?;
            let (echoed,) = echo.call_async(&mut *store, ("sync path",)).await?;
            let (sum,) = add.call_async(&mut *store, (1, 2)).await?;
            let (doubled,) = twice.call_async(&mut *store, (20, 21)).await?;
            Ok((echoed, sum, doubled))
        })
        .await
        .unwrap();
    assert_eq!((echoed.as_str(), sum, doubled), ("sync path", 3, 42));
    assert_eq!(session.finish().state.calls, 1);
}

/// While the guest awaits the host import, the guest is suspended rather
/// than blocked: another export call in the same session completes first.
#[tokio::test(flavor = "multi_thread")]
async fn async_host_imports_suspend_the_guest() {
    let mut session = start(&shared()).await;
    let started = Instant::now();
    let (doubled, echoed_after) = session
        .call_concurrent(async |accessor, instance| {
            let fixture = guest(accessor, instance)?;
            let (doubled, echoed) =
                tokio::join!(fixture.call_twice_later(accessor, 1000, 21), async {
                    let echoed = fixture.call_echo(accessor, "meanwhile".into()).await;
                    (echoed, started.elapsed())
                },);
            assert_eq!(echoed.0?, "meanwhile");
            Ok((doubled?, echoed.1))
        })
        .await
        .unwrap();
    assert_eq!(doubled, 42);
    assert!(started.elapsed() >= Duration::from_millis(1000));
    assert!(
        echoed_after < Duration::from_millis(700),
        "the echo waited for the import: {echoed_after:?}"
    );
    assert_eq!(session.finish().state.calls, 1);
}

/// Several export calls run at once in one session: four calls that each
/// wait 1 s on the host, and three that sleep 1 s on the guest's
/// monotonic clock, finish in about 1 s rather than 7 s.
#[tokio::test(flavor = "multi_thread")]
async fn export_calls_run_concurrently() {
    let mut session = start(&shared()).await;
    let started = Instant::now();
    let results = session
        .call_concurrent(async |accessor, instance| {
            let fixture = guest(accessor, instance)?;
            let (a, b, c, d, e, f, g) = tokio::join!(
                fixture.call_twice_later(accessor, 1000, 1),
                fixture.call_twice_later(accessor, 1000, 2),
                fixture.call_twice_later(accessor, 1000, 3),
                fixture.call_twice_later(accessor, 1000, 4),
                fixture.call_sleep(accessor, 1000),
                fixture.call_sleep(accessor, 1000),
                fixture.call_sleep(accessor, 1000),
            );
            e?;
            f?;
            g?;
            Ok([a?, b?, c?, d?])
        })
        .await
        .unwrap();
    let elapsed = started.elapsed();
    assert_eq!(results, [2, 4, 6, 8]);
    assert!(elapsed >= Duration::from_millis(1000), "{elapsed:?}");
    assert!(
        elapsed < Duration::from_millis(3000),
        "ran one by one: {elapsed:?}"
    );
    let state = session.finish().state;
    assert_eq!((state.calls, state.max_in_flight), (4, 4));
}

// ---------------------------------------------------------------------------
// Deadlines and cancellation
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_spinning_guest_times_out_and_poisons_the_session() {
    let mut session = start(&shared()).await;

    // Only the spinning call gets the short budget; starting the session does not.
    session.set_timeout(Duration::from_millis(300));
    let started = Instant::now();
    let result = spin(&mut session).await;
    let elapsed = started.elapsed();
    assert!(matches!(result, Err(Error::Timeout(_))), "got {result:?}");
    assert!(
        elapsed >= Duration::from_millis(250),
        "too early: {elapsed:?}"
    );
    assert!(elapsed < Duration::from_secs(10), "too late: {elapsed:?}");

    assert!(session.is_poisoned());
    assert!(matches!(
        echo(&mut session, "x").await,
        Err(Error::Poisoned)
    ));
}

/// A guest asleep on the 0.3 monotonic clock runs no code the epoch deadline
/// could interrupt; the wall-clock backstop ends the call, and the state
/// still comes back.
#[tokio::test(flavor = "multi_thread")]
async fn a_guest_sleeping_on_the_monotonic_clock_times_out() {
    let mut session = start(&shared()).await;
    assert_eq!(
        session
            .call_concurrent(async |accessor, instance| {
                guest(accessor, instance)?
                    .call_twice_later(accessor, 1, 5)
                    .await
            })
            .await
            .unwrap(),
        10
    );

    // The short budget starts after the warm-up call, which has to succeed.
    session.set_timeout(Duration::from_millis(300));
    let started = Instant::now();
    let result = sleep(&mut session, 60_000).await;
    let elapsed = started.elapsed();
    assert!(matches!(result, Err(Error::Timeout(_))), "got {result:?}");
    assert!(elapsed < Duration::from_secs(10), "took {elapsed:?}");
    assert!(session.is_poisoned());
    assert_eq!(session.finish().state.calls, 1);
}

/// Dropping the call's future mid-call poisons the session, whether the
/// guest was computing or suspended on the host.
#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_call_poisons_the_session() {
    let runtime = runtime(Limits::default().timeout(Duration::from_secs(30)));

    let mut spinning = start(&runtime).await;
    let cancelled = tokio::time::timeout(Duration::from_millis(200), spin(&mut spinning)).await;
    assert!(cancelled.is_err(), "the outer timeout fires first");
    assert!(spinning.is_poisoned());
    assert!(matches!(
        echo(&mut spinning, "x").await,
        Err(Error::Poisoned)
    ));
    drop(spinning.finish());

    let mut sleeping = start(&runtime).await;
    let cancelled =
        tokio::time::timeout(Duration::from_millis(200), sleep(&mut sleeping, 60_000)).await;
    assert!(cancelled.is_err(), "the outer timeout fires first");
    assert!(sleeping.is_poisoned());
    assert!(matches!(
        echo(&mut sleeping, "x").await,
        Err(Error::Poisoned)
    ));
}

/// An error returned from the closure poisons the session too: export calls
/// it started may still be in flight.
#[tokio::test(flavor = "multi_thread")]
async fn an_error_from_the_closure_poisons_the_session() {
    let mut session = start(&shared()).await;
    let result = session
        .call_concurrent(async |_, _| -> wasmtime::Result<()> {
            Err(wasmtime::format_err!("the embedder gave up"))
        })
        .await;
    assert!(matches!(&result, Err(Error::Trap(e)) if e.to_string().contains("gave up")));
    assert!(session.is_poisoned());
}

/// On a current-thread executor a spinning 0.3 guest yields at every epoch
/// tick, inside the concurrent event loop too, so other tasks keep running.
#[tokio::test(flavor = "current_thread")]
async fn a_spinning_guest_does_not_starve_a_current_thread_executor() {
    let runtime = shared();
    let spinner = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            let mut session = start(&runtime).await;
            // The budget covers the spinning call only, not starting the
            // session; a starved executor would show as a latency of about
            // this long.
            session.set_timeout(Duration::from_secs(3));
            spin(&mut session).await
        }
    });
    tokio::task::yield_now().await;

    let started = Instant::now();
    tokio::time::sleep(Duration::from_millis(10)).await;
    let latency = started.elapsed();
    assert!(
        latency < Duration::from_secs(2),
        "a 10ms sleep took {latency:?} next to a spinning guest"
    );
    assert!(matches!(spinner.await.unwrap(), Err(Error::Timeout(_))));
}

// ---------------------------------------------------------------------------
// Memory and fuel
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn memory_past_the_limit_is_refused_and_recorded() {
    const MIB: u64 = 1024 * 1024;
    let runtime = runtime(Limits::default().memory(8 * MIB as usize));
    let mut session = start(&runtime).await;

    assert!(allocate(&mut session, MIB, true).await.unwrap());
    assert!(!session.memory_limit_hit());

    // Refused, and the guest carries on: a success the host can recognise.
    assert!(!allocate(&mut session, 64 * MIB, true).await.unwrap());
    assert!(session.memory_limit_hit());
    assert!(matches!(session.out_of_memory(), Error::OutOfMemory(n) if n == 8 * MIB as usize));

    assert!(allocate(&mut session, MIB, true).await.unwrap());
    assert!(
        !session.memory_limit_hit(),
        "cleared when the next call began"
    );
    assert!(session.stats().memory_limit_hit, "the session remembers it");

    // A guest that cannot cope aborts, and that is reported as out of memory.
    let result = allocate(&mut session, 64 * MIB, false).await;
    assert!(
        matches!(result, Err(Error::OutOfMemory(_))),
        "got {result:?}"
    );
    assert!(session.is_poisoned());
}

/// The fuel budget is per call, like the time budget, and shared by every
/// export call started within it.
#[tokio::test(flavor = "multi_thread")]
async fn fuel_budget_is_per_call() {
    let runtime = builder().fuel_metering(true).build().unwrap();
    let mut session = runtime
        .session(SessionOptions::default().limits(Limits::default().fuel(2_000_000)))
        .await
        .unwrap();

    let before = session.stats().fuel_used.unwrap();
    for _ in 0..5 {
        assert_eq!(echo(&mut session, "cheap").await.unwrap(), "cheap");
    }
    let used = session.stats().fuel_used.unwrap() - before;
    assert!(used > 0, "calls are metered");

    assert!(matches!(spin(&mut session).await, Err(Error::OutOfFuel)));
    let total = session.stats().fuel_used.unwrap();
    assert!(
        total >= before + used + 1_990_000,
        "the spinning call used its whole tank: {total}"
    );
}

// ---------------------------------------------------------------------------
// WASI 0.3 through the session's WASI context
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn stdout_and_stderr_are_captured_and_capped() {
    let mut session = start(&shared()).await;
    print(&mut session, "to stdout\n", "to stderr\n")
        .await
        .unwrap();
    let console = session.console();
    assert_eq!(console.stdout, "to stdout\n");
    assert_eq!(console.stderr, "to stderr\n");
    assert!(!console.truncated);

    let runtime = runtime(Limits::default().console(16));
    let mut session = start(&runtime).await;
    let long = "x".repeat(100);
    print(&mut session, &long, "").await.unwrap();
    let console = session.console();
    assert_eq!(console.stdout, "x".repeat(16));
    assert!(console.truncated);
    assert!(!session.is_poisoned(), "a chatty guest keeps running");
}

#[tokio::test(flavor = "multi_thread")]
async fn frozen_clocks_and_seeded_randomness_repeat_across_runs() {
    let mut wasi = WasiConfig::deterministic(7);
    wasi.env = vec![("MODE".into(), "test".into())];
    wasi.args = vec!["fixture".into(), "--flag".into()];

    async fn observe(runtime: &Runtime<State>) -> bindings::Observation {
        let mut session = start(runtime).await;
        session
            .call_concurrent(async |accessor, instance| {
                guest(accessor, instance)?.call_observe(accessor).await
            })
            .await
            .unwrap()
    }

    let seeded = builder().wasi(wasi.clone()).build().unwrap();
    let first = observe(&seeded).await;
    assert_eq!(first.wall_seconds, 1_767_225_600);
    assert_eq!(first.wall_nanoseconds, 0);
    assert_eq!(first.monotonic, 0);
    assert_eq!(first.env, wasi.env);
    assert_eq!(first.args, wasi.args);
    assert_eq!(first.random.len(), 16);

    // Another session, and another runtime, see exactly the same.
    let again = observe(&seeded).await;
    let rebuilt = observe(&builder().wasi(wasi.clone()).build().unwrap()).await;
    for other in [&again, &rebuilt] {
        assert_eq!(other.random, first.random);
        assert_eq!(other.insecure, first.insecure);
        assert_eq!(other.insecure_seed, first.insecure_seed);
        assert_eq!(other.wall_seconds, first.wall_seconds);
    }

    // A different seed gives different bytes; the real clocks and the OS
    // generator are not frozen.
    let other_seed = observe(
        &builder()
            .wasi(WasiConfig::deterministic(8))
            .build()
            .unwrap(),
    )
    .await;
    assert_ne!(other_seed.random, first.random);
    let real = observe(&shared()).await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    assert!(
        (real.wall_seconds - now).abs() < 60,
        "{}",
        real.wall_seconds
    );
    assert!(
        real.env.is_empty() && real.args.is_empty(),
        "nothing by default"
    );
}

/// With frozen clocks the guest's monotonic clock stands still, but its
/// timers still wait in real time.
#[tokio::test(flavor = "multi_thread")]
async fn a_frozen_monotonic_clock_still_waits() {
    let runtime = builder()
        .wasi(WasiConfig::deterministic(1))
        .build()
        .unwrap();
    let mut session = start(&runtime).await;
    let started = Instant::now();
    assert_eq!(sleep(&mut session, 100).await.unwrap(), 0);
    assert!(started.elapsed() >= Duration::from_millis(100));

    let mut real = start(&shared()).await;
    assert!(sleep(&mut real, 100).await.unwrap() >= 100_000_000);
}

/// No filesystem and no sockets: the 0.3 interfaces are linked, and refuse.
#[tokio::test(flavor = "multi_thread")]
async fn filesystem_and_sockets_are_sealed() {
    let mut session = start(&shared()).await;
    let sealed = session
        .call_concurrent(async |accessor, instance| {
            guest(accessor, instance)?.call_probe(accessor).await
        })
        .await
        .unwrap();
    assert_eq!(sealed.preopens, 0);
    assert!(
        sealed.tcp_connect.contains("AccessDenied"),
        "{}",
        sealed.tcp_connect
    );
    // wasmtime reports a disallowed lookup as a resolver failure; a real
    // lookup of `localhost` would have succeeded.
    assert!(
        sealed.name_lookup.contains("PermanentResolverFailure"),
        "{}",
        sealed.name_lookup
    );
}

// ---------------------------------------------------------------------------
// Outgoing HTTP through the session's egress
// ---------------------------------------------------------------------------

/// A tiny HTTP/1.1 server that answers with what it received:
/// `<method> <path> auth=<authorization> body=<body>`. Returns `host:port`.
async fn echo_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 4096];
                let (head, mut body) = loop {
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                    }
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
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => body.extend_from_slice(&chunk[..n]),
                    }
                }
                let mut request_line = head.lines().next().unwrap_or_default().split(' ');
                let reply = format!(
                    "{} {} auth={} body={}",
                    request_line.next().unwrap_or_default(),
                    request_line.next().unwrap_or_default(),
                    header("authorization").unwrap_or_default(),
                    String::from_utf8_lossy(&body),
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
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
async fn http_is_denied_by_default_with_a_console_note() {
    let mut session = start(&shared()).await;
    let denied = fetch(&mut session, get("https", "example.com", "/secret?token=x"))
        .await
        .unwrap();
    assert!(
        matches!(&denied, Err(e) if e.contains("HttpRequestDenied")),
        "{denied:?}"
    );
    assert!(!session.is_poisoned(), "a denial is the guest's to handle");

    let console = session.console();
    assert!(
        console
            .stderr
            .contains("[codelib] fetch denied: GET https://example.com/secret"),
        "{}",
        console.stderr
    );
    assert!(!console.stderr.contains("token=x"), "{}", console.stderr);

    // A denied request with a body: the body the guest streams is drained,
    // so the denial is prompt and the guest's writer is not left hanging.
    let started = Instant::now();
    let post = Outgoing {
        method: "POST".into(),
        body: vec![b'y'; 300_000],
        ..get("https", "example.com", "/upload")
    };
    let denied = fetch(&mut session, post).await.unwrap();
    assert!(denied.is_err(), "{denied:?}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(session.stats().http_requests, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn allowed_requests_get_host_side_credentials() {
    let server = echo_server().await;
    let egress = Egress::builder()
        .allow(
            Rule::host("127.0.0.1")
                .https_only(false)
                .credential(StaticCredential::bearer("the-real-token")),
        )
        .deny_private_ips(false)
        .build();
    let mut session = shared()
        .session(SessionOptions::default().egress(egress))
        .await
        .unwrap();

    let request = Outgoing {
        method: "POST".into(),
        headers: vec![
            ("authorization".into(), "Bearer forged-by-the-guest".into()),
            // Spares the test server from decoding a chunked body.
            ("content-length".into(), "5".into()),
        ],
        body: b"hello".to_vec(),
        ..get("http", &server, "/things?x=1")
    };
    let (status, body) = fetch(&mut session, request).await.unwrap().unwrap();
    assert_eq!(status, 200);
    assert_eq!(
        body,
        "POST /things?x=1 auth=Bearer the-real-token body=hello"
    );

    // Hosts outside the rules stay denied.
    let denied = fetch(&mut session, get("http", "localhost:1", "/"))
        .await
        .unwrap();
    assert!(denied.is_err(), "{denied:?}");
    assert_eq!(session.stats().http_requests, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn virtual_hosts_answer_in_process() {
    let egress = Egress::builder()
        .virtual_host(
            "files.internal",
            |request: codelib_sandbox::codelib_http::Request| async move {
                text_response(StatusCode::OK, format!("served {}", request.uri().path()))
            },
        )
        .build();
    let mut session = shared()
        .session(SessionOptions::default().egress(egress))
        .await
        .unwrap();

    let (status, body) = fetch(&mut session, get("http", "files.internal", "/report.csv"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!((status, body.as_str()), (200, "served /report.csv"));
    assert_eq!(session.stats().http_requests, 1);
}

/// Requests from concurrent export calls all go through the egress and are
/// all counted.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_requests_are_all_counted() {
    let egress = Egress::builder()
        .virtual_host(
            "files.internal",
            |request: codelib_sandbox::codelib_http::Request| async move {
                text_response(StatusCode::OK, request.uri().path().to_string())
            },
        )
        .build();
    let mut session = shared()
        .session(SessionOptions::default().egress(egress))
        .await
        .unwrap();

    let (a, b, c) = session
        .call_concurrent(async |accessor, instance| {
            let fixture = guest(accessor, instance)?;
            let (a, b, c) = tokio::join!(
                fixture.call_fetch(accessor, get("http", "files.internal", "/a")),
                fixture.call_fetch(accessor, get("http", "files.internal", "/b")),
                fixture.call_fetch(accessor, get("https", "example.com", "/")),
            );
            Ok((a?, b?, c?))
        })
        .await
        .unwrap();
    assert_eq!(a.unwrap().body, b"/a");
    assert_eq!(b.unwrap().body, b"/b");
    assert!(c.is_err());
    assert_eq!(session.stats().http_requests, 3);
}

// ---------------------------------------------------------------------------
// Compile cache, and 0.2 components on the same kind of runtime
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_compile_cache_works_for_0_3_components() {
    let dir = tempfile::tempdir().unwrap();
    let build = || builder().cache_dir(dir.path()).build().unwrap();
    let entries = || {
        std::fs::read_dir(dir.path())
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|x| x == "cwasm")
            })
            .count()
    };

    let started = Instant::now();
    let cold = build();
    let cold_build = started.elapsed();
    assert_eq!(entries(), 1, "one cache entry is written");
    let started = Instant::now();
    let warm = build();
    let warm_build = started.elapsed();
    for runtime in [cold, warm] {
        let mut session = start(&runtime).await;
        assert_eq!(echo(&mut session, "cached").await.unwrap(), "cached");
    }
    assert_eq!(entries(), 1);
    // Loading the cached entry skips the compile; compared with the cold build
    // so the bound scales with the machine.
    assert!(
        warm_build < cold_build / 2,
        "warm {warm_build:?}, cold {cold_build:?}"
    );
}

/// A WASI 0.2 component (sync exports, a 0.2 import) on a runtime built with
/// the `p3` feature.
const COMPONENT_0_2: &str = r#"
(component
  (import "wasi:random/random@0.2.12" (instance $random
    (export "get-random-u64" (func (result u64)))))
  (core func $random_u64 (canon lower (func $random "get-random-u64")))
  (core module $m
    (import "wasi" "random-u64" (func $random_u64 (result i64)))
    (func (export "add") (param i32 i32) (result i32)
      local.get 0 local.get 1 i32.add)
    (func (export "roll") (result i64)
      call $random_u64))
  (core instance $i (instantiate $m
    (with "wasi" (instance (export "random-u64" (func $random_u64))))))
  (func (export "add") (param "a" u32) (param "b" u32) (result u32)
    (canon lift (core func $i "add")))
  (func (export "roll") (result u64)
    (canon lift (core func $i "roll"))))
"#;

#[tokio::test(flavor = "multi_thread")]
async fn a_p3_runtime_still_runs_0_2_components() {
    let runtime = Runtime::<()>::builder()
        .component_bytes(COMPONENT_0_2.as_bytes().to_vec())
        .no_cache()
        .wasi(WasiConfig::deterministic(3))
        .build()
        .unwrap();

    let roll = async |runtime: &Runtime<()>| {
        let mut session = runtime.session(SessionOptions::default()).await.unwrap();
        session
            .call(async |store, instance| {
                let add = instance.get_typed_func::<(u32, u32), (u32,)>(&mut *store, "add")?;
                let roll = instance.get_typed_func::<(), (u64,)>(&mut *store, "roll")?;
                assert_eq!(add.call_async(&mut *store, (2, 40)).await?, (42,));
                Ok(roll.call_async(&mut *store, ()).await?.0)
            })
            .await
            .unwrap()
    };
    assert_eq!(roll(&runtime).await, roll(&runtime).await, "seeded");
}

// ---------------------------------------------------------------------------
// The browser kit's `clock-p3` fixture, hosted natively
// ---------------------------------------------------------------------------

/// `web/test/fixtures/clock-p3.wasm`: a `wasi:cli/run@0.3.0` command that the
/// browser kit's tests run too, so both kits are checked against one component.
fn clock_runtime(wasi: WasiConfig, limits: Limits) -> Runtime<()> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../web/test/fixtures/clock-p3.wasm"
    );
    Runtime::<()>::builder()
        .component_bytes(std::fs::read(path).expect("the clock-p3 fixture is built"))
        .no_cache()
        .wasi(wasi)
        .limits(limits)
        .build()
        .expect("clock-p3 builds")
}

fn with_args(mut wasi: WasiConfig, args: &[&str]) -> WasiConfig {
    // Like a real command line, the first argument is the program name.
    wasi.args = std::iter::once("clock-p3")
        .chain(args.iter().copied())
        .map(String::from)
        .collect();
    wasi
}

/// Runs the `run` export; the inner result is the guest's own `ok`/`err`.
async fn run_command(session: &mut Session<()>) -> Result<Result<(), ()>, Error> {
    session
        .call_concurrent(async |accessor, instance| {
            let command = accessor.with(|mut store| {
                wasmtime_wasi::p3::bindings::Command::new(&mut store, instance)
            })?;
            command.wasi_cli_run().call_run(accessor).await
        })
        .await
}

async fn clock_run(wasi: WasiConfig, limits: Limits) -> (Result<Result<(), ()>, Error>, Console) {
    let runtime = clock_runtime(wasi, limits);
    let mut session = runtime.session(SessionOptions::default()).await.unwrap();
    let result = run_command(&mut session).await;
    (result, session.console())
}

/// The first line of a probe's output that starts with `prefix`.
fn line<'a>(stdout: &'a str, prefix: &str) -> &'a str {
    stdout
        .lines()
        .find(|l| l.starts_with(prefix))
        .unwrap_or_else(|| panic!("no `{prefix}` line in {stdout:?}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn clock_p3_probe_runs_to_completion() {
    let started = Instant::now();
    let (result, console) = clock_run(
        with_args(WasiConfig::default(), &["probe", "20"]),
        Limits::default(),
    )
    .await;
    assert_eq!(result.unwrap(), Ok(()));

    // The stable parts: the argument echo, a concurrent wait that ends the
    // shorter timer first, and a sleep of at least the requested time on the
    // real monotonic clock. The values themselves vary run to run.
    let out = &console.stdout;
    assert!(out.starts_with("t0="), "{out}");
    assert!(
        line(out, "args=").starts_with("args=clock-p3,probe,20 env="),
        "{out}"
    );
    assert!(line(out, "random=").contains("insecure=0x"), "{out}");
    assert_eq!(line(out, "bytes=").len(), "bytes=".len() + 32, "{out}");
    let number = |prefix: &str, line: &str| -> u64 {
        let rest = &line[line.find(prefix).unwrap() + prefix.len()..];
        rest.split(|c: char| !c.is_ascii_digit())
            .next()
            .unwrap()
            .parse()
            .unwrap()
    };
    let timers = line(out, "fast=");
    assert!(
        number("fast=+", timers) >= 10_000_000
            && number("fast=+", timers) < number("slow=+", timers),
        "{timers}"
    );
    assert!(number("slow=+", timers) >= 30_000_000, "{timers}");
    assert!(number("slept=", line(out, "slept=")) >= 20_000_000, "{out}");
    assert_eq!(console.stderr, "done\n");
    assert!(started.elapsed() >= Duration::from_millis(50));
}

/// Same seed, same bytes; another seed, other bytes. Every line of the probe
/// derives from the seeded generators, the frozen clocks or the arguments, so
/// the whole output is compared.
#[tokio::test(flavor = "multi_thread")]
async fn clock_p3_is_deterministic_under_a_seed() {
    let run = async |seed: u64| {
        let (result, console) = clock_run(
            with_args(WasiConfig::deterministic(seed), &["probe", "20"]),
            Limits::default(),
        )
        .await;
        assert_eq!(result.unwrap(), Ok(()));
        console
    };
    let (a, b, c) = (run(42).await, run(42).await, run(43).await);
    assert_eq!(a.stdout.as_bytes(), b.stdout.as_bytes());
    assert_eq!(a.stderr, b.stderr);
    assert_ne!(a.stdout, c.stdout);

    // Frozen clocks: the wall clock sits at 2026-01-01 and the monotonic
    // clock at zero, so the timers' elapsed times and the sleep read as zero
    // although they waited in real time.
    let out = &a.stdout;
    assert!(out.contains("t0=0 wall=1767225600.000000000"), "{out}");
    assert!(out.contains("args=clock-p3,probe,20 env=\n"), "{out}");
    assert!(out.contains("fast=+0 slow=+0"), "{out}");
    assert!(out.contains("slept=0\n"), "{out}");
    assert_ne!(line(out, "random="), line(&c.stdout, "random="));
    assert_ne!(line(out, "bytes="), line(&c.stdout, "bytes="));
}

#[tokio::test(flavor = "multi_thread")]
async fn clock_p3_spin_is_stopped_by_the_deadline() {
    let runtime = clock_runtime(
        with_args(WasiConfig::default(), &["spin"]),
        Limits::default(),
    );
    let mut session = runtime.session(SessionOptions::default()).await.unwrap();
    // Only the spinning run gets the short budget; starting the session does not.
    session.set_timeout(Duration::from_millis(300));
    let started = Instant::now();
    let result = run_command(&mut session).await;
    assert!(matches!(result, Err(Error::Timeout(_))), "got {result:?}");
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(session.is_poisoned());
    assert_eq!(session.console().stdout, "spinning\n");
}

/// `fail` returns `err` from `run`: a result the host reads, not a trap, so
/// the session stays usable.
#[tokio::test(flavor = "multi_thread")]
async fn clock_p3_fail_is_an_error_result_not_a_trap() {
    let runtime = clock_runtime(
        with_args(WasiConfig::default(), &["fail"]),
        Limits::default(),
    );
    let mut session = runtime.session(SessionOptions::default()).await.unwrap();
    assert_eq!(run_command(&mut session).await.unwrap(), Err(()));
    assert!(!session.is_poisoned());
    let console = session.console();
    assert_eq!(console.stderr, "failing on purpose\n");
    assert_eq!(console.stdout, "");
}
