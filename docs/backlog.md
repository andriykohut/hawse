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
  Measured for UDP on 2026-09-18 with one link-rate TCP download beside it:
  service-to-visitor UDP lost 13-50% at 50-100 Mbit/s over QUIC and 16-45% over
  the fallback, so for UDP the shared window cost the fallback nothing QUIC did
  not also pay. The fallback's own limit is the other direction, where one
  yamux stream carries at most 40-65 Mbit/s.

## Deferred from the phase 2a transport work

- **The TCP accept path is unauthenticated.** A semaphore now caps concurrent
  in-flight TLS handshakes at 256, so exhaustion queues in the kernel backlog
  instead of reaching EMFILE, but nothing yet rate-limits a peer that keeps
  completing handshakes: `limits.auth_failures_per_minute` does not take effect,
  and QUIC's listen side has `quic_retry` available for the same job and does
  not use it either.
- **No way to decline the TCP listener entirely.** The bind is mandatory and
  fatal, so a deployment that will only ever use QUIC still publishes an
  unauthenticated TCP accept path. A `transport.tcp_fallback = false` switch is
  the right shape for it; it was left out as new config surface belonging to a
  later phase.
- **Streams past the control stream are never accepted on a session.** The
  server calls `accept_bi` once, for the control stream, and never again. On
  QUIC the extras sit in quinn's accept queue against the client's stream
  credit; on TCP they accumulate in `TcpTransport`'s unbounded inbound channel
  and hold slots in yamux's stream budget, which counts both directions in one
  number and kills the connection rather than backpressuring when it is full.
  Harmless against our own client, which never opens one, but it is an
  authenticated client's way to end its own session.
- **A refusal is unobservable on the TCP fallback.** `client::visitor::refuse`
  resets the stream with `reset::UNKNOWN_SERVICE` or `reset::LOCAL_REFUSED`, and
  a QUIC visitor's read fails with that code. On yamux neither half carries one,
  so the visitor gets a clean empty close and reads a refusal as a service that
  answered with nothing. Same root cause as the truncation gap below, and the
  same application-level signal fixes both.
- **`transport.prefer = "auto"` does not fall back to TCP.** It dials QUIC and
  nothing else, because yamux hands a reset stream to its reader as a clean
  end-of-stream: a transfer cut short on the fallback arrives looking complete,
  and neither end can tell. Moving a user onto that because their network blocks
  UDP trades a loud failure for a silent one, so the fallback is reachable only
  by asking for it. Give the tunnel an application-level completeness signal — a
  trailer on each visitor stream, or a length the reader checks — and
  `Client::connect`'s `Prefer::Auto` arm can fall back again. Lifting the gate
  means splitting `Prefer::Auto` back off `Prefer::Quic`, choosing a probe
  deadline and racing `connect_tcp` after it; the deadline wants measuring
  against a real high-latency path, since the 2 s this branch carried for a
  while was sized on loopback and would fail a satellite handshake that works.
- **`Transport::close()` does not complete symmetrically.** On QUIC it is
  immediate and `endpoint.wait_idle()` bounds the flush; on TCP it only cancels
  a token, and the driver then flushes yamux's queue for up to `idle_timeout` in
  a task the server's `TaskTracker` does not know about. The process still
  exits, but a shutdown that looks drained may not be.
- **`SendHalf::Tcp::reset` is nondeterministic in the peer's write direction.**
  Its single noop-waker `poll_shutdown` normally emits FIN, but when that poll
  returns `Pending` the close reaches the peer as an RST from the stream drop
  instead — leaving it in `RecvClosed` or in `Closed`, and only the latter fails
  its next write. This matters for the completeness signal above: a probe-write
  was the leading candidate for telling an abort from a clean finish, and it
  cannot be, while `reset` picks between FIN and RST on timing.

## Deferred from the phase 2b UDP work

- **PROXY protocol for UDP needs the visitor's address on the wire.** A packet
  carries a service id and a session id; the client never learns who the
  visitor is. PROXY v2 on a UDP service needs the address sent once per
  session, which is a wire change.
- **A bulk stream that ends is not reopened.** If the stream fails while the
  session lives, payloads too large for a datagram drop, and on the TCP
  transport all of them do, until the service is bound again. A transport
  failure ends the session anyway, so this waits for a case where the stream
  dies alone.
- **Some drops are not counted.** A datagram naming a service id nobody holds
  has no service to be counted against. quinn's own queue drops are counted as
  `queue_full` when a send finds the queue full, but one send can evict more
  than one older datagram, so the count is a floor.
- **The idle time and the bulk queue depth are constants**, 60 s and 256
  packets. A knob for either waits for a measurement that asks for one.
- **A hostname `local` is resolved on the client's datagram path.** Each new
  session looks `local` up before it opens its socket, and the one task that
  routes datagrams waits for it, so a slow resolver pauses every UDP service on
  the connection for the length of the lookup, once per new visitor. Sessions
  are opened by whoever sends to the public port, so the lookup rate is theirs
  to set, and the lookup has no timeout. A literal address is never looked up.
  Resolving per session is what lets a hostname follow DNS the way the TCP path
  does: caching per service would go stale until the client reconnects, and
  handing each first packet to a task of its own is unbounded under a burst.
  Waits for a design that keeps both. The README tells users to give a literal
  address.
- **Service ids wrap.** `ServiceIds` reuses an id after 65,536 binds in one
  session, skipping only ids still bound. With UDP, a reply still in flight for
  the old service names an id the new service now holds, and the new service's
  sessions start again from 0, so that reply could reach a different visitor.
  Not reachable until hot reload can bind and unbind in a loop; worth closing
  before reload lands.
- **The client's session table can be filled from outside.** The server evicts
  at its cap without telling the client, so what bounds the client's sockets is
  its own cap of 4096 per service, a constant, and a sender rotating source
  ports holds it there whatever `udp_sessions_per_service` says. At the cap the
  client drops the session quiet longest, as the server does, so new visitors
  still get in. 4096 is above the usual soft descriptor limit of 1024. Waits
  for a decision between a knob, a shorter life for a session `local` never
  answered, and an eviction notice on the wire.
- **One yamux stream carries UDP on the TCP fallback.** From visitor to
  service it tops out between 40 and 65 Mbit/s, about 5,000 packets a second,
  and the bulk queue drops the rest: the stream's 256 KiB window over the round
  trip to a home connection is the limit.
- **The server's QUIC socket keeps the kernel's default receive buffer.** With
  `bbr` on the client, bursts overran the box's 208 KiB buffer and it dropped
  13-212 datagrams in 6 of 10 runs at 100 Mbit/s. Raising it needs `SO_RCVBUF`
  on the socket and a `net.core.rmem_max` above the distribution's default, so
  it is a deployment note as much as a code change.

## Deferred from the release work

- **Keys default to the config directory.** `paths::locate` gives config and
  keys one directory, so wherever config is read-only the key needs an absolute
  path: the systemd units need `key = "/var/lib/hawse/server.key"`, and the image
  sets `XDG_CONFIG_HOME=/var/lib` so `keygen` and a server without config can
  write theirs. Defaulting keys to a state directory (`$STATE_DIRECTORY` under
  systemd, `$XDG_STATE_HOME/hawse`, `/var/lib/hawse` for root) would remove
  both. Waits because it moves where existing installs look for their keys.

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

### Config

- `ConfigError::ServiceName` and `::ListenPort` have no direct tests; add one
  each.
- Nothing checks that `listen` falls outside `dynamic_ports`, or that a fixed
  grant does not name the listen port; both would fail later at bind time.
- `quic_retry` and `auth_failures_per_minute` parse but do not take effect yet,
  and nothing warns that they are ignored.
  `transport.stream_window` and `transport.congestion` join them under
  `prefer = "tcp"`, where yamux guarantees every stream 256 KiB and grows it
  only into the connection window's slack, leaving no per-stream knob, and the
  kernel owns congestion control: both are validated and then silently inert.
  The README and `Tuning` now say so; a warning at startup would say it
  louder.

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
  tasks one session can hold. On the TCP fallback the consequence differs in
  kind, not degree — yamux has no credit to push back with, and the first
  stream past `max_num_streams` tears the whole connection down, so a burst
  QUIC merely queues kills every service on that session at once.
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

- The control-channel scaffolding (framed stream, ping bookkeeping, `send`
  and `next`) is duplicated in the server and the client; extract it before
  the transport trait arrives.
- `client::Event` carries strings where it could carry errors; callers cannot
  match on a cause.
- The listen port is reserved from grants and from the pool now: `validate`
  rejects both, and `Server::bind` reserves the *resolved* port rather than the
  configured one. The latter also closed a latent bug — a configured port of 0
  used to reserve 0, and Linux's ephemeral range overlaps the default
  40000-41000 pool, so the kernel could hand the listener a port the allocator
  would later hand to a service.
- Visitor accept needs a per-session bound: past `streams_per_client` every
  accepted socket waits on a stream while holding a descriptor. UDP sessions
  have their own cap.

### From the final re-review

- **Session teardown can exceed the server's 5 s drain.** A session's 4 s drain plus the 2 s `Shutdown` linger can outlast the outer 5 s wait; the process still exits, but with a spurious "did not drain in time" warning. Shorten the linger or start it inside the drain budget.
- **The supersede wait is not cancel-aware.** A session superseding another at the moment the server is cancelled sits up to 5 s before noticing. Select on cancel as well.
- **`expect("a validated buffer")` is reachable from the library API.** `Server::bind` and `Client::run_once` do not call `validate()`; a 32-bit caller with an unvalidated 4 GiB buffer panics where the sibling window conversion returns an error. Validate inside those entry points or return an error.
- **One `%err` log remains without its cause chain.** The "cannot bind" warning in the server session; route it through `error::chain` like the rest.
- **Two live processes sharing one key supersede each other forever.** Each `Shutdown` triggers the other's reconnect. Intended consequence of one-session-per-key; phase 2's backoff should at least make it slow, and the server log should say which remote won.
- **A session that panics between insert and retire leaves its map entry.** Every later session for that key then pays the full 5 s supersede wait. Make the entry removal a drop guard.
- **TIME_WAIT can refuse an immediate rebind of a freed fixed port** despite `SO_REUSEADDR`. Pre-existing; a retry-once on `EADDRINUSE` for fixed ports would cover it.
- **A SIGINT during dial is not cancel-aware.** `Client::run_once` awaits `self.connect(remote)` outside the cancel-aware `select!`; only the post-connect loop watches `cancel`. Same as pre-branch, so not a regression against `main` — but dropping the 2 s probe deadline raised the wait from ≤2 s to quinn's ~30 s default `idle_timeout`, so `systemctl stop` can now block that long mid-dial. Make the dial itself cancel-aware.
- **The fatal-`DisconnectCause` predicate is duplicated.** `client/mod.rs` and `hawse/src/commands/client.rs` each independently test `matches!(cause, DisconnectCause::Config(_))`; nothing keeps the two in sync, and drift would make the CLI print "retrying in 5 s" for a cause that exits, or the reverse. Expose the predicate once from `hawse-core` and have the CLI call it.

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
  control.
- **Favour streams that have transferred least.** Written and parked on the
  local `stream-priority-ladder` branch, unmeasured; `main` does not carry it.
  A visitor stream keeps quinn's default priority of 0 until it has sent
  64 KiB, then drops one step per doubling to a floor of -8 at 8 MiB, so a
  request-shaped stream outranks one pushing a hundred megabytes. No wire
  change and no config.

  It waits on evidence because two objections look fatal on paper. Priority
  only reorders what is sent inside one congestion window, so it cannot touch
  the throughput gap recorded below, which is what motivated it. And every
  fresh stream starts at 0, so a port taking a trickle of short connections can
  hold a long transfer at the floor indefinitely: the starvation the ladder
  exists to prevent, reached from the other side. Lifetime bytes also mislabels
  a long-lived SSH or database connection as bulk, where a recent-window rate
  would not.

  To measure it, build the branch and `main`, run the service and `hawse
  client` beside each other, `hawse server` on the public box, and the load
  generator from the machine that consumes the traffic, which is the same
  vantage as the numbers below and not the server. Alternate the two binaries
  in one sitting under the same cap:

      hawse-bench sink 9000
      hawse-bench load <public-host>:<port> 30 200 12

  A lower probe p99 at unchanged throughput is the ladder working. Lower
  throughput is the starvation above, and the floor or the first step is wrong.
- **Revisit the streaming criterion against evidence.** The 3x rule was written
  before anything existed and no configuration meets it, a direct connection
  included, so it does not discriminate.

### From measuring over a real link

- **One congestion window does not fill a long path the way many do.** Over a
  40 ms link with forty parallel transfers, a connection-per-visitor tunnel
  reached 214 Mbit/s with a 44 ms concurrent request, where hawse managed
  65-114 Mbit/s at 45 ms on `cubic`, or 187 Mbit/s at 109 ms on `bbr`. Neither
  setting gets both, because many small queues in parallel beat one large one.
  This is the evidence for revisiting connection-per-visitor, which was
  dismissed on loopback numbers that could not show the effect: a round trip of
  zero hides everything congestion control does.
- **A fixed pool of connections per session.** Designed on 2026-09-16 in
  `docs/superpowers/specs/2026-09-16-connection-pool-design.md`, spiked the
  same day, and **not built**: the gate failed. Over the real 40 ms path,
  bulk throughput was flat across one, two, four and eight client
  connections (144-199 Mbit/s at one, 140-167 at more), because a single QUIC
  connection already reached the home downlink's ceiling and left no idle
  capacity for a pool to claim. The probe cost 142 ms idle and 145 ms under
  load at every N, so there was no starvation to relieve either. The
  2026-09-15 numbers that motivated the work (cubic 65 vs rathole 214) did
  not reproduce, so the evidence for the pool did not survive a second look;
  likely that evening's conditions or a handshake-bound harness, not
  one-window-vs-many. Revisit only if a deployment shows one connection
  failing to fill a path that parallel flows fill. The spec and
  `~/code/hawse-bench-compare/spike/` keep the method.
- **The client's `cubic` collapses on a path with random loss.** Measured on
  2026-09-18 and 19 with UDP datagrams at 100 Mbit/s from service to visitor,
  ten runs per setting: about 0.01% of packets lost on the home uplink cut
  `cubic`'s window by 30% each time, it regrew over seconds, and the client's
  1 MiB datagram queue dropped the excess, for a median 28% loss (0.4-41%);
  `bbr` lost 0.03% (worst 0.3%). A larger initial window (25% median) or a
  4 MiB queue (11%) did not stop the collapse. The same mechanism fits the
  2026-09-15 numbers above and their failure to reproduce the next evening,
  since how much it costs depends on that evening's loss. Whether `bbr` should
  be the client default waits on a real-path run of `cubic`, `bbr` and a
  connection-per-visitor tunnel with the interactive probe, because `bbr`
  queued the probe at 109 ms on 2026-09-15.
