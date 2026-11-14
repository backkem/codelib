//! clock-p3: a WASI 0.3 command (`wasi:cli/run@0.3.0`, an `async func`) for codelib-web's
//! browser tests. Everything it prints comes from the host's clocks and entropy, so two runs in
//! one seeded virtual world must print the same bytes, and a run in the real world must show
//! real time passing.
//!
//! Arguments (`wasi:cli/environment@0.3.0#get-arguments`, the first is the program name):
//!
//! - `probe [MS]` (the default): print the clocks, random numbers and the environment, wait on
//!   two concurrent timers, then sleep `MS` milliseconds (default 10 000) on the monotonic clock
//!   and print how long that took on it. A line goes to stderr at the end.
//! - `spin`: print `spinning`, then compute forever without yielding (for deadline tests).
//! - `fail`: print a line to stderr and return `Err(())`.
//!
//! Output goes through p3 `stream<u8>`s on `wasi:cli/stdout@0.3.0` and `stderr@0.3.0`.

#![cfg_attr(not(target_family = "wasm"), allow(dead_code))]

use std::fmt::Write as _;
use std::future::{Future, IntoFuture as _};
use std::pin::pin;
use std::task::Poll;

use wasip3::cli::{environment, stderr, stdout};
use wasip3::clocks::{monotonic_clock, system_clock};
use wasip3::random::{insecure, insecure_seed, random};

#[cfg(target_family = "wasm")]
wasip3::cli::command::export!(ClockP3);

struct ClockP3;

impl wasip3::exports::cli::run::Guest for ClockP3 {
    async fn run() -> Result<(), ()> {
        let args = environment::get_arguments();
        let mode = args.get(1).map(String::as_str).unwrap_or("probe");
        match mode {
            "spin" => {
                print(Out::Stdout, "spinning\n".into()).await?;
                spin()
            }
            "fail" => {
                print(Out::Stderr, "failing on purpose\n".into()).await?;
                Err(())
            }
            _ => {
                let ms = args.get(2).and_then(|s| s.parse::<u64>().ok()).unwrap_or(10_000);
                probe(&args, ms).await
            }
        }
    }
}

async fn probe(args: &[String], ms: u64) -> Result<(), ()> {
    let mut out = String::new();
    let t0 = monotonic_clock::now();
    let wall = system_clock::now();
    let _ = writeln!(out, "t0={t0} wall={}.{:09}", wall.seconds, wall.nanoseconds);
    let (s0, s1) = insecure_seed::get_insecure_seed();
    let _ = writeln!(
        out,
        "random={:#018x} insecure={:#018x} seed={s0:#x},{s1:#x}",
        random::get_random_u64(),
        insecure::get_insecure_random_u64(),
    );
    let _ = writeln!(out, "bytes={}", hex(&random::get_random_bytes(16)));
    let env: Vec<String> = environment::get_environment()
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    let _ = writeln!(out, "args={} env={}", args.join(","), env.join(","));

    // Two concurrent waits: the shorter one must finish first.
    let (slow, fast) = join(timed(30_000_000), timed(10_000_000)).await;
    let _ = writeln!(out, "fast=+{} slow=+{}", fast - t0, slow - t0);

    let before = monotonic_clock::now();
    monotonic_clock::wait_for(ms * 1_000_000).await;
    let _ = writeln!(out, "slept={}", monotonic_clock::now() - before);
    print(Out::Stdout, out).await?;
    print(Out::Stderr, "done\n".into()).await
}

#[inline(never)]
fn spin() -> ! {
    let mut x: u64 = 1;
    loop {
        x = std::hint::black_box(x.wrapping_mul(6364136223846793005).wrapping_add(1));
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

enum Out {
    Stdout,
    Stderr,
}

/// Writes `text` to stdout or stderr and waits until the host has taken all of it.
async fn print(to: Out, text: String) -> Result<(), ()> {
    let (mut tx, rx) = wasip3::wit_stream::new();
    let done = match to {
        Out::Stdout => stdout::write_via_stream(rx),
        Out::Stderr => stderr::write_via_stream(rx),
    };
    let ((), result) = join(
        async move {
            let rest = tx.write_all(text.into_bytes()).await;
            debug_assert!(rest.is_empty());
        },
        done.into_future(),
    )
    .await;
    result.map_err(|_| ())
}

/// Waits `ns` nanoseconds and returns the monotonic time observed afterwards.
async fn timed(ns: u64) -> u64 {
    monotonic_clock::wait_for(ns).await;
    monotonic_clock::now()
}

/// A minimal two-way join, so the fixture needs no executor crate.
async fn join<A: Future, B: Future>(a: A, b: B) -> (A::Output, B::Output) {
    let (mut a, mut b) = (pin!(a), pin!(b));
    let (mut ra, mut rb) = (None, None);
    std::future::poll_fn(move |cx| {
        if ra.is_none()
            && let Poll::Ready(v) = a.as_mut().poll(cx)
        {
            ra = Some(v);
        }
        if rb.is_none()
            && let Poll::Ready(v) = b.as_mut().poll(cx)
        {
            rb = Some(v);
        }
        match (ra.take(), rb.take()) {
            (Some(x), Some(y)) => Poll::Ready((x, y)),
            (x, y) => {
                (ra, rb) = (x, y);
                Poll::Pending
            }
        }
    })
    .await
}
