#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
if [[ "${1:-}" == --help || "${1:-}" == -h ]]; then
  echo 'Usage: TEST_DATABASE_URL=postgres://... ./scripts/smoke-test.sh [--browser]'
  echo 'Creates and removes an isolated database; uses local provider fixtures, not mainnet.'
  echo 'Requires Rust, Python 3 and psql. --browser also requires Playwright (see web-tests/package.json).'
  exit 0
fi
for tool in cargo python3 psql; do
  command -v "$tool" >/dev/null || { echo "Required command not found: $tool" >&2; exit 1; }
done
cargo build --locked --bin spectraplex-api
exec python3 scripts/smoke_test.py "$@"
