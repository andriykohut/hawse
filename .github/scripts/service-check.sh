#!/usr/bin/env bash
# Installs both units on a runner with systemd, checks the client connects, then removes them.
set -euo pipefail
bin=/usr/local/bin/hawse
me=$(id -un)
sudo install -m755 target/debug/hawse "$bin"
server_key=$(sudo "$bin" service install server | sed -n 's/^server key: //p')
test -n "$server_key"
# hawse upgrade finds the binary a unit runs as systemd reports it.
systemctl show -p ExecStart --value hawse-server | grep -q "path=$bin "
# A second install is how a unit is upgraded.
sudo "$bin" service install server >/dev/null
sudo install -d -o "$me" /srv/hawse
printf 'server = "127.0.0.1"\nserver_key = "%s"\n' "$server_key" >/srv/hawse/client.toml
client_key=$("$bin" -q keygen --config /srv/hawse/client.toml)
printf '[clients.ci]\nkey = "%s"\n' "$client_key" | sudo tee /etc/hawse/server.toml >/dev/null
sudo "$bin" service install client --config /srv/hawse/client.toml --user "$me"
for _ in $(seq 40); do
  systemctl show -p StatusText --value hawse-client | grep -q '^connected' && break
  sleep 0.5
done
systemctl show -p StatusText --value hawse-client | grep '^connected'
sudo "$bin" status server | grep -q '1 of 1 clients'
"$bin" status client | grep -q connected
sudo "$bin" service uninstall client
sudo "$bin" service uninstall server
test ! -e /etc/systemd/system/hawse-client.service
test ! -e /etc/systemd/system/hawse-server.service
sudo test -f /var/lib/hawse/server.key
test -f /srv/hawse/client.key
