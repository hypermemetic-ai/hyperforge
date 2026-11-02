#!/usr/bin/env bash
# The whole check: real binaries over loopback, each test with a private HOME
# and its own state directory. No age CLI is needed; see tests/interop.rs for the
# check against one.
set -euo pipefail
cd "$(dirname "$0")/.."
# The tests must not announce to, or read, anything of the host's.
export PLEXUS_NO_REGISTRY=1
cargo test --all-features "$@" -- --test-threads=4
