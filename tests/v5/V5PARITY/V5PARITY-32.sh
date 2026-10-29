#!/usr/bin/env bash
# tier: 1
# V5PARITY-32 acceptance: v5 is the canonical hyperforge.
set -euo pipefail
source "$(dirname "$0")/../harness/lib.sh"

cd "$(dirname "$0")/../../.."

# --- (1) Cargo version is 5.x ---
ver=$(grep '^version = ' Cargo.toml | head -1 | sed -E 's/.*"([^"]+)".*/\1/')
case "$ver" in
    5.*) echo "version: $ver (v5)" ;;
    *)   echo "FAIL: expected 5.x, got $ver" >&2; exit 1 ;;
esac

# --- (2) Binary entries: hyperforge points at v5 source; legacy preserved ---
grep -A 1 'name = "hyperforge"$' Cargo.toml | grep -q 'src/bin/hyperforge.rs' \
    || { echo "FAIL: [[bin]] hyperforge does not point at src/bin/hyperforge.rs" >&2; exit 1; }
grep -q 'name = "hyperforge-legacy"' Cargo.toml \
    || { echo "FAIL: hyperforge-legacy bin entry missing" >&2; exit 1; }
grep -q 'name = "hyperforge-v5"' Cargo.toml \
    || { echo "FAIL: hyperforge-v5 alias missing (needed for in-flight tests)" >&2; exit 1; }
echo "binaries: hyperforge (canonical), hyperforge-v5 (alias), hyperforge-legacy (v4)"

# --- (3) hyperforge.rs is the v5 source (recognizable by HyperforgeService use) ---
grep -q "use hyperforge::v5::service::HyperforgeService" src/bin/hyperforge.rs \
    || { echo "FAIL: src/bin/hyperforge.rs is not the v5 source" >&2; exit 1; }
echo "src/bin/hyperforge.rs: v5 source"

# --- (4) Default port is 44104 ---
grep -q 'default_value = "44104"' src/bin/hyperforge.rs \
    || { echo "FAIL: hyperforge default port not 44104" >&2; exit 1; }
echo "default port: 44104"

# --- (5) README leads with v5 ---
grep -qE '^# Hyperforge\b' README.md \
    || { echo "FAIL: README missing top-level header" >&2; exit 1; }
grep -q "v5" README.md \
    || { echo "FAIL: README does not mention v5" >&2; exit 1; }
grep -q "orgs bootstrap" README.md \
    || { echo "FAIL: README quick-start does not show V5PARITY-21 flow" >&2; exit 1; }
echo "README: leads with v5"

# --- (6) MIGRATION.md exists and covers the v4→v5 handoff ---
[[ -f MIGRATION.md ]] || { echo "FAIL: MIGRATION.md missing" >&2; exit 1; }
grep -q "hyperforge-legacy" MIGRATION.md \
    || { echo "FAIL: MIGRATION.md doesn't mention legacy binary" >&2; exit 1; }
echo "MIGRATION.md: present + documents handoff"

# --- (7) CONTRACTS D1 updated ---
grep -q 'default port is \*\*44104\*\*' plans/v5/CONTRACTS.md \
    || { echo "FAIL: CONTRACTS D1 still says 44105" >&2; exit 1; }
echo "CONTRACTS D1: pinned at 44104"

# --- (8) hyperforge binary actually runs (not just builds) ---
PORT=$(__hf_pick_port)
TMP=$(mktemp -d)
nohup target/debug/hyperforge --port "$PORT" --config-dir "$TMP" \
    >/tmp/v5parity32-daemon.log 2>&1 &
PID=$!
sleep 1.5
if synapse -P "$PORT" --json lforge-v5 hyperforge status 2>&1 | grep -q '"version":"5'; then
    echo "daemon: hyperforge --port $PORT responds with v5 status"
else
    kill $PID 2>/dev/null
    rm -rf "$TMP"
    echo "FAIL: hyperforge daemon did not respond as v5" >&2
    cat /tmp/v5parity32-daemon.log >&2
    exit 1
fi
kill $PID 2>/dev/null
rm -rf "$TMP"

echo "PASS"
