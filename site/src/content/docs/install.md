---
title: Install
description: Release binaries, the install script, running as a service, upgrading, Docker images and building from source.
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
curl -LO https://github.com/andriykohut/hawse/releases/download/v0.12.0/hawse-0.12.0-x86_64-unknown-linux-musl.tar.gz
tar -xzf hawse-0.12.0-x86_64-unknown-linux-musl.tar.gz
install -m755 hawse-0.12.0-x86_64-unknown-linux-musl/hawse /usr/local/bin/hawse
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

## Running as a service

`hawse service install` sets the server or the client up to start by itself and
to be started again when it stops. It checks the config `hawse server` or
`hawse client` would read, or the one `--config` names, writes the service for
the binary that runs it, starts it, and prints the public key.

On Linux it writes a systemd unit, which starts at boot, so it runs as root:

```sh
sudo hawse service install server
sudo hawse service install client
```

A client's config is the one `sudo hawse join` writes to `/etc/hawse`; run
without sudo, `join` writes to your own config directory, which the unit does not
read. The unit runs under a throwaway account that keeps its key in
`/var/lib/hawse`.
A server with no config yet runs on the defaults and reads
`/etc/hawse/server.toml` once it is written. `systemctl status hawse-server`
shows how it stands. `sudo hawse status` shows more: the clients and their
ports, the services and their totals, and the keys that knocked. `--json`
prints the same for scripts.

On macOS it writes a LaunchAgent, which starts when you log in, so it runs as
you, without sudo:

```sh
hawse service install client
```

The agent logs to macOS's unified log, which [Logging](/hawse/logging/) says how
to read. `hawse status` for the agent needs no sudo.

Run it again after replacing the binary: it rewrites the service from that
version and restarts it. `hawse service uninstall server` or `client` stops the
service and removes it, and leaves the config and the key. Install replaces or
removes only a service it wrote, so a unit copied by hand has to be removed
first, and a change to an installed unit belongs in a drop-in made with
`systemctl edit`.

`--user NAME` runs the unit as that account instead and keeps the key beside the
config, for a config kept where the throwaway account cannot write, such as a
pool on TrueNAS SCALE. The binary goes in a directory only root can write, since
root runs it to install and to upgrade, and both refuse one another account
could replace:

```sh
sudo install -D -m755 hawse /mnt/pool/hawse-bin/hawse
sudo /mnt/pool/hawse-bin/hawse service install client \
  --config /mnt/pool/apps/hawse/client.toml --user someone
```

Run `hawse join` for that config as the same account, without sudo, so the key
it creates is the one the service reads. A TrueNAS upgrade drops `/etc` and the
unit with it; a POSTINIT init script that runs the same command puts it back.
`hawse status` for a unit installed with `--user` needs no sudo when run as that
account.

Both units keep their keys in `/var/lib/hawse`, so a host runs one of them as a
unit; a second on the same host needs `--user`.

## Upgrading

`hawse upgrade` replaces the binary that runs it with the latest release and
restarts the systemd unit or LaunchAgent that runs that binary, whoever wrote
it. It checks the release against its `SHA256SUMS` and runs it once before
anything is replaced, and keeps the binary it replaces beside it as
`hawse.old`. It restarts `hawse-server` and `hawse-client` when they are running,
and says so when nothing runs the binary. Run it on each host, the server first:

```sh
sudo hawse upgrade
```

Restarting a systemd unit needs root, and so does a binary in a directory only
root can write, such as `/usr/local/bin`. On macOS, an agent is restarted for the
user who ran sudo.

`--check` says whether a newer release exists and changes nothing. `--version`
installs another release, newer or older, down to 0.10.0: an older one looks for
its key elsewhere and would make a new one. Without `--version`, a binary newer
than the latest release is left as it is. To go back by hand, move `hawse.old`
over the binary and restart the service.

Upgrade restarts a unit and does not rewrite it. When a release changes a unit
`hawse service install` wrote, upgrade says so, and running the same install
command again rewrites it.

A binary installed by cargo, Homebrew or Nix is upgraded with that tool, and an
image by its tag. The checksums show the download is the release's;
`gh attestation verify` also shows the release was built by this repository.

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
  ghcr.io/andriykohut/hawse:0.12.0 server
```

The client needs host networking too when a `local` address points at the host.
On a Compose network it can name other services instead, as in
`local = "jellyfin:8096"`.

```sh
docker run -d --name hawse-client --network host --restart unless-stopped \
  -v /etc/hawse:/etc/hawse:ro -v hawse:/var/lib/hawse \
  ghcr.io/andriykohut/hawse:0.12.0 client
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
