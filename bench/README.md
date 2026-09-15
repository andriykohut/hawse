# Benchmark

`hawse-bench` measures two things about a TCP path: how fast a bulk transfer
moves, and what a small concurrent request costs while that transfer runs. It
takes a host and port, so it can measure a forwarded port or the service
directly, and it does not know which it is talking to.

    cd bench && cargo build --release

Run the service side, which drains a bulk connection and echoes a probe one:

    bench/target/release/hawse-bench sink 9000

Then drive it, for six seconds with a hundred probes:

    bench/target/release/hawse-bench load 127.0.0.1:9000 6 100

    throughput_mib_s 268.5
    probe_unloaded p50 0.18 p99 0.31 max 0.35 n 100
    probe_loaded p50 1.78 p99 7.30 max 7.56 n 100

A fifth argument caps the bulk sender in MiB/s:

    bench/target/release/hawse-bench load 127.0.0.1:9000 8 150 12

Use it. An uncapped transfer saturates the CPU, and the probe then measures
that rather than the path: the same probe degrades on a direct connection with
no tunnel in it at all. A cap near the uplink a deployment really has is the
only setting where the latency numbers mean anything.

It is a separate crate so it never builds as part of the workspace or its CI.

## Measure the path, not one leg of it

Running the load generator beside the tunnel server measures the tunnel and
nothing else, and that number can point the wrong way. Measured from the server
itself, one congestion controller looked two to three times slower than an
alternative; measured from where a viewer actually sits, with the reply
travelling back out over the same household link, the two were level and the
supposedly slower one moved more data. Run it from the machine that will
consume the traffic.
