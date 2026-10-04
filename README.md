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

<img src="https://raw.githubusercontent.com/andriykohut/hawse/main/assets/hawse-demo.gif" width="880" alt="A hawse server and a client starting in two panes, then a visitor's curl to the server's port 8080 answered by the client's local service">

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
- **Databases and admin interfaces.** `allow` limits a service to the networks
  you list, and the server turns everyone else away. See Configuration.
- **Networks that block outbound UDP.** When QUIC does not connect, the client
  carries the tunnel over TLS instead, and `transport.prefer = "tcp"` asks for
  that outright.
- **WireGuard, DNS, and game servers.** `port = "51820/udp"` exposes a UDP
  service. Read the note on UDP under Configuration first: a payload too large
  for one QUIC datagram is carried differently.

Waiting on features that are not implemented yet:

- **Many HTTPS services on one port 443** need SNI routing.

## Status

TCP and UDP forwarding work, with fixed or dynamically assigned public ports.

0.5.0 changed the wire format. A 0.5.0 end does not talk to an older one, and a
0.5.0 client says so when it meets one: upgrade the server and its clients
together.

Besides the features named above, configuration hot reload and the `expose`,
`authorize`, `revoke` and `check` subcommands are not implemented either.
The [issues](https://github.com/andriykohut/hawse/issues) list everything that
is planned or deliberately deferred.

## Install

Each [release](https://github.com/andriykohut/hawse/releases) has a static binary
for x86_64, aarch64 and armv7 Linux, and one for Apple Silicon macOS. The archive
holds the binary, its licenses, `THIRD-PARTY-LICENSES.txt` for the crates
compiled into it, and the systemd units from `contrib/`:

```sh
curl -LO https://github.com/andriykohut/hawse/releases/download/v0.5.0/hawse-0.5.0-x86_64-unknown-linux-musl.tar.gz
tar -xzf hawse-0.5.0-x86_64-unknown-linux-musl.tar.gz
install -m755 hawse-0.5.0-x86_64-unknown-linux-musl/hawse /usr/local/bin/hawse
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
  ghcr.io/andriykohut/hawse:0.5.0 server
```

The client needs host networking too when a `local` address points at the host.
On a Compose network it can name other services instead, as in
`local = "jellyfin:8096"`.

```sh
docker run -d --name hawse-client --network host --restart unless-stopped \
  -v /etc/hawse:/etc/hawse:ro -v hawse:/var/lib/hawse \
  ghcr.io/andriykohut/hawse:0.5.0 client
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

When the server is unreachable or ends the session, the client reconnects on
its own. It waits about a second after the first failure and doubles the wait
up to 30 s; a session that stayed up for a minute starts it over.

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
than come up with the fallback silently missing. Set
`transport.tcp_fallback = false` on a server whose clients all use QUIC: it
then binds the port on UDP only, and a client set to `prefer = "tcp"` cannot
connect. Public ports bound for clients are TCP or UDP, as each service asks.
`hawse server --listen ADDR` overrides `listen` for that run.

`congestion` selects the controller, on either end, for the data that end
sends. The server defaults to `cubic`, the client to `bbr`:

```toml
[transport]
congestion = "bbr"    # or "cubic"
```

The choice matters most on a path with a long round trip and some random loss,
where `cubic` shrinks its window on every packet the path loses and `bbr` does
not.
[docs/measurements.md](https://github.com/andriykohut/hawse/blob/main/docs/measurements.md)
has both measured over a real path, and `bench/` measures them on the path you
have.

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

`allow` limits a service to visitors whose address falls in one of the listed
networks. The server turns everyone else away before the client hears of them:
a TCP visitor sees its connection closed, a UDP packet is dropped. For UDP the
source address is whatever the packet claims, so a forged packet naming an
allowed network still reaches the service. Set it on the service in
`client.toml`:

```toml
[expose.postgres]
local = "127.0.0.1:5432"
port = 5432
allow = ["203.0.113.0/24", "2001:db8::/48"]
```

The same key on a `[clients.NAME]` table in `server.toml` caps what that client
may publish. A service then admits only the addresses both lists cover, or all
of the server's list when the service names none, and a service whose list
shares nothing with the server's fails to bind as not granted. Without either
list a service admits everyone.

`proxy_protocol = true` on a service sends a PROXY protocol v2 header ahead
of each visitor's bytes, naming the visitor's address and the public address it
reached, so a service that logs or limits by address sees the visitor instead of
the client. The service has to expect the header, or it reads it as the start
of the request. On a UDP service the header goes in front of every datagram the
service receives, and its replies carry none. Where the server answers on every
interface, a UDP service's header names the address the client dialed as the
public one, whichever of the server's addresses the visitor reached.

A service behind a reverse proxy on the server, with `bind = "127.0.0.1"` as
above, sees every visitor arrive from the proxy, so its allow list and its PROXY
header describe the proxy, not the visitor.

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
new port.

`limits.auth_failures_per_minute`, 30 by default, limits how often one address
may fail to authenticate: a key the server does not know, even one whose
connection never sends its greeting, or a TCP handshake that fails. An IPv6
address counts together with the rest of its /64. Past the limit the server
refuses that address's connections before the TLS or QUIC handshake, and lets
one more try through every two seconds at the default. Clients sharing an
address, behind carrier-grade NAT for instance, share its limit. The server
tracks at most 16384 addresses; while that many are failing at once, new ones go
unlimited, and the log says so. `0` turns the limit off.

`quic_retry = true` makes a QUIC client prove its address with one extra round
trip before the server spends anything on a handshake. It also lets a failed
QUIC handshake count against the limit above; without it only failures after
the handshake do, since an unproven source address can be forged.

Client settings: `server` and `server_key` are required, `key` defaults to
`client.key`, and each `[expose.NAME]` table needs a `local` address.

`transport.prefer` chooses how the client reaches the server:

```toml
[transport]
prefer = "auto"    # or "quic", or "tcp"
```

`auto` is the default. It dials QUIC, and when QUIC has not connected after 2
seconds it dials the fallback beside it and takes whichever connects first; if
QUIC fails sooner, the fallback is dialed at once. `quic` never falls back.
`tcp` selects the fallback outright, which carries every stream over one TLS
connection multiplexed with yamux — for networks that block outbound UDP. A
session that lands on the fallback stays there until it ends, and the client
logs a warning when it does; the next connection tries QUIC first again. The 2
seconds are measured, so that one lost packet does not move a session onto the
fallback: see
[docs/measurements.md](https://github.com/andriykohut/hawse/blob/main/docs/measurements.md).

A visitor's connection that is cut short ends in a connection reset, on either
transport: when the local service resets it, when nothing is listening on
`local`, or when the tunnel itself drops. One that finished ends as the service
ended it. On the fallback that takes a marker written into every stream, at a
small cost to a request competing with heavy transfers, which
[docs/measurements.md](https://github.com/andriykohut/hawse/blob/main/docs/measurements.md)
puts a figure on.

A UDP service needs `/udp` on both ends: `port = "51820/udp"` or `"any/udp"` in
the client's `[expose.NAME]` table, and a grant such as `"51820/udp"` in the
server's `ports`. Each payload crosses the tunnel as one QUIC datagram when it
fits. One that does not fit — a full-size packet from a WireGuard tunnel at its
default MTU is one — goes over a reliable stream instead, where packets arrive
in order and a lost one delays those behind it. Setting `MTU = 1370` on both
WireGuard peers keeps their packets inside a datagram on a path with the usual
1500-byte MTU: payloads up to 1412 bytes crossed as datagrams, and 1370 leaves
room for hawse's own header to grow as a service sees more visitors. A service
with `proxy_protocol` sends the visitor's address in every packet, 7 bytes for
an IPv4 visitor and 19 for IPv6, so its payloads leave a datagram that much
sooner. On the fallback every payload takes that stream. hawse never holds a
UDP sender back: when the tunnel cannot keep up, packets are dropped, as on any
congested path. On the fallback each UDP service's traffic from visitor to
service takes one stream, which caps it well below what QUIC carries;
[docs/measurements.md](https://github.com/andriykohut/hawse/blob/main/docs/measurements.md)
has the loss and jitter measured on both. The service end line in the client's
log counts packets dropped from a full datagram queue as `queue_full`.

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
tcp_fallback = true
```

The client uses the same values except `stream_window = "2MiB"` and
`connection_window = "16MiB"`, and has no `tcp_fallback`.

The rest of `[transport]` is not honoured equally by the two. Both use
`idle_timeout` (on TCP it is the quiet time before the kernel starts probing),
`connection_window` and `buffer`. `stream_window` and `congestion` are QUIC's
alone: yamux guarantees every stream 256 KiB and grows it only into the
connection window's slack, so there is no per-stream knob to set, and TCP's
congestion control belongs to the kernel — so on the fallback both settings
are accepted, validated and then ignored.

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
