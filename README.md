<h1>
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/hawse-lockup-inverse.svg">
    <img src="assets/hawse-lockup.svg" width="296" alt="hawse">
  </picture>
</h1>

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

Working today, with TCP forwarding and key-based authorization:

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

Waiting on features that are not implemented yet:

- **WireGuard, DNS, and most game servers** need UDP forwarding.
- **Databases and admin interfaces** should wait for source-address
  allowlists, or be restricted by a firewall on the server. A public port is
  reachable by anyone today.
- **Services that log or rate-limit by client address** need PROXY protocol v2
  to see the real visitor address rather than the client's own connection.
- **Many HTTPS services on one port 443** need SNI routing.

## Status

TCP forwarding works, with fixed or dynamically assigned public ports.

Besides the features named above, configuration hot reload and the `expose`,
`authorize`, `revoke` and `check` subcommands are not implemented either.
`docs/backlog.md` lists everything that is planned or deliberately deferred.

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
default), or from `/etc/hawse/` when running as root. `--config PATH` and the
`HAWSE_CONFIG` environment variable override the location. Relative key paths
resolve against the directory containing the config file.

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
are TCP.

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
have, which is the only way to settle it.

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

The rest of `[transport]` is not honoured equally by the two. Both use
`idle_timeout` (on TCP it is the quiet time before the kernel starts probing),
`connection_window` and `buffer`. `stream_window` and `congestion` are QUIC's
alone: yamux guarantees every stream 256 KiB and grows it only into the
connection window's slack, so there is no per-stream knob to set, and TCP's
congestion control belongs to the kernel — so under `prefer = "tcp"` both
settings are accepted, validated and then ignored.

## Logging

Logs go to stderr, formatted for a terminal when stderr is one and as JSON
otherwise; `--log` overrides that choice. `-v` adds per-connection events, `-vv`
adds trace output, and `-q` restricts output to warnings and errors. The
`HAWSE_LOG` environment variable takes a `tracing` filter directive and
overrides all of them.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or
  <http://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
