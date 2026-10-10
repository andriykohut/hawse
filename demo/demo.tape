# The README's recording. From the repository root, after `cargo build --release`:
#
#     vhs demo/demo.tape
#
# Needs vhs, tmux, python3 and curl, and ports 4433, 8080 and 3000 free on loopback.
# HAWSE_DEMO_PORT moves the first of them.

Output assets/hawse-demo.gif

Require tmux
Require python3
Require curl

Set Shell bash
Set FontSize 15
Set Width 1200
Set Height 620
Set Padding 20
Set TypingSpeed 45ms
Set Theme "Catppuccin Mocha"

Hide
Type "sh demo/start.sh" Enter
Wait+Screen@10s /visitor/
Sleep 500ms
Show

Sleep 1s
Type "hawse server" Sleep 400ms Enter
Wait+Screen@10s /hawse join/
Sleep 2s

Ctrl+B
Type "o"
Sleep 500ms
Type "cat client.toml" Sleep 400ms Enter
Sleep 2500ms
Type "hawse client" Sleep 400ms Enter
Wait+Screen@10s /localhost:8080/
Sleep 3s

Ctrl+B
Type "o"
Sleep 500ms
Type "curl localhost:8080" Sleep 400ms Enter
Wait+Screen@10s /hello/
Sleep 4s

Hide
Ctrl+B
Type ":kill-server" Enter
Sleep 500ms
