#!/usr/bin/env bash
# What CI checks: formatting, lints (warnings are errors), and tests.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
cargo fmt --all --check
RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets --quiet
cargo test --workspace --quiet
echo "All checks passed."
