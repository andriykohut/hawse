#!/usr/bin/env bash
# Prints the workspace version. On a tag push, fails unless the tag is v<version>.
set -euo pipefail

version=$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[] | select(.name == "hawse") | .version')
if [[ "${GITHUB_REF_TYPE:-}" == tag && "${GITHUB_REF_NAME:-}" != "v$version" ]]; then
  echo "tag ${GITHUB_REF_NAME} does not match the workspace version $version" >&2
  exit 1
fi
echo "$version"
