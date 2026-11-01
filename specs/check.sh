#!/usr/bin/env bash
# Invoke with bash; kept non-executable for the disposable-runner patch policy.
set -euo pipefail
specs=$(dirname "$0")
exec cargo run --quiet --locked --manifest-path "$specs/../Cargo.toml" -p agent-board --bin specs-check -- "$specs"
