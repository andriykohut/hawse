# Measurements

The numbers behind hawse's defaults and limits. Unless a section says
otherwise, they were taken from a home connection through a Hetzner server
27 ms away. `bench/` measures the same things on the path you have.

## Congestion control

Measured from a home connection through a Hetzner server 27 ms away, with
forty parallel transfers through the client and a small request timed alongside
them, median of four runs each: `bbr` reached 204 Mbit/s with the request at
157 ms (200 ms at p90), a tunnel opening one connection per visitor 199 Mbit/s
at 158 ms (211), and `cubic` 192 Mbit/s at 165 ms (255). That path lost no
packets; on one that does, `cubic` falls further behind, as the UDP section
below shows.

## UDP

Measured with `iperf3` from a home connection through a Hetzner server: payloads
of 1100 bytes, small enough for a datagram before QUIC has probed the path,
crossed from visitor to service with at most 0.04% loss and under 1 ms of
jitter at 10, 50 and 100 Mbit/s, and from service to visitor with none up to
50 Mbit/s. At 100 Mbit/s from service to visitor a client on `cubic` lost
between 0.4% and 41% of packets over ten runs, median 28%: it shrinks its
window on every packet the path loses, and while it regrows the client queues
only about 1 MiB of datagrams and drops the oldest. On `bbr`, the client's
default, the same runs lost 0.03%, worst 0.3%. The service end line in the
client's log counts these drops as `queue_full`.
With `prefer = "tcp"`, traffic from visitor to service shares one yamux stream
and tops out between 40 and 65 Mbit/s, dropping the rest; from service to
visitor it lost under 2.5% at every rate.

## The fallback delay

`transport.prefer = "auto"` gives QUIC 2 seconds before it dials the TCP
fallback beside it. That is the smallest whole second above the 99th percentile
of a QUIC handshake on a path losing 5% of its packets each way. A thousand
handshakes for each row, three hundred for the last two, in milliseconds:

| Path | Dial | p50 | p90 | p99 | max |
|---|---|---|---|---|---|
| clean | QUIC | 36 | 37 | 39 | 57 |
| clean | TLS over TCP | 62 | 65 | 67 | 73 |
| 5% lost each way | QUIC | 36 | 1036 | 1605 | 3043 |
| 5% lost each way | TLS over TCP | 63 | 1060 | 1298 | 3467 |
| 10% lost each way | QUIC | 36 | 1041 | 2072 | 20816 |
| 10% lost each way | TLS over TCP | 63 | 1064 | 3064 | 6515 |
| 300 ms added each way | QUIC | 636 | 638 | 641 | 664 |
| 300 ms added each way | TLS over TCP | 1263 | 1266 | 1269 | 1270 |

A lost first packet costs about a second and two in a row about three, so one
lost packet does not move a session onto the fallback. Added latency alone
never does: QUIC connects in one round trip and TLS over TCP in two, and the
TCP dial starts 2 seconds later.

## What the fallback's records cost

The TCP fallback writes every stream in records, so that a stream that was cut
short can be told from one that finished. With forty parallel transfers through
the client and a small request timed alongside them, median of the runs:

| Version, transport | Runs | Throughput | Request | Request at p90 |
|---|---|---|---|---|
| 0.4.0, fallback | 12 | 208 Mbit/s | 152 ms | 178 ms |
| 0.5.0, fallback | 8 | 204 Mbit/s | 155 ms | 203 ms |
| 0.5.0, QUIC | 8 | 203 Mbit/s | 157 ms | 191 ms |

Throughput stayed inside the spread of the 0.4.0 runs. The request is about
3 ms slower at the median and about 25 ms at the 90th percentile. In the first
two runs after the 0.5.0 client reconnected, the 90th percentile read 465 and
422 ms; 0.4.0's first run after its own reconnect read 165 ms. That is not
explained.

On loopback, where the tunnel is the only limit, the fallback moved 1070 MiB/s
from client to server where 0.4.0 moved 1141, and 1199 MiB/s from server to
client where 0.4.0 moved 1118, median of three runs each. QUIC did not move.

## Earlier findings

Measurements that decided a change was not worth making, or that the sections
above have since replaced. Kept on file.

### From the first benchmark

- **A bulk transfer delays a concurrent small request.** At a capped 12 MiB/s
  the probe's p99 is 1.85 ms against 0.34 ms with no tunnel, and the spec's own
  criterion of 3x the idle p99 is met by neither hawse at 5.6x nor a direct
  connection at 2.6x. Shrinking the client's `stream_window` does not move it,
  so the queue is in congestion control and packet scheduling, not stream flow
  control.
- **Favour streams that have transferred least.** Written on the local
  `stream-priority-ladder` branch on 2026-09-16 and dropped on 2026-10-02
  without landing.
  A visitor stream keeps quinn's default priority of 0 until it has sent
  64 KiB, then drops one step per doubling to a floor of -8 at 8 MiB, so a
  request-shaped stream outranks one pushing a hundred megabytes. No wire
  change and no config.

  The real path gave it nothing to fix. Under forty transfers the probe cost
  142 ms idle and 145 ms loaded in the pool spike, and on 2026-10-01 `bbr` put
  it at 157 ms p50 against 158 for the connection-per-visitor tunnel (both
  below), so no request was waiting behind bulk traffic for priority to move
  ahead. Two objections also looked fatal on paper. Priority
  only reorders what is sent inside one congestion window, so it cannot touch
  the throughput gap recorded below, which is what motivated it. And every
  fresh stream starts at 0, so a port taking a trickle of short connections can
  hold a long transfer at the floor indefinitely: the starvation the ladder
  exists to prevent, reached from the other side. Lifetime bytes also mislabels
  a long-lived SSH or database connection as bulk, where a recent-window rate
  would not.

  Revisit only if a real path shows a request queued behind bulk traffic. To
  measure it, build the change and `main`, run the service and `hawse
  client` beside each other, `hawse server` on the public box, and the load
  generator from the machine that consumes the traffic, which is the same
  vantage as the numbers below and not the server. Alternate the two binaries
  in one sitting under the same cap:

      hawse-bench sink 9000
      hawse-bench load <public-host>:<port> 30 200 12

  A lower probe p99 at unchanged throughput is the ladder working. Lower
  throughput is the starvation above, and the floor or the first step is wrong.

### From measuring over a real link

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
- **One congestion window does not fill a long path the way many do.** Over a
  40 ms link with forty parallel transfers, a connection-per-visitor tunnel
  reached 214 Mbit/s with a 44 ms concurrent request, where hawse managed
  65-114 Mbit/s at 45 ms on `cubic`, or 187 Mbit/s at 109 ms on `bbr`. Neither
  setting gets both, because many small queues in parallel beat one large one.
  On 2026-10-01 `bbr` got both; see the last entry.
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
  since how much it costs depends on that evening's loss. The client now
  defaults to `bbr`, after a real-path run of all three with the interactive
  probe on 2026-10-01: a loss-free 27 ms path, forty transfers, four
  interleaved runs each, medians. `bbr` 204 Mbit/s with the probe at 157 ms
  p50 and 200 ms p90; the connection-per-visitor tunnel 199 at 158 / 211;
  `cubic` 192 at 165 / 255. The 109 ms probe `bbr` showed on 2026-09-15 did
  not reproduce. The server stays on `cubic`: traffic from visitor to service
  was not measured.
