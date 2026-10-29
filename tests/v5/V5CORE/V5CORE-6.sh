#!/usr/bin/env bash
# tier: 1
set -euo pipefail
source "$(dirname "$0")/../harness/lib.sh"

hf_spawn

# Schema shows an orgs child backed by OrgsService with zero methods/children.
schema=$(hf_cmd __schema__ 2>/dev/null || hf_cmd)

echo "$schema" | hf_assert_event '.path == "orgs" and .activation == "OrgsService"'
echo "$schema" | hf_assert_event '.path == "orgs" and (.methods | length == 0)'
echo "$schema" | hf_assert_event '.path == "orgs" and (.children | length == 0)'

# Invoking a nonexistent method under orgs emits an error event, no crash.
set +e
out=$(hf_cmd orgs does_not_exist 2>&1)
set -e
echo "$out" | hf_assert_event '.type == "error"'

# Daemon is still responsive after the error.
hf_cmd status | hf_assert_event '.type == "status"'

hf_teardown
echo "PASS"
