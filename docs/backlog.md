# Backlog

Items agreed during design but deliberately left out of v1, and review findings
parked for later. Each entry says what it is and why it waits.

## Deferred from the 2026-09-12 design

- **SNI routing.** One public 443 shared by many HTTPS services, routed by
  server name without terminating TLS. Waits because it has its own design
  surface (name matching, defaults, interaction with allowlists).
- **Metrics.** Prometheus endpoint on the server: bytes, connections, latency
  per service. Cheap; waits for the core to settle first.
- **WebSocket transport.** Lets the tunnel sit behind Cloudflare or pass
  HTTP-only egress. A third `Transport` impl. Only if users ask.
- **Invite-based enrollment.** `hawse server invite` minting one-time tokens so
  a new client enrolls itself, plus a persistent store of enrolled keys. Waits
  because v1 scope is "you control both ends".
- **`hawse status`.** Needs a local admin socket on the running process to
  report connected clients, bound ports, and unknown keys that knocked.
- **Windows.** Nothing in the design prevents it; not a v1 target.
- **MIPS.** Kept only if the week-one ring spike on mipsel-musl succeeds.
- **Per-client allowlist ceiling on the server.** Server-side `allow` that
  intersects with client-declared allowlists. Useful once clients are not the
  server admin.
- **Passphrase-protected private keys.** File permissions only in v1.
- **Connection-per-visitor on the TCP fallback.** The rejected alternative to
  yamux: a fresh TLS connection per visitor plus a pre-opened pool, the shape
  other reverse tunnels take. Kept on file because yamux puts every visitor in one congestion
  window, so a video stream at link rate delays interactive requests behind it.
  Revisit only if the streaming benchmark shows the fallback transport is
  unusable for streaming; QUIC, the primary path, does not have this problem.
