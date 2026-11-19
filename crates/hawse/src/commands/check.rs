use std::path::{Path, PathBuf};

use hawse_core::client::Client;
use hawse_core::identity::Identity;
use hawse_core::transport::quic;
use miette::{IntoDiagnostic as _, WrapErr as _};

use crate::config_file::{self, LoadError};
use crate::paths::{self, Role};

/// The key at `path`, or `None` when there is none yet. A start would create it; this writes
/// nothing.
pub(crate) fn load_key(path: &Path) -> miette::Result<Option<Identity>> {
    let pem = match std::fs::read_to_string(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        read => read,
    };
    pem.into_diagnostic()
        .and_then(|pem| Identity::from_pem(&pem).into_diagnostic())
        .map(Some)
        .wrap_err_with(|| format!("cannot load the key at {}", path.display()))
}

/// What to tell an operator whose server would get a QUIC receive buffer short of what it asks
/// for, or `None` when this host grants it or the probe fails.
pub(crate) fn receive_buffer_hint() -> Option<&'static str> {
    quic::receive_buffer().ok().and_then(buffer_hint)
}

fn buffer_hint(granted: usize) -> Option<&'static str> {
    (granted < quic::RECEIVE_BUFFER).then_some(
        "net.core.rmem_max holds the server's QUIC socket below the receive buffer it asks for, \
         so it drops datagrams under load: `sysctl -w net.core.rmem_max=7500000` raises the \
         limit, `net.core.rmem_max = 7500000` in a file under /etc/sysctl.d/ keeps it across \
         reboots, and the server takes it when it restarts",
    )
}

pub async fn run(config: Option<PathBuf>, connect: bool) -> miette::Result<()> {
    let mut client = None;
    for role in config_file::roles(config.as_deref())? {
        let (located, key, cfg) = match role {
            Role::Server => {
                let (cfg, located) = config_file::load_server(config.clone())?;
                (located, cfg.key, None)
            }
            Role::Client => {
                let (cfg, located) = config_file::load_client(config.clone())?;
                (located, cfg.key.clone(), Some(cfg))
            }
        };
        let name = role.name();
        let key_path = paths::key_path(role, &located, key.as_deref());
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
        if let Some(hint) = matches!(role, Role::Server)
            .then(receive_buffer_hint)
            .flatten()
        {
            tracing::warn!("{hint}");
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

#[cfg(test)]
mod tests {
    use hawse_core::transport::quic::RECEIVE_BUFFER;

    use super::buffer_hint;

    #[test]
    fn a_capped_buffer_gets_the_lines_that_lift_the_cap() {
        assert_eq!(buffer_hint(RECEIVE_BUFFER), None);
        assert_eq!(buffer_hint(2 * RECEIVE_BUFFER), None);
        let hint = buffer_hint(RECEIVE_BUFFER - 1).unwrap();
        assert!(
            hint.contains("sysctl -w net.core.rmem_max=7500000"),
            "{hint}"
        );
        assert!(hint.contains("/etc/sysctl.d/"), "{hint}");
    }
}
