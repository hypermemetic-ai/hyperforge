#!/usr/bin/env bash
# Puts the real age CLI (a pinned upstream release) in .tools/age/ for the
# tests. Nothing is installed: .tools is gitignored and lives in this repo.
set -euo pipefail
cd "$(dirname "$0")/.."
VERSION=v1.3.2
SHA256=cbe24006683f8eb669266162894b9a522a1af52f2665fbc63a4bb032ed26ac10
if [ -x .tools/age/age ]; then exit 0; fi
mkdir -p .tools
url="https://github.com/FiloSottile/age/releases/download/$VERSION/age-$VERSION-linux-amd64.tar.gz"
curl -fsSL "$url" -o .tools/age.tar.gz
echo "$SHA256  .tools/age.tar.gz" | sha256sum -c -
tar -C .tools -xzf .tools/age.tar.gz
rm .tools/age.tar.gz
.tools/age/age --version
