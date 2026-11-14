#!/usr/bin/env sh
# Builds the guest component and installs it into the codelib-guest crate.
# Needs Node 20.6 or newer and npm. Run from anywhere:
#
#   ./guest/build.sh
#
# The build is not byte-reproducible, so the crate pins the artifact by
# sha256 (crates/codelib-guest/codelib-guest.wasm.sha256). Commit both.
set -eu

cd "$(dirname "$0")"

if [ -f package-lock.json ]; then
    npm ci --no-audit --no-fund
else
    npm install --no-audit --no-fund
fi
npm run build

out=../crates/codelib-guest
cp codelib-guest.wasm "$out/codelib-guest.wasm"
sha256sum codelib-guest.wasm | cut -d ' ' -f 1 > "$out/codelib-guest.wasm.sha256"
echo "codelib-guest.wasm: $(wc -c < codelib-guest.wasm) bytes, sha256 $(cat "$out/codelib-guest.wasm.sha256")"
