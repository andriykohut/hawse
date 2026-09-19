<h1>
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/andriykohut/hawse/main/assets/hawse-lockup-inverse.svg">
    <img src="https://raw.githubusercontent.com/andriykohut/hawse/main/assets/hawse-lockup.svg" width="296" alt="hawse">
  </picture>
</h1>

[![CI](https://github.com/andriykohut/hawse/actions/workflows/ci.yml/badge.svg)](https://github.com/andriykohut/hawse/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/hawse.svg)](https://crates.io/crates/hawse)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

hawse publishes TCP services on a server's ports. The client opens one outbound
QUIC connection to the server. The server listens on the configured ports and
relays each incoming connection over that connection, to an address the client
can reach.

Only the client dials out, so the network holding the services needs no inbound
connectivity: no port forwarding, no public address, no firewall rules for
incoming traffic. The server needs an address that its clients and their
visitors can reach, which is usually a public one but does not have to be.

Both ends authenticate with Ed25519 keys inside TLS 1.3. The client pins the
server's public key. The server holds a list of client keys and the ports each
client may bind.

## What it is for

Working today, with TCP and UDP forwarding and key-based authorization:

- **Reaching machines you cannot port-forward into.** A home server behind
  CGNAT, a build machine on a corporate network, a Raspberry Pi at a relative's
  house. Each machine holds its own key, and the server grants it specific
  ports.
- **SSH without a shell account on the relay.** The usual alternative is
  `autossh -R`, which needs an account on the public box and reconnects only as
  well as the wrapper around it. Here the public server never gets shell access,
  and reconnection is part of the client.
- **Self-hosted web services.** Expose the service on a public port and put a
  reverse proxy such as Caddy or nginx in front of it on the server for TLS and
  a hostname. hawse forwards bytes and does not read HTTP.
- **Webhooks and demos during development.** Give a payment provider or a
  colleague a stable address that lands on a laptop.
- **A doorway into one network.** Since `local` can name any host the client
  reaches, a single client can publish services running on several machines
  beside it.
- **Networks that block outbound UDP.** `transport.prefer = "tcp"` carries the
  tunnel over TLS instead of QUIC. Read the caveat under Configuration first:
  the fallback cannot report a transfer cut short.
- **WireGuard, DNS, and game servers.** `port = "51820/udp"` exposes a UDP
  service. Read the note on UDP under Configuration first: a payload too large
  for one QUIC datagram is carried differently.

Waiting on features that are not implemented yet:

- **Databases and admin interfaces** should wait for source-address
  allowlists, or be restricted by a firewall on the server. A public port is
  reachable by anyone today.
- **Services that log or rate-limit by client address** need PROXY protocol v2
  to see the real visitor address rather than the client's own connection.
- **Many HTTPS services on one port 443** need SNI routing.

## Status

TCP and UDP forwarding work, with fixed or dynamically assigned public ports.

Besides the features named above, configuration hot reload and the `expose`,
`authorize`, `revoke` and `check` subcommands are not implemented either.
`docs/backlog.md` lists everything that is planned or deliberately deferred.

## Install

Each [release](https://github.com/andriykohut/hawse/releases) has a static binary
for x86_64, aarch64 and armv7 Linux, and one for Apple Silicon macOS. The archive
holds the binary, its licenses, `THIRD-PARTY-LICENSES.txt` for the crates
compiled into it, and the systemd units from `contrib/`:

```sh
curl -LO https://github.com/andriykohut/hawse/releases/download/v0.2.0/hawse-0.2.0-x86_64-unknown-linux-musl.tar.gz
tar -xzf hawse-0.2.0-x86_64-unknown-linux-musl.tar.gz
install -m755 hawse-0.2.0-x86_64-unknown-linux-musl/hawse /usr/local/bin/hawse
```

`SHA256SUMS` in the same release covers every archive, and
`gh attestation verify FILE --repo andriykohut/hawse` checks that an archive was
built by this repository's release workflow.

With a Rust toolchain, `cargo install --locked hawse` builds from source and
`cargo binstall hawse` downloads the release binary.

### Docker

Images for `linux/amd64`, `linux/arm64` and `linux/arm/v7` are published to
`ghcr.io/andriykohut/hawse`, tagged with each version and `latest`. A server and
a client on different versions do not work together, so run the same tag on both
ends.

The container reads its config from `/etc/hawse` and keeps keys in
`/var/lib/hawse`, which is the only directory it can write. The config has to set
`key` to a full path, because the default resolves next to the config:

```toml
key = "/var/lib/hawse/server.key"
```

Run the server with host networking. It binds public ports as clients ask for
them, and Docker only forwards ports named when the container starts:

```sh
docker run -d --name hawse-server --network host --restart unless-stopped \
  -v /etc/hawse:/etc/hawse:ro -v hawse:/var/lib/hawse \
  ghcr.io/andriykohut/hawse:0.2.0 server
```

The client needs host networking too when a `local` address points at the host.
On a Compose network it can name other services instead, as in
`local = "jellyfin:8096"`.

```sh
docker run -d --name hawse-client --network host --restart unless-stopped \
  -v /etc/hawse:/etc/hawse:ro -v hawse:/var/lib/hawse \
  ghcr.io/andriykohut/hawse:0.2.0 client
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

## Usage

Start the server:

```sh
hawse server
```

On first run it generates `server.key` and prints the corresponding public key.
Connections from unknown keys are refused, so until a client is authorized the
server accepts nothing.

Generate a key on the client:

```sh
hawse keygen
```

Add the printed key to `server.toml` on the server, together with the ports
that client may bind:

```toml
[clients.laptop]
key = "ed25519:AAAA..."
ports = ["2222"]
```

Restart the server. Configuration is read at startup only.

Write `client.toml` on the client:

```toml
server = "tunnel.example.com:4433"
server_key = "ed25519:BBBB..."

[expose.ssh]
local = "127.0.0.1:22"
port = 2222

[expose.dev]
local = "127.0.0.1:3000"
```

Then run:

```sh
hawse client
```

The `ssh` service binds port 2222 on the server and forwards to port 22 on the
client. The `dev` service sets no `port`, so the server assigns one from its
`dynamic_ports` range and the client logs which port it got.

`local` is any address the client can open a TCP connection to, not only one on
the client itself. `local = "192.168.1.50:80"` forwards to another host on the
client's network.

## Configuration

Configuration is read from `$XDG_CONFIG_HOME/hawse/` (`~/.config/hawse/` by
default), falling back to `/etc/hawse/` when the file is not there. `--config
PATH` and the `HAWSE_CONFIG` environment variable override the location.
Relative key paths resolve against the directory containing the config file.
When neither directory has a config, keys are written to `/etc/hawse/` if
running as root and to `$XDG_CONFIG_HOME/hawse/` otherwise.

Server settings and their defaults:

```toml
listen = "[::]:4433"
bind = "::"
key = "server.key"
dynamic_ports = "40000-41000"
```

The server answers on the listen port twice: UDP for QUIC, and TCP for the
fallback transport. Both have to be free at startup and reachable through the
firewall — the server refuses to start if it cannot bind the TCP side, rather
than come up with the fallback silently missing. Public ports bound for clients
are TCP or UDP, as each service asks. `hawse server --listen ADDR` overrides
`listen` for that run.

`congestion` selects the controller, on either end, for the data that end
sends:

```toml
[transport]
congestion = "bbr"    # or "cubic", the default
```

The choice matters most on a path with a long round trip, where `cubic` keeps
a shorter queue and `bbr` reaches a higher rate. Prefer `cubic` when the tunnel
carries interactive traffic, `bbr` when it carries bulk transfers and
throughput is what you are short of. `bench/` measures both on the path you
have, which is the only way to settle it. A client forwarding a UDP service
that sends near the link's rate is the exception; see the UDP note below.

`bind` is the address those public ports listen on. The default answers on
every interface. Set it to `127.0.0.1` when a reverse proxy on the same host is
the only thing that should reach them:

```toml
bind = "127.0.0.1"

[clients.nas]
key = "ed25519:AAAA..."
ports = ["8096"]
```

A client may override the server-wide value with its own `bind`, so one server
can keep some services behind a proxy and publish others directly.

`limits.streams_per_client` caps how many streams one client's connection may
carry, one per visitor connection, and defaults to 4096. On the TCP fallback,
yamux promises every stream 256 KiB of receive window and drops the whole
connection when the cap is passed, so the default lets a session hold about
1 GiB of unread data. Lower it on a server with little memory, but not below the
number of visitor connections a client carries at once.
`limits.udp_sessions_per_service` caps how many visitors one UDP service keeps
track of at once, and defaults to 4096. Past it the visitor that has been quiet
longest is forgotten, and any visitor is forgotten after 60 s of silence; its
next packet starts a new session, which the local service sees arrive from a
new port. `limits.auth_failures_per_minute` and `quic_retry` are parsed but not
enforced yet.

Client settings: `server` and `server_key` are required, `key` defaults to
`client.key`, and each `[expose.NAME]` table needs a `local` address.

`transport.prefer` chooses how the client reaches the server:

```toml
[transport]
prefer = "auto"    # or "quic", or "tcp"
```

`auto` is the default and today dials QUIC and only QUIC, exactly as `quic`
does; a failed connection is retried from the beginning every five seconds.
`tcp` selects the fallback transport, which carries every stream over one TLS
connection multiplexed with yamux — for networks that block outbound UDP.

**The TCP fallback cannot detect a truncated transfer.** yamux has no way to
abort a stream distinguishably from finishing one: the reader sees
end-of-stream either way. So if a visitor connection is cut short in the
middle, the data that did arrive is delivered as if it were the whole thing,
and neither end can tell. On QUIC the stream is reset and the read fails
loudly. This is why `auto` does not fall back on its own — blocked UDP is not
consent to silent truncation. Choose `tcp` when the traffic can survive
arriving short, or when UDP leaves you no other way through.

`auto` stays a separate setting because it is where the fallback returns once a
visitor stream can report that it arrived whole. Until then it means "let hawse
choose", and hawse chooses the transport that can tell you when a transfer was
cut short.

A UDP service needs `/udp` on both ends: `port = "51820/udp"` or `"any/udp"` in
the client's `[expose.NAME]` table, and a grant such as `"51820/udp"` in the
server's `ports`. Each payload crosses the tunnel as one QUIC datagram when it
fits. One that does not fit — a full-size packet from a WireGuard tunnel at its
default MTU is one — goes over a reliable stream instead, where packets arrive
in order and a lost one delays those behind it. Setting `MTU = 1370` on both
WireGuard peers keeps their packets inside a datagram on a path with the usual
1500-byte MTU: payloads up to 1412 bytes crossed as datagrams, and 1370 leaves
room for hawse's own header to grow as a service sees more visitors. With
`prefer = "tcp"` every payload takes that stream. hawse never holds a UDP sender
back: when the tunnel cannot keep up, packets are dropped, as on any congested
path.

Measured with `iperf3` from a home connection through a Hetzner server: payloads
of 1100 bytes, small enough for a datagram before QUIC has probed the path,
crossed from visitor to service with at most 0.04% loss and under 1 ms of
jitter at 10, 50 and 100 Mbit/s, and from service to visitor with none up to
50 Mbit/s. At 100 Mbit/s from service to visitor the client's default `cubic`
controller lost between 0.4% and 41% of packets over ten runs, median 28%: it
shrinks its window on every packet the path loses, and while it regrows the
client queues only about 1 MiB of datagrams and drops the oldest. With
`congestion = "bbr"` in the client's `[transport]` the same runs lost 0.03%,
worst 0.3%. Set it on a client whose UDP service sends near the link's rate;
the service end line in the client's log counts these drops as `queue_full`.
With `prefer = "tcp"`, traffic from visitor to service shares one yamux stream
and tops out between 40 and 65 Mbit/s, dropping the rest; from service to
visitor it lost under 2.5% at every rate.

Give a UDP service's `local` as a literal address such as `127.0.0.1:51820`. A
hostname is looked up again for every new visitor, only its first address is
tried, and while a lookup runs every UDP service on the connection waits. The
client holds one socket for each visitor seen in the last 60 s, up to 4096 per
service, and past that the visitor quiet longest is dropped to make room, so a
busy or publicly reachable UDP service needs a descriptor limit to match, such
as `LimitNOFILE=` under systemd. On a server with several addresses, set `bind`
to the one visitors use: a reply from a wildcard bind leaves from whichever
address the route picks.

Defaults for the other `[transport]` settings, on the server:

```toml
[transport]
idle_timeout = "30s"
stream_window = "8MiB"
connection_window = "64MiB"
buffer = "16KiB"
```

The client uses the same values except `stream_window = "2MiB"` and
`connection_window = "16MiB"`.

The rest of `[transport]` is not honoured equally by the two. Both use
`idle_timeout` (on TCP it is the quiet time before the kernel starts probing),
`connection_window` and `buffer`. `stream_window` and `congestion` are QUIC's
alone: yamux guarantees every stream 256 KiB and grows it only into the
connection window's slack, so there is no per-stream knob to set, and TCP's
congestion control belongs to the kernel — so under `prefer = "tcp"` both
settings are accepted, validated and then ignored.

`--threads` sets the number of worker threads and defaults to the number of
CPUs.

## Logging

Logs go to stderr, formatted for a terminal when stderr is one and as JSON
otherwise; `--log` overrides that choice. Colour follows the same rule, and
`--color` overrides it. `-v` adds per-connection events, `-vv` adds trace
output, and `-q` restricts output to warnings and errors. The `HAWSE_LOG`
environment variable takes a `tracing` filter directive and overrides all of
them.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](https://github.com/andriykohut/hawse/blob/main/LICENSE-APACHE) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](https://github.com/andriykohut/hawse/blob/main/LICENSE-MIT) or
  <http://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
