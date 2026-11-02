#!/usr/bin/env bash
# tier: 1
set -euo pipefail
source "$(dirname "$0")/../harness/lib.sh"
hf_spawn
TMP="$(mktemp -d -t v5daily-XXXXXX)"
trap 'hf_teardown; rm -rf "$TMP"' EXIT
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t
mkrepo() { # name version
    git init -q --bare "$TMP/$1.git"
    git init -q -b main "$TMP/$1"
    printf '[package]\nname = "%s"\nversion = "%s"\n\n[lib]\npath = "lib.rs"\n' "$1" "$2" > "$TMP/$1/Cargo.toml"
    : > "$TMP/$1/lib.rs"
    git -C "$TMP/$1" remote add origin "$TMP/$1.git"
    git -C "$TMP/$1" add .
    git -C "$TMP/$1" commit -qm seed
    git -C "$TMP/$1" push -qu origin main
}
mkdir -p "$HF_CONFIG/workspaces" "$HF_CONFIG/orgs"
printf 'provider_map: {}\n' > "$HF_CONFIG/config.yaml"
mkrepo alpha 0.1.0
mkrepo beta 0.2.0
cat > "$HF_CONFIG/orgs/demo.yaml" <<YAML
name: demo
forge:
  provider: github
  credentials: []
repos:
  - name: alpha
    remotes:
      - url: $TMP/alpha.git
  - name: beta
    remotes:
      - url: $TMP/beta.git
YAML
cat > "$HF_CONFIG/workspaces/main.yaml" <<YAML
name: main
path: $TMP
repos:
  - demo/alpha
  - demo/beta
YAML

# Worktrees for a ticket, in the named repos only.
out=$(hf_cmd workspaces worktree --name main --ticket HF-1 --repos alpha --dry-run true)
echo "$out" | hf_assert_event '.status == "would"'
[[ ! -e "$TMP/wt-HF-1" ]]
out=$(hf_cmd workspaces worktree --name main --ticket HF-1 --repos alpha)
echo "$out" | hf_assert_event '.type == "workspace_git_summary" and .ok == 1 and .errored == 0'
[[ "$(git -C "$TMP/wt-HF-1/alpha" branch --show-current)" == "HF-1" ]]
[[ ! -e "$TMP/wt-HF-1/beta" ]]
# Asking again refuses; a bad ticket and a stranger too.
out=$(hf_cmd workspaces worktree --name main --ticket HF-1 --repos alpha)
echo "$out" | hf_assert_event '.type == "workspace_git_summary" and .errored == 1'
hf_cmd workspaces worktree --name main --ticket ../x --repos alpha | hf_assert_event '.type == "error"'
hf_cmd workspaces worktree --name main --ticket HF-2 --repos nope | hf_assert_event '.type == "error"'

# Orient sees worktrees and dirt.
touch "$TMP/wt-HF-1/alpha/new.txt"
out=$(hf_cmd workspaces orient --name main)
echo "$out" | hf_assert_event '.type == "orient_checkout" and .row.worktree == true and .row.dirty == 1 and .row.upstream == null'
out=$(hf_cmd workspaces orient --name main --dirty-only true)
[[ "$(jq -s 'map(select(.type == "orient_checkout")) | length' <<<"$out")" == 1 ]]
out=$(hf_cmd workspaces orient --name main --table true)
echo "$out" | hf_assert_event '.type == "orient_table" and (.text | contains("alpha (wt)"))'

# Removal keeps uncommitted work, then goes.
out=$(hf_cmd workspaces worktree --name main --ticket HF-1 --repos alpha --remove true)
echo "$out" | hf_assert_event '.type == "workspace_git_summary" and .errored == 1'
rm "$TMP/wt-HF-1/alpha/new.txt"
out=$(hf_cmd workspaces worktree --name main --ticket HF-1 --repos alpha --remove true)
echo "$out" | hf_assert_event '.type == "workspace_git_summary" and .ok == 1'
[[ ! -e "$TMP/wt-HF-1" ]]
git -C "$TMP/alpha" rev-parse -q --verify refs/heads/HF-1 >/dev/null
echo PASS
