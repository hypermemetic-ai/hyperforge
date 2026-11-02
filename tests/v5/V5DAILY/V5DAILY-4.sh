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

# link: alpha builds against beta's checkout; nothing tracked changes.
printf '[dependencies]\nbeta = "0.2"\n' >> "$TMP/alpha/Cargo.toml"
git -C "$TMP/alpha" add . ; git -C "$TMP/alpha" commit -qm dep
out=$(hf_cmd build link --name main --path "$TMP/alpha" --include beta)
echo "$out" | hf_assert_event '.type == "linked" and .crates == ["beta"]'
(cd "$TMP/alpha" && cargo check --offline -q 2>&1)
[[ -f "$TMP/alpha/.cargo/config.toml" ]]
git -C "$TMP/alpha" check-ignore -q .cargo/config.toml
git -C "$TMP/alpha" add -A; git -C "$TMP/alpha" commit -qm lock
# Unlink with a clean lock restores it, and the config goes.
hf_cmd build link --name main --path "$TMP/alpha" --include beta | hf_assert_event '.type == "linked"'
printf '# moved\n' >> "$TMP/alpha/Cargo.lock"
hf_cmd build unlink --path "$TMP/alpha" | hf_assert_event '.type == "unlinked"'
! grep -q "# moved" "$TMP/alpha/Cargo.lock"
[[ ! -e "$TMP/alpha/.cargo" ]]
# A config of someone else's is left alone.
mkdir "$TMP/beta/.cargo"; echo '[build]' > "$TMP/beta/.cargo/config.toml"
hf_cmd build link --name main --path "$TMP/beta" --include alpha | hf_assert_event '.type == "error" and .code == "conflict"'
hf_cmd build unlink --path "$TMP/beta" | hf_assert_event '.type == "error" and .code == "conflict"'
grep -q '^\[build\]$' "$TMP/beta/.cargo/config.toml"
echo PASS
