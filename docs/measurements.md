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

On loopback, where the tunnel is the only limit, the fallback moved 1075 MiB/s
from client to server where 0.4.0 moved 1156, and 1200 MiB/s from server to
client where 0.4.0 moved 1119. QUIC did not move.
