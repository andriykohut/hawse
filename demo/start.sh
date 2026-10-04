#!/bin/sh
# Sets up a throwaway server and client and opens them in three tmux panes, for demo.tape to
# type into. The keys and the server's config are made here, off screen.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
bin=$here/../target/release
root=${TMPDIR:-/tmp}/hawse-demo
port=${HAWSE_DEMO_PORT:-4433}
tmux="tmux -L hawse-demo -f $here/tmux.conf"

if [ ! -x "$bin/hawse" ]; then
    echo "no hawse binary in $bin: run cargo build --release first" >&2
    exit 1
fi
PATH=$bin:$PATH
export PATH

$tmux kill-server 2>/dev/null || true
rm -rf "$root"
mkdir -p "$root/server/hawse" "$root/client/hawse" "$root/www"
echo "hello from the laptop" > "$root/www/index.html"

server_key=$(hawse -q keygen --out "$root/server/hawse/server.key")
client_key=$(hawse -q keygen --out "$root/client/hawse/client.key")

# The public port answers on loopback only, so nothing is published while this records.
cat > "$root/server/hawse/server.toml" <<TOML
listen = "[::]:$port"
bind = "127.0.0.1"

[clients.laptop]
key = "$client_key"
ports = ["8080"]
TOML

cat > "$root/client/hawse/client.toml" <<TOML
server = "localhost:$port"
server_key = "$server_key"

[expose.web]
local = "127.0.0.1:3000"
port = 8080
TOML

export BASH_SILENCE_DEPRECATION_WARNING=1
export PS1='\[\e[38;2;232;99;43m\]$\[\e[0m\] '

# Each pane finds its own config through XDG_CONFIG_HOME, so the commands typed are the bare
# ones. The local service runs in a second window, out of sight.
exec $tmux new-session -s demo -c "$root/server/hawse" -e "XDG_CONFIG_HOME=$root/server" \; \
    select-pane -T "server, on a public address" \; \
    split-window -v -c "$root/client/hawse" -e "XDG_CONFIG_HOME=$root/client" \; \
    select-pane -T "client, behind NAT" \; \
    split-window -v -c "$root" \; \
    select-pane -T "visitor" \; \
    new-window -d "python3 -m http.server 3000 --bind 127.0.0.1 -d $root/www" \; \
    resize-pane -t demo:0.0 -y 10 \; \
    resize-pane -t demo:0.1 -y 13 \; \
    select-pane -t demo:0.0
