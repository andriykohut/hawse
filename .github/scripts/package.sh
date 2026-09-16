#!/usr/bin/env bash
# Usage: package.sh <target> <version> <out-dir>, after a release build of <target>.
set -euo pipefail

target=$1
version=$2
out=$3
name="hawse-$version-$target"
stage=$(mktemp -d)

mkdir -p "$stage/$name/contrib" "$out"
cp "${CARGO_TARGET_DIR:-target}/$target/release/hawse" LICENSE-MIT LICENSE-APACHE README.md "$stage/$name/"
cp contrib/* "$stage/$name/contrib/"
# macOS tar otherwise stores extended attributes as ._ entries.
COPYFILE_DISABLE=1 tar -czf "$out/$name.tar.gz" -C "$stage" "$name"
