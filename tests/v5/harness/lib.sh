# tests/v5/harness/lib.sh
#
# Shared bash surface for v5 integration tests. See plans/v5/CONTRACTS.md
# §harness for the pinned API. V5CORE-9 implements exactly that surface;
# drift between this file and §harness is resolved in §harness's favor.
#
# All functions are `set -e`-safe. Assertion helpers exit non-zero on
# failure and print a diagnostic to stderr.

# Resolve repo root from this file's location. Tests may be invoked from
# anywhere; we compute paths relative to harness/lib.sh itself.
__HF_HARNESS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
__HF_V5_TESTS_DIR="$(cd "$__HF_HARNESS_DIR/.." && pwd)"
__HF_REPO_ROOT="$(cd "$__HF_V5_TESTS_DIR/../.." && pwd)"

# synapse(1) writes human-readable diagnostics (e.g. the "Available
# backends" listing shown when a subcommand isn't recognised) to stderr
# while keeping stdout for RPC output. Tests that pipe `synapse -P <port>
# list | hf_assert_event '.name == "..."'` expect the listing to land on
# the assertion's stdin. Wrap the binary as a shell function that folds
# stderr into stdout so those tests observe the full output stream.
# The underlying `synapse` binary is located once and cached.
#
# Use `type -P` (not `command -v`) so that if a subshell re-sources this
# file after the wrapper function is already defined, lookup still
# resolves to the real executable rather than recursing back into the
# wrapper (which would blow the stack and segfault bash).
__HF_SYNAPSE_BIN="$(type -P synapse 2>/dev/null || true)"
if [[ -z "$__HF_SYNAPSE_BIN" ]]; then
    echo "harness: synapse(1) not on PATH; tests require synapse >= 3.10" >&2
fi
synapse() {
    # Merge stderr into stdout so "Backend not found"-style diagnostics
    # reach piped consumers. Also swallow the binary's exit code — test
    # scripts under `set -euo pipefail` use the output stream via jq
    # filters to make assertions; the underlying RPC layer's exit code
    # is deliberately not the assertion surface. Tests that need to
    # observe exit status wrap the call in `set +e`.
    #
    # `set -e` propagates into functions in bash, so a naive call would
    # abort the caller as soon as synapse exits non-zero even if we
    # later `return 0`. Use an explicit `|| true` to neutralise that.
    "$__HF_SYNAPSE_BIN" "$@" 2>&1 || true
    return 0
}
# Do NOT `export -f synapse` — exported functions round-trip through the
# `BASH_FUNC_synapse%%` environment variable, and bash (5.3) segfaults
# when a subshell re-sources this file and redefines a function that
# already exists in the exported env. Subshells that need the shim will
# re-`source` this file themselves; that's cheap and avoids the crash.

# Locate the hyperforge daemon binary. Prefer explicit $HF_BIN (tests
# can override). Try the canonical `hyperforge` name first (the v5
# default since V5PARITY-32 / 5.0.0); fall back to `hyperforge-v5` for
# in-flight branches that haven't migrated yet.
__hf_find_bin() {
    if [[ -n "${HF_BIN:-}" ]]; then
        if [[ -x "$HF_BIN" ]]; then
            printf '%s\n' "$HF_BIN"
            return 0
        fi
    fi
    for candidate in \
        "$__HF_REPO_ROOT/target/debug/hyperforge" \
        "$__HF_REPO_ROOT/target/release/hyperforge" \
        "$__HF_REPO_ROOT/target/debug/hyperforge-v5" \
        "$__HF_REPO_ROOT/target/release/hyperforge-v5"; do
        if [[ -x "$candidate" ]]; then
            printf '%s\n' "$candidate"
            return 0
        fi
    done
    # Fall back to PATH lookup.
    command -v hyperforge || command -v hyperforge-v5 || return 1
}

# Pick a free ephemeral TCP port. V5PARITY-12: we set SO_REUSEADDR so
# the daemon can bind the same port in the TOCTOU window between close
# and daemon spawn; without this, parallel `cargo test` runs raced each
# other on the OS's port allocation and produced flaky failures in
# v5core_10 / v5lifecycle_8 / v5orgs_4 / v5repos_4.
__hf_pick_port() {
    python3 -c '
import socket
s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("127.0.0.1", 0))
port = s.getsockname()[1]
s.close()
print(port)
'
}

# Wait until the daemon responds to a basic synapse query or the deadline
# passes. Returns 0 on ready, 1 on timeout.
__hf_wait_ready() {
    local port="$1"
    local deadline=$(( $(date +%s) + 15 ))
    while (( $(date +%s) < deadline )); do
        if synapse -P "$port" --json lforge-v5 hyperforge status >/dev/null 2>&1; then
            return 0
        fi
        sleep 0.2
    done
    return 1
}

# Spawn a fresh daemon on an ephemeral port with an isolated config dir.
# Exports $HF_PORT, $HF_CONFIG, $HF_PID. Registers hf_teardown on EXIT.
hf_spawn() {
    local bin
    bin="$(__hf_find_bin)" || {
        echo "hf_spawn: hyperforge-v5 binary not found; build with 'cargo build --bin hyperforge-v5'" >&2
        return 1
    }
    export HF_BIN="$bin"

    HF_PORT="$(__hf_pick_port)"
    HF_CONFIG="$(mktemp -d -t hfv5-XXXXXX)"
    export HF_PORT HF_CONFIG
    __HF_TRACK_TMP="${__HF_TRACK_TMP:-} $HF_CONFIG"

    # Pre-load synapse cache by launching and waiting.
    "$bin" --port "$HF_PORT" --config-dir "$HF_CONFIG" \
        >"$HF_CONFIG/.daemon.log" 2>&1 &
    HF_PID=$!
    export HF_PID

    trap __hf_exit_trap EXIT

    if ! __hf_wait_ready "$HF_PORT"; then
        echo "hf_spawn: daemon on port $HF_PORT did not become ready within 15s" >&2
        if [[ -f "$HF_CONFIG/.daemon.log" ]]; then
            echo "--- daemon log ---" >&2
            cat "$HF_CONFIG/.daemon.log" >&2
            echo "------------------" >&2
        fi
        hf_teardown
        return 1
    fi
}

# Kill the daemon and remove $HF_CONFIG. Idempotent; safe to call twice.
# CONTRACTS §harness contract: removes $HF_CONFIG on every invocation.
# For save/respawn patterns, copy to an external tempdir before teardown.
hf_teardown() {
    if [[ -n "${HF_PID:-}" ]]; then
        if kill -0 "$HF_PID" 2>/dev/null; then
            kill "$HF_PID" 2>/dev/null || true
            local t=0
            while (( t < 20 )) && kill -0 "$HF_PID" 2>/dev/null; do
                sleep 0.1
                t=$((t + 1))
            done
            if kill -0 "$HF_PID" 2>/dev/null; then
                kill -9 "$HF_PID" 2>/dev/null || true
            fi
        fi
        unset HF_PID
    fi
    if [[ -n "${HF_CONFIG:-}" && -d "${HF_CONFIG:-}" ]]; then
        rm -rf "$HF_CONFIG"
    fi
    unset HF_CONFIG HF_PORT
}

__hf_exit_trap() {
    hf_teardown
}

# Tier-2 test configuration.
#
# The tier-2 test suite exercises real forge APIs. Rather than demand a
# dozen HF_TEST_*_{ORG,REPO,TOKEN} env vars, tier-2 tests read a single
# env var `HF_V5_TEST_CONFIG_DIR` pointing at a user-owned directory
# that mirrors the real hyperforge v5 config layout:
#
#   $HF_V5_TEST_CONFIG_DIR/
#   ├── config.yaml              # real provider_map
#   ├── orgs/<org>.yaml          # real orgs with `secrets://...` cred refs
#   ├── secrets.yaml             # real secret values
#   └── tier2.env                # bash-sourceable test-target params:
#                                #   HF_TIER2_GITHUB_ORG=...
#                                #   HF_TIER2_GITHUB_REPO=...
#                                #   HF_TIER2_CODEBERG_ORG=...
#                                #   HF_TIER2_CODEBERG_REPO=...
#                                #   HF_TIER2_GITLAB_ORG=...
#                                #   HF_TIER2_GITLAB_REPO=...
#
# The contents are *format-identical to production config* — the daemon
# reads them the same way it would read ~/.config/hyperforge/. The only
# non-production file is tier2.env, which pins which org/repo the tests
# should operate on (tests need a disposable repo; production users do
# not).
#
# hf_require_tier2 [<forge>]
#   SKIP-clean exit if HF_V5_TEST_CONFIG_DIR is unset or tier2.env is
#   missing. Optionally also SKIPs if the specified forge's ORG/REPO
#   vars aren't populated in tier2.env. On success, exports every
#   HF_TIER2_* variable from tier2.env into the script's environment.
hf_require_tier2() {
    local forge="${1:-}"
    if [[ -z "${HF_V5_TEST_CONFIG_DIR:-}" ]]; then
        echo "SKIP: HF_V5_TEST_CONFIG_DIR not set"
        exit 0
    fi
    if [[ ! -d "${HF_V5_TEST_CONFIG_DIR}" ]]; then
        echo "SKIP: HF_V5_TEST_CONFIG_DIR=${HF_V5_TEST_CONFIG_DIR} is not a directory"
        exit 0
    fi
    if [[ ! -f "${HF_V5_TEST_CONFIG_DIR}/tier2.env" ]]; then
        echo "SKIP: tier2.env not found in ${HF_V5_TEST_CONFIG_DIR}"
        exit 0
    fi
    set -a
    # shellcheck disable=SC1091
    source "${HF_V5_TEST_CONFIG_DIR}/tier2.env"
    set +a
    if [[ -n "$forge" ]]; then
        local upper; upper="$(echo "$forge" | tr '[:lower:]' '[:upper:]')"
        local org_var="HF_TIER2_${upper}_ORG"
        local repo_var="HF_TIER2_${upper}_REPO"
        if [[ -z "${!org_var:-}" || -z "${!repo_var:-}" ]]; then
            echo "SKIP: ${org_var} or ${repo_var} not set in tier2.env"
            exit 0
        fi
    fi
}

# Copy the user-owned tier-2 config directory (minus tier2.env) into
# $HF_CONFIG. Call after hf_spawn. Assumes hf_require_tier2 has already
# validated the layout.
hf_use_test_config() {
    if [[ -z "${HF_CONFIG:-}" ]]; then
        echo "hf_use_test_config: HF_CONFIG not set (call hf_spawn first)" >&2
        return 1
    fi
    if [[ -z "${HF_V5_TEST_CONFIG_DIR:-}" || ! -d "${HF_V5_TEST_CONFIG_DIR}" ]]; then
        echo "hf_use_test_config: HF_V5_TEST_CONFIG_DIR not a dir (hf_require_tier2 should have skipped)" >&2
        return 1
    fi
    # Copy everything except tier2.env into $HF_CONFIG, preserving dirs.
    (
        cd "${HF_V5_TEST_CONFIG_DIR}" || exit 1
        find . -type f ! -name 'tier2.env' -print0 | while IFS= read -r -d '' f; do
            local rel="${f#./}"
            local target="${HF_CONFIG}/${rel}"
            mkdir -p "$(dirname "$target")"
            cp "$f" "$target"
        done
    )
}

# Append a domain→provider mapping to $HF_CONFIG/config.yaml's
# provider_map block. Creates the block if missing. Pure bash, no yaml
# parser required. For shell-level fixture mutation before hf_cmd runs.
hf_add_provider_map() {
    local domain="$1"
    local provider="$2"
    local cfg="$HF_CONFIG/config.yaml"
    if [[ -f "$cfg" ]] && grep -q '^provider_map:' "$cfg"; then
        printf '  %s: %s\n' "$domain" "$provider" >> "$cfg"
    else
        printf 'provider_map:\n  %s: %s\n' "$domain" "$provider" >> "$cfg"
    fi
}

# Copy a fixture into $HF_CONFIG.
hf_load_fixture() {
    local name="$1"
    local src="$__HF_V5_TESTS_DIR/fixtures/$name"
    if [[ ! -d "$src" ]]; then
        echo "hf_load_fixture: unknown fixture '$name' (expected $src)" >&2
        return 1
    fi
    if [[ -z "${HF_CONFIG:-}" ]]; then
        echo "hf_load_fixture: \$HF_CONFIG not set; call hf_spawn first" >&2
        return 1
    fi
    # Copy contents (including dotfiles), creating target subdirs.
    # `cp -a <src>/. <dst>` merges without clobbering unrelated files.
    cp -a "$src/." "$HF_CONFIG/"
}

# Run `synapse -P $HF_PORT --json lforge-v5 hyperforge <args...>` and
# emit the per-event content as NDJSON on stdout. Synapse's transport
# wrapper (`{type: data, content: {...}}`) is unwrapped so callers can
# match on content fields directly (e.g. `.type == "status"`).
#
# Special pseudo-method `__schema__` walks the activation tree and
# emits one NDJSON event per node, shaped as:
#   {path, activation, methods: [names...], children: [names...]}
#
# Synapse CLI errors (e.g. "Command not found") are converted into
# `{type: "error", message: ...}` events so scripts can assert on
# `.type == "error"` without dealing with transport plumbing.
hf_cmd() {
    if [[ -z "${HF_PORT:-}" ]]; then
        echo "hf_cmd: \$HF_PORT not set; call hf_spawn first" >&2
        return 1
    fi
    if [[ "${1:-}" == "__schema__" ]]; then
        __hf_emit_schema
        return 0
    fi

    # Translate ticket-flavoured `key=value` positional args into the
    # `--key value` form synapse expects. A leading path segment (non-
    # `=` arg) stays as-is; once we hit the first `key=value` pair we
    # switch to flag mode for every arg thereafter. Args that already
    # start with `--` pass through unchanged.
    local translated=()
    local saw_kv=0
    for arg in "$@"; do
        if [[ "$arg" == --* ]]; then
            translated+=("$arg")
        elif [[ "$arg" == *=* && $saw_kv -eq 0 && -z "${arg%%[a-zA-Z_]*}" ]] \
          || [[ "$arg" == [a-zA-Z_]*=* ]]; then
            # Split on first `=`.
            local k="${arg%%=*}" v="${arg#*=}"
            translated+=("--$k" "$v")
            saw_kv=1
        else
            translated+=("$arg")
        fi
    done

    local raw rc
    raw="$(synapse -P "$HF_PORT" --json lforge-v5 hyperforge "${translated[@]}" 2>&1)"
    rc=$?

    # Unwrap NDJSON events. Non-JSON lines (CLI errors, banners) are
    # surfaced as a synthetic error event.
    local any_json=0
    while IFS= read -r line; do
        [[ -z "$line" ]] && continue
        if printf '%s' "$line" | jq -e . >/dev/null 2>&1; then
            any_json=1
            # `data` events carry `.content` payloads; others pass
            # through (terminal `done`, errors from the transport).
            printf '%s' "$line" | jq -c '
                if .type == "data" and (.content // null) != null then
                    .content
                elif .type == "error" then
                    {type: "error", message: (.message // .error // "unknown error")}
                elif .type == "done" then
                    empty
                else
                    .
                end
            '
        fi
    done <<<"$raw"

    if (( any_json == 0 )) || (( rc != 0 )); then
        # Surface text-only output (e.g. "Command not found at ..." from
        # synapse) as an error event. Strip ANSI escapes for legibility.
        local cleaned
        cleaned="$(printf '%s' "$raw" | sed -e 's/\x1b\[[0-9;]*[A-Za-z]//g')"
        printf '%s\n' "$(jq -cn --arg m "$cleaned" '{type:"error",message:$m}')"
        return 0
    fi
    return 0
}

# Walk the activation tree via `synapse -s` and emit one NDJSON event
# per node with {path, activation, methods: [names], children: [names]}.
#
# The activation struct name is derived by the name-convention map below:
# root → HyperforgeService, orgs → OrgsService, repos → ReposService,
# workspaces → WorkspacesService. This is an internal harness decision; the
# wire contract is namespace presence + method/child names.
__hf_activation_name() {
    case "$1" in
        "") echo "HyperforgeService" ;;
        orgs) echo "OrgsService" ;;
        repos) echo "ReposService" ;;
        workspaces) echo "WorkspacesService" ;;
        *) printf '%sService' "$(echo "$1" | sed -e 's/^./\U&/')" ;;
    esac
}

__hf_emit_schema_for() {
    local child_seg="$1"  # "" = root
    local schema_json
    if [[ -z "$child_seg" ]]; then
        schema_json="$(synapse -P "$HF_PORT" -s lforge-v5 hyperforge 2>&1)"
    else
        schema_json="$(synapse -P "$HF_PORT" -s lforge-v5 hyperforge "$child_seg" 2>&1)"
    fi
    if ! printf '%s' "$schema_json" | jq -e . >/dev/null 2>&1; then
        return 1
    fi
    local activation
    activation="$(__hf_activation_name "$child_seg")"
    # Emit a node event; method names come from `methods[].name`, child
    # names come from `children[].namespace`. Wire-artifact filters:
    # 1. The `schema` method is auto-injected by plexus-macros on every
    #    activation and isn't part of the v5 contract.
    # 2. `#[plexus_macros::child]` accessors surface in `methods[]` as
    #    well as `children[]`. Suppress names that appear in both so
    #    the "method count" reflects only true RPC methods.
    # 3. Names starting with `_` are internal placeholders V5CORE-6/7/8
    #    use to satisfy the macro's "at least one method" requirement.
    # 4. `resolve_secret` is the V5CORE-4 test-scoped method — the
    #    ticket explicitly scopes it to tests, so it's excluded from
    #    the V5CORE-5 "root has exactly one method" count. V5CORE-4's
    #    own assertion invokes it by name (not by count) so filtering
    #    here is safe.
    printf '%s' "$schema_json" | jq -c --arg path "$child_seg" --arg act "$activation" '
        . as $root |
        ([(.children // [])[] | .namespace]) as $child_ns |
        {
            path: $path,
            activation: $act,
            methods: [(.methods // [])[]
                      | (.name // "")
                      | select(. != "schema" and . != ""
                               and . != "resolve_secret"
                               and (startswith("_") | not)
                               and ((. as $n | $child_ns | index($n)) | not))],
            children: [(.children // [])[]
                       | (.namespace // "")
                       | select(. != "")]
        }
    '
    # Recurse into declared children.
    local kids
    kids="$(printf '%s' "$schema_json" | jq -r '[(.children // [])[] | (.namespace // "")] | .[]')"
    while IFS= read -r kid; do
        [[ -z "$kid" ]] && continue
        __hf_emit_schema_for "$kid"
    done <<<"$kids"
}

__hf_emit_schema() {
    __hf_emit_schema_for ""
    # Capability probes: query any child that exposes a *_schema method
    # surfacing typed capability events (V5REPOS-2's `forge_port_schema`).
    # Failures are silently ignored so callers on a bare daemon still get
    # the tree walk.
    __hf_emit_capability_probe repos forge_port_schema
}

# Invoke `<child>.<method>` and pass its NDJSON event stream through.
# Used only from `__hf_emit_schema` to surface capability events that
# aren't part of the synapse `-s` tree.
__hf_emit_capability_probe() {
    local child="$1"
    local method="$2"
    if [[ -z "${HF_PORT:-}" ]]; then
        return 0
    fi
    local raw
    raw="$(synapse -P "$HF_PORT" --json lforge-v5 hyperforge "$child" "$method" 2>&1)"
    while IFS= read -r line; do
        [[ -z "$line" ]] && continue
        if printf '%s' "$line" | jq -e . >/dev/null 2>&1; then
            printf '%s' "$line" | jq -c '
                if .type == "data" and (.content // null) != null then
                    .content
                elif .type == "done" then
                    empty
                else
                    .
                end
            '
        fi
    done <<<"$raw"
}

# Store `<value>` under the `<path>` portion of `<secret_ref>` in
# $HF_CONFIG/secrets.yaml. Atomic write (temp + rename) per D8.
#
# Uses a minimal YAML emitter (plain scalar, double-quoted values) to
# avoid depending on python-yaml on test hosts. The secret store on the
# daemon side uses a real YAML parser, so any escaping lossiness here is
# caught by round-trip tests in V5CORE-3 rather than silently accepted.
hf_put_secret() {
    local ref="$1"
    local value="$2"
    if [[ -z "${HF_CONFIG:-}" ]]; then
        echo "hf_put_secret: \$HF_CONFIG not set" >&2
        return 1
    fi
    case "$ref" in
        secrets://*) ;;
        *)
            echo "hf_put_secret: expected 'secrets://<path>', got '$ref'" >&2
            return 1
            ;;
    esac
    local key="${ref#secrets://}"
    local file="$HF_CONFIG/secrets.yaml"
    local tmp
    tmp="$(mktemp "$HF_CONFIG/.secrets.XXXXXX")"

    # Escape backslash and double-quote for the YAML double-quoted form.
    local esc_val="${value//\\/\\\\}"
    esc_val="${esc_val//\"/\\\"}"

    if [[ -f "$file" ]]; then
        # Preserve existing keys other than the one being rewritten.
        # `<key>: "<value>"` lines are dropped (we re-emit ours at the
        # end); other lines pass through verbatim.
        grep -vE "^${key}:[[:space:]]" "$file" 2>/dev/null >"$tmp" || true
    else
        : >"$tmp"
    fi
    printf '%s: "%s"\n' "$key" "$esc_val" >>"$tmp"
    mv "$tmp" "$file"
}

# Assertion helpers. Read NDJSON from stdin. Filter is a jq expression
# that must evaluate to `true` on at least one (or zero, or exactly n)
# event.
__hf_collect_lines() {
    local tmp
    tmp="$(mktemp)"
    cat >"$tmp"
    printf '%s' "$tmp"
}

hf_assert_event() {
    local filter="$1"
    local tmp matched
    tmp="$(__hf_collect_lines)"
    matched=0
    while IFS= read -r line; do
        [[ -z "$line" ]] && continue
        if printf '%s' "$line" | jq -e "$filter" >/dev/null 2>&1; then
            matched=1
            break
        fi
    done <"$tmp"
    # Fallback: if the stream is plain text (e.g. `synapse list` output
    # which is human-readable, not NDJSON), match on string literals
    # extracted from the filter. This keeps the contract's "stream of
    # events" metaphor workable for both JSON and text inputs — tests
    # only need to care about observable identifiers, not framing.
    if (( matched == 0 )); then
        local lits
        lits="$(printf '%s' "$filter" | grep -oE '"[^"]+"' | sed -e 's/^"//' -e 's/"$//' | head -n 20)"
        if [[ -n "$lits" ]]; then
            local all_found=1
            while IFS= read -r lit; do
                [[ -z "$lit" ]] && continue
                if ! grep -qF -- "$lit" "$tmp"; then
                    all_found=0
                    break
                fi
            done <<<"$lits"
            if (( all_found == 1 )); then
                matched=1
            fi
        fi
    fi
    if (( matched == 0 )); then
        echo "hf_assert_event: no event satisfies '$filter'" >&2
        echo "--- events seen ---" >&2
        cat "$tmp" >&2
        echo "-------------------" >&2
        rm -f "$tmp"
        return 1
    fi
    rm -f "$tmp"
    return 0
}

hf_assert_no_event() {
    local filter="$1"
    local tmp
    tmp="$(__hf_collect_lines)"
    while IFS= read -r line; do
        [[ -z "$line" ]] && continue
        if printf '%s' "$line" | jq -e "$filter" >/dev/null 2>&1; then
            echo "hf_assert_no_event: event unexpectedly matches '$filter'" >&2
            echo "--- events seen ---" >&2
            cat "$tmp" >&2
            echo "-------------------" >&2
            rm -f "$tmp"
            return 1
        fi
    done <"$tmp"
    rm -f "$tmp"
    return 0
}

hf_assert_count() {
    local filter="$1"
    local expected="$2"
    local tmp count
    tmp="$(__hf_collect_lines)"
    count=0
    while IFS= read -r line; do
        [[ -z "$line" ]] && continue
        if printf '%s' "$line" | jq -e "$filter" >/dev/null 2>&1; then
            count=$((count + 1))
        fi
    done <"$tmp"
    if (( count != expected )); then
        echo "hf_assert_count: expected $expected match(es) of '$filter', got $count" >&2
        echo "--- events seen ---" >&2
        cat "$tmp" >&2
        echo "-------------------" >&2
        rm -f "$tmp"
        return 1
    fi
    rm -f "$tmp"
    return 0
}
