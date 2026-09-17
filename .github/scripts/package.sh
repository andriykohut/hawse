#!/usr/bin/env bash
# Usage: package.sh <target> <version> <out-dir>, from the repository root after a release build
# of <target>.
set -euo pipefail
# Modes in the archive follow the umask.
umask 022

target=$1
version=$2
out=$3
name="hawse-$version-$target"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

mkdir -p "$stage/$name" "$out"
cp "${CARGO_TARGET_DIR:-target}/$target/release/hawse" LICENSE-MIT LICENSE-APACHE README.md "$stage/$name/"
cp -R contrib "$stage/$name/"

# tar run as root restores the stored owner, so store root rather than the build account.
if tar --version | grep -q GNU; then
  flags=(--owner=0 --group=0 --numeric-owner)
else
  flags=(--uid 0 --gid 0 --uname root --gname root --no-mac-metadata)
fi
tar -czf "$out/$name.tar.gz" --no-xattrs "${flags[@]}" -C "$stage" "$name"
