#!/usr/bin/env bash
# Usage: unpublished-crates.sh <version> <crate>...
# Prints each crate that lacks <version> on crates.io, in argument order.
set -euo pipefail

version=$1
shift
for crate in "$@"; do
  # crates.io refuses API requests without a User-Agent.
  status=$(curl -sS -o /dev/null -w '%{http_code}' \
    -A 'hawse release workflow (https://github.com/andriykohut/hawse)' \
    "https://crates.io/api/v1/crates/$crate/$version")
  case $status in
    200) ;;
    404) echo "$crate" ;;
    *)
      echo "crates.io answered $status for $crate $version" >&2
      exit 1
      ;;
  esac
done
