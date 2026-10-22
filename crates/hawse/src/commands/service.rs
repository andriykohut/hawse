//! `hawse service`: run the server or the client as a service that starts by itself, as a systemd
//! unit on Linux or a launchd agent on macOS, from files hawse writes and later recognises as its
//! own.

pub mod launchd;
pub mod systemd;

/// The words that mark a unit or an agent as written by `hawse service install`, which replaces
/// and removes only a file that carries them.
pub const MARKER: &str = "Written by hawse service install.";

/// Whether `text` is a unit or an agent install wrote: the marker is on its first line, or on the
/// second after a plist's XML declaration.
pub fn is_ours(text: &str) -> bool {
    text.lines().take(2).any(|line| line.contains(MARKER))
}
