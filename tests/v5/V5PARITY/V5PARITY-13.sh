#!/usr/bin/env bash
# tier: 1 (some slices need tier-2 credentials; they record and move on)
# V5PARITY-13: checkpoint — verify v5 covers every v4 capability the
# epic pinned. Tier-2 stories (U1/U5/U6) SKIP cleanly without
# HF_V5_TEST_CONFIG_DIR; tier-1 stories run always.
set -euo pipefail
source "$(dirname "$0")/../harness/lib.sh"

cd "$(dirname "$0")/../../.."

results=()
record() { results+=("$1"); echo "STORY $1"; }

# --- DRY + harness invariants inherited from V5PARITY-12 ---
# We capture V5LIFECYCLE-11's output to a temp file rather than piping
# directly — `set -o pipefail` can mask the grep result otherwise.
dry_out="$(mktemp)"
bash tests/v5/V5LIFECYCLE/V5LIFECYCLE-11.sh >"$dry_out" 2>&1 || true
if grep -q 'DRY:command-git green' "$dry_out"; then
    record "DRY: grep invariants all green"
else
    record "DRY: red — V5LIFECYCLE-11 did not confirm command-git"
fi
rm -f "$dry_out"

# --- U7: begin on empty config (tier 1) ---
hf_spawn
out=$(hf_cmd begin)
if echo "$out" | hf_assert_event '.type == "begin_next_step"' >/dev/null 2>&1 \
    && [[ -f "$HF_CONFIG/config.yaml" ]]; then
    record "U7 green: begin produces usable starting state"
else
    record "U7 red: begin did not initialize config"
fi
hf_teardown

# --- U4: repo_sizes aggregate (tier 1) ---
hf_spawn
TMP="$(mktemp -d -t v5prty13-XXXXXX)"
for name in alpha beta gamma; do
    mkdir -p "$TMP/$name"
    printf 'x%.0s' {1..100} > "$TMP/$name/file.txt"
done
mkdir -p "$HF_CONFIG/orgs" "$HF_CONFIG/workspaces"
cat > "$HF_CONFIG/config.yaml" <<'YAML'
provider_map: {}
YAML
cat > "$HF_CONFIG/orgs/demo.yaml" <<YAML
name: demo
forge:
  provider: github
  credentials: []
repos:
  - name: alpha
    remotes:
      - url: "file://$TMP/alpha"
  - name: beta
    remotes:
      - url: "file://$TMP/beta"
  - name: gamma
    remotes:
      - url: "file://$TMP/gamma"
YAML
cat > "$HF_CONFIG/workspaces/main.yaml" <<YAML
name: main
path: $TMP
repos:
  - demo/alpha
  - demo/beta
  - demo/gamma
YAML
out=$(hf_cmd workspaces repo_sizes --name main)
if echo "$out" | hf_assert_event '.type == "workspace_analytics_summary" and .metric == "size" and .total == 3 and .ok == 3' >/dev/null 2>&1; then
    record "U4 green: workspace repo_sizes aggregate"
else
    record "U4 red: repo_sizes aggregate missing or wrong"
fi
rm -rf "$TMP"
hf_teardown

# --- U6-tier1: build.unify + build.release (tier 1 slice) ---
hf_spawn
TMP="$(mktemp -d -t v5prty13-bld-XXXXXX)"
REMOTE="$TMP/remote.git"
REPO="$TMP/widget"
git init -q --bare "$REMOTE"
git init -q "$REPO"
git -C "$REPO" config user.email t@t
git -C "$REPO" config user.name t
git -C "$REPO" commit --allow-empty -m initial -q
git -C "$REPO" branch -m main 2>/dev/null || true
git -C "$REPO" remote add origin "$REMOTE"
cat > "$REPO/Cargo.toml" <<'TOML'
[package]
name = "widget"
version = "0.1.0"
TOML
git -C "$REPO" add Cargo.toml
git -C "$REPO" commit -m seed -q
git -C "$REPO" push -q origin main
mkdir -p "$HF_CONFIG/orgs" "$HF_CONFIG/workspaces"
cat > "$HF_CONFIG/config.yaml" <<'YAML'
provider_map: {}
YAML
cat > "$HF_CONFIG/orgs/demo.yaml" <<YAML
name: demo
forge:
  provider: github
  credentials: []
repos:
  - name: widget
    remotes:
      - url: $REMOTE
YAML
cat > "$HF_CONFIG/workspaces/main.yaml" <<YAML
name: main
path: $TMP
repos:
  - demo/widget
YAML
out=$(hf_cmd build unify --name main)
if echo "$out" | hf_assert_event '.type == "package_manifest" and .name == "widget" and .version == "0.1.0"' >/dev/null 2>&1; then
    record "U6a green: build.unify produces unified manifest"
else
    record "U6a red: build.unify failed"
fi
out=$(hf_cmd build release --org demo --name widget --bump patch --execute true)
if echo "$out" | hf_assert_event '.type == "release_created" and .tag == "v0.1.1"' >/dev/null 2>&1; then
    record "U6b green: build.release completes end-to-end (local)"
else
    record "U6b red: build.release did not reach release_created"
fi
rm -rf "$TMP"
hf_teardown

# --- Tier-2 stories: require HF_V5_TEST_CONFIG_DIR ---
if [[ -z "${HF_V5_TEST_CONFIG_DIR:-}" || ! -f "${HF_V5_TEST_CONFIG_DIR}/tier2.env" ]]; then
    for s in U1 U2 U3 U5; do
        record "$s yellow: tier-2 config not set"
    done
else
    record "U1 yellow: tier-2 daily-repo-management scripted elsewhere (V5WS-9)"
    record "U2 yellow: tier-2 remote-management scripted elsewhere (V5PARITY-6)"
    record "U3 yellow: tier-2 lifecycle scripted elsewhere (V5LIFECYCLE-11)"
    record "U5 yellow: tier-2 auth_check scripted elsewhere (V5PARITY-7)"
fi

# --- Ticket-state audit: every V5PARITY-2..12 is Complete ---
status_issues=0
for n in 2 3 4 5 6 7 8 9 10 11 12; do
    tf="plans/v5/V5PARITY/V5PARITY-$n.md"
    if ! grep -qE '^status: Complete$' "$tf"; then
        record "V5PARITY-$n red: status not Complete"
        status_issues=$((status_issues+1))
    fi
done
if (( status_issues == 0 )); then
    record "tickets: all V5PARITY-2..12 marked Complete"
fi

echo
echo "=== V5PARITY checkpoint map ==="
printf '%s\n' "${results[@]}"

for r in "${results[@]}"; do
    [[ "$r" != *" red"* ]] || { echo "FAIL: at least one story is red"; exit 1; }
done
echo "PASS"
