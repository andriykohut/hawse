# hawse

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

Waiting on features that are not implemented yet:

- **WireGuard, DNS, and most game servers** need UDP forwarding.
- **Databases and admin interfaces** should wait for source-address
  allowlists, or be restricted by a firewall on the server. A public port is
  reachable by anyone today.
- **Services that log or rate-limit by client address** need PROXY protocol v2
  to see the real visitor address rather than the client's own connection.
- **Many HTTPS services on one port 443** need SNI routing.
- **Networks that block outbound UDP** need the TCP fallback transport.

## Status

TCP forwarding works, with fixed or dynamically assigned public ports.

Besides the features named above, configuration hot reload and the `expose`,
`authorize`, `revoke` and `check` subcommands are not implemented either.
`docs/backlog.md` lists everything that is planned or deliberately deferred.

## Building

    cargo build --release

The binary is `target/release/hawse`. Cross builds use the `ring` crypto
provider in place of the default `aws-lc-rs`:

    cargo build --release --no-default-features --features ring

## Usage

Start the server:

    hawse server

On first run it generates `server.key` and prints the corresponding public key.
Connections from unknown keys are refused, so until a client is authorized the
server accepts nothing.

Generate a key on the client:

    hawse keygen

Add the printed key to `server.toml` on the server, together with the ports
that client may bind:

    [clients.laptop]
    key = "ed25519:AAAA..."
    ports = ["2222"]

Restart the server. Configuration is read at startup only.

Write `client.toml` on the client:

    server = "tunnel.example.com:4433"
    server_key = "ed25519:BBBB..."

    [expose.ssh]
    local = "127.0.0.1:22"
    port = 2222

    [expose.dev]
    local = "127.0.0.1:3000"

Then run:

    hawse client

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

    listen = "[::]:4433"
    key = "server.key"
    dynamic_ports = "40000-41000"

The listen port is UDP, because QUIC runs over UDP. Public ports bound for
clients are TCP.

Client settings: `server` and `server_key` are required, `key` defaults to
`client.key`, and each `[expose.NAME]` table needs a `local` address.

## Logging

Logs go to stderr, formatted for a terminal when stderr is one and as JSON
otherwise; `--log` overrides that choice. `-v` adds per-connection events, `-vv`
adds trace output, and `-q` restricts output to warnings and errors. The
`HAWSE_LOG` environment variable takes a `tracing` filter directive and
overrides all of them.

## License

MIT or Apache-2.0, at your option.
