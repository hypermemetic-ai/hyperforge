#!/usr/bin/env bash
# tier: 1
# V5PARITY-9 acceptance: build.{unify, analyze, check_manifests,
#   detect_name_mismatches, package_diff}.
set -euo pipefail
source "$(dirname "$0")/../harness/lib.sh"

hf_spawn

TMP="$(mktemp -d -t v5prty9-XXXXXX)"
ALPHA="$TMP/alpha"
BETA="$TMP/beta"

# --- alpha: a well-formed Rust crate ---
mkdir -p "$ALPHA"
git init -q "$ALPHA"
git -C "$ALPHA" -c user.email=t@t -c user.name=t commit --allow-empty -m initial -q
cat > "$ALPHA/Cargo.toml" <<'TOML'
[package]
name = "alpha"
version = "0.1.0"

[dependencies]
serde = "1.0.200"
TOML
git -C "$ALPHA" add Cargo.toml
git -C "$ALPHA" -c user.email=t@t -c user.name=t commit -m "v0.1.0" -q

# Bump the version in a second commit so package_diff has a signal.
cat > "$ALPHA/Cargo.toml" <<'TOML'
[package]
name = "alpha"
version = "0.2.0"

[dependencies]
serde = "1.0.200"
TOML
git -C "$ALPHA" add Cargo.toml
git -C "$ALPHA" -c user.email=t@t -c user.name=t commit -m "v0.2.0" -q

# --- beta: version-mismatched serde ---
mkdir -p "$BETA"
git init -q "$BETA"
git -C "$BETA" -c user.email=t@t -c user.name=t commit --allow-empty -m initial -q
cat > "$BETA/Cargo.toml" <<'TOML'
[package]
name = "beta-pkg"
version = "0.1.0"

[dependencies]
serde = "1.0.150"
TOML

# Register workspace.
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

# --- build.unify ---
out=$(hf_cmd build unify --name main)
echo "$out" | hf_assert_event '.type == "package_manifest" and .name == "alpha" and .version == "0.2.0"'
echo "$out" | hf_assert_event '.type == "package_manifest" and .name == "beta-pkg" and .version == "0.1.0"'

# --- build.analyze spots the serde version_mismatch ---
out=$(hf_cmd build analyze --name main)
echo "$out" | hf_assert_event '.type == "analyze_finding" and .kind == "version_mismatch" and .dep == "serde"'

# --- build.check_manifests passes ---
out=$(hf_cmd build check_manifests --name main)
echo "$out" | hf_assert_event '.type == "validate_ok" and .total == 2'

# --- the old name refuses, naming the new one ---
out=$(hf_cmd build validate --name main || true)
echo "$out" | hf_assert_event '.type == "error" and .code == "renamed" and (.message | contains("check_manifests"))'

# --- HYPERFORGE-57: a Cabal-only package and a virtual cargo workspace
#     parse; a docs-only member holds no package and is skipped, not failed ---
mkdir -p "$TMP/docs" "$TMP/hask" "$TMP/virt/member"
echo "# notes" > "$TMP/docs/README.md"
cat > "$TMP/hask/hask.cabal" <<'CABAL'
cabal-version: 2.4
name:          hask
version:       0.1.0.0
library
  build-depends: base
CABAL
cat > "$TMP/virt/Cargo.toml" <<'TOML'
[workspace]
members = ["member"]
TOML
cat > "$TMP/virt/member/Cargo.toml" <<'TOML'
[package]
name = "virt-member"
version = "0.1.0"
TOML
cat >> "$HF_CONFIG/orgs/demo.yaml" <<YAML
  - name: docs
    remotes:
      - url: "file://$TMP/docs"
  - name: hask
    remotes:
      - url: "file://$TMP/hask"
  - name: virt
    remotes:
      - url: "file://$TMP/virt"
YAML
cat >> "$HF_CONFIG/workspaces/main.yaml" <<YAML
  - demo/docs
  - demo/hask
  - demo/virt
YAML
out=$(hf_cmd build check_manifests --name main)
echo "$out" | hf_assert_event '.type == "validate_ok" and .total == 5 and .skipped == 1'

# --- break beta's Cargo.toml, check_manifests now fails ---
printf 'this = [is not (valid toml\n' > "$BETA/Cargo.toml"
out=$(hf_cmd build check_manifests --name main)
echo "$out" | hf_assert_event '.type == "validate_failed" and .failed == 1'
echo "$out" | hf_assert_event '.type == "error" and .code == "manifest_parse_error"'

# Restore beta.
cat > "$BETA/Cargo.toml" <<'TOML'
[package]
name = "beta-pkg"
version = "0.1.0"
TOML

# --- detect_name_mismatches flags beta (manifest name "beta-pkg" ≠ repo name "beta") ---
out=$(hf_cmd build detect_name_mismatches --name main)
echo "$out" | hf_assert_event '.type == "name_mismatch" and .ref.name == "beta" and .manifest_name == "beta-pkg"'

# --- package_diff between the two alpha commits ---
out=$(hf_cmd build package_diff --name main --from_ref HEAD~1 --to_ref HEAD)
echo "$out" | hf_assert_event '.type == "package_diff_entry" and .kind == "version_changed" and .from == "0.1.0" and .to == "0.2.0"'

rm -rf "$TMP"
hf_teardown
echo "PASS"
