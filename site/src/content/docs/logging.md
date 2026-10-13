---
title: Logging
description: Where the logs go, and how to change their format and level.
tableOfContents: false
---

Logs go to stderr, formatted for a terminal when stderr is one and as JSON
otherwise; `--log` overrides that choice. Colour follows the same rule, and
`--color` overrides it. `-v` adds per-connection events, `-vv` adds trace
output, and `-q` restricts output to warnings and errors. The `HAWSE_LOG`
environment variable takes a `tracing` filter directive and overrides all of
them.

On a terminal the client prints a header when it connects and a row for each
service in place of its log lines, and the server a header when it starts and
a row for each client and service, with warnings and errors logged beside
them. `--log pretty`, `-v`, `-q` and `HAWSE_LOG` each bring the log lines back.
