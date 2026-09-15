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
- **MIPS static linking.** The mipsel build links dynamically against
  `/lib/ld-musl-mipsel-sf.so.1`; release packaging should try `+crt-static` and
  fall back to documenting the loader requirement.
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

## Deferred from the phase 1 reviews

Findings from the task and branch reviews that were real but out of scope for
phase 1. Each says what the code does today and what the fix would be.

### Proto

- A single-port `PortRange` and every `PortSpan` have no `Display` test; add
  both next to the range cases that do.
- `PortSpan` prints `40000-40000` for a one-port span; render the bare number
  when `first == last`.
- `stream_headers_round_trip` always puts v4 in `visitor` and v6 in
  `listener`; swap one case so each field sees both families.
- `name::validate` checks the length first, so a 40-character name starting
  with `-` reports `TooLong`; check the first character before the length.

### Identity and TLS

- `WrongAlgorithm` doubles as the error for a key whose length is wrong, which
  Ed25519 keys cannot reach; give the length case its own variant or drop the
  branch.
- `certificate()` clones the DER on every call; hand out a shared `Arc` built
  once per identity.
- `peer_key` ignores bytes trailing the SubjectPublicKeyInfo and compares only
  the algorithm OID rather than the whole `AlgorithmIdentifier`; webpki's
  re-parse covers the handshake path, so tighten it when the parser is
  revisited.
- `supported_verify_schemes` advertises every scheme the provider has instead
  of Ed25519 alone; narrow it to the one algorithm we accept.
- A client presenting a certificate whose private key it does not hold has no
  test; add one that fails the handshake.
- `PeerKeyError::Der` and `::Length` are never constructed in a test; feed the
  parser a truncated and an over-long key.
- The verifiers are `pub` where `pub(crate)` is enough; narrow them.
- `provider()` allocates a new `CryptoProvider` on every call; cache it in a
  `OnceLock`.
- The handshake test helper's inner read loops have no progress guard, so a
  stalled peer hangs the test instead of failing it.
- RFC 7250 raw public keys would replace the certificate with the key itself
  and collapse the two-parser trust path into one.

### QUIC

- `listen`'s IPv4 fallback branch never runs on a dual-stack host, so it is
  untested; exercise it with an endpoint that refuses v6.
- The `connect` comment lost the clause explaining that the server name is a
  placeholder because the verifier ignores SNI; restore it.
- `client/mod.rs` dials only the first address the resolver returns; try the
  rest before giving up.

### Cross builds

- The mipsel binary links dynamically against the musl loader, as the design
  list above already records.
- MIPS is proven to build and link, not to run; nothing executes the binary.
- CI installs `cross` unpinned, so a new release can change the build under
  us; pin the version.
- The cross workflow has never run on a real runner, only locally.

### Config

- `ConfigError::ServiceName` and `::ListenPort` have no direct tests; add one
  each.
- Nothing checks that `listen` falls outside `dynamic_ports`, or that a fixed
  grant does not name the listen port; both would fail later at bind time.
- `congestion`, `quic_retry`, `auth_failures_per_minute`,
  `udp_sessions_per_service` and `prefer` parse but do not take effect yet,
  and nothing warns that they are ignored.

### Pump

- The `writer.shutdown()` error at half-close is discarded; report it or say
  in the code why it cannot matter.
- The 8 MiB `assert_eq` dumps both buffers when it fails; compare lengths and
  the first differing index instead.
- `write_frame` is not cancel-safe, so wrapping it in a timeout would truncate
  a frame; note the constraint or make it atomic before anyone does.

### Server

- Visitor pumps keep running after `Unbind`; only the listener stops, so
  existing visitors outlive the service.
- The 100 ms backoff after an accept error is not cancel-aware and delays
  shutdown by up to that long.
- The unknown-key log line is unbounded until the phase 2 rate limiter lands,
  so a stranger can fill the log.
- The 45 s liveness deadline is only checked on the 15 s tick, so detection
  lands between 45 and 60 s.
- Two early exits close the connection without an explicit code, so the client
  cannot tell them apart.
- `limits.streams_per_client` bounds the streams the client may open, not the
  visitor streams the server opens toward it.
- Visitor accept is unbounded: only QUIC stream credit limits how many visitor
  tasks one session can hold.
- Connection close codes are ad hoc integer literals; name them next to the
  `reset` codes in the proto crate.
- A malformed control frame ends the session exactly like a clean EOF, logged
  as "client left".
- `SO_KEEPALIVE` from spec section 5 is not set on visitor or local sockets.
- The unknown-key log names `hawse authorize`, which does not exist before
  phase 3.
- Config errors exit 1 where the spec asks for 2.

### Client

- The 45 s liveness deadline is only checked on the 15 s tick, so an
  unresponsive server is detected somewhere in [45, 60) s.
- The early returns before `Welcome` skip the explicit close and
  `endpoint.wait_idle()` that the normal path does.
- `run` takes `&self` and shares one `targets` map across reconnects; a
  per-session owner would make the lifetime obvious.
- The visitor module is private to the crate, so its handler cannot be tested
  directly.

### Tests

- The corruption assertion in the 100 MiB transfer reports the start of the
  chunk, not the offending byte.
- `rlimit`'s result and the echo server's `accept` are unwrapped, so file
  descriptor exhaustion surfaces as a stream-credit failure.
- The dynamic-port walk is O(n^2) and holds every claim for the length of the
  walk, and no named test guards that behavior.
- `free_port` is TOCTOU by design: the port it returns can be taken before the
  caller binds it.
- The insta snapshot lives under `src/snapshots` rather than beside the test
  that owns it.

### Phase 2 shape

- `BindFailure` needs `Unsupported` and `BadName`, and stream opens need a
  `StreamOpen` enum, before UDP bulk streams go on the wire; today every
  refusal is `BadPort`.
- The control-channel scaffolding (framed stream, ping bookkeeping, `send`
  and `next`) is duplicated in the server and the client; extract it before
  the transport trait arrives.
- `client::Event` carries strings where it could carry errors; callers cannot
  match on a cause.
- The listen port is not reserved from grants or from the dynamic pool, which
  will collide once the TCP fallback listener shares it.
- Visitor accept needs a per-session bound before UDP sessions add another
  unbounded map.

### From the final re-review

- **Session teardown can exceed the server's 5 s drain.** A session's 4 s drain plus the 2 s `Shutdown` linger can outlast the outer 5 s wait; the process still exits, but with a spurious "did not drain in time" warning. Shorten the linger or start it inside the drain budget.
- **The supersede wait is not cancel-aware.** A session superseding another at the moment the server is cancelled sits up to 5 s before noticing. Select on cancel as well.
- **`expect("a validated buffer")` is reachable from the library API.** `Server::bind` and `Client::run_once` do not call `validate()`; a 32-bit caller with an unvalidated 4 GiB buffer panics where the sibling window conversion returns an error. Validate inside those entry points or return an error.
- **One `%err` log remains without its cause chain.** The "cannot bind" warning in the server session; route it through `error::chain` like the rest.
- **A superseded client's message contradicts itself.** `ClientError::Shutdown` renders "server is shutting down: another session for this key took over". Split the variant or reword the prefix.
- **Two live processes sharing one key supersede each other forever.** Each `Shutdown` triggers the other's reconnect. Intended consequence of one-session-per-key; phase 2's backoff should at least make it slow, and the server log should say which remote won.
- **A session that panics between insert and retire leaves its map entry.** Every later session for that key then pays the full 5 s supersede wait. Make the entry removal a drop guard.
- **TIME_WAIT can refuse an immediate rebind of a freed fixed port** despite `SO_REUSEADDR`. Pre-existing; a retry-once on `EADDRINUSE` for fixed ports would cover it.

### From the bind setting

- **A specific bind can shadow a wildcard bind on the same port.** Public
  listeners set `SO_REUSEADDR`, so on a host already serving port 8096 on every
  interface, a hawse service bound to `127.0.0.1:8096` binds successfully and
  takes loopback traffic for itself instead of failing with `EADDRINUSE`. It
  surfaced as a flake between two servers in one test binary. Worth a warning
  when a bound port is already answering, or a documented note.

### From the first benchmark

- **A bulk transfer delays a concurrent small request.** At a capped 12 MiB/s
  the probe's p99 is 1.85 ms against 0.34 ms with no tunnel, and the spec's own
  criterion of 3x the idle p99 is met by neither hawse at 5.6x nor a direct
  connection at 2.6x. Shrinking the client's `stream_window` does not move it,
  so the queue is in congestion control and packet scheduling, not stream flow
  control. Two things to try: wire the `congestion` setting so BBR can be
  measured, and revisit the criterion against evidence rather than the guess it
  was written from.
