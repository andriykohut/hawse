use std::path::{Path, PathBuf};

use hawse_core::client::Client;
use hawse_core::identity::Identity;
use miette::{IntoDiagnostic as _, WrapErr as _};

use crate::config_file::{self, LoadError};
use crate::paths::{self, Role};

/// The key at `path`, or `None` when there is none yet. A start would create it; this writes
/// nothing.
fn load_key(path: &Path) -> miette::Result<Option<Identity>> {
    let pem = match std::fs::read_to_string(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        read => read,
    };
    pem.into_diagnostic()
        .and_then(|pem| Identity::from_pem(&pem).into_diagnostic())
        .map(Some)
        .wrap_err_with(|| format!("cannot load the key at {}", path.display()))
}

pub async fn run(config: Option<PathBuf>, connect: bool) -> miette::Result<()> {
    let mut client = None;
    for role in config_file::roles(config.as_deref())? {
        let (name, located, key, cfg) = match role {
            Role::Server => {
                let (cfg, located) = config_file::load_server(config.clone())?;
                ("server", located, cfg.key, None)
            }
            Role::Client => {
                let (cfg, located) = config_file::load_client(config.clone())?;
                ("client", located, cfg.key.clone(), Some(cfg))
            }
        };
        let key_path = located.dir.join(key);
        let identity = load_key(&key_path)?;
        let file = located.file.display();
        if let Some(identity) = &identity {
            tracing::info!(config = %file, key = %identity.public_key(), "valid {name} config");
        } else {
            tracing::info!(
                config = %file,
                key = %key_path.display(),
                "valid {name} config; its key is not there yet, and the first start creates it"
            );
        }
        if let Some(cfg) = cfg {
            client = Some((cfg, identity, key_path));
        }
    }
    if !connect {
        return Ok(());
    }
    let Some((cfg, identity, key_path)) = client else {
        return Err(LoadError::MissingClient(paths::locate(Role::Client, config).file).into());
    };
    let Some(identity) = identity else {
        miette::bail!(
            "no client key at {} to connect with; `hawse keygen` creates it",
            key_path.display()
        );
    };
    let server = cfg.server.clone();
    let (remote, transport) = Client::new(cfg, identity)
        .probe()
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("cannot connect to {server}"))?;
    tracing::info!(%remote, %transport, "the server answered, and its key is the config's `server_key`");
    Ok(())
}
