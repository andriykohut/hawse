#!/bin/sh
# Installs a hawse release binary: the latest one, or HAWSE_VERSION. It goes to /usr/local/bin,
# or to HAWSE_INSTALL_DIR.
set -eu

repo=https://github.com/andriykohut/hawse

sha256() {
    if command -v sha256sum >/dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1
}

# A function, called on the last line, so that a download cut short runs nothing.
main() {
    case "$(uname -s)-$(uname -m)" in
        Linux-x86_64) target=x86_64-unknown-linux-musl ;;
        Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-musl ;;
        Linux-armv7l) target=armv7-unknown-linux-musleabihf ;;
        Darwin-arm64) target=aarch64-apple-darwin ;;
        *)
            echo "hawse: no release binary for $(uname -sm), try: cargo install --locked hawse" >&2
            exit 1
            ;;
    esac

    # The latest release redirects to its tag, which costs no API call and has no rate limit.
    version=${HAWSE_VERSION:-$(curl -fsSLI -o /dev/null -w '%{url_effective}' "$repo/releases/latest")}
    version=${version##*/}
    version=${version#v}
    name=hawse-$version-$target

    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    cd "$tmp"
    curl -fsSL -O "$repo/releases/download/v$version/$name.tar.gz" \
        -O "$repo/releases/download/v$version/SHA256SUMS"

    want=$(grep "  $name.tar.gz\$" SHA256SUMS | cut -d' ' -f1)
    if [ -z "$want" ] || [ "$want" != "$(sha256 "$name.tar.gz")" ]; then
        echo "hawse: $name.tar.gz does not match SHA256SUMS" >&2
        exit 1
    fi

    tar -xzf "$name.tar.gz"
    dir=${HAWSE_INSTALL_DIR:-/usr/local/bin}
    if [ -w "$dir" ]; then sudo=; else sudo=sudo; fi
    $sudo mkdir -p "$dir"
    $sudo install -m755 "$name/hawse" "$dir/hawse"
    echo "hawse $version installed to $dir/hawse"
}

main
