# Service units

`hawse-server.service` and `hawse-client.service` run hawse under systemd. Both
expect the binary at `/usr/local/bin/hawse` and their config at
`/etc/hawse/server.toml` or `/etc/hawse/client.toml`.

```sh
install -m755 target/release/hawse /usr/local/bin/hawse
install -m644 contrib/hawse-server.service /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now hawse-server
```

Both units run under `DynamicUser=yes`, so there is no account to create and no
uid to keep track of. systemd creates `/etc/hawse` for the config, which stays
owned by root, and `/var/lib/hawse` for the key, which the service owns. Point
the config at the second one:

```toml
key = "/var/lib/hawse/server.key"
```

A relative `key` resolves next to the config file, where the service cannot
write.

## Getting the public key

Because the service owns its key, generate it by starting the service and
reading the key it prints:

```sh
systemctl start hawse-server
journalctl -u hawse-server -n 20 | grep 'server key'
```

The same works for the client. Paste the client's key into the server's
`server.toml` and restart the server; configuration is read at startup only.

## Ports below 1024

`DynamicUser` runs unprivileged, so binding a public port under 1024 needs one
capability. Add it to both lines in the server unit:

```ini
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
AmbientCapabilities=CAP_NET_BIND_SERVICE
```

## Different paths

`systemctl edit hawse-client` to override `ExecStart` where `/usr/local/bin` or
`/etc` is not a good home. On TrueNAS SCALE, for example, put both the binary
and the config on a pool:

```ini
[Service]
ExecStart=
ExecStart=/mnt/pool/apps/hawse/hawse client --config /mnt/pool/apps/hawse/client.toml --log pretty
ReadWritePaths=/mnt/pool/apps/hawse
```

`ProtectSystem=strict` makes everything outside the unit's own directories
read-only, so a config directory elsewhere needs the `ReadWritePaths` line above
for the service to create its key.

## Logging

The units pass `--log pretty` because journald is the reader. Drop it to get one
JSON object per line, which `journalctl -u hawse-server -o cat` feeds to `jq`.
