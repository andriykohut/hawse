#!/usr/bin/env bash
# Usage: image-context.sh <archive-dir> <version> <context-dir>
set -euo pipefail

archives=$1
version=$2
context=$3

while read -r target platform; do
  archive="$archives/hawse-$version-$target.tar.gz"
  top="hawse-$version-$target"
  mkdir -p "$context/$platform/doc"
  tar -xzf "$archive" -C "$context/$platform" --strip-components=1 "$top/hawse"
  tar -xzf "$archive" -C "$context/$platform/doc" --strip-components=1 \
    "$top/LICENSE-APACHE" "$top/LICENSE-MIT" "$top/THIRD-PARTY-LICENSES.txt"
done <<'EOF'
x86_64-unknown-linux-musl linux/amd64
aarch64-unknown-linux-musl linux/arm64
armv7-unknown-linux-musleabihf linux/arm/v7
EOF
mkdir -p "$context/state"
