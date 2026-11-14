#!/bin/sh
# Rebuild ../p3-guest.wasm, the WASI 0.3 component behind tests/p3.rs.
#
# Needs Rust 1.98.1 with the wasm32-wasip2 target
# (`rustup target add wasm32-wasip2`); rustc's own wasm-component-ld turns
# the module into a component, so no other tool is involved. Works from a
# POSIX shell on Linux or from Git Bash on Windows.
#
# The build shares the workspace's target directory (or $CARGO_TARGET_DIR)
# instead of creating one of its own.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../../../.." && pwd)
target=${CARGO_TARGET_DIR:-$root/target}
registry=${CARGO_HOME:-$HOME/.cargo}/registry/src

# Panic locations embed source paths; keep the builder's home directory and
# checkout location out of the checked-in binary.
if command -v cygpath >/dev/null 2>&1; then
    registry=$(cygpath -w "$registry")
    here_native=$(cygpath -w "$here")
else
    here_native=$here
fi
RUSTFLAGS="--remap-path-prefix=$registry=/cargo/registry/src --remap-path-prefix=$here_native=/p3-guest"
export RUSTFLAGS

cd "$here"
cargo build --locked --release --target wasm32-wasip2 --target-dir "$target"
cp "$target/wasm32-wasip2/release/codelib_p3_fixture.wasm" "$here/../p3-guest.wasm"

wc -c <"$here/../p3-guest.wasm"
sha256sum "$here/../p3-guest.wasm" 2>/dev/null || shasum -a 256 "$here/../p3-guest.wasm"
