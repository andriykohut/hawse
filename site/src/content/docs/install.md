---
title: Install
description: Release binaries, the install script, Docker images and building from source.
---

Each [release](https://github.com/andriykohut/hawse/releases) has a static binary
for x86_64, aarch64 and armv7 Linux, and one for Apple Silicon macOS. The archive
holds the binary, its licenses, `THIRD-PARTY-LICENSES.txt` for the crates
compiled into it, and the systemd units from `contrib/`.

`install.sh` picks the archive for the machine it runs on from the latest
release, checks it against `SHA256SUMS` and installs the binary to
`/usr/local/bin`:

```sh
curl -fsSL https://raw.githubusercontent.com/andriykohut/hawse/main/install.sh | sh
```

`HAWSE_VERSION` picks another release and `HAWSE_INSTALL_DIR` another directory.
The same by hand, for one version and one target:

```sh
curl -LO https://github.com/andriykohut/hawse/releases/download/v0.9.0/hawse-0.9.0-x86_64-unknown-linux-musl.tar.gz
tar -xzf hawse-0.9.0-x86_64-unknown-linux-musl.tar.gz
install -m755 hawse-0.9.0-x86_64-unknown-linux-musl/hawse /usr/local/bin/hawse
```

`SHA256SUMS` in the same release covers every archive, and
`gh attestation verify FILE --repo andriykohut/hawse` checks that an archive was
built by this repository's release workflow.

With a Rust toolchain, `cargo install --locked hawse` builds from source and
`cargo binstall hawse` downloads the release binary.

`hawse completions SHELL` prints the completion script for bash, zsh, fish,
elvish or PowerShell. A shell that asks the installed binary as it starts has
the script of the version it runs:

```sh
eval "$(hawse completions bash)"    # ~/.bashrc
eval "$(hawse completions zsh)"     # ~/.zshrc, after compinit
hawse completions fish | source     # ~/.config/fish/config.fish
```

## Docker

Images for `linux/amd64`, `linux/arm64` and `linux/arm/v7` are published to
`ghcr.io/andriykohut/hawse`, tagged with each version and `latest`. A server and
a client on different versions do not work together, so run the same tag on both
ends.

The container reads its config from `/etc/hawse` and keeps keys in
`/var/lib/hawse`, which is the only directory it can write.

Run the server with host networking. It binds public ports as clients ask for
them, and Docker only forwards ports named when the container starts:

```sh
docker run -d --name hawse-server --network host --restart unless-stopped \
  -v /etc/hawse:/etc/hawse:ro -v hawse:/var/lib/hawse \
  ghcr.io/andriykohut/hawse:0.9.0 server
```

The client needs host networking too when a `local` address points at the host.
On a Compose network it can name other services instead, as in
`local = "jellyfin:8096"`.

```sh
docker run -d --name hawse-client --network host --restart unless-stopped \
  -v /etc/hawse:/etc/hawse:ro -v hawse:/var/lib/hawse \
  ghcr.io/andriykohut/hawse:0.9.0 client
```

`keygen --out /var/lib/hawse/client.key` with the same volume creates the
client's key and prints its public key. The container runs as uid 65532, so a
public port below 1024 needs `--user 0`.

## Building

```sh
cargo build --release
```

The binary is `target/release/hawse`. Cross builds use the `ring` crypto
provider in place of the default `aws-lc-rs`:

```sh
cargo build --release --no-default-features --features ring
```
