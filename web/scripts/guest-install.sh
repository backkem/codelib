#!/bin/sh
# Installs the package's dependencies: `npm ci` from the lockfile (skipped when node_modules is
# already current), or `npm install` to create the lockfile.
set -eu
cd "$(dirname "$0")/.."
if [ -f package-lock.json ]; then
    if [ ! -f node_modules/.package-lock.json ] || [ package-lock.json -nt node_modules/.package-lock.json ]; then
        npm ci --no-audit --no-fund
    fi
else
    npm install --no-audit --no-fund
fi
