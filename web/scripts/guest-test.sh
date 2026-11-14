#!/usr/bin/env bash
# Builds the package and runs every test: unit tests under Node, then the browser tests in
# headless Chromium. Arguments go to the browser test runner (name filters, --no-transpile).
# Logs go to test-results/.
set -uo pipefail
cd "$(dirname "$0")/.."
mkdir -p test-results
npm run build 2>&1 | tee test-results/build.log || exit 1
status=0
node --test 'test/unit/*.test.mjs' 2>&1 | tee test-results/unit.log || status=1
node test/browser/run.mjs "$@" 2>&1 | tee test-results/browser.log || status=1
exit $status
