//! Bring your own component: the host kit without the JavaScript engine.
//!
//! `codelib::sandbox` hosts any WebAssembly component with any imports. Here
//! it runs a tiny hand-written component with one host import, and shows the
//! deadline catching a guest that never returns.
//!
//! ```text
//! cargo run --example custom_world
//! ```

use std::time::Duration;

use codelib::sandbox::{Error, Limits, Runtime, SessionOptions};

/// A component in the text format: `square-sum` calls the host's `log`
/// import, `spin` loops forever. A real embedder would compile this from
/// Rust, C, Go, ... against its own WIT world.
const COMPONENT: &str = r#"
(component
  (import "log" (func $log (param "value" u32)))
  (core func $log_core (canon lower (func $log)))
  (core module $m
    (import "host" "log" (func $log (param i32)))
    (func (export "square-sum") (param i32 i32) (result i32)
      (local $sum i32)
      (local.set $sum (i32.add
        (i32.mul (local.get 0) (local.get 0))
        (i32.mul (local.get 1) (local.get 1))))
      (call $log (local.get $sum))
      (local.get $sum))
    (func (export "spin")
      (loop br 0)))
  (core instance $i (instantiate $m
    (with "host" (instance (export "log" (func $log_core))))))
  (func (export "square-sum") (param "a" u32) (param "b" u32) (result u32)
    (canon lift (core func $i "square-sum")))
  (func (export "spin")
    (canon lift (core func $i "spin"))))
"#;

/// Per-session state the host import writes to.
#[derive(Default)]
struct Log(Vec<u32>);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = Runtime::<Log>::builder()
        .component_bytes(COMPONENT.as_bytes())
        .without_wasi()
        .limits(Limits::default().timeout(Duration::from_millis(500)))
        // Host imports are linked against `Ctx<Log>`; `ctx.state` is the Log.
        .link(|linker| {
            linker
                .root()
                .func_wrap("log", |mut store, (value,): (u32,)| {
                    store.data_mut().state.0.push(value);
                    Ok(())
                })
        })
        .build()?;

    let mut session = runtime.session(SessionOptions::default()).await?;
    let sum = session
        .call(async |store, instance| {
            let square_sum =
                instance.get_typed_func::<(u32, u32), (u32,)>(&mut *store, "square-sum")?;
            let (sum,) = square_sum.call_async(&mut *store, (3, 4)).await?;
            Ok(sum)
        })
        .await?;
    println!(
        "square-sum(3, 4) = {sum}; the host logged {:?}",
        session.state().0
    );

    // A guest that never returns is interrupted when its budget runs out.
    let spinning = session
        .call(async |store, instance| {
            let spin = instance.get_typed_func::<(), ()>(&mut *store, "spin")?;
            spin.call_async(&mut *store, ()).await
        })
        .await;
    match spinning {
        Err(Error::Timeout(after)) => println!("spin() was interrupted after {after:?}"),
        other => println!("unexpected: {other:?}"),
    }

    Ok(())
}
