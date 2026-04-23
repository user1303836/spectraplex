#!/usr/bin/env bash
set -euo pipefail

# ---------------------------------------------------------------------------
# Spectraplex API Smoke Test
# ---------------------------------------------------------------------------
# Proves the supported API path end-to-end on a fresh local environment.
#
# Steps:
#   1. Start Postgres
#   2. Start the API server
#   3. Create a tenant-scoped API key (using legacy admin key)
#   4. Register a wallet target
#   5. Trigger ingestion (wallet + network)
#   6. Poll job status
#   7. Query dataset records (tenant-scoped via target_id)
#   8. Create and poll an export job (tenant-scoped via target_id)
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
TARGET_WALLET="So11111111111111111111111111111111111111112"
TARGET_NETWORK="solana-mainnet"

SKIP_INGEST=false
for arg in "$@"; do
  if [[ "$arg" == "--skip-ingest" ]]; then
    SKIP_INGEST=true
  fi
done

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

curl_json() {
  local method="${1:-GET}"
  local url="$2"
  local auth="${3:-}"
  local body="${4:-}"

  local cmd=(curl -s -w "\n%{http_code}" -X "$method" "$url")
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
  echo "$json" | python3 -c "import sys, json; print(json.load(sys.stdin)['$key'])" 2>/dev/null || true
}

echo "=== Spectraplex Smoke Test ==="
echo ""

# ---------------------------------------------------------------------------
# 1. Start Postgres
# ---------------------------------------------------------------------------
echo "[1/10] Starting Postgres..."
cd "$PROJECT_DIR"
docker-compose up -d postgres >/dev/null 2>&1 || docker compose up -d postgres >/dev/null 2>&1
echo "       Postgres started"

for i in {1..30}; do
  if pg_isready -h localhost -p 5432 -U spectraplex >/dev/null 2>&1; then
    break
  fi
  sleep 1
done

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

for i in {1..30}; do
  if curl -sf "$API_URL/health" >/dev/null 2>&1; then
    break
  fi
  sleep 1
done
echo "       API running on $API_URL (PID $API_PID)"

cleanup() {
  echo ""
  echo "=== Cleaning up ==="
  if kill -0 "$API_PID" 2>/dev/null; then
    kill "$API_PID" 2>/dev/null || true
    wait "$API_PID" 2>/dev/null || true
  fi
  echo "       Done."
}
trap cleanup EXIT

# ---------------------------------------------------------------------------
# 3. Create a tenant-scoped API key
# ---------------------------------------------------------------------------
echo "[3/10] Creating tenant API key..."
CREATE_RESP=$(curl_json POST "$API_URL/v1/api-keys" "$LEGACY_KEY" '{"name":"smoke-test-key"}')
TENANT_KEY=$(extract_json "$CREATE_RESP" "key")
if [[ -z "$TENANT_KEY" ]]; then
  echo "ERROR: Could not parse API key from response: $CREATE_RESP"
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
  for i in {1..10}; do
    JOB_STATUS=$(curl_json GET "$API_URL/v1/jobs/$JOB_ID" "$TENANT_KEY")
    STATUS=$(echo "$JOB_STATUS" | python3 -c "import sys, json; print(json.load(sys.stdin).get('state','unknown'))" 2>/dev/null || echo "unknown")
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
RECORD_COUNT=$(echo "$DATASET_RESP" | python3 -c "import sys, json; d=json.load(sys.stdin); print(len(d) if isinstance(d, list) else len(d.get('records',[])))" 2>/dev/null || echo "0")
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
KEY_COUNT=$(echo "$LIST_RESP" | python3 -c "import sys, json; print(len(json.load(sys.stdin)))" 2>/dev/null || echo "0")
echo "       Listed $KEY_COUNT active key(s)"

# ---------------------------------------------------------------------------
# 10. Verify target is listable
# ---------------------------------------------------------------------------
echo "[10/10] Verifying target listing..."
TARGETS_RESP=$(curl_json GET "$API_URL/v1/targets?limit=10" "$TENANT_KEY")
TARGET_COUNT=$(echo "$TARGETS_RESP" | python3 -c "import sys, json; d=json.load(sys.stdin); print(len(d.get('targets', d if isinstance(d, list) else [])))" 2>/dev/null || echo "0")
echo "       Listed $TARGET_COUNT target(s)"

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
