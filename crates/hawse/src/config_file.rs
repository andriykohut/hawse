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
    #[diagnostic(help("fix the highlighted value and start hawse again"))]
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

pub fn parse<T: DeserializeOwned>(path: &Path, text: &str) -> Result<T, LoadError> {
    toml::from_str(text).map_err(|e| LoadError::Parse {
        message: e.message().to_owned(),
        src: NamedSource::new(path.display().to_string(), text.to_owned()),
        span: e.span().map(SourceSpan::from),
    })
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
        // The top-level keys a client's config has and a server's does not.
        let client = ["server", "server_key", "expose"]
            .iter()
            .any(|key| table.contains_key(*key));
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

    #[test]
    fn unknown_keys_point_at_the_key() {
        let text = "listne = \"[::]:4433\"\n";
        let err = parse::<ServerConfig>(std::path::Path::new("server.toml"), text).unwrap_err();
        let rendered = render(&err);
        assert!(rendered.contains("listne"), "{rendered}");
        assert!(rendered.contains("server.toml"), "{rendered}");
    }
}
