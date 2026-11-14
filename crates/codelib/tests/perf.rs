//! Per-call cost of `host.call` must not depend on how many calls the run
//! has already made.
//!
//! Regression tests for the engine glue retaining every import argument
//! until the run ends: each later call then scanned the whole retained list,
//! so a run's cost grew quadratically with its number of host calls, and the
//! retained argument bytes counted against the memory limit. The guest build
//! fixes that in `guest/patch-bindings.mjs`.

use std::time::{Duration, Instant};

use codelib::{Failure, Limits, Sandbox, host_fn};

const CHUNK: usize = 5_000;
const CHUNKS: usize = 8;

/// Counts host calls and notes the time at every chunk boundary.
#[derive(Default)]
struct Marks {
    calls: usize,
    at: Vec<Instant>,
}

fn sandbox(limits: Limits) -> Sandbox<Marks> {
    Sandbox::<Marks>::builder()
        .limits(limits)
        .host_calls(host_fn(|marks: &mut Marks, method, _args| {
            if marks.calls.is_multiple_of(CHUNK) {
                marks.at.push(Instant::now());
            }
            marks.calls += 1;
            match method {
                "ok" => Ok("1".to_string()),
                other => Err(Failure::unknown_method(other)),
            }
        }))
        .build()
        .expect("the sandbox builds")
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

#[tokio::test(flavor = "multi_thread")]
async fn host_call_cost_stays_flat_over_a_run() {
    let sandbox = sandbox(Limits::default().timeout(Duration::from_secs(300)));
    let total = CHUNK * CHUNKS;
    let code = format!(
        "let n = 0; for (let i = 0; i < {total}; i++) n += host.call('ok', {{ i }}); \
         host.call('ok', null); return n"
    );

    let outcome = sandbox.script(code).run_with(Marks::default()).await;
    assert_eq!(outcome.result.unwrap(), serde_json::json!(total));

    let marks = outcome.state;
    assert_eq!(marks.calls, total + 1);
    assert_eq!(marks.at.len(), CHUNKS + 1);
    let chunks: Vec<Duration> = marks.at.windows(2).map(|w| w[1] - w[0]).collect();
    let shown: Vec<String> = chunks.iter().map(|d| format!("{:.1}", ms(*d))).collect();
    eprintln!("ms per {CHUNK} host calls: {}", shown.join(" "));

    // The fastest of the first chunks against the fastest of the last ones:
    // a busy machine slows single chunks, rarely all of them. With per-call
    // cost growing with earlier calls the last chunks take several times
    // as long as the first; flat, the ratio is about 1.
    let first = chunks[..3].iter().min().unwrap();
    let last = chunks[CHUNKS - 3..].iter().min().unwrap();
    let ratio = last.as_secs_f64() / first.as_secs_f64();
    assert!(
        ratio < 2.0,
        "host calls got slower as the run went on: the last chunks took {ratio:.1}x \
         as long as the first ({:.1} ms vs {:.1} ms per {CHUNK} calls; all: {})",
        ms(*last),
        ms(*first),
        shown.join(" "),
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn host_call_arguments_are_not_retained_until_the_run_ends() {
    // 3000 calls with 64 KiB of arguments each: 192 MiB in total, well over
    // the 128 MiB limit if the run kept them all.
    let sandbox = sandbox(
        Limits::default()
            .memory(128 * 1024 * 1024)
            .timeout(Duration::from_secs(300)),
    );
    let code = "const big = 'x'.repeat(64 * 1024); \
                for (let i = 0; i < 3000; i++) host.call('ok', big); return 'done'";

    let outcome = sandbox.script(code).run_with(Marks::default()).await;
    assert_eq!(outcome.result.unwrap(), serde_json::json!("done"));
    assert_eq!(outcome.state.calls, 3000);
}
