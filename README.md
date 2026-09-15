# hawse

Reverse tunnels with keys instead of secrets. A client behind NAT connects out
to a server with a public address; the server exposes the client's local TCP
services on public ports. One binary, QUIC with TLS 1.3, Ed25519 identity on
both ends.

Status: phase 1. TCP forwarding, fixed and dynamic ports, key-based
authorization. UDP, the TCP fallback transport, hot reload, `hawse expose`, and
`hawse authorize` are on the way; see `docs/backlog.md` and the spec under
`docs/superpowers/specs/`.

## Quickstart

On the server:

    hawse server

It creates `server.key`, prints the server's public key, and tells you no
clients are authorized yet.

On the client:

    hawse keygen

Copy the printed key into the server's `server.toml`:

    [clients.laptop]
    key = "ed25519:…"
    ports = ["2222"]

Restart the server for now (hot reload comes in phase 3). Then write
`client.toml` next to the client key:

    server = "tunnel.example.com:4433"
    server_key = "ed25519:…"          # printed by hawse server

    [expose.ssh]
    local = "127.0.0.1:22"
    port = 2222

    [expose.dev]
    local = "localhost:3000"          # no port: the server picks one

and run:

    hawse client

Config lives in `~/.config/hawse/` for a user or `/etc/hawse/` for root;
`--config` overrides. Logs go to stderr, JSON when stderr is not a terminal,
`-v` for per-connection detail.

## Building

    cargo build --release

Router and cross builds use the `ring` crypto provider:

    cargo build --release --no-default-features --features ring

## License

MIT or Apache-2.0, at your option.
