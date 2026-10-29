#!/usr/bin/env bash
# tier: 1
set -euo pipefail
source "$(dirname "$0")/../harness/lib.sh"

# V5REPOS-2: pin the ForgePort capability via schema introspection.
# The wire-level check is that the capability surface declares exactly
# the four DriftFieldKind fields as its metadata shape. Implementation
# may expose this via schema introspection, diagnostics, or an
# integration test of an adapter's output. This script exercises the
# schema path; the adapter scripts (V5REPOS-9/10/11) cover the live-API
# shape check.

hf_spawn
hf_load_fixture empty

schema=$(hf_cmd __schema__ 2>/dev/null || hf_cmd)

# The capability must be discoverable and name exactly the four fields.
# Implementers may surface it under a sub-service of repos or via a typed
# schema record. The assertion targets the field set, not its location.
echo "$schema" | hf_assert_event '(.type == "capability" or .type == "forge_port_schema")
  and ((.fields // [] | sort) == ["archived","default_branch","description","visibility"])'

# The error-class set must be closed to exactly five variants.
echo "$schema" | hf_assert_event '(.type == "capability" or .type == "forge_port_schema")
  and ((.error_classes // [] | sort) == ["auth","network","not_found","rate_limited","unsupported_field"])'

hf_teardown
echo "PASS"
