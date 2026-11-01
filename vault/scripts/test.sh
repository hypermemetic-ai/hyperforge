#!/usr/bin/env bash
# The whole check: real binaries over loopback, against the real age CLI, each
# test with a private HOME and its own state directory.
set -euo pipefail
cd "$(dirname "$0")/.."
scripts/fetch-age.sh
# The tests must not announce to, or read, anything of the host's.
export PLEXUS_NO_REGISTRY=1
cargo test --all-features "$@" -- --test-threads=4
