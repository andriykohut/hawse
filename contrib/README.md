# Service units

`hawse-server.service` and `hawse-client.service` run hawse under systemd.
`hawse service install` writes them for the binary that runs it and the config
it reads, enables and starts them, and prints the public key:

```sh
sudo hawse service install server
```

The [install page](https://andriykohut.github.io/hawse/install/#running-as-a-service)
says more. As they are here, both expect the binary at `/usr/local/bin/hawse`
and their config at `/etc/hawse/server.toml` or `/etc/hawse/client.toml`, and
release archives carry them beside the binary.

Both units run under `DynamicUser=yes`, so there is no account to create and no
uid to keep track of. systemd creates `/etc/hawse` for the config, which stays
owned by root, and `/var/lib/hawse` for the key, which the service owns. A config
that sets no `key` keeps it there. For the socket `hawse status` asks, systemd
creates `/run/hawse-server` or `/run/hawse-client`.

The running server reads a changed `server.toml` within a second, and
`systemctl reload` sends either unit the SIGHUP that has it read its config
again.

## Ports below 1024

`DynamicUser` runs unprivileged, so binding a public port under 1024 needs one
capability. Add it to both lines with `systemctl edit hawse-server`, in a
drop-in that a reinstall leaves alone:

```ini
[Service]
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
AmbientCapabilities=CAP_NET_BIND_SERVICE
```

## Different paths

`hawse service install` writes the unit for the binary that runs it and the
config it is given, where `/usr/local/bin` or `/etc` is not a good home. On
TrueNAS SCALE, for example, put both the binary and the config on a pool, and
run the unit as the account that owns the config, which keeps the key beside
it. The binary goes in a directory only root can write, since root runs it to
install and to upgrade, and both refuse one another account could replace:

```sh
sudo install -D -m755 hawse /mnt/pool/hawse-bin/hawse
sudo /mnt/pool/hawse-bin/hawse service install client \
  --config /mnt/pool/apps/hawse/client.toml --user someone
```

`ProtectSystem=strict` makes everything outside the unit's own directories
read-only, so the unit it writes allows that one directory with
`ReadWritePaths`. A TrueNAS upgrade drops `/etc` and the unit with it; a
POSTINIT init script that runs the same command puts it back.

## Status

Both units are `Type=notify`. The server counts as started once it listens, and
the client once it has read its config, so a server that is down does not fail
the client's start. `systemctl status` shows a line that follows how each one
stands:

```
Status: "listening on [::]:4433, 2 clients: laptop, nas"
Status: "connected over QUIC, 2 services bound"
Status: "disconnected: server stopped answering, retrying"
```

## Logging

The units pass `--log pretty` because journald is the reader. Drop it to get one
JSON object per line, which `journalctl -u hawse-server -o cat` feeds to `jq`.
