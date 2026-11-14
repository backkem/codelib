//! On-disk cache of compiled components.
//!
//! Compiling a JavaScript engine to native code takes tens of seconds;
//! loading the result back takes milliseconds. Entries are keyed by the
//! sha256 of the component bytes plus wasmtime's own compatibility hash, so a
//! different component, wasmtime version, target or engine configuration
//! never loads a stale entry.
//!
//! Each entry `<key>.cwasm` has a sidecar `<key>.crc` holding the entry's
//! length and CRC-32, written after the entry. An entry is only loaded when
//! it matches its sidecar; anything else (a missing sidecar, a damaged or
//! half-written entry) is recompiled and rewritten. Entries are never
//! removed once they are older than [`MAX_AGE`] (an entry is touched every
//! time it is loaded, so only unused ones age). Removal happens when a new
//! entry is written, and a failure to remove anything is ignored.

use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};
use wasmtime::Engine;
use wasmtime::component::Component;

use crate::Error;

/// The environment variable that overrides the default cache directory.
const CACHE_DIR_VAR: &str = "CODELIB_CACHE_DIR";

/// Entries not loaded for this long are removed when a new one is written.
pub(crate) const MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Where compiled components are cached.
#[derive(Clone, Debug)]
pub(crate) enum CachePolicy {
    /// `$CODELIB_CACHE_DIR` if set and not empty, else
    /// `<OS cache dir>/codelib/compiled`, or no cache if the OS has none.
    Default,
    Dir(PathBuf),
    Disabled,
}

impl CachePolicy {
    fn dir(&self) -> Option<PathBuf> {
        match self {
            CachePolicy::Default => match std::env::var_os(CACHE_DIR_VAR) {
                Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
                _ => dirs::cache_dir().map(|d| d.join("codelib").join("compiled")),
            },
            CachePolicy::Dir(dir) => Some(dir.clone()),
            CachePolicy::Disabled => None,
        }
    }
}

/// Compile `bytes` (a component binary, or its text format), going through
/// the cache when one is configured. Cache problems are never fatal: the
/// component is compiled in memory instead.
pub(crate) fn load(
    engine: &Engine,
    bytes: &[u8],
    policy: &CachePolicy,
) -> Result<Component, Error> {
    let Some(dir) = policy.dir() else {
        return Component::new(engine, bytes).map_err(Error::Load);
    };
    let key = key(engine, bytes);
    let path = dir.join(format!("{key}.cwasm"));
    let sidecar = dir.join(format!("{key}.crc"));

    if let Some(entry) = read_verified(&path, &sidecar) {
        // SAFETY: `deserialize` trusts the bytes to be wasmtime's own output.
        // Entries are written only by `store` below, into a directory the
        // embedder chose (by default the user's private cache directory);
        // these bytes match the checksum recorded after the entry was
        // written in full, and wasmtime re-checks its version and
        // configuration header.
        match unsafe { Component::deserialize(engine, &entry) } {
            Ok(component) => {
                touch(&path);
                return Ok(component);
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), "discarding unreadable compile cache entry: {error:#}");
            }
        }
    }

    let compiled = engine.precompile_component(bytes).map_err(Error::Load)?;
    match store(&dir, &path, &sidecar, &compiled) {
        Ok(Written::Ours) => evict_stale(&dir, SystemTime::now()),
        Ok(Written::Theirs) => {}
        Err(error) => {
            tracing::warn!(path = %path.display(), "could not write compile cache entry: {error}");
        }
    }
    // SAFETY: `compiled` was produced by this engine one line above.
    unsafe { Component::deserialize(engine, &compiled) }.map_err(Error::Load)
}

fn key(engine: &Engine, bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hasher = DefaultHasher::new();
    engine.precompile_compatibility_hash().hash(&mut hasher);
    let mut key = String::with_capacity(64 + 1 + 16);
    for byte in digest {
        key.push_str(&format!("{byte:02x}"));
    }
    key.push_str(&format!("-{:016x}", hasher.finish()));
    key
}

/// What the sidecar records about an entry.
fn checksum(bytes: &[u8]) -> String {
    format!("{} {:08x}\n", bytes.len(), crc32fast::hash(bytes))
}

/// The entry's bytes, if it exists and matches its sidecar.
fn read_verified(path: &Path, sidecar: &Path) -> Option<Vec<u8>> {
    let expected = fs::read_to_string(sidecar).ok()?;
    let entry = fs::read(path).ok()?;
    if checksum(&entry) == expected {
        Some(entry)
    } else {
        tracing::warn!(path = %path.display(), "compile cache entry does not match its checksum; recompiling");
        None
    }
}

/// Write the entry, then its sidecar, each to a temporary file that is
/// renamed into place, so a concurrent reader never sees a half-written
/// file, and an entry without a matching sidecar is never loaded.
fn store(dir: &Path, path: &Path, sidecar: &Path, compiled: &[u8]) -> std::io::Result<Written> {
    fs::create_dir_all(dir)?;
    match write_atomically(path, compiled)? {
        Written::Ours => {
            write_atomically(sidecar, checksum(compiled).as_bytes())?;
            Ok(Written::Ours)
        }
        // Another writer won the race; it writes the sidecar for its entry.
        Written::Theirs => Ok(Written::Theirs),
    }
}

/// Mark the entry as used now, so that eviction measures time since the last
/// load. Failing to (a read-only directory) only makes the entry age.
fn touch(path: &Path) {
    let _ = fs::File::options()
        .write(true)
        .open(path)
        .and_then(|file| file.set_modified(SystemTime::now()));
}

/// Remove cache files in `dir` (entries, sidecars and abandoned temporary
/// files) last modified more than [`MAX_AGE`] before `now`. Best effort: a
/// file that cannot be inspected or removed is left alone.
pub(crate) fn evict_stale(dir: &Path, now: SystemTime) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let ours = name.ends_with(".cwasm") || name.ends_with(".crc") || name.contains(".tmp-");
        if !ours {
            continue;
        }
        let stale = entry
            .metadata()
            .ok()
            .filter(|meta| meta.is_file())
            .and_then(|meta| meta.modified().ok())
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > MAX_AGE);
        if stale {
            let _ = fs::remove_file(path);
        }
    }
}

enum Written {
    Ours,
    Theirs,
}

fn write_atomically(path: &Path, contents: &[u8]) -> std::io::Result<Written> {
    // Unique per process and per call, so concurrent writers (threads or
    // processes) never share a temporary file.
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp-{}-{unique}", std::process::id()));
    let tmp = path.with_file_name(name);

    let written = (|| {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(contents)?;
        file.sync_all()
    })();
    if let Err(error) = written {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(Written::Ours),
        Err(error) => {
            let _ = fs::remove_file(&tmp);
            if path.is_file() {
                Ok(Written::Theirs)
            } else {
                Err(error)
            }
        }
    }
}
