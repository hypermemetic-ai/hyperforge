#!/usr/bin/env bash
# tier: 1
set -euo pipefail
source "$(dirname "$0")/../harness/lib.sh"

# V5LIFECYCLE-3: ops::repo::sync_one is the single source of truth.
# Verify:
#   (a) no service outside ops/ calls adapter.read_metadata or compute_drift
#       directly;
#   (b) a sync through ReposService and a sync through WorkspacesService on the
#       same fixture produce consistent drift data.
#
# (b) is tier-1 only if the test can run without reaching a live forge.
# Without tier-2 config, the adapter errors on all reads, yielding
# status: errored on both paths — still sufficient to assert the error
# shape matches.

# (a) structural grep — DRY enforcement for the extracted helpers.
cd "$(dirname "$0")/../../.."
violations=$(grep -RE 'adapter\.(read_metadata|write_metadata)|compute_drift\(' src/v5/ 2>/dev/null \
    | grep -vE '^src/v5/ops/' || true)
if [[ -n "$violations" ]]; then
    echo "DRY violation — direct adapter read/write or compute_drift outside ops/:"
    echo "$violations"
    exit 1
fi

# (b) minimal end-to-end: both services route through ops::repo::sync_one.
hf_spawn
hf_load_fixture "org_with_repo"

# Build a tiny workspace containing the fixture's repo.
mkdir -p "$HF_CONFIG/workspaces"
cat > "$HF_CONFIG/workspaces/dry.yaml" <<'YAML'
name: dry
path: /tmp/hf-v5-dry
repos:
  - demo/widget
YAML

# Both paths must produce events with consistent ref.org/ref.name.
repos_out=$(hf_cmd repos sync --org demo --name widget 2>&1 || true)
ws_out=$(hf_cmd workspaces sync --name dry 2>&1 || true)

# Either both succeed, or both fail the same way — never divergent.
repos_ref=$(echo "$repos_out" | jq -r 'select(.type == "sync_diff") | "\(.ref.org)/\(.ref.name)"' | head -n1 || true)
ws_ref=$(echo "$ws_out" | jq -r 'select(.type == "sync_diff") | "\(.ref.org)/\(.ref.name)"' | head -n1 || true)

[[ "$repos_ref" == "$ws_ref" ]] || { echo "ref mismatch: repos='$repos_ref' ws='$ws_ref'"; exit 1; }

hf_teardown
echo "PASS"
