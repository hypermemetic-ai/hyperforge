#!/usr/bin/env bash
# tier: 1 (bump + release-without-publish)
# tier: 2 for actual publish flow — skipped when HF_V5_TEST_CONFIG_DIR not set.
# V5PARITY-10 acceptance: build.{bump, publish, release, release_all}.
set -euo pipefail
source "$(dirname "$0")/../harness/lib.sh"

hf_spawn

TMP="$(mktemp -d -t v5prty10-XXXXXX)"
REMOTE="$TMP/remote.git"
REPO="$TMP/repo"

# Bare remote so release() can push.
git init -q --bare "$REMOTE"

# Checkout with a Cargo.toml at 0.1.0.
git init -q "$REPO"
git -C "$REPO" -c user.email=t@t -c user.name=t commit --allow-empty -m initial -q
git -C "$REPO" branch -m main 2>/dev/null || true
git -C "$REPO" remote add origin "$REMOTE"
cat > "$REPO/Cargo.toml" <<'TOML'
[package]
name = "widget"
version = "0.1.0"
TOML
git -C "$REPO" -c user.email=t@t -c user.name=t add Cargo.toml
git -C "$REPO" -c user.email=t@t -c user.name=t commit -m seed -q
git -C "$REPO" -c user.email=t@t -c user.name=t push -q origin main

# Register org + workspace.
mkdir -p "$HF_CONFIG/workspaces" "$HF_CONFIG/orgs"
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
# The workspace member dir is `widget`; symlink from `$REPO` to
# `$TMP/widget` so the workspace path resolves.
ln -s "$REPO" "$TMP/widget"
# Set committer identity at repo level so the bump commit succeeds.
git -C "$REPO" config user.email t@t
git -C "$REPO" config user.name t

# --- build.bump patch: 0.1.0 → 0.1.1 ---
out=$(hf_cmd build bump --org demo --name widget --bump patch)
echo "$out" | hf_assert_event '.type == "version_bumped" and .old == "0.1.0" and .new == "0.1.1"'
grep -q 'version = "0.1.1"' "$REPO/Cargo.toml"
git -C "$REPO" rev-parse v0.1.1 >/dev/null

# --- build.bump --to 1.5.0 (exact target) ---
out=$(hf_cmd build bump --org demo --name widget --to 1.5.0)
echo "$out" | hf_assert_event '.type == "version_bumped" and .new == "1.5.0"'
grep -q 'version = "1.5.0"' "$REPO/Cargo.toml"

# --- build.release without --channel: bumps, pushes, tags, emits release_created ---
out=$(hf_cmd build release --org demo --name widget --bump minor)
echo "$out" | hf_assert_event '.type == "version_bumped" and .new == "1.6.0"'
echo "$out" | hf_assert_event '.type == "release_created" and .tag == "v1.6.0"'
# And the tag landed on the remote:
git -C "$REMOTE" tag --list | grep -q v1.6.0

# --- build.release_all ---
out=$(hf_cmd build release_all --name main)
echo "$out" | hf_assert_event '.type == "release_summary" and .total == 1 and .ok == 1'

# --- build.publish: tier-2 skip unless a cargo/token secret exists ---
if [[ -f "$HF_CONFIG/secrets.yaml" ]] && grep -q '^cargo/token:' "$HF_CONFIG/secrets.yaml" 2>/dev/null; then
    out=$(hf_cmd build publish --org demo --name widget --channel crates.io --execute true)
    echo "$out" | hf_assert_event '.type == "package_published" or (.type == "error" and (.code == "publish_failed" or .code == "missing_token"))'
else
    out=$(hf_cmd build publish --org demo --name widget --channel crates.io --execute true)
    echo "$out" | hf_assert_event '.type == "error" and .code == "missing_token"'
fi

rm -rf "$TMP"
hf_teardown
echo "PASS"
