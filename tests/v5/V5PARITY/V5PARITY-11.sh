#!/usr/bin/env bash
# tier: 1 (brew-push + distribution publishing are tier-2 and not exercised here).
# V5PARITY-11 acceptance: build.{init_configs, binstall_init, brew_formula,
#   dist_init, dist_show, run, exec}.
set -euo pipefail
source "$(dirname "$0")/../harness/lib.sh"

hf_spawn

TMP="$(mktemp -d -t v5prty11-XXXXXX)"
ALPHA="$TMP/alpha"
BETA="$TMP/beta"

mkdir -p "$ALPHA" "$BETA"
git init -q "$ALPHA"
git init -q "$BETA"
cat > "$ALPHA/Cargo.toml" <<'TOML'
[package]
name = "alpha"
version = "0.1.0"
TOML
cat > "$BETA/Cargo.toml" <<'TOML'
[package]
name = "beta"
version = "0.1.0"
TOML

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
  - name: alpha
    remotes:
      - url: "file://$ALPHA"
  - name: beta
    remotes:
      - url: "file://$BETA"
YAML
cat > "$HF_CONFIG/workspaces/main.yaml" <<YAML
name: main
path: $TMP
repos:
  - demo/alpha
  - demo/beta
YAML

# --- init_configs writes .hyperforge/dist.toml (idempotent) ---
out=$(hf_cmd build init_configs --org demo --name alpha)
echo "$out" | hf_assert_event '.type == "dist_init" and .created == true'
[[ -f "$ALPHA/.hyperforge/dist.toml" ]]

out=$(hf_cmd build init_configs --org demo --name alpha)
echo "$out" | hf_assert_event '.type == "dist_init" and .created == false'

# --- binstall_init adds [package.metadata.binstall] + idempotent ---
out=$(hf_cmd build binstall_init --path "$ALPHA")
echo "$out" | hf_assert_event '.type == "binstall_init" and .modified == true'
grep -q '\[package.metadata.binstall\]' "$ALPHA/Cargo.toml"
# Original fields untouched.
grep -q 'name = "alpha"' "$ALPHA/Cargo.toml"
grep -q 'version = "0.1.0"' "$ALPHA/Cargo.toml"

out=$(hf_cmd build binstall_init --path "$ALPHA")
echo "$out" | hf_assert_event '.type == "binstall_init" and .modified == false'

# --- brew_formula dry-run emits content without writing ---
out=$(hf_cmd build brew_formula --org demo --name alpha \
    --url "https://example.com/alpha-0.1.0.tar.gz" \
    --sha256 "deadbeef" --version "0.1.0" --dry_run true)
echo "$out" | hf_assert_event '.type == "brew_formula" and .dry_run == true and (.content | contains("class Alpha < Formula"))'

# --- brew_formula (non-dry) writes to disk ---
TAP="$TMP/tap"
mkdir -p "$TAP"
out=$(hf_cmd build brew_formula --org demo --name alpha --tap "$TAP" \
    --url "https://example.com/alpha-0.1.0.tar.gz" \
    --sha256 "deadbeef" --version "0.1.0")
echo "$out" | hf_assert_event ".type == \"brew_formula\" and .written_to == \"$TAP/alpha.rb\""
[[ -f "$TAP/alpha.rb" ]]

# --- dist_init across the workspace; idempotent ---
out=$(hf_cmd build dist_init --name main)
echo "$out" | hf_assert_event '.type == "dist_init" and .ref.name == "beta"'

# --- dist_show reads each member ---
out=$(hf_cmd build dist_show --name main)
echo "$out" | hf_assert_event '.type == "dist_show" and .ref.name == "alpha" and (.content // "" | contains("[dist]"))'

# --- foreach: echo $PWD inside each member ---
out=$(hf_cmd build foreach --name main --cmd 'pwd')
echo "$out" | hf_assert_event ".type == \"exec_output\" and .exit_code == 0 and .ref.name == \"alpha\" and (.stdout | contains(\"$ALPHA\"))"
echo "$out" | hf_assert_event ".type == \"exec_output\" and .ref.name == \"beta\" and (.stdout | contains(\"$BETA\"))"
echo "$out" | hf_assert_event '.type == "exec_summary" and .total == 2 and .ok == 2'

# --- foreach tolerates per-member failure ---
out=$(hf_cmd build foreach --name main --cmd 'test -f expected-file')
echo "$out" | hf_assert_event '.type == "exec_summary" and .total == 2 and .errored == 2'

# --- the old name refuses, naming the new one ---
out=$(hf_cmd build run --name main --cmd pwd || true)
echo "$out" | hf_assert_event '.type == "error" and .code == "renamed" and (.message | contains("foreach"))'

# --- exec: single repo ---
out=$(hf_cmd build exec --org demo --name alpha --cmd 'echo hi')
echo "$out" | hf_assert_event '.type == "exec_output" and .ref.name == "alpha" and (.stdout | contains("hi"))'

rm -rf "$TMP"
hf_teardown
echo "PASS"
