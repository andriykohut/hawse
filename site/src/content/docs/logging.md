---
title: Logging
description: Where the logs go, and how to change their format and level.
tableOfContents: false
---

Logs go to stderr, formatted for a terminal when stderr is one and as JSON
otherwise; `--log` overrides that choice. Colour follows the same rule, and
`--color` overrides it, for `--help` and errors as well. `-v` adds
per-connection events, `-vv` adds trace output, and `-q` restricts output to
warnings and errors. The `HAWSE_LOG` environment variable takes a `tracing`
filter directive and overrides all of them.

On macOS, `--log oslog` sends the log to the unified log, which keeps and
expires it, under the subsystem `io.github.andriykohut.hawse` and the category
`server`, `client` or `cli`. A service installed with `hawse service install`
logs there. `/usr/bin/log stream --predicate 'subsystem == "io.github.andriykohut.hawse"'`
follows it, and `/usr/bin/log show --last 1d` with the same predicate reads it
back; the full path is because zsh has a `log` builtin of its own.

On a terminal the client keeps a header and a row for each service at the
bottom of the screen in place of its log lines, redrawn as the connections
through each service and their rates change, and the server prints a header when
it starts and a row for each client and service, with warnings and errors logged
beside them. `--log pretty`, `-v`, `-q` and `HAWSE_LOG` each bring the log lines
back.

In the client's rows, the public address of a TCP service is a link on a
terminal that supports them: `https://` when the service's local port is 443 or
8443, `http://` otherwise, and none for local port 22.

When the client stops it prints how long it ran, how many times it reconnected,
and what each service carried: its visitors, the bytes each way, and for a UDP
service the datagrams each way and those dropped. Without the rows, the same
figures are logged at info level as a `stopped` line and a `service totals` line
for each service.
