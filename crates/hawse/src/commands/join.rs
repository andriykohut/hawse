use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use hawse_core::client::{Client, DisconnectCause, Event};
use hawse_core::config::ClientConfig;
use hawse_core::identity::Identity;
use hawse_proto::key::PublicKey;
use miette::{IntoDiagnostic as _, WrapErr as _};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config_file::{self, LoadError};
use crate::paths::{self, Role};

/// Never over a file that is there: a config someone wrote is not this command's to replace.
fn write_new(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?
        .write_all(text.as_bytes())
}

/// 0 once the server has welcomed this machine's key.
///
/// The config is written when the server first answers, welcome or denial: either one shows that
/// the address is right and the key behind it is `server_key`. An address that does not answer,
/// or answers with another key, leaves no config behind to start a client on.
pub async fn run(
    server: String,
    server_key: PublicKey,
    config: Option<PathBuf>,
) -> miette::Result<ExitCode> {
    let located = paths::locate(Role::Client, config);
    let file = located.file;
    if located.exists {
        miette::bail!(
            "a client config is already at {}, and `hawse client` runs it",
            file.display()
        );
    }
    let text = format!(
        "server = {}\nserver_key = \"{server_key}\"\n",
        toml::Value::from(server.as_str())
    );
    // Read back the way a start reads it, so what is written is a config that loads.
    let cfg: ClientConfig = config_file::parse(&file, &text)?;
    cfg.validate().map_err(LoadError::Invalid)?;
    let key_path = located.dir.join(&cfg.key);
    let (identity, created) = Identity::load_or_create(&key_path)
        .into_diagnostic()
        .wrap_err_with(|| format!("cannot load the client key at {}", key_path.display()))?;
    if created {
        tracing::info!(path = %key_path.display(), "created client key");
    }

    let cancel = CancellationToken::new();
    tokio::spawn(super::shutdown_signal(cancel.clone()));
    let (tx, mut events) = mpsc::channel(64);
    let session = tokio::spawn({
        let cancel = cancel.clone();
        async move { Client::new(cfg, identity).run(cancel, tx).await }
    });
    let mut written = false;
    let mut joined = false;
    let mut failed = None;
    // Ends when the client does, which drops the last sender.
    while let Some(event) = events.recv().await {
        if matches!(event, Event::Connected { .. } | Event::Denied { .. }) && !written {
            write_new(&file, &text)
                .into_diagnostic()
                .wrap_err_with(|| format!("cannot write {}", file.display()))?;
            written = true;
            tracing::info!(config = %file.display(), "wrote the client config");
            if let Event::Denied { key } = &event {
                // ponytail: the client's backoff paces the wait, so up to 30 s pass after the
                // table is added. A shorter wait is a retry loop of its own, kept under the
                // server's `auth_failures_per_minute`.
                tracing::warn!(
                    "not authorized yet. on the server, add to server.toml:\n[clients.NAME]\nkey = \"{key}\"\nwaiting for it"
                );
            }
        }
        match event {
            Event::Connected { name, .. } => {
                tracing::info!("joined as {name}");
                joined = true;
                cancel.cancel();
            }
            Event::Disconnected { cause, .. }
                if !written || !matches!(cause, DisconnectCause::Denied) =>
            {
                let reason = super::client::reason(cause);
                if written {
                    tracing::warn!(%reason, "disconnected; still waiting");
                } else {
                    failed = Some(reason);
                    cancel.cancel();
                }
            }
            _ => {}
        }
    }
    session.await.into_diagnostic()?;
    if let Some(reason) = failed {
        miette::bail!("cannot join {server}, and no config was written: {reason}");
    }
    Ok(if joined {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}
