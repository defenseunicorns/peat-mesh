#!/usr/bin/env bash
#
# Functional test: enrollment flow on two peat-mesh-node instances.
#
# Runs on a single host (e.g., RPi) with two instances:
#   - Node A (authority): has enrollment tokens, issues certificates
#   - Node B (enrollee): connects and requests enrollment
#
# Tests:
#   1. Both nodes start and discover each other via mDNS
#   2. Authority node accepts sync connections
#   3. Health endpoints respond
#   4. Certificate store is initialized on authority
#   5. Enrollment ALPN is registered
#
# Usage: ./functional-test-enrollment.sh [binary_path]
#
set -euo pipefail

BINARY="${1:-./peat-mesh-node}"
FORMATION_SECRET="dGVzdC1mb3JtYXRpb24tc2VjcmV0LTEyMzQ1Njc4" # base64("test-formation-secret-12345678")
AUTHORITY_KEY="$(printf '%064d' 0 | sed 's/0/ab/g' | head -c 64)" # deterministic 32-byte hex key
CLEANUP_PIDS=()

cleanup() {
    echo ""
    echo "=== Cleaning up ==="
    for pid in "${CLEANUP_PIDS[@]}"; do
        if kill -0 "$pid" 2>/dev/null; then
            kill "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
        fi
    done
    rm -rf /tmp/peat-test-node-a /tmp/peat-test-node-b /tmp/peat_iroh_blobs_node-a /tmp/peat_iroh_blobs_node-b
    echo "Done."
}
trap cleanup EXIT

check_binary() {
    if [[ ! -x "$BINARY" ]]; then
        echo "ERROR: Binary not found or not executable: $BINARY"
        exit 1
    fi
    echo "Binary: $BINARY"
    file "$BINARY" 2>/dev/null || true
}

wait_for_health() {
    local port="$1"
    local name="$2"
    local max_wait=15
    local i=0
    while [[ $i -lt $max_wait ]]; do
        if curl -sf "http://127.0.0.1:${port}/api/v1/health" >/dev/null 2>&1; then
            echo "  [$name] Health OK (${i}s)"
            return 0
        fi
        sleep 1
        i=$((i + 1))
    done
    echo "  [$name] FAILED: health endpoint not responding after ${max_wait}s"
    return 1
}

# ── Preflight ────────────────────────────────────────────────
echo "=== Enrollment Functional Test ==="
echo ""
check_binary
mkdir -p /tmp/peat-test-node-a /tmp/peat-test-node-b

# ── Test 1: Start authority node (Node A) ─────────────────────
echo ""
echo "--- Test 1: Start authority node ---"

RUST_LOG="info,peat_mesh=debug" \
PEAT_FORMATION_SECRET="$FORMATION_SECRET" \
PEAT_DISCOVERY="mdns" \
PEAT_BROKER_PORT=8091 \
PEAT_IROH_BIND_PORT=11214 \
PEAT_DATA_DIR="/tmp/peat-test-node-a" \
PEAT_AUTHORITY_KEY="$AUTHORITY_KEY" \
PEAT_REQUIRE_CERTIFICATES="false" \
PEAT_ENROLLMENT_TOKENS="test-token-1=tactical,test-token-2=edge" \
HOSTNAME="node-a" \
"$BINARY" > /tmp/peat-test-node-a/stdout.log 2>&1 &
PID_A=$!
CLEANUP_PIDS+=("$PID_A")
echo "  Node A started (PID=$PID_A, broker=8091, iroh=11214)"

# ── Test 2: Start enrollee node (Node B) ─────────────────────
echo ""
echo "--- Test 2: Start enrollee node ---"

RUST_LOG="info,peat_mesh=debug" \
PEAT_FORMATION_SECRET="$FORMATION_SECRET" \
PEAT_DISCOVERY="mdns" \
PEAT_BROKER_PORT=8092 \
PEAT_IROH_BIND_PORT=11215 \
PEAT_DATA_DIR="/tmp/peat-test-node-b" \
HOSTNAME="node-b" \
"$BINARY" > /tmp/peat-test-node-b/stdout.log 2>&1 &
PID_B=$!
CLEANUP_PIDS+=("$PID_B")
echo "  Node B started (PID=$PID_B, broker=8092, iroh=11215)"

# ── Test 3: Wait for health endpoints ─────────────────────────
echo ""
echo "--- Test 3: Health endpoints ---"
PASS=0
FAIL=0

if wait_for_health 8091 "Node A"; then
    PASS=$((PASS + 1))
else
    FAIL=$((FAIL + 1))
fi

if wait_for_health 8092 "Node B"; then
    PASS=$((PASS + 1))
else
    FAIL=$((FAIL + 1))
fi

# ── Test 4: Check authority node logs for certificate store ────
echo ""
echo "--- Test 4: Authority node certificate store ---"
sleep 2  # Give logs time to flush

if grep -q "Certificate store initialized" /tmp/peat-test-node-a/stdout.log 2>/dev/null; then
    echo "  [Node A] Certificate store initialized: PASS"
    PASS=$((PASS + 1))
else
    echo "  [Node A] Certificate store initialized: FAIL"
    echo "  (checking logs...)"
    tail -20 /tmp/peat-test-node-a/stdout.log 2>/dev/null || echo "  No logs found"
    FAIL=$((FAIL + 1))
fi

# ── Test 5: Check enrollment ALPN registered ───────────────────
echo ""
echo "--- Test 5: Enrollment ALPN registration ---"

if grep -q "Enrollment ALPN" /tmp/peat-test-node-a/stdout.log 2>/dev/null; then
    echo "  [Node A] Enrollment ALPN registered: PASS"
    PASS=$((PASS + 1))
else
    echo "  [Node A] Enrollment ALPN registered: FAIL"
    FAIL=$((FAIL + 1))
fi

# ── Test 6: Check Layer 2 certificate gating ───────────────────
echo ""
echo "--- Test 6: Layer 2 certificate gating ---"

if grep -q "Layer 2 certificate gating enabled" /tmp/peat-test-node-a/stdout.log 2>/dev/null; then
    echo "  [Node A] Layer 2 gating enabled: PASS"
    PASS=$((PASS + 1))
else
    echo "  [Node A] Layer 2 gating enabled: FAIL"
    FAIL=$((FAIL + 1))
fi

# ── Test 7: Check enrollment token registration ───────────────
echo ""
echo "--- Test 7: Enrollment token registration ---"

TOKEN_COUNT=$(grep -c "Registered enrollment token" /tmp/peat-test-node-a/stdout.log 2>/dev/null || echo 0)
if [[ "$TOKEN_COUNT" -eq 2 ]]; then
    echo "  [Node A] $TOKEN_COUNT tokens registered: PASS"
    PASS=$((PASS + 1))
else
    echo "  [Node A] Expected 2 tokens, got $TOKEN_COUNT: FAIL"
    FAIL=$((FAIL + 1))
fi

# ── Test 8: Check mDNS discovery started ───────────────────────
echo ""
echo "--- Test 8: mDNS discovery ---"

if grep -q "mDNS discovery\|Using mDNS" /tmp/peat-test-node-a/stdout.log 2>/dev/null; then
    echo "  [Node A] mDNS discovery: PASS"
    PASS=$((PASS + 1))
else
    echo "  [Node A] mDNS discovery: FAIL"
    FAIL=$((FAIL + 1))
fi

if grep -q "mDNS discovery\|Using mDNS" /tmp/peat-test-node-b/stdout.log 2>/dev/null; then
    echo "  [Node B] mDNS discovery: PASS"
    PASS=$((PASS + 1))
else
    echo "  [Node B] mDNS discovery: FAIL"
    FAIL=$((FAIL + 1))
fi

# ── Test 9: Check peer discovery (wait for mDNS to propagate) ──
echo ""
echo "--- Test 9: Peer discovery (waiting up to 30s) ---"

PEER_FOUND=false
for i in $(seq 1 30); do
    if grep -q "Peer connected to Iroh\|peer authenticated\|accepted sync" /tmp/peat-test-node-a/stdout.log 2>/dev/null; then
        PEER_FOUND=true
        echo "  [Node A] Peer discovered (${i}s): PASS"
        PASS=$((PASS + 1))
        break
    fi
    sleep 1
done
if [[ "$PEER_FOUND" == "false" ]]; then
    echo "  [Node A] No peer discovered after 30s: FAIL"
    FAIL=$((FAIL + 1))
fi

PEER_FOUND_B=false
for i in $(seq 1 5); do
    if grep -q "Peer connected to Iroh\|peer authenticated\|accepted sync" /tmp/peat-test-node-b/stdout.log 2>/dev/null; then
        PEER_FOUND_B=true
        echo "  [Node B] Peer discovered: PASS"
        PASS=$((PASS + 1))
        break
    fi
    sleep 1
done
if [[ "$PEER_FOUND_B" == "false" ]]; then
    echo "  [Node B] No peer discovered: FAIL (may be expected with mDNS on same host)"
    FAIL=$((FAIL + 1))
fi

# ── Test 10: Broker API - mesh status ──────────────────────────
echo ""
echo "--- Test 10: Broker API ---"

STATUS_A=$(curl -sf "http://127.0.0.1:8091/api/v1/health" 2>/dev/null || echo "UNREACHABLE")
STATUS_B=$(curl -sf "http://127.0.0.1:8092/api/v1/health" 2>/dev/null || echo "UNREACHABLE")

if [[ "$STATUS_A" != "UNREACHABLE" ]]; then
    echo "  [Node A] /api/v1/health: $STATUS_A"
    PASS=$((PASS + 1))
else
    echo "  [Node A] /api/v1/health: UNREACHABLE"
    FAIL=$((FAIL + 1))
fi

if [[ "$STATUS_B" != "UNREACHABLE" ]]; then
    echo "  [Node B] /api/v1/health: $STATUS_B"
    PASS=$((PASS + 1))
else
    echo "  [Node B] /api/v1/health: UNREACHABLE"
    FAIL=$((FAIL + 1))
fi

# ── Summary ──────────────────────────────────────────────────
echo ""
echo "============================================"
echo "  RESULTS: $PASS passed, $FAIL failed"
echo "============================================"

# Dump logs on failure
if [[ "$FAIL" -gt 0 ]]; then
    echo ""
    echo "--- Node A logs (last 30 lines) ---"
    tail -30 /tmp/peat-test-node-a/stdout.log 2>/dev/null || echo "(no logs)"
    echo ""
    echo "--- Node B logs (last 30 lines) ---"
    tail -30 /tmp/peat-test-node-b/stdout.log 2>/dev/null || echo "(no logs)"
fi

exit "$FAIL"
