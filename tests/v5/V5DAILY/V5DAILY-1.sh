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

# One-repo bump: dry run writes nothing, commit=false stops at the files.
before="$(git -C "$TMP/alpha" rev-parse HEAD)"
out=$(hf_cmd build bump --org demo --name alpha --bump patch --dry-run true)
echo "$out" | hf_assert_event '.type == "chain_note"'
grep -q 'version = "0.1.0"' "$TMP/alpha/Cargo.toml"
[[ -z "$(git -C "$TMP/alpha" status --porcelain)" ]]
[[ -z "$(git -C "$TMP/alpha" tag)" ]]

out=$(hf_cmd build bump --org demo --name alpha --bump patch --commit false)
echo "$out" | hf_assert_event '.type == "version_bumped" and .new == "0.1.1"'
grep -q 'version = "0.1.1"' "$TMP/alpha/Cargo.toml"
[[ "$(git -C "$TMP/alpha" rev-parse HEAD)" == "$before" ]]
[[ -z "$(git -C "$TMP/alpha" tag)" ]]

# Over an uncommitted manifest it refuses.
out=$(hf_cmd build bump --org demo --name alpha --bump patch)
echo "$out" | hf_assert_event '.type == "error" and .code == "dirty"'
git -C "$TMP/alpha" checkout -q Cargo.toml

# A bump brings a tracked lockfile along in its commit.
(cd "$TMP/alpha" && cargo generate-lockfile --offline -q 2>/dev/null)
git -C "$TMP/alpha" add Cargo.lock
git -C "$TMP/alpha" commit -qm lock
out=$(hf_cmd build bump --org demo --name alpha --bump patch)
echo "$out" | hf_assert_event '.type == "version_bumped" and .new == "0.1.1"'
git -C "$TMP/alpha" show --stat --format= HEAD | grep -q Cargo.lock
[[ -z "$(git -C "$TMP/alpha" status --porcelain)" ]]
[[ "$(git -C "$TMP/alpha" tag)" == "v0.1.1" ]]
echo PASS
