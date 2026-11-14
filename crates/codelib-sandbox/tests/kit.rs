//! Exercises the kit with a tiny hand-written component: no JavaScript
//! engine, no WASI, one host import.

use std::time::{Duration, Instant};

use codelib_sandbox::{Error, Limits, Runtime, SessionOptions};

/// `add`, `twice` (calls the host import), `spin` (never returns), and
/// `grow` (asks for linear memory, returns the old size in pages or -1).
const COMPONENT: &str = r#"
(component
  (import "host-double" (func $double (param "x" u32) (result u32)))
  (core func $double_core (canon lower (func $double)))
  (core module $m
    (import "host" "double" (func $double (param i32) (result i32)))
    (memory (export "memory") 1)
    (func (export "add") (param i32 i32) (result i32)
      local.get 0 local.get 1 i32.add)
    (func (export "twice") (param i32) (result i32)
      local.get 0 call $double)
    (func (export "spin")
      (loop br 0))
    (func (export "grow") (param i32) (result i32)
      local.get 0 memory.grow))
  (core instance $i (instantiate $m
    (with "host" (instance (export "double" (func $double_core))))))
  (func (export "add") (param "a" u32) (param "b" u32) (result u32)
    (canon lift (core func $i "add")))
  (func (export "twice") (param "x" u32) (result u32)
    (canon lift (core func $i "twice")))
  (func (export "spin")
    (canon lift (core func $i "spin")))
  (func (export "grow") (param "pages" u32) (result s32)
    (canon lift (core func $i "grow"))))
"#;

/// State the host import counts calls in.
#[derive(Default)]
struct Calls(u32);

fn runtime(limits: Limits) -> Runtime<Calls> {
    Runtime::<Calls>::builder()
        .component_bytes(COMPONENT.as_bytes().to_vec())
        .no_cache()
        .without_wasi()
        .limits(limits)
        .link(|linker| {
            linker
                .root()
                .func_wrap_async("host-double", |mut store, (x,): (u32,)| {
                    Box::new(async move {
                        // A host import that really suspends.
                        tokio::time::sleep(Duration::from_millis(5)).await;
                        store.data_mut().state.0 += 1;
                        Ok((x * 2,))
                    })
                })
        })
        .build()
        .expect("the test component builds")
}

async fn call_u32(
    session: &mut codelib_sandbox::Session<Calls>,
    name: &'static str,
    arg: u32,
) -> Result<u32, Error> {
    session
        .call(async move |store, instance| {
            let func = instance.get_typed_func::<(u32,), (u32,)>(&mut *store, name)?;
            let (value,) = func.call_async(&mut *store, (arg,)).await?;
            Ok(value)
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn calls_exports_and_async_host_imports() {
    let runtime = runtime(Limits::default());
    let mut session = runtime.session(SessionOptions::default()).await.unwrap();

    let sum = session
        .call(async |store, instance| {
            let add = instance.get_typed_func::<(u32, u32), (u32,)>(&mut *store, "add")?;
            let (sum,) = add.call_async(&mut *store, (2, 40)).await?;
            Ok(sum)
        })
        .await
        .unwrap();
    assert_eq!(sum, 42);

    assert_eq!(call_u32(&mut session, "twice", 21).await.unwrap(), 42);
    assert_eq!(call_u32(&mut session, "twice", 4).await.unwrap(), 8);

    // The state travels in, is mutated by the host import, and comes back.
    let finished = session.finish();
    assert_eq!(finished.state.0, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_spinning_guest_times_out_and_poisons_the_session() {
    let runtime = runtime(Limits::default());
    let mut session = runtime.session(SessionOptions::default()).await.unwrap();

    // Only the spinning call gets the short budget; starting the session does not.
    session.set_timeout(Duration::from_millis(300));
    let started = Instant::now();
    let result = session
        .call(async |store, instance| {
            let spin = instance.get_typed_func::<(), ()>(&mut *store, "spin")?;
            spin.call_async(&mut *store, ()).await
        })
        .await;
    let elapsed = started.elapsed();

    assert!(matches!(result, Err(Error::Timeout(_))), "got {result:?}");
    assert!(
        elapsed >= Duration::from_millis(250),
        "interrupted too early: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "interrupted too late: {elapsed:?}"
    );

    assert!(session.is_poisoned());
    assert!(matches!(
        call_u32(&mut session, "twice", 1).await,
        Err(Error::Poisoned)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spinning_guests_do_not_starve_the_executor() {
    let runtime = runtime(Limits::default());

    // Twice as many spinning guests as this runtime has worker threads.
    let mut spinners = Vec::new();
    for _ in 0..4 {
        let runtime = runtime.clone();
        spinners.push(tokio::spawn(async move {
            let mut session = runtime.session(SessionOptions::default()).await.unwrap();
            // The budget covers the spinning call only, not starting the session.
            session.set_timeout(Duration::from_secs(5));
            session
                .call(async |store, instance| {
                    let spin = instance.get_typed_func::<(), ()>(&mut *store, "spin")?;
                    spin.call_async(&mut *store, ()).await
                })
                .await
        }));
    }

    // A well-behaved session still gets served while they spin: it is done
    // before any of them has used up its budget. Each spinner yields its
    // worker every epoch tick, so how long this takes depends on the number
    // of spinners per worker, not on how long they spin.
    let mut session = runtime.session(SessionOptions::default()).await.unwrap();
    assert_eq!(call_u32(&mut session, "twice", 3).await.unwrap(), 6);
    assert!(
        spinners.iter().all(|spinner| !spinner.is_finished()),
        "the well-behaved call only completed once the spinners were gone"
    );

    for spinner in spinners {
        assert!(matches!(spinner.await.unwrap(), Err(Error::Timeout(_))));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn memory_growth_past_the_limit_is_refused_and_recorded() {
    // One 64 KiB page to start with; allow four.
    let runtime = runtime(Limits::default().memory(4 * 65536));
    let mut session = runtime.session(SessionOptions::default()).await.unwrap();

    async fn grow(session: &mut codelib_sandbox::Session<Calls>, pages: u32) -> i32 {
        session
            .call(async move |store, instance| {
                let grow = instance.get_typed_func::<(u32,), (i32,)>(&mut *store, "grow")?;
                let (old,) = grow.call_async(&mut *store, (pages,)).await?;
                Ok(old)
            })
            .await
            .unwrap()
    }

    assert_eq!(
        grow(&mut session, 2).await,
        1,
        "growing within the limit succeeds"
    );
    assert!(!session.memory_limit_hit());

    assert_eq!(
        grow(&mut session, 100).await,
        -1,
        "growing past the limit is refused"
    );
    assert!(session.memory_limit_hit());
    assert!(matches!(session.out_of_memory(), Error::OutOfMemory(n) if n == 4 * 65536));
}

#[tokio::test(flavor = "multi_thread")]
async fn fuel_runs_out() {
    let runtime = runtime(Limits::default().fuel(50_000));
    let mut session = runtime.session(SessionOptions::default()).await.unwrap();

    let result = session
        .call(async |store, instance| {
            let spin = instance.get_typed_func::<(), ()>(&mut *store, "spin")?;
            spin.call_async(&mut *store, ()).await
        })
        .await;
    assert!(matches!(result, Err(Error::OutOfFuel)), "got {result:?}");
    assert!(session.stats().fuel_used.unwrap() >= 49_000);
}

#[tokio::test(flavor = "multi_thread")]
async fn max_concurrent_queues_sessions() {
    let runtime = Runtime::<Calls>::builder()
        .component_bytes(COMPONENT.as_bytes().to_vec())
        .no_cache()
        .without_wasi()
        .max_concurrent(1)
        .link(|linker| {
            linker
                .root()
                .func_wrap_async("host-double", |_store, (x,): (u32,)| {
                    Box::new(async move { Ok((x * 2,)) })
                })
        })
        .build()
        .unwrap();

    let first = runtime.session(SessionOptions::default()).await.unwrap();

    // The second session has to wait for the first to finish.
    let waiting = tokio::spawn({
        let runtime = runtime.clone();
        async move { runtime.session(SessionOptions::default()).await.map(|_| ()) }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!waiting.is_finished());

    drop(first);
    tokio::time::timeout(Duration::from_secs(30), waiting)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn compiled_components_are_cached_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let build = || {
        Runtime::<Calls>::builder()
            .component_bytes(COMPONENT.as_bytes().to_vec())
            .cache_dir(dir.path())
            .without_wasi()
            .link(|linker| {
                linker
                    .root()
                    .func_wrap_async("host-double", |_store, (x,): (u32,)| {
                        Box::new(async move { Ok((x * 2,)) })
                    })
            })
            .build()
            .unwrap()
    };

    // Entries are `.cwasm` files, each with a `.crc` checksum sidecar.
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

    let cold = build();
    assert_eq!(entries(), 1, "one cache entry is written");

    // The second build loads the entry; both runtimes work.
    let warm = build();
    for runtime in [cold, warm] {
        let mut session = runtime.session(SessionOptions::default()).await.unwrap();
        assert_eq!(call_u32(&mut session, "twice", 5).await.unwrap(), 10);
    }
    assert_eq!(entries(), 1);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
}
