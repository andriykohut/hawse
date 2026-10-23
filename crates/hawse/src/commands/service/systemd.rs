//! The systemd unit `hawse service install` writes: the contrib unit for the role, run from the
//! binary and on the config install was given.

use std::fmt::Write as _;
use std::path::PathBuf;

use super::MARKER;
use crate::paths::Role;

/// The account a unit runs as in place of a throwaway one, and the directory it writes its key
/// to.
pub struct Account<'a> {
    pub name: &'a str,
    pub key_dir: &'a str,
}

/// Where the unit for `role` is installed.
pub fn file(role: Role) -> PathBuf {
    PathBuf::from(format!("/etc/systemd/system/hawse-{}.service", role.name()))
}

/// The contrib unit for `role`, marked as install's, running `exe` on `config`. With an
/// `account` it runs as that account, may write to the account's key directory, and has systemd
/// make no directories for it.
pub fn unit(role: Role, exe: &str, config: &str, account: Option<&Account<'_>>) -> String {
    let contrib = contrib(role);
    let name = role.name();
    let mut text = format!(
        "# {MARKER} Run it again to update this file, and put changes in a drop-in (systemctl edit hawse-{name}).\n"
    );
    let (mut exec, mut user) = (false, account.is_none());
    for line in contrib.lines() {
        let directory =
            line.starts_with("ConfigurationDirectory=") || line.starts_with("StateDirectory=");
        match account {
            _ if line.starts_with("ExecStart=") => {
                exec = true;
                // `:` turns off `$` expansion, which would reach the arguments but not the path
                // systemd looks the binary up by.
                let _ = writeln!(
                    text,
                    "ExecStart=:{} {name} --config {} --log pretty",
                    word(exe),
                    word(config)
                );
            }
            Some(account) if line == "DynamicUser=yes" => {
                user = true;
                let _ = writeln!(
                    text,
                    "User={}\nReadWritePaths={}",
                    account.name,
                    word(account.key_dir)
                );
            }
            Some(_) if directory => {}
            _ => {
                text += line;
                text.push('\n');
            }
        }
    }
    // A contrib edit that drops either line fails the tests here rather than an install.
    assert!(exec, "the contrib unit has an ExecStart= line");
    assert!(user, "the contrib unit has a DynamicUser=yes line");
    text
}

/// The contrib unit for `role`, as this release ships it.
pub fn contrib(role: Role) -> &'static str {
    match role {
        Role::Server => include_str!("../../../units/hawse-server.service"),
        Role::Client => include_str!("../../../units/hawse-client.service"),
    }
}

/// Whether systemd runs a binary at `path`: it refuses one that holds a quote, a backslash or a
/// control character, however the path is quoted.
pub fn can_execute(path: &str) -> bool {
    !path.contains(['"', '\'', '\\']) && !path.chars().any(char::is_control)
}

/// `arg` as one word of a unit setting: `%` doubled so systemd does not read a specifier, and in
/// double quotes when it holds whitespace, `"` or `\`.
fn word(arg: &str) -> String {
    let arg = arg.replace('%', "%%");
    if arg.contains(|c: char| c.is_whitespace() || c == '"' || c == '\\') {
        format!("\"{}\"", arg.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        arg
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_server_unit_runs_the_binary_it_was_installed_from() {
        insta::assert_snapshot!(unit(
            Role::Server,
            "/usr/local/bin/hawse",
            "/etc/hawse/server.toml",
            None
        ));
    }

    #[test]
    fn a_unit_for_an_account_runs_as_it_and_writes_beside_its_config() {
        let account = Account {
            name: "someone",
            key_dir: "/mnt/pool/apps/hawse",
        };
        let text = unit(
            Role::Client,
            "/mnt/pool/apps/hawse/hawse",
            "/mnt/pool/apps/hawse/client.toml",
            Some(&account),
        );
        assert!(text.contains("\nUser=someone\nReadWritePaths=/mnt/pool/apps/hawse\n"));
        for gone in ["DynamicUser=", "ConfigurationDirectory=", "StateDirectory="] {
            assert!(!text.contains(gone), "{gone}");
        }
        insta::assert_snapshot!(text);
    }

    #[test]
    fn a_path_systemd_would_split_or_expand_is_quoted() {
        assert_eq!(word("/usr/local/bin/hawse"), "/usr/local/bin/hawse");
        assert_eq!(word("/opt/my apps/hawse"), "\"/opt/my apps/hawse\"");
        assert_eq!(word("/opt/50%/$x"), "/opt/50%%/$x");
        assert_eq!(word("/opt/a \"b\""), "\"/opt/a \\\"b\\\"\"");
    }

    #[test]
    fn a_binary_path_with_a_dollar_runs_as_it_is() {
        let text = unit(
            Role::Server,
            "/opt/a$b/hawse",
            "/etc/hawse/server.toml",
            None,
        );
        assert!(text.contains(
            "\nExecStart=:/opt/a$b/hawse server --config /etc/hawse/server.toml --log pretty\n"
        ));
    }

    #[test]
    fn systemd_runs_no_binary_from_a_path_with_a_quote_a_backslash_or_a_control_character() {
        assert!(can_execute("/usr/local/bin/hawse"));
        assert!(can_execute("/opt/my apps/hawse"));
        for path in ["/opt/a\"b", "/opt/a'b", "/opt/a\\b", "/opt/a\nb"] {
            assert!(!can_execute(path), "{path:?}");
        }
    }

    #[test]
    fn a_unit_install_wrote_is_known_by_its_marker() {
        assert!(super::super::is_ours(&unit(Role::Server, "/h", "/c", None)));
        assert!(!super::super::is_ours(include_str!(
            "../../../units/hawse-server.service"
        )));
    }
}
