#!/usr/bin/env bash
# tier: 1
set -euo pipefail
source "$(dirname "$0")/../harness/lib.sh"

hf_spawn

schema=$(hf_cmd __schema__ 2>/dev/null || hf_cmd)

echo "$schema" | hf_assert_event '.path == "repos" and .activation == "ReposService"'
echo "$schema" | hf_assert_event '.path == "repos" and (.methods | length == 0)'
echo "$schema" | hf_assert_event '.path == "repos" and (.children | length == 0)'

set +e
out=$(hf_cmd repos does_not_exist 2>&1)
set -e
echo "$out" | hf_assert_event '.type == "error"'

hf_cmd status | hf_assert_event '.type == "status"'

hf_teardown
echo "PASS"
