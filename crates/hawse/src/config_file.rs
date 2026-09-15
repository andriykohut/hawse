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
