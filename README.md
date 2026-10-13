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
  you list, and the server turns everyone else away. See
  [Configuration](https://andriykohut.github.io/hawse/configuration/).
- **Networks that block outbound UDP.** When QUIC does not connect, the client
  carries the tunnel over TLS instead, and `transport.prefer = "tcp"` asks for
  that outright.
- **A client that changes network.** Over QUIC a session follows its client to
  a new address, and the connections open through it carry on.
- **WireGuard, DNS, and game servers.** `port = "51820/udp"` exposes a UDP
  service. Read the note on UDP under
  [Configuration](https://andriykohut.github.io/hawse/configuration/) first: a
  payload too large for one QUIC datagram is carried differently.

Waiting on features that are not implemented yet:

- **Many HTTPS services on one port 443** need SNI routing.

## Status

TCP and UDP forwarding work, with fixed or dynamically assigned public ports.

0.5.0 changed the wire format. A 0.5.0 end does not talk to an older one, and a
0.5.0 client says so when it meets one: upgrade the server and its clients
together.

Besides the features named above, the `expose` subcommand is not implemented
either.
The [issues](https://github.com/andriykohut/hawse/issues) list everything that
is planned or deliberately deferred.

## Install

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

The [documentation](https://andriykohut.github.io/hawse/) has the other ways to
install it, and [usage](https://andriykohut.github.io/hawse/usage/),
[configuration](https://andriykohut.github.io/hawse/configuration/) and
[logging](https://andriykohut.github.io/hawse/logging/).

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
