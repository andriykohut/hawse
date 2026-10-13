---
title: Usage
description: Start a server, join it from a client and expose a service.
---

Start the server:

```sh
hawse server
```

On first run it generates `server.key` and prints the corresponding public key.
Connections from unknown keys are refused, so until a client is authorized the
server accepts nothing.

Join it from the client, with the server's address and the key it printed:

```sh
hawse join tunnel.example.com:4433 --server-key ed25519:BBBB...
```

This generates the client's key, writes a `client.toml` naming the server, and
waits for the server to accept the key. Add the key it prints to `server.toml`
on the server, together with the ports that client may bind:

```toml
[clients.laptop]
key = "ed25519:AAAA..."
ports = ["2222"]
```

A running server reads the file again within a second of the change, so there
is nothing to restart. `join` ends at its next retry after that, which is at
most 30 s away.

Add the services to the `client.toml` it wrote:

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

`hawse check` reads a config the way `server` and `client` do and exits 2 when
it would not load, without starting anything or creating a key. `hawse check
--connect` also dials the server a client config names and logs the transport
it answered on. It hangs up before the client would introduce itself, so a
client already running with the same key keeps its session, and the server
does not say whether the key is authorized.

Both ends read their config again when the file changes, and on SIGHUP. The
server takes up `[clients.*]` without touching the other sessions: a client
whose table is gone is disconnected at once, one whose `bind` or `allow`
changed reconnects, and a fixed port taken out of `ports` is unbound. The
client takes up `[expose.*]` within its session, binding what was added and
unbinding what was removed, and reconnects for any other change. A SIGHUP also
has the client ask again for the services the server refused, which is how it
picks up a port granted since. Every other server setting waits for a restart,
and a file that does not load is logged and changes nothing.
