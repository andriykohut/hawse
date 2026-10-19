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

On a terminal the client keeps a header and a row for each service at the
bottom of the screen in place of its log lines, redrawn as the connections
through each service and their rates change, and the server prints a header when
it starts and a row for each client and service, with warnings and errors logged
beside them. `--log pretty`, `-v`, `-q` and `HAWSE_LOG` each bring the log lines
back.
