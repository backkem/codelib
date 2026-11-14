//! The compile cache's location and eviction, with trivial components.

use std::fs::{self, File};
use std::path::Path;
use std::time::{Duration, SystemTime};

use codelib_sandbox::Runtime;

const DAY: Duration = Duration::from_secs(24 * 60 * 60);

/// A distinct component per `n`, so each gets its own cache entry.
fn component(n: u32) -> Vec<u8> {
    format!(r#"(component (core module $m (func (export "f") (result i32) i32.const {n})))"#)
        .into_bytes()
}

fn build(n: u32, dir: Option<&Path>) {
    let mut builder = Runtime::<()>::builder()
        .component_bytes(component(n))
        .without_wasi();
    if let Some(dir) = dir {
        builder = builder.cache_dir(dir);
    }
    builder.build().expect("builds");
}

fn files(dir: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn modified(path: &Path) -> SystemTime {
    fs::metadata(path).unwrap().modified().unwrap()
}

fn set_age(path: &Path, age: Duration) {
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(SystemTime::now() - age)
        .unwrap();
}

fn age_all(dir: &Path, age: Duration) {
    for name in files(dir) {
        set_age(&dir.join(name), age);
    }
}

fn entry(dir: &Path) -> std::path::PathBuf {
    fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|e| e == "cwasm"))
        .expect("an entry")
}

#[test]
fn loading_an_entry_refreshes_its_modified_time() {
    let dir = tempfile::tempdir().unwrap();
    build(1, Some(dir.path()));
    let path = entry(dir.path());
    set_age(&path, 10 * DAY);

    build(1, Some(dir.path()));
    let age = SystemTime::now().duration_since(modified(&path)).unwrap();
    assert!(age < DAY, "the entry is {age:?} old after a load");
}

#[test]
fn writing_a_new_entry_removes_entries_unused_for_30_days() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    build(1, Some(path));
    let old_entry = entry(path);
    let old_name = old_entry
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    fs::write(path.join("abandoned.cwasm.tmp-1-1"), b"partial").unwrap();
    fs::write(path.join("fresh.cwasm.tmp-1-2"), b"partial").unwrap();
    fs::write(path.join("notes.txt"), b"not ours").unwrap();
    for name in files(path) {
        if name != "fresh.cwasm.tmp-1-2" {
            set_age(&path.join(name), 31 * DAY);
        }
    }

    build(2, Some(path));

    let left = files(path);
    assert!(!left.contains(&old_name), "{left:?}");
    assert!(
        !left.contains(&"abandoned.cwasm.tmp-1-1".to_string()),
        "{left:?}"
    );
    assert!(
        left.contains(&"fresh.cwasm.tmp-1-2".to_string()),
        "{left:?}"
    );
    assert!(left.contains(&"notes.txt".to_string()), "{left:?}");
    // The new entry and its sidecar, plus the two files that stay.
    assert_eq!(left.len(), 4, "{left:?}");
}

#[test]
fn an_entry_that_is_still_loaded_is_not_evicted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    build(1, Some(path));
    let first = entry(path);
    age_all(path, 31 * DAY);

    build(1, Some(path)); // a load: refreshes the entry
    build(2, Some(path)); // a new entry: evicts what is stale
    assert!(first.exists());
}

#[test]
fn eviction_does_not_happen_on_a_cache_hit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    build(1, Some(path));
    fs::write(path.join("old.crc"), b"x").unwrap();
    set_age(&path.join("old.crc"), 31 * DAY);
    build(1, Some(path));
    assert!(path.join("old.crc").exists());
}

#[test]
fn the_environment_sets_the_default_directory_but_not_an_explicit_one() {
    let from_env = tempfile::tempdir().unwrap();
    let explicit = tempfile::tempdir().unwrap();
    // SAFETY: this is the only test in this binary that touches the
    // environment, and nothing else reads it concurrently except cache
    // lookups of the other tests, which use explicit directories.
    unsafe { std::env::set_var("CODELIB_CACHE_DIR", from_env.path()) };

    build(3, None);
    assert_eq!(files(from_env.path()).len(), 2);

    build(4, Some(explicit.path()));
    assert_eq!(files(explicit.path()).len(), 2);
    assert_eq!(files(from_env.path()).len(), 2);

    unsafe { std::env::remove_var("CODELIB_CACHE_DIR") };
}
