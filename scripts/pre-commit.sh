#!/usr/bin/env bash
set -euo pipefail

echo "Running pre-commit checks..."

cargo fmt --check || { echo "Run 'cargo fmt' to fix formatting."; exit 1; }
cargo clippy -- -D warnings || { echo "Fix clippy warnings before committing."; exit 1; }

echo "All checks passed."
