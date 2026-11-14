//! The socket of the pair fixture: a WASI 0.3 command that calls `test:pair/svc` twice
//! concurrently (the second call finds the callee busy, so its start is deferred), then once
//! synchronously, and prints `work=2,4 ping=6`.

#![cfg_attr(not(target_family = "wasm"), allow(dead_code))]

use std::future::{Future, IntoFuture as _};
use std::pin::pin;
use std::task::Poll;

wit_bindgen::generate!({ path: "../svc.wit", world: "caller" });

use test::pair::svc;

#[cfg(target_family = "wasm")]
wasip3::cli::command::export!(Caller);

struct Caller;

impl wasip3::exports::cli::run::Guest for Caller {
    async fn run() -> Result<(), ()> {
        let (a, b) = join(svc::work(1), svc::work(2)).await;
        let p = svc::ping(5);
        let text = format!("work={a},{b} ping={p}\n");
        let (mut tx, rx) = wasip3::wit_stream::new();
        let done = wasip3::cli::stdout::write_via_stream(rx);
        let ((), result) = join(
            async move {
                tx.write_all(text.into_bytes()).await;
            },
            done.into_future(),
        )
        .await;
        result.map_err(|_| ())
    }
}

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
