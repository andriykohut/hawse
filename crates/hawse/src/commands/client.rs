use std::path::PathBuf;

use hawse_core::client::{Client, DisconnectCause, Event};
use hawse_core::config::split_host_port;
use hawse_core::identity::Identity;
use miette::{IntoDiagnostic as _, WrapErr as _};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config_file;

pub async fn run(config: Option<PathBuf>) -> miette::Result<()> {
    let (cfg, located) = config_file::load_client(config)?;
    let key_path = located.dir.join(&cfg.key);
    let (identity, created) = Identity::load_or_create(&key_path)
        .into_diagnostic()
        .wrap_err_with(|| format!("cannot load the client key at {}", key_path.display()))?;
    if created {
        tracing::info!(path = %key_path.display(), key = %identity.public_key(), "created client key");
    }
    let host = split_host_port(&cfg.server)
        .map(|(h, _)| h)
        .unwrap_or_default();
    let locals: std::collections::BTreeMap<String, String> = cfg
        .expose
        .iter()
        .map(|(name, e)| (name.clone(), e.local.clone()))
        .collect();
    let key = identity.public_key();
    let client = Client::new(cfg, identity);
    let cancel = CancellationToken::new();
    tokio::spawn(super::shutdown_signal(cancel.clone()));
    let (tx, mut events) = mpsc::channel(64);
    let printer = tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            match event {
                Event::Connected {
                    remote,
                    transport,
                    name,
                    agent,
                } => {
                    tracing::info!(%remote, %transport, name = %name, server = %agent, "connected");
                }
                Event::Bound { service, port } => {
                    let local = locals.get(&service).cloned().unwrap_or_default();
                    tracing::info!("{service}  {host}:{port} <- {local}");
                }
                Event::BindFailed { service, reason } => tracing::warn!("{service}  {reason}"),
                Event::Denied { key } => tracing::warn!(
                    "not authorized. on the server, add to server.toml:\n[clients.NAME]\nkey = \"{key}\"\nthen wait; retrying every 5 s"
                ),
                Event::Disconnected { cause } => {
                    let reason = match cause {
                        DisconnectCause::Shutdown(why) => {
                            format!("server ended the session: {why}")
                        }
                        DisconnectCause::Unresponsive => "server stopped answering".to_owned(),
                        DisconnectCause::Denied => {
                            "this machine's key is not authorized on the server".to_owned()
                        }
                        DisconnectCause::Transport(reason) | DisconnectCause::Config(reason) => {
                            reason
                        }
                    };
                    tracing::warn!(%reason, "disconnected; retrying in 5 s");
                }
            }
        }
    });
    tracing::info!(%key, "client key");
    client.run(cancel, tx).await;
    printer.await.into_diagnostic()?;
    Ok(())
}
