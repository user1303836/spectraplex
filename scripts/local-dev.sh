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

usage() {
  cat <<'USAGE'
Usage: ./scripts/local-dev.sh [stop]

Commands:
  start (default)  Start Postgres for local development.
  stop             Stop local development services.
  -h, --help       Show this help message.
USAGE
}

fail() {
  echo "ERROR: $*" >&2
  exit 1
}

compose() {
  if command -v docker-compose >/dev/null 2>&1 && docker-compose version >/dev/null 2>&1; then
    docker-compose "$@"
  elif command -v docker >/dev/null 2>&1 && docker compose version >/dev/null 2>&1; then
    docker compose "$@"
  else
    fail "Docker Compose is required. Install either 'docker-compose' or Docker with the 'docker compose' plugin."
  fi
}

wait_for_postgres() {
  for _ in {1..30}; do
    if command -v pg_isready >/dev/null 2>&1; then
      if pg_isready -h localhost -p 5432 -U spectraplex >/dev/null 2>&1; then
        return 0
      fi
    fi

    if compose exec -T postgres pg_isready -U spectraplex >/dev/null 2>&1; then
      return 0
    fi

    sleep 1
  done

  return 1
}

case "${1:-start}" in
  start)
    ;;
  stop)
    echo "Stopping local dev services..."
    compose down
    echo "Done."
    exit 0
    ;;
  -h | --help)
    usage
    exit 0
    ;;
  *)
    echo "ERROR: Unknown command: $1" >&2
    usage >&2
    exit 1
    ;;
esac

echo "Starting Postgres for local development..."
compose up -d postgres

if ! wait_for_postgres; then
  fail "Postgres did not become ready on localhost:5432 within 30 seconds. Check Docker Compose logs and local port usage."
fi

echo ""
echo "Postgres is ready on localhost:5432"
echo ""
echo "Next steps:"
echo "  1. Copy spectraplex.toml.example to spectraplex.toml and edit as needed."
echo "  2. Run: cargo run --bin spectraplex-api"
echo "  3. Test: ./scripts/smoke-test.sh"
echo ""
