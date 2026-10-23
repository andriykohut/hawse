//! The launchd agent `hawse service install` writes on macOS: it starts the role at login, runs
//! the binary on the config install was given, and logs to the unified log.

use std::path::{Path, PathBuf};

use super::MARKER;
use crate::paths::Role;

/// The unified log's subsystem for an agent's lines, and the prefix of every agent's label.
pub const SUBSYSTEM: &str = "io.github.andriykohut.hawse";

/// The agent's label for `role`, which `launchctl` names it by.
pub fn label(role: Role) -> String {
    format!("{SUBSYSTEM}.{}", role.name())
}

/// Where the agent for `role` is installed, under the home directory `home`.
pub fn file(home: &Path, role: Role) -> PathBuf {
    home.join("Library/LaunchAgents")
        .join(format!("{}.plist", label(role)))
}

/// Where launchd writes what the agent prints outside the unified log: an error before logging
/// starts, or a panic.
pub fn log_file(home: &Path, role: Role) -> PathBuf {
    home.join("Library/Logs/hawse")
        .join(format!("{}.log", role.name()))
}

/// The agent for `role`, marked as install's: it runs `exe` on `config` at login, starts it again
/// 5 s after it ends, and sends its output outside the unified log to `log`.
pub fn plist(role: Role, exe: &str, config: &str, log: &str) -> String {
    let [label, exe, config, log] = [&label(role), exe, config, log].map(escape);
    let role = role.name();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!-- {MARKER} Run it again to update this file. -->
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{label}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{exe}</string>
		<string>{role}</string>
		<string>--config</string>
		<string>{config}</string>
		<string>--log</string>
		<string>oslog</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>ThrottleInterval</key>
	<integer>5</integer>
	<key>StandardOutPath</key>
	<string>{log}</string>
	<key>StandardErrorPath</key>
	<string>{log}</string>
</dict>
</plist>
"#
    )
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_client_agent_runs_the_binary_it_was_installed_from() {
        insta::assert_snapshot!(plist(
            Role::Client,
            "/opt/hawse/hawse",
            "/Users/someone/.config/hawse/client.toml",
            "/Users/someone/Library/Logs/hawse/client.log"
        ));
    }

    #[test]
    fn a_path_is_escaped_for_xml() {
        let text = plist(Role::Server, "/opt/a&b/<hawse>", "/c", "/l");
        assert!(text.contains("<string>/opt/a&amp;b/&lt;hawse&gt;</string>"));
    }

    #[test]
    fn an_agent_install_wrote_is_known_by_its_marker() {
        assert!(super::super::is_ours(&plist(
            Role::Server,
            "/h",
            "/c",
            "/l"
        )));
        assert_eq!(label(Role::Client), "io.github.andriykohut.hawse.client");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn an_agent_is_a_plist_macos_reads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.plist");
        std::fs::write(&path, plist(Role::Client, "/h", "/c", "/l")).unwrap();
        let lint = std::process::Command::new("plutil")
            .arg("-lint")
            .arg(&path)
            .status()
            .unwrap();
        assert!(lint.success());
    }
}
