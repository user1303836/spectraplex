#!/usr/bin/env bash
set -euo pipefail

# ---------------------------------------------------------------------------
# Spectraplex Local Dev Environment
# ---------------------------------------------------------------------------
# Starts Postgres via Docker Compose so you can run the API locally with
# cargo run.
#
# Usage:
#   ./scripts/local-dev.sh        # start Postgres
#   ./scripts/local-dev.sh stop   # stop Postgres
# ---------------------------------------------------------------------------

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

cd "$PROJECT_DIR"

if [[ "${1:-}" == "stop" ]]; then
  echo "Stopping local dev services..."
  docker-compose down 2>/dev/null || docker compose down 2>/dev/null
  echo "Done."
  exit 0
fi

echo "Starting Postgres for local development..."
docker-compose up -d postgres 2>/dev/null || docker compose up -d postgres 2>/dev/null

# Wait for readiness
for i in {1..30}; do
  if pg_isready -h localhost -p 5432 -U spectraplex >/dev/null 2>&1; then
    break
  fi
  sleep 1
done

echo ""
echo "Postgres is ready on localhost:5432"
echo ""
echo "Next steps:"
echo "  1. Copy spectraplex.toml.example to spectraplex.toml and edit as needed."
echo "  2. Run: cargo run --bin spectraplex-api"
echo "  3. Test: ./scripts/smoke-test.sh"
echo ""
