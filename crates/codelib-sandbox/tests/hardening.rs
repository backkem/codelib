//! Hardening tests for the host kit: hand-written components, no JavaScript.
//! Deadlines, cancellation, memory and fuel accounting, and the compile
//! cache under concurrency and corruption.

use std::time::{Duration, Instant};

use codelib_sandbox::{Error, Limits, Runtime, Session, SessionOptions};

/// `spin` (never returns), `wait` (calls a host import that never answers),
/// `grow` (grows linear memory by n pages, returns the old size or -1),
/// `grow-table` (grows a funcref table, returns the old size or -1), `add`.
const COMPONENT: &str = r#"
(component
  (import "host-wait" (func $wait (result u32)))
  (core func $wait_core (canon lower (func $wait)))
  (core module $m
    (import "host" "wait" (func $wait (result i32)))
    (memory (export "memory") 1)
    (table $t 1 funcref)
    (func (export "spin")
      (loop br 0))
    (func (export "wait") (result i32)
      call $wait)
    (func (export "grow") (param i32) (result i32)
      local.get 0 memory.grow)
    (func (export "grow-table") (param i32) (result i32)
      ref.null func
      local.get 0
      table.grow $t)
    (func (export "add") (param i32 i32) (result i32)
      local.get 0 local.get 1 i32.add))
  (core instance $i (instantiate $m
    (with "host" (instance (export "wait" (func $wait_core))))))
  (func (export "spin")
    (canon lift (core func $i "spin")))
  (func (export "wait") (result u32)
    (canon lift (core func $i "wait")))
  (func (export "grow") (param "pages" u32) (result s32)
    (canon lift (core func $i "grow")))
  (func (export "grow-table") (param "n" u32) (result s32)
    (canon lift (core func $i "grow-table")))
  (func (export "add") (param "a" u32) (param "b" u32) (result u32)
    (canon lift (core func $i "add"))))
"#;

/// A component whose core module spins in its start function.
const SPINS_ON_START: &str = r#"
(component
  (core module $m
    (func $spin (loop br 0))
    (start $spin))
  (core instance $i (instantiate $m)))
"#;

#[derive(Default)]
struct State {
    waits: u32,
}

fn builder() -> codelib_sandbox::RuntimeBuilder<State> {
    Runtime::<State>::builder()
        .component_bytes(COMPONENT.as_bytes().to_vec())
        .no_cache()
        .without_wasi()
        .link(|linker| {
            linker
                .root()
                .func_wrap_async("host-wait", |mut store, (): ()| {
                    store.data_mut().state.waits += 1;
                    Box::new(async move {
                        // A host import that never answers.
                        std::future::pending::<()>().await;
                        Ok((0u32,))
                    })
                })
        })
}

fn runtime(limits: Limits) -> Runtime<State> {
    builder()
        .limits(limits)
        .build()
        .expect("the component builds")
}

async fn spin(session: &mut Session<State>) -> Result<(), Error> {
    session
        .call(async |store, instance| {
            let spin = instance.get_typed_func::<(), ()>(&mut *store, "spin")?;
            spin.call_async(&mut *store, ()).await
        })
        .await
}

async fn add(session: &mut Session<State>, a: u32, b: u32) -> Result<u32, Error> {
    session
        .call(async move |store, instance| {
            let add = instance.get_typed_func::<(u32, u32), (u32,)>(&mut *store, "add")?;
            let (sum,) = add.call_async(&mut *store, (a, b)).await?;
            Ok(sum)
        })
        .await
}

async fn grow(session: &mut Session<State>, export: &'static str, n: u32) -> Result<i32, Error> {
    session
        .call(async move |store, instance| {
            let grow = instance.get_typed_func::<(u32,), (i32,)>(&mut *store, export)?;
            let (old,) = grow.call_async(&mut *store, (n,)).await?;
            Ok(old)
        })
        .await
}

// ---------------------------------------------------------------------------
// Cancellation and deadlines
// ---------------------------------------------------------------------------

/// Dropping a call's future mid-guest (the caller's own timeout, `select!`,
/// a client disconnect) poisons the session: the instance was unwound
/// mid-call and further calls are refused.
#[tokio::test(flavor = "multi_thread")]
async fn cancelled_call_poisons_session() {
    let runtime = runtime(Limits::default().timeout(Duration::from_secs(30)));
    let mut session = runtime.session(SessionOptions::default()).await.unwrap();

    let cancelled = tokio::time::timeout(Duration::from_millis(200), spin(&mut session)).await;
    assert!(cancelled.is_err(), "the outer timeout fires first");

    let poisoned = session.is_poisoned();
    let next = add(&mut session, 1, 2).await;
    assert!(
        poisoned && matches!(next, Err(Error::Poisoned)),
        "is_poisoned() was {poisoned}, next call returned {next:?}"
    );
}

/// A successful call leaves the session usable.
#[tokio::test(flavor = "multi_thread")]
async fn successful_calls_do_not_poison() {
    let runtime = runtime(Limits::default());
    let mut session = runtime.session(SessionOptions::default()).await.unwrap();
    for n in 0..5 {
        assert_eq!(add(&mut session, n, 1).await.unwrap(), n + 1);
        assert!(!session.is_poisoned());
    }
}

/// Blocked in a host import that never answers: the wall-clock backstop ends
/// the call promptly, poisons the session, and the concurrency slot is
/// released once the session is finished.
#[tokio::test(flavor = "multi_thread")]
async fn timeout_in_a_blocked_host_import_is_prompt_and_frees_the_slot() {
    let runtime = builder().max_concurrent(1).build().unwrap();
    let mut session = runtime.session(SessionOptions::default()).await.unwrap();

    // Only the blocked call gets the short budget; starting the session does not.
    session.set_timeout(Duration::from_millis(300));
    let started = Instant::now();
    let result = session
        .call(async |store, instance| {
            let wait = instance.get_typed_func::<(), (u32,)>(&mut *store, "wait")?;
            let (value,) = wait.call_async(&mut *store, ()).await?;
            Ok(value)
        })
        .await;
    let elapsed = started.elapsed();
    assert!(matches!(result, Err(Error::Timeout(_))), "got {result:?}");
    assert!(elapsed < Duration::from_secs(10), "took {elapsed:?}");
    assert!(session.is_poisoned());

    let finished = session.finish();
    assert_eq!(
        finished.state.waits, 1,
        "the state comes back after a timeout"
    );

    let next = tokio::time::timeout(
        Duration::from_secs(30),
        runtime.session(SessionOptions::default()),
    )
    .await
    .expect("the slot of the timed-out session is free again");
    assert!(next.is_ok());
}

/// A component that spins while being instantiated is interrupted too, the
/// state comes back with the error, and the slot is released.
#[tokio::test(flavor = "multi_thread")]
async fn spinning_instantiation_times_out_and_hands_the_state_back() {
    let runtime = Runtime::<String>::builder()
        .component_bytes(SPINS_ON_START.as_bytes().to_vec())
        .no_cache()
        .without_wasi()
        .limits(Limits::default().timeout(Duration::from_millis(300)))
        .max_concurrent(1)
        .build()
        .unwrap();

    let started = Instant::now();
    let failed = runtime
        .session(SessionOptions::new("mine".to_string()))
        .await
        .err()
        .expect("instantiation fails");
    assert!(
        matches!(failed.error, Error::Timeout(_)),
        "{:?}",
        failed.error
    );
    assert_eq!(failed.state, "mine");
    assert!(started.elapsed() < Duration::from_secs(10));

    let again = tokio::time::timeout(
        Duration::from_secs(30),
        runtime.session(SessionOptions::new(String::new())),
    )
    .await;
    assert!(again.is_ok(), "the slot of a failed start is released");
}

/// On a current-thread runtime a spinning guest yields at every epoch tick,
/// so other tasks keep running.
#[tokio::test(flavor = "current_thread")]
async fn current_thread_runtime_is_not_starved() {
    let runtime = runtime(Limits::default());
    let spinner = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            let mut session = runtime.session(SessionOptions::default()).await.unwrap();
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

/// Nothing (in particular not the epoch thread) keeps the engine alive once
/// the runtime and its sessions are gone.
#[tokio::test(flavor = "multi_thread")]
async fn engine_is_released_when_the_runtime_is_dropped() {
    let runtime = runtime(Limits::default());
    let weak = runtime.engine().weak();
    let mut session = runtime.session(SessionOptions::default()).await.unwrap();
    assert_eq!(add(&mut session, 2, 3).await.unwrap(), 5);
    drop(session.finish());
    drop(runtime);

    // The epoch thread holds the engine for an instant on every tick.
    tokio::time::sleep(codelib_sandbox::DEFAULT_EPOCH_TICK * 3).await;
    assert!(weak.upgrade().is_none(), "something still holds the engine");
}

// ---------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------

/// Table growth counts against the memory cap (two pointers per element),
/// so `table.grow` cannot make the host allocate past it.
#[tokio::test(flavor = "multi_thread")]
async fn table_growth_is_capped_too() {
    let runtime = runtime(Limits::default().memory(1024 * 1024));
    let mut session = runtime.session(SessionOptions::default()).await.unwrap();

    // 16M elements is at least 128 MiB of host memory, under a 1 MiB cap.
    let grown = grow(&mut session, "grow-table", 16 * 1024 * 1024).await;
    assert!(matches!(grown, Ok(-1)), "got {grown:?}");
    assert!(session.memory_limit_hit());

    // Within the cap it still grows.
    assert_eq!(grow(&mut session, "grow-table", 100).await.unwrap(), 1);
}

/// The memory flag is per call: a later call that is refused nothing does
/// not report the earlier refusal, while the session's stats remember it.
#[tokio::test(flavor = "multi_thread")]
async fn memory_limit_flag_is_per_call() {
    let runtime = runtime(Limits::default().memory(4 * 65536));
    let mut session = runtime.session(SessionOptions::default()).await.unwrap();
    assert!(!session.stats().memory_limit_hit);

    assert_eq!(grow(&mut session, "grow", 100).await.unwrap(), -1);
    assert!(session.memory_limit_hit());

    assert_eq!(add(&mut session, 1, 1).await.unwrap(), 2);
    assert!(
        !session.memory_limit_hit(),
        "cleared when the next call began"
    );
    assert!(session.stats().memory_limit_hit, "the session remembers it");

    // A later timeout is a timeout, not out-of-memory.
    session.set_timeout(Duration::from_millis(200));
    assert!(matches!(spin(&mut session).await, Err(Error::Timeout(_))));
}

// ---------------------------------------------------------------------------
// Fuel
// ---------------------------------------------------------------------------

/// A session that asks for fuel on a runtime without fuel metering does not
/// start (instead of running unmetered), and the state comes back.
#[tokio::test(flavor = "multi_thread")]
async fn fuel_without_metering_is_refused() {
    let runtime = Runtime::<String>::builder()
        .component_bytes(SPINS_ON_START.as_bytes().to_vec())
        .no_cache()
        .without_wasi()
        .build()
        .unwrap();
    let failed = runtime
        .session(SessionOptions::new("mine".to_string()).limits(Limits::default().fuel(1000)))
        .await
        .err()
        .expect("a fuel budget without metering is refused");
    assert!(
        matches!(&failed.error, Error::Config(message) if message.contains("fuel")),
        "{:?}",
        failed.error
    );
    assert_eq!(failed.state, "mine");
}

/// With metering turned on at build time, a session can set its own budget.
/// The budget is per call, like the time budget, and the stats add up what
/// every call used.
#[tokio::test(flavor = "multi_thread")]
async fn fuel_budget_is_per_call() {
    let runtime = builder().fuel_metering(true).build().unwrap();
    let mut session = runtime
        .session(SessionOptions::default().limits(Limits::default().fuel(200)))
        .await
        .unwrap();

    // Many calls, each well inside the budget, together far beyond it.
    let before = session.stats().fuel_used.unwrap();
    for _ in 0..3000 {
        assert_eq!(add(&mut session, 1, 2).await.unwrap(), 3);
    }
    let used = session.stats().fuel_used.unwrap() - before;
    assert!(used > 200, "fuel use adds up across calls: {used}");

    assert!(matches!(spin(&mut session).await, Err(Error::OutOfFuel)));
    let total = session.stats().fuel_used.unwrap();
    assert!(
        total >= before + used + 190,
        "the spinning call used its whole tank: {total}"
    );
}

// ---------------------------------------------------------------------------
// Compile cache
// ---------------------------------------------------------------------------

fn cached_runtime(dir: &std::path::Path) -> Runtime<State> {
    Runtime::<State>::builder()
        .component_bytes(COMPONENT.as_bytes().to_vec())
        .cache_dir(dir)
        .without_wasi()
        .link(|linker| {
            linker
                .root()
                .func_wrap_async("host-wait", |_store, (): ()| {
                    Box::new(async move { Ok((7u32,)) })
                })
        })
        .build()
        .expect("builds despite the cache")
}

/// The one cache entry in `dir`, after checking that it has its checksum
/// sidecar and that no temporary files were left behind.
fn only_entry(dir: &std::path::Path) -> std::path::PathBuf {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    files.sort();
    assert_eq!(files.len(), 2, "one entry and its sidecar: {files:?}");
    let entry = files
        .iter()
        .find(|p| p.extension().is_some_and(|e| e == "cwasm"))
        .unwrap_or_else(|| panic!("no entry in {files:?}"))
        .clone();
    assert_eq!(files[0].with_extension("cwasm"), entry, "{files:?}");
    assert!(
        files
            .iter()
            .any(|p| p.extension().is_some_and(|e| e == "crc"))
    );
    entry
}

/// Truncated, garbage or empty entries are discarded and rewritten.
#[tokio::test(flavor = "multi_thread")]
async fn corrupted_cache_entries_are_recompiled() {
    let dir = tempfile::tempdir().unwrap();
    drop(cached_runtime(dir.path()));
    let entry = only_entry(dir.path());
    let good = std::fs::read(&entry).unwrap();

    for corrupt in [
        good[..good.len() / 2].to_vec(),
        vec![0xAB; 4096],
        Vec::new(),
    ] {
        std::fs::write(&entry, &corrupt).unwrap();
        let runtime = cached_runtime(dir.path());
        let mut session = runtime.session(SessionOptions::default()).await.unwrap();
        assert_eq!(add(&mut session, 20, 22).await.unwrap(), 42);
        assert_eq!(
            std::fs::read(only_entry(dir.path())).unwrap(),
            good,
            "the corrupted entry is replaced by a good one"
        );
    }
}

/// An entry whose body was damaged in place (same length, a few flipped
/// bytes) fails its checksum and is recompiled, never loaded.
#[tokio::test(flavor = "multi_thread")]
async fn damaged_cache_entry_body_is_detected() {
    let dir = tempfile::tempdir().unwrap();
    drop(cached_runtime(dir.path()));
    let entry = only_entry(dir.path());
    let good = std::fs::read(&entry).unwrap();

    let mut damaged = good.clone();
    let middle = damaged.len() / 2;
    for byte in &mut damaged[middle..middle + 16] {
        *byte ^= 0xFF;
    }
    std::fs::write(&entry, &damaged).unwrap();

    let runtime = cached_runtime(dir.path());
    let mut session = runtime.session(SessionOptions::default()).await.unwrap();
    assert_eq!(add(&mut session, 1, 2).await.unwrap(), 3);
    assert_eq!(std::fs::read(only_entry(dir.path())).unwrap(), good);
}

/// An entry without its sidecar (a writer that died in between) is not
/// trusted: it is recompiled and the sidecar written.
#[tokio::test(flavor = "multi_thread")]
async fn cache_entry_without_sidecar_is_recompiled() {
    let dir = tempfile::tempdir().unwrap();
    drop(cached_runtime(dir.path()));
    let entry = only_entry(dir.path());
    std::fs::remove_file(entry.with_extension("crc")).unwrap();
    std::fs::write(&entry, b"not a compiled component").unwrap();

    let runtime = cached_runtime(dir.path());
    let mut session = runtime.session(SessionOptions::default()).await.unwrap();
    assert_eq!(add(&mut session, 2, 2).await.unwrap(), 4);
    only_entry(dir.path());
}

/// Several threads of one process compiling the same component into the same
/// cache directory at once.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_cold_builds_in_one_process() {
    for _round in 0..3 {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || cached_runtime(&path))
            })
            .collect();
        for thread in threads {
            let runtime = thread.join().expect("a concurrent build must not panic");
            let mut session = runtime.session(SessionOptions::default()).await.unwrap();
            assert_eq!(add(&mut session, 1, 1).await.unwrap(), 2);
        }
        only_entry(dir.path());
        // A fresh build loads what was left behind.
        let runtime = cached_runtime(dir.path());
        let mut session = runtime.session(SessionOptions::default()).await.unwrap();
        assert_eq!(add(&mut session, 2, 2).await.unwrap(), 4);
    }
}
