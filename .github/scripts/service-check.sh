#!/usr/bin/env bash
# Installs both units on a runner with systemd, checks the client connects, then removes them.
set -euo pipefail
# Not /usr/local/bin, which the runner image makes writable by everyone and service install refuses.
bin=/usr/local/sbin/hawse
me=$(id -un)
sudo install -D -m755 target/debug/hawse "$bin"
# A key left in /var/lib/hawse, which both units shared before #148, is refused; moved where the
# refusal says, it stays the server's identity.
old_key=$(sudo "$bin" -q keygen --out /var/lib/hawse/server.key)
if sudo "$bin" service install server; then exit 1; fi
sudo mkdir -p /var/lib/hawse-server && sudo mv /var/lib/hawse/server.key /var/lib/hawse-server/
server_key=$(sudo "$bin" service install server | sed -n 's/^server key: //p')
test "$server_key" = "$old_key"
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
# Both units under throwaway accounts keep their own keys: before #148 the unit that started last
# took the shared directory, and the server failed its next start.
sudo cp /srv/hawse/client.toml /etc/hawse/client.toml
client_key=$(sudo "$bin" -q keygen --config /etc/hawse/client.toml)
printf '[clients.ci]\nkey = "%s"\n' "$client_key" | sudo tee /etc/hawse/server.toml >/dev/null
sudo "$bin" service install client >/dev/null
sudo systemctl restart hawse-server
# The client pins the server's key and the server grants the client's, so this proves both kept.
for _ in $(seq 40); do
  sudo "$bin" status server | grep -q '1 of 1 clients' && break
  sleep 0.5
done
sudo "$bin" status server | grep -q '1 of 1 clients'
sudo "$bin" service uninstall client
sudo "$bin" service uninstall server
test ! -e /etc/systemd/system/hawse-client.service
test ! -e /etc/systemd/system/hawse-server.service
sudo test -f /var/lib/hawse-server/server.key
sudo test -f /var/lib/hawse-client/client.key
test -f /srv/hawse/client.key
