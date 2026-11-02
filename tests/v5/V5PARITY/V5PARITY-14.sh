#!/usr/bin/env bash
# tier: 1
# V5PARITY-14 acceptance: workspaces.{push, status, checkout, commit, tag}
# + dispatch refactor (push_all gone).
set -euo pipefail
source "$(dirname "$0")/../harness/lib.sh"

hf_spawn

TMP="$(mktemp -d -t v5prty14-XXXXXX)"
REMOTE_A="$TMP/alpha.git"
REMOTE_B="$TMP/beta.git"
ALPHA="$TMP/alpha"
BETA="$TMP/beta"

# Two bare remotes + two worktrees pre-cloned.
git init -q --bare "$REMOTE_A"
git init -q --bare "$REMOTE_B"
# Point each bare's HEAD at refs/heads/main so the subsequent clone
# checks out a working tree (otherwise: "remote HEAD refers to
# nonexistent ref" warning + a HEAD-less worktree that breaks `git tag`).
git --git-dir="$REMOTE_A" symbolic-ref HEAD refs/heads/main
git --git-dir="$REMOTE_B" symbolic-ref HEAD refs/heads/main
for r in alpha beta; do
    seed="$TMP/${r}_seed"
    git init -q "$seed"
    git -C "$seed" config user.email t@t
    git -C "$seed" config user.name t
    printf 'seed\n' > "$seed/README.md"
    git -C "$seed" add README.md
    git -C "$seed" commit -q -m initial
    git -C "$seed" branch -m main 2>/dev/null || true
    git -C "$seed" remote add origin "$TMP/${r}.git"
    git -C "$seed" push -q -u origin main
done
git clone -q "$REMOTE_A" "$ALPHA"
git clone -q "$REMOTE_B" "$BETA"
for d in "$ALPHA" "$BETA"; do
    git -C "$d" config user.email t@t
    git -C "$d" config user.name t
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
      - url: $REMOTE_A
  - name: beta
    remotes:
      - url: $REMOTE_B
YAML
cat > "$HF_CONFIG/workspaces/main.yaml" <<YAML
name: main
path: $TMP
repos:
  - demo/alpha
  - demo/beta
YAML

# --- (1) push_all is GONE ---
out=$(hf_cmd workspaces push_all --name main)
echo "$out" | hf_assert_event '.type == "error"'

# --- (2) workspaces.push works ---
# Make a commit in alpha so push has something to send.
printf 'change\n' >> "$ALPHA/README.md"
git -C "$ALPHA" add README.md
git -C "$ALPHA" commit -q -m "alpha update"
out=$(hf_cmd workspaces push --name main)
echo "$out" | hf_assert_event '.type == "member_git_result" and .ref.name == "alpha" and .op == "push" and .status == "ok"'
echo "$out" | hf_assert_event '.type == "workspace_git_summary" and .op == "push" and .total == 2'

# --- (3) workspaces.status — clean tree ---
out=$(hf_cmd workspaces status --name main)
echo "$out" | hf_assert_event '.type == "status_snapshot" and .ref.name == "alpha" and .dirty == false'
echo "$out" | hf_assert_event '.type == "workspace_status_summary" and .total == 2 and .dirty == 0'

# --- (4) status reports dirty ---
echo "noise" > "$BETA/extra.txt"
out=$(hf_cmd workspaces status --name main)
echo "$out" | hf_assert_event '.type == "status_snapshot" and .ref.name == "beta" and .dirty == true and .untracked == 1'
echo "$out" | hf_assert_event '.type == "workspace_status_summary" and .dirty == 1 and .clean == 1'
rm "$BETA/extra.txt"

# --- (5) workspaces.checkout --create ---
out=$(hf_cmd workspaces checkout --name main --branch feat-x --create true)
echo "$out" | hf_assert_event '.type == "member_git_result" and .op == "checkout" and .status == "ok" and .ref.name == "alpha"'
echo "$out" | hf_assert_event '.type == "workspace_git_summary" and .op == "checkout" and .ok == 2'
[[ "$(git -C "$ALPHA" branch --show-current)" == "feat-x" ]]
[[ "$(git -C "$BETA" branch --show-current)" == "feat-x" ]]
# An existing branch is refused and left where it is, never reset.
before="$(git -C "$ALPHA" rev-parse feat-x)"
git -C "$ALPHA" commit -q --allow-empty -m "ahead of the base"
ahead="$(git -C "$ALPHA" rev-parse feat-x)"
git -C "$ALPHA" checkout -q HEAD~1
out=$(hf_cmd workspaces checkout --name main --branch feat-x --create true)
echo "$out" | hf_assert_event '.type == "workspace_git_summary" and .op == "checkout" and .errored == 2'
[[ "$(git -C "$ALPHA" rev-parse feat-x)" == "$ahead" ]]
[[ "$(git -C "$ALPHA" rev-parse HEAD)" == "$before" ]]
git -C "$ALPHA" checkout -q feat-x
git -C "$ALPHA" reset -q --hard "$before"

# --- (6) workspaces.commit with --only_dirty (default) ---
# Stage a change in alpha only.
printf 'edit\n' > "$ALPHA/edit.txt"
git -C "$ALPHA" add edit.txt
out=$(hf_cmd workspaces commit --name main --message "uniform edit")
# alpha commits, beta is skipped (no staged changes).
echo "$out" | hf_assert_event '.type == "member_git_result" and .op == "commit" and .status == "ok" and .ref.name == "alpha"'
echo "$out" | hf_assert_event '.type == "member_git_result" and .op == "commit" and .status == "skipped" and .ref.name == "beta"'

# --- (7) workspaces.tag ---
out=$(hf_cmd workspaces tag --name main --tag v0.1.0)
echo "$out" | hf_assert_event '.type == "member_git_result" and .op == "tag" and .status == "ok" and .ref.name == "alpha"'
echo "$out" | hf_assert_event '.type == "workspace_git_summary" and .op == "tag" and .ok == 2'
git -C "$ALPHA" rev-parse v0.1.0 >/dev/null
git -C "$BETA" rev-parse v0.1.0 >/dev/null

# Tag conflict on second invocation — both members report errored.
out=$(hf_cmd workspaces tag --name main --tag v0.1.0)
echo "$out" | hf_assert_event '.type == "workspace_git_summary" and .op == "tag" and .errored == 2'

# --- (8) Refactor invariant: no string-dispatch helper, no central match ---
cd "$__HF_REPO_ROOT"
hits=$(grep -RE 'fn git_op|"clone" =>' src/v5/workspaces.rs 2>/dev/null || true)
if [[ -n "$hits" ]]; then
    echo "FAIL: string-dispatch leftovers in workspaces.rs:" >&2
    echo "$hits" >&2
    exit 1
fi
echo "refactor: no string-dispatch leftovers"

rm -rf "$TMP"
hf_teardown
echo "PASS"
