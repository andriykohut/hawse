---
title: Reverse TCP and UDP tunnel over QUIC with Ed25519 authentication
template: splash
hero:
  tagline: "Only the client dials out, so the network holding the services needs no inbound connectivity: no port forwarding, no public address, no firewall rules for incoming traffic."
  actions:
    - text: Install
      link: /hawse/install/
    - text: Usage
      link: /hawse/usage/
      variant: secondary
    - text: GitHub
      link: https://github.com/andriykohut/hawse
      icon: external
      variant: minimal
---

## What it is for

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
  you list, and the server turns everyone else away. See [Configuration](/hawse/configuration/).
- **Networks that block outbound UDP.** When QUIC does not connect, the client
  carries the tunnel over TLS instead, and `transport.prefer = "tcp"` asks for
  that outright.
- **A client that changes network.** Over QUIC a session follows its client to
  a new address, and the connections open through it carry on.
- **WireGuard, DNS, and game servers.** `port = "51820/udp"` exposes a UDP
  service. Read the note on UDP under [Configuration](/hawse/configuration/) first: a payload too large
  for one QUIC datagram is carried differently.
