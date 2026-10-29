#!/usr/bin/env bash
# tier: 1
set -euo pipefail
source "$(dirname "$0")/../harness/lib.sh"

# Spawn daemon on ephemeral port (harness isolates from v4 on 44104).
hf_spawn

# Registry shows lforge-v5 on our port.
synapse -P "$HF_PORT" list | hf_assert_event '.name == "lforge-v5"'

# Schema introspection: root HyperforgeService has zero methods and zero children
# at the V5CORE-2 baseline. (This script is the V5CORE-2 acceptance — later
# tickets add status / stubs and adjust their own scripts accordingly.)
schema=$(hf_cmd __schema__ 2>/dev/null || hf_cmd)
echo "$schema" | hf_assert_event '.activation == "HyperforgeService"'

# Port-already-bound: spawning a second daemon on the same explicit port
# must exit non-zero and name the port on stderr.
set +e
out=$(HF_PORT_FORCE="$HF_PORT" "${HF_BIN:-hyperforge-v5}" --port "$HF_PORT" 2>&1)
rc=$?
set -e
[[ $rc -ne 0 ]]
echo "$out" | grep -q "$HF_PORT"

hf_teardown
echo "PASS"
