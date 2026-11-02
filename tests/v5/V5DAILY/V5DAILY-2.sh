#!/usr/bin/env bash
# tier: 1
# Every flag begin suggests is one the method takes.
set -euo pipefail
source "$(dirname "$0")/../harness/lib.sh"
hf_spawn
trap hf_teardown EXIT
out=$(hf_cmd begin)
n=0
while IFS= read -r msg; do
    cmd="$(sed -E 's/^[^`]*`([^`]*)`.*$/\1/' <<<"$msg")"
    read -r -a words <<<"$cmd"
    help="$("$__HF_SYNAPSE_BIN" -P "$HF_PORT" lforge-v5 hyperforge "${words[0]}" "${words[1]}" --help 2>&1)"
    for w in "${words[@]:2}"; do
        if [[ "$w" == --* ]]; then
            grep -q -- "$w " <<<"$help" || { echo "FAIL: $cmd: $w not in help"; exit 1; }
            n=$((n+1))
        fi
    done
done < <(jq -r 'select(.type == "begin_next_step") | .message' <<<"$out")
(( n >= 9 )) || { echo "FAIL: only $n flags seen"; exit 1; }
jq -e -s 'map(select(.type == "begin_next_step").action) | index("workspaces.discover")' <<<"$out" >/dev/null
echo PASS
