#!/usr/bin/env bash
set -euo pipefail

# ---------------------------------------------------------------------------
# Spectraplex API Smoke Test
# ---------------------------------------------------------------------------
# Proves the supported API path end-to-end on a fresh local environment.
#
# What this verifies:
#   - API server boots and responds to health checks
#   - Tenant API key creation and authentication work
#   - Target registration works
#   - Ingestion can be enqueued and its job status polled
#   - Tenant-scoped dataset queries accept target_id and return data (if materialized)
#   - Tenant-scoped export jobs can be created with a sink config
#   - API key and target listing work
#
# What this does NOT verify (these run in background workers):
#   - Actual RPC ingestion completion against live providers
#   - Silver/Gold materialization (async, may take minutes)
#   - Export job completion and file delivery (async worker process)
#
# Steps:
#   1. Start Postgres
#   2. Start the API server
#   3. Create a tenant-scoped API key (using legacy admin key)
#   4. Register a wallet target
#   5. Trigger ingestion (wallet + network)
#   6. Poll job status (terminal state check)
#   7. Query dataset records (tenant-scoped via target_id)
#   8. Create an export job (tenant-scoped via target_id)
#   9. Verify tenant-scoped API key lifecycle
#   10. Clean up
#
# The script fails loudly on any unexpected HTTP response.
# If live providers are unavailable, use --skip-ingest to test the API surface
# without provider-dependent steps.
# ---------------------------------------------------------------------------

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
API_URL="http://localhost:3000"
LEGACY_KEY="admin-smoke-test-key"
TENANT_KEY=""
API_KEY_ID=""
API_PID=""
TARGET_WALLET="So11111111111111111111111111111111111111112"
TARGET_NETWORK="solana-mainnet"

usage() {
  cat <<'USAGE'
Usage: ./scripts/smoke-test.sh [--skip-ingest]

Options:
  --skip-ingest  Skip provider-dependent ingestion and job polling.
  -h, --help     Show this help message.
USAGE
}

SKIP_INGEST=false
for arg in "$@"; do
  case "$arg" in
    --skip-ingest)
      SKIP_INGEST=true
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "ERROR: Unknown option: $arg" >&2
      usage >&2
      exit 1
      ;;
  esac
done

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

curl_json() {
  local method="${1:-GET}"
  local url="$2"
  local auth="${3:-}"
  local body="${4:-}"

  local cmd=(curl -sS -w "\n%{http_code}" -X "$method" "$url")
  if [[ -n "$auth" ]]; then
    cmd+=(-H "Authorization: Bearer $auth")
  fi
  cmd+=(-H "Content-Type: application/json")
  if [[ -n "$body" ]]; then
    cmd+=(-d "$body")
  fi

  local resp
  resp=$("${cmd[@]}")
  local http_code
  http_code=$(echo "$resp" | tail -n1)
  local body_lines
  body_lines=$(echo "$resp" | sed '$d')

  if [[ "$http_code" -lt 200 || "$http_code" -ge 300 ]]; then
    echo "ERROR: HTTP $http_code from $url" >&2
    echo "Response: $body_lines" >&2
    return 1
  fi

  echo "$body_lines"
}

extract_json() {
  local json="$1"
  local key="$2"
  echo "$json" | jq -r --arg key "$key" 'if type == "object" then (.[$key] // empty) else empty end' 2>/dev/null || true
}

extract_json_or_default() {
  local json="$1"
  local key="$2"
  local default="$3"
  echo "$json" | jq -r --arg key "$key" --arg default "$default" 'if type == "object" then (.[$key] // $default) else $default end' 2>/dev/null || echo "$default"
}

count_json_collection() {
  local json="$1"
  local key="${2:-}"
  echo "$json" | jq -r --arg key "$key" '
    if type == "array" then length
    elif type == "object" then ((.[$key] // []) | if type == "array" then length else 0 end)
    else 0
    end
  ' 2>/dev/null || echo "0"
}

json_collection_contains_id() {
  local json="$1"
  local key="${2:-}"
  local id="$3"
  echo "$json" | jq -e --arg key "$key" --arg id "$id" '
    (if type == "array" then . elif type == "object" then (.[$key] // []) else [] end)
    | map(select(.id == $id))
    | length > 0
  ' >/dev/null 2>&1
}

fail() {
  echo "ERROR: $*" >&2
  exit 1
}

require_command() {
  local cmd="$1"
  local hint="$2"
  if ! command -v "$cmd" >/dev/null 2>&1; then
    fail "Required command '$cmd' is not available. $hint"
  fi
}

start_postgres() {
  if command -v docker-compose >/dev/null 2>&1 && docker-compose version >/dev/null 2>&1; then
    if docker-compose up -d postgres >/dev/null 2>&1; then
      echo "docker-compose"
      return 0
    fi
  fi
  if command -v docker >/dev/null 2>&1 && docker compose version >/dev/null 2>&1; then
    if docker compose up -d postgres >/dev/null 2>&1; then
      echo "docker compose"
      return 0
    fi
  fi
  return 1
}

wait_for_postgres() {
  local compose_cmd="$1"
  for i in {1..30}; do
    if command -v pg_isready >/dev/null 2>&1; then
      if pg_isready -h localhost -p 5432 -U spectraplex >/dev/null 2>&1; then
        return 0
      fi
    elif $compose_cmd exec -T postgres pg_isready -U spectraplex >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  return 1
}

wait_for_api() {
  for _ in {1..30}; do
    if curl -sf "$API_URL/health" >/dev/null 2>&1; then
      return 0
    fi

    if [[ -n "$API_PID" ]] && ! kill -0 "$API_PID" 2>/dev/null; then
      return 2
    fi

    sleep 1
  done

  return 1
}

require_command curl "Install curl to call the local API."
require_command cargo "Install Rust/Cargo to build spectraplex-api."
require_command jq "Install jq for JSON response parsing in the smoke script."

echo "=== Spectraplex Smoke Test ==="
echo ""

# ---------------------------------------------------------------------------
# 1. Start Postgres
# ---------------------------------------------------------------------------
echo "[1/10] Starting Postgres..."
cd "$PROJECT_DIR"
COMPOSE_CMD="$(start_postgres)" || fail "Could not start Postgres with either 'docker-compose up -d postgres' or 'docker compose up -d postgres'. Check Docker/Compose availability and local container logs."
echo "       Postgres started via $COMPOSE_CMD"

if ! wait_for_postgres "$COMPOSE_CMD"; then
  fail "Postgres did not become ready on localhost:5432 within 30 seconds. Check docker compose logs and local port usage."
fi

# ---------------------------------------------------------------------------
# 2. Build and start the API server
# ---------------------------------------------------------------------------
echo "[2/10] Building API server..."
cd "$PROJECT_DIR"
cargo build --bin spectraplex-api --quiet

echo "       Starting API server..."
export SPECTRAPLEX_CONFIG="$SCRIPT_DIR/smoke-config.toml"
"$PROJECT_DIR/target/debug/spectraplex-api" &
API_PID=$!

cleanup() {
  echo ""
  echo "=== Cleaning up ==="
  if [[ -n "$API_PID" ]] && kill -0 "$API_PID" 2>/dev/null; then
    kill "$API_PID" 2>/dev/null || true
    wait "$API_PID" 2>/dev/null || true
  fi
  echo "       Done."
}
trap cleanup EXIT

if wait_for_api; then
  echo "       API running on $API_URL (PID $API_PID)"
else
  status=$?
  if [[ "$status" -eq 2 ]]; then
    fail "API process exited before $API_URL/health became healthy."
  fi
  fail "API did not become healthy at $API_URL/health within 30 seconds."
fi

# ---------------------------------------------------------------------------
# 3. Create a tenant-scoped API key
# ---------------------------------------------------------------------------
echo "[3/10] Creating tenant API key..."
CREATE_RESP=$(curl_json POST "$API_URL/v1/api-keys" "$LEGACY_KEY" '{"name":"smoke-test-key"}')
API_KEY_ID=$(extract_json "$CREATE_RESP" "id")
TENANT_KEY=$(extract_json "$CREATE_RESP" "key")
if [[ -z "$API_KEY_ID" || -z "$TENANT_KEY" ]]; then
  echo "ERROR: Could not parse API key ID/key from response: $CREATE_RESP"
  exit 1
fi
echo "       Created tenant key: ${TENANT_KEY:0:12}..."

# ---------------------------------------------------------------------------
# 4. Register a wallet target
# ---------------------------------------------------------------------------
echo "[4/10] Registering wallet target..."
TARGET_RESP=$(curl_json POST "$API_URL/v1/targets" "$TENANT_KEY" \
  "{\"kind\":\"wallet\",\"network\":\"$TARGET_NETWORK\",\"address\":\"$TARGET_WALLET\",\"mode\":\"both\"}")
TARGET_ID=$(extract_json "$TARGET_RESP" "id")
if [[ -z "$TARGET_ID" ]]; then
  echo "ERROR: Could not parse target ID from response: $TARGET_RESP"
  exit 1
fi
echo "       Registered target: $TARGET_ID"

# ---------------------------------------------------------------------------
# 5. Trigger ingestion
# ---------------------------------------------------------------------------
echo "[5/10] Triggering ingestion..."
if [[ "$SKIP_INGEST" == true ]]; then
  echo "       SKIPPED (--skip-ingest): skipping provider-dependent ingestion"
  JOB_ID=""
else
  INGEST_RESP=$(curl_json POST "$API_URL/v1/ingest" "$TENANT_KEY" \
    "{\"wallet\":\"$TARGET_WALLET\",\"network\":\"$TARGET_NETWORK\"}")
  JOB_ID=$(extract_json "$INGEST_RESP" "id")
  if [[ -z "$JOB_ID" ]]; then
    echo "ERROR: Could not parse job ID from ingest response: $INGEST_RESP"
    exit 1
  fi
  echo "       Ingest job enqueued: $JOB_ID"
fi

# ---------------------------------------------------------------------------
# 6. Poll job status
# ---------------------------------------------------------------------------
echo "[6/10] Polling job status..."
if [[ -n "$JOB_ID" ]]; then
  for i in {1..30}; do
    JOB_STATUS=$(curl_json GET "$API_URL/v1/jobs/$JOB_ID" "$TENANT_KEY")
    STATUS=$(extract_json_or_default "$JOB_STATUS" "state" "unknown")
    echo "       Job status: $STATUS"
    if [[ "$STATUS" == "completed" || "$STATUS" == "failed" ]]; then
      break
    fi
    sleep 2
  done
  if [[ "$STATUS" == "failed" ]]; then
    echo "ERROR: Ingest job $JOB_ID failed"
    exit 1
  fi
  if [[ "$STATUS" != "completed" ]]; then
    echo "ERROR: Ingest job $JOB_ID did not complete within poll window (last status: $STATUS)"
    exit 1
  fi
else
  echo "       Skipped (no job ID)"
fi

# ---------------------------------------------------------------------------
# 7. Query dataset records (tenant-scoped via target_id)
# ---------------------------------------------------------------------------
echo "[7/10] Querying token_transfers dataset (tenant-scoped)..."
DATASET_RESP=$(curl_json GET "$API_URL/v1/datasets/token_transfers/records?target_id=$TARGET_ID&limit=10" "$TENANT_KEY")
RECORD_COUNT=$(count_json_collection "$DATASET_RESP" "records")
echo "       Records returned: $RECORD_COUNT"

# ---------------------------------------------------------------------------
# 8. Create an export job (tenant-scoped via target_id)
# ---------------------------------------------------------------------------
echo "[8/10] Creating dataset export job (tenant-scoped)..."
EXPORT_RESP=$(curl_json POST "$API_URL/v1/export/dataset" "$TENANT_KEY" \
  "{\"dataset\":\"token_transfers\",\"format\":\"jsonl\",\"target_id\":\"$TARGET_ID\",\"network\":\"$TARGET_NETWORK\",\"sink\":{\"sink_type\":\"local_file\",\"file_path\":\"smoke-export.jsonl\"}}")
EXPORT_JOB_ID=$(extract_json "$EXPORT_RESP" "id")
if [[ -z "$EXPORT_JOB_ID" ]]; then
  echo "ERROR: Could not parse export job ID from response: $EXPORT_RESP"
  exit 1
fi
echo "       Export job created: $EXPORT_JOB_ID"

# ---------------------------------------------------------------------------
# 9. Verify API key lifecycle
# ---------------------------------------------------------------------------
echo "[9/10] Verifying API key lifecycle..."
LIST_RESP=$(curl_json GET "$API_URL/v1/api-keys" "$TENANT_KEY")
KEY_COUNT=$(count_json_collection "$LIST_RESP")
echo "       Listed $KEY_COUNT active key(s)"
if [[ "$KEY_COUNT" -lt 1 ]]; then
  fail "Expected at least one active API key in list response."
fi
if ! json_collection_contains_id "$LIST_RESP" "" "$API_KEY_ID"; then
  fail "Created API key $API_KEY_ID was not returned by the API key list endpoint."
fi

# ---------------------------------------------------------------------------
# 10. Verify target is listable
# ---------------------------------------------------------------------------
echo "[10/10] Verifying target listing..."
TARGETS_RESP=$(curl_json GET "$API_URL/v1/targets?limit=10" "$TENANT_KEY")
TARGET_COUNT=$(count_json_collection "$TARGETS_RESP" "targets")
echo "       Listed $TARGET_COUNT target(s)"
if [[ "$TARGET_COUNT" -lt 1 ]]; then
  fail "Expected at least one target in list response."
fi
if ! json_collection_contains_id "$TARGETS_RESP" "targets" "$TARGET_ID"; then
  fail "Created target $TARGET_ID was not returned by the target list endpoint."
fi

echo ""
echo "=== Smoke test completed successfully ==="
echo ""
echo "Summary:"
echo "  - Tenant API key created and usable"
echo "  - Target registration works"
if [[ "$SKIP_INGEST" == true ]]; then
  echo "  - Ingestion SKIPPED (--skip-ingest)"
else
  echo "  - Ingestion enqueue works"
  echo "  - Job status polling works"
fi
echo "  - Tenant-scoped dataset queries work (target_id required)"
echo "  - Tenant-scoped export job creation works (target_id required)"
echo "  - API key listing works"
echo "  - Target listing works"
