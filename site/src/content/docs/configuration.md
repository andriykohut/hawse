---
title: Configuration
description: The server and client configuration files and their defaults.
tableOfContents: false
---

Configuration is read from `$XDG_CONFIG_HOME/hawse/` (`~/.config/hawse/` by
default), falling back to `/etc/hawse/` when the file is not there. `--config
PATH` and the `HAWSE_CONFIG` environment variable override the location.
Relative key paths resolve against the directory containing the config file.
A config that sets no `key` uses `server.key` or `client.key` beside it when
that file is there. Otherwise the key goes in `$STATE_DIRECTORY`, which a
systemd unit sets to `/var/lib/hawse/`, then in `/var/lib/hawse/` if running as
root, and beside the config otherwise.

Server settings and their defaults:

```toml
listen = "[::]:4433"
bind = "::"
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
[Measurements](/hawse/measurements/)
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
carry: the control stream, one per visitor connection and one per UDP service.
It defaults to 4096, and a visitor that arrives when every stream is taken is
reset. A client accepts 4096 streams from the server, so a value above 4097 does
nothing more. On the TCP fallback, yamux keeps room for twice this many streams
and promises each 256 KiB of receive window, so the default lets a session hold
about 2.06 GiB of unread data.
Lower it on a server with little memory, but not below the number of visitor
connections a client carries at once.
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

Client settings: `server` and `server_key` are required, `key` defaults as
above, and each `[expose.NAME]` table needs a `local` address.

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
[Measurements](/hawse/measurements/).

A visitor's connection that is cut short ends in a connection reset, on either
transport: when the local service resets it, when nothing is listening on
`local`, or when the tunnel itself drops. One that finished ends as the service
ended it. On the fallback that takes a marker written into every stream, at a
small cost to a request competing with heavy transfers, which
[Measurements](/hawse/measurements/)
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
[Measurements](/hawse/measurements/)
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
