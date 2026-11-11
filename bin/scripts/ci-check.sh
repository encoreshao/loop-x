#!/usr/bin/env bash
# Runs exactly what .github/workflows/rust.yml runs, so a push never turns CI red.
# Run it before every push:   bin/scripts/ci-check.sh
# A page change also needs its golden snapshots regenerated and committed:
#   LOOPX_GOLDEN_BLESS=1 cargo test -p loopx-server --test golden_ui -- --test-threads=1
set -euo pipefail
cd "$(dirname "$0")/../.."
export PATH="$HOME/.cargo/bin:$PATH"

echo "==> cargo fmt --check";  cargo fmt --check
echo "==> cargo clippy";       cargo clippy --workspace --all-targets -- -D warnings
echo "==> cargo test";         cargo test --workspace
echo "==> loop audit"
cargo build -p loopx
target/debug/loopx loop audit loops/*/loop.yaml --min-score 70
if git status --porcelain tests/golden_ui | grep -q .; then
  echo "golden snapshots changed but are not committed: git add tests/golden_ui" >&2
  exit 1
fi
echo "==> CI checks passed"
