#!/bin/sh
# The gate every change passes before it is committed: format, lint, test.
set -eu
cd "$(dirname "$0")/.."
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
