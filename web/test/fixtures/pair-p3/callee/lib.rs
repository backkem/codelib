//! The plug of the pair fixture: exports `test:pair/svc`.

#![cfg_attr(not(target_family = "wasm"), allow(dead_code))]

wit_bindgen::generate!({ path: "../svc.wit", world: "callee" });

struct Callee;

impl exports::test::pair::svc::Guest for Callee {
    async fn work(n: u32) -> u32 {
        // Each yield ends a slice, so a second concurrent call finds the instance busy.
        for _ in 0..3 {
            wit_bindgen::yield_async().await;
        }
        n * 2
    }

    fn ping(n: u32) -> u32 {
        n + 1
    }
}

#[cfg(target_family = "wasm")]
export!(Callee);
