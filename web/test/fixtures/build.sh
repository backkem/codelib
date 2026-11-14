#!/bin/sh
# Rebuilds the WASI 0.3 fixtures of the browser tests:
#
#   clock-p3.wasm  from clock-p3/: clocks, random, sleep, print (see clock-p3/src/lib.rs)
#   pair-p3.wasm   from pair-p3/: two components composed with `wac plug`, for jco's
#                  guest-to-guest call machinery
#
#   sh test/fixtures/build.sh
#
# Needs rustup (toolchain and target pinned in rust-toolchain.toml), `wac` (wac-cli 0.12) for the
# composition, and network access for the first fetch. The builds are reproducible: lockfiles are
# committed, paths are remapped and the release profile strips debug info, so a rebuild gives the
# same bytes (the `.sha256` files). The fixtures are checked in; the tests never build them.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
target="${CARGO_TARGET_DIR:-$here/../../.fixture-target}"
cargo_home="${CARGO_HOME:-$HOME/.cargo}"

build() { # build CRATE_DIR [cargo args...]
    dir="$1"; shift
    (cd "$here/$dir" &&
        RUSTFLAGS="--remap-path-prefix=$here=/fixtures --remap-path-prefix=$cargo_home=/cargo" \
            cargo build --locked --release --target wasm32-wasip2 --target-dir "$target" "$@")
}
out="$target/wasm32-wasip2/release"

build clock-p3
cp "$out/clock_p3.wasm" "$here/clock-p3.wasm"

build pair-p3 --workspace
wac plug "$out/pair_caller.wasm" --plug "$out/pair_callee.wasm" -o "$here/pair-p3.wasm"

cd "$here"
for f in clock-p3.wasm pair-p3.wasm; do
    sha256sum "$f" | cut -d ' ' -f 1 > "$f.sha256"
    echo "$f: $(wc -c < "$f") bytes, sha256 $(cat "$f.sha256")"
done
