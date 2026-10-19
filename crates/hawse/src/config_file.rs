use std::fs;
use std::path::{Path, PathBuf};

use hawse_core::config::{ClientConfig, ConfigError, ServerConfig};
use miette::{Diagnostic, NamedSource, SourceSpan};
use serde::de::DeserializeOwned;

use crate::paths::{self, Located, Role};

#[derive(Debug, thiserror::Error, Diagnostic)]
pub enum LoadError {
    #[error("cannot read {path}")]
    #[diagnostic(help("check that the file exists and is readable by this user"))]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{message}")]
    #[diagnostic(help("{}", help(.message)))]
    Parse {
        message: String,
        #[source_code]
        src: NamedSource<String>,
        #[label("here")]
        span: Option<SourceSpan>,
    },
    #[error("{0}")]
    #[diagnostic(help("edit the config and start hawse again"))]
    Invalid(ConfigError),
    #[error("no client config at {0}")]
    #[diagnostic(help(
        "create it with `server`, `server_key` and an `[expose.NAME]` table, or pass --config"
    ))]
    MissingClient(PathBuf),
    #[error("no config at {server} or at {client}")]
    #[diagnostic(help("pass --config"))]
    Nothing { server: PathBuf, client: PathBuf },
}

/// The top-level keys a client's config has and a server's does not.
const CLIENT_ONLY: [&str; 4] = ["server", "server_key", "name", "expose"];
/// The top-level keys a server's config has and a client's does not.
const SERVER_ONLY: [&str; 6] = [
    "listen",
    "bind",
    "dynamic_ports",
    "quic_retry",
    "limits",
    "clients",
];

pub fn parse<T: DeserializeOwned>(path: &Path, text: &str) -> Result<T, LoadError> {
    toml::from_str(text).map_err(|e| LoadError::Parse {
        message: e.message().to_owned(),
        src: NamedSource::new(path.display().to_string(), text.to_owned()),
        span: e.span().map(SourceSpan::from),
    })
}

/// The help under a parse error's `message`: a suggestion where [`suggest`] has one.
fn help(message: &str) -> String {
    suggest(message).unwrap_or_else(|| "fix the highlighted value and start hawse again".to_owned())
}

/// The help for `message` when it names an unknown key or value, as serde's do, ``unknown field
/// `listne`, expected one of `listen`, …``: where a key of the other role's config belongs, or the
/// expected name nearest the unknown one. `None` for any other message, and for a name nothing is
/// near.
fn suggest(message: &str) -> Option<String> {
    let mut quoted = message
        .strip_prefix("unknown ")?
        .split('`')
        .skip(1)
        .step_by(2);
    let unknown = quoted.next()?;
    let expected: Vec<&str> = quoted.collect();
    // Only the top level of a server's config takes `listen`, and of a client's `server_key`.
    let other = if expected.contains(&"listen") {
        Some(("client", CLIENT_ONLY.as_slice()))
    } else if expected.contains(&"server_key") {
        Some(("server", SERVER_ONLY.as_slice()))
    } else {
        None
    };
    if let Some((role, keys)) = other
        && keys.contains(&unknown)
    {
        return Some(format!(
            "`{unknown}` belongs in a {role}'s config, read by `hawse {role}`"
        ));
    }
    // One edit in three characters, a swap of two neighbours counting as one, as cargo allows.
    let limit = (unknown.chars().count() / 3).max(1);
    expected
        .iter()
        .map(|name| (strsim::osa_distance(unknown, name), name))
        .filter(|&(distance, _)| distance <= limit)
        .min_by_key(|&(distance, _)| distance)
        .map(|(_, name)| format!("did you mean `{name}`?"))
}

fn read(path: &Path) -> Result<String, LoadError> {
    fs::read_to_string(path).map_err(|source| LoadError::Read {
        path: path.to_owned(),
        source,
    })
}

pub fn load_server(explicit: Option<PathBuf>) -> Result<(ServerConfig, Located), LoadError> {
    let located = paths::locate(Role::Server, explicit);
    let cfg = if located.exists {
        parse::<ServerConfig>(&located.file, &read(&located.file)?)?
    } else {
        ServerConfig::default()
    };
    cfg.validate().map_err(LoadError::Invalid)?;
    Ok((cfg, located))
}

pub fn load_client(explicit: Option<PathBuf>) -> Result<(ClientConfig, Located), LoadError> {
    let located = paths::locate(Role::Client, explicit);
    if !located.exists {
        return Err(LoadError::MissingClient(located.file));
    }
    let cfg = parse::<ClientConfig>(&located.file, &read(&located.file)?)?;
    cfg.validate().map_err(LoadError::Invalid)?;
    Ok((cfg, located))
}

/// The roles `check` has a config for: the one the file at `explicit` is written for, and without
/// that every role with a config where hawse looks for one.
pub fn roles(explicit: Option<&Path>) -> Result<Vec<Role>, LoadError> {
    if let Some(path) = explicit {
        let table = parse::<toml::Table>(path, &read(path)?)?;
        let client = CLIENT_ONLY.iter().any(|key| table.contains_key(*key));
        return Ok(vec![if client { Role::Client } else { Role::Server }]);
    }
    let [server, client] = [Role::Server, Role::Client].map(|role| paths::locate(role, None));
    let found: Vec<Role> = [(Role::Server, &server), (Role::Client, &client)]
        .into_iter()
        .filter_map(|(role, located)| located.exists.then_some(role))
        .collect();
    if found.is_empty() {
        return Err(LoadError::Nothing {
            server: server.file,
            client: client.file,
        });
    }
    Ok(found)
}

/// Where the client's key is: the path `client` loads it from, for `keygen` to write it to. A
/// config that is not there yet names no key, so the key goes beside where the config will be.
pub fn client_key_path(explicit: Option<PathBuf>) -> Result<PathBuf, LoadError> {
    let located = paths::locate(Role::Client, explicit);
    let key = if located.exists {
        parse::<ClientConfig>(&located.file, &read(&located.file)?)?.key
    } else {
        PathBuf::from(Role::Client.key_name())
    };
    Ok(located.dir.join(key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use miette::{GraphicalReportHandler, GraphicalTheme};

    fn render(err: &LoadError) -> String {
        let mut out = String::new();
        GraphicalReportHandler::new_themed(GraphicalTheme::unicode_nocolor())
            .render_report(&mut out, err)
            .unwrap();
        out
    }

    #[test]
    fn parse_errors_point_at_the_line() {
        let text = "listen = \"[::]:4433\"\n\n[clients.homelab]\nkey = \"not-a-key\"\n";
        let err = parse::<ServerConfig>(std::path::Path::new("server.toml"), text).unwrap_err();
        insta::assert_snapshot!(render(&err));
    }

    /// The help under the error `text` fails to load with as a config of `T`.
    fn help_for<T: DeserializeOwned>(text: &str) -> String {
        match parse::<T>(Path::new("x.toml"), text) {
            Err(err) => err.help().map(|help| help.to_string()).unwrap_or_default(),
            Ok(_) => panic!("{text} loaded"),
        }
    }

    const CLIENT: &str =
        "server = \"a:1\"\nserver_key = \"ed25519:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\"\n";

    #[test]
    fn a_mistyped_key_or_value_names_the_nearest() {
        let client = |rest: &str| help_for::<ClientConfig>(&format!("{CLIENT}{rest}"));
        assert_eq!(
            help_for::<ServerConfig>("listne = \"[::]:4433\"\n"),
            "did you mean `listen`?"
        );
        assert_eq!(
            help_for::<ServerConfig>("[transport]\ncongestion = \"bbrr\"\n"),
            "did you mean `bbr`?"
        );
        assert_eq!(
            client("[transport]\nprefer = \"qiuc\"\n"),
            "did you mean `quic`?"
        );
        assert_eq!(
            client("[expose.web]\nlocla = \"127.0.0.1:80\"\n"),
            "did you mean `local`?"
        );
        assert_eq!(
            client("[expose.dns]\nlocal = \"127.0.0.1:53\"\nport = \"53/upd\"\n"),
            "did you mean `udp`?"
        );
        assert_eq!(
            help_for::<ServerConfig>("foo = 1\n"),
            "fix the highlighted value and start hawse again"
        );
    }

    #[test]
    fn a_key_of_the_other_role_names_its_config() {
        assert_eq!(
            help_for::<ServerConfig>("server_key = \"ed25519:AAAA\"\n"),
            "`server_key` belongs in a client's config, read by `hawse client`"
        );
        assert_eq!(
            help_for::<ClientConfig>(&format!("{CLIENT}[clients.laptop]\nports = [\"80\"]\n")),
            "`clients` belongs in a server's config, read by `hawse server`"
        );
    }

    #[test]
    fn unknown_keys_point_at_the_key() {
        let text = "listne = \"[::]:4433\"\n";
        let err = parse::<ServerConfig>(std::path::Path::new("server.toml"), text).unwrap_err();
        let rendered = render(&err);
        assert!(rendered.contains("listne"), "{rendered}");
        assert!(rendered.contains("server.toml"), "{rendered}");
    }
}
