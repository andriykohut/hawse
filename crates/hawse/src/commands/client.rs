use std::path::PathBuf;
use std::process::ExitCode;

use hawse_core::client::{Client, DisconnectCause, Event};
use hawse_core::config::split_host_port;
use hawse_core::identity::Identity;
use miette::{IntoDiagnostic as _, WrapErr as _};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config_file;

/// 2 when a config error stopped the client, the exit of a config that does not load: starting
/// it again will not fix either.
pub async fn run(config: Option<PathBuf>) -> miette::Result<ExitCode> {
    let (cfg, located) = config_file::load_client(config)?;
    let inert = cfg.transport.inert();
    if !inert.is_empty() {
        tracing::warn!(
            config = %located.file.display(),
            settings = inert.join(", "),
            "[transport] settings that tune QUIC do nothing under prefer = \"tcp\""
        );
    }
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
        let mut stopped = false;
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
                    "not authorized. on the server, add to server.toml:\n[clients.NAME]\nkey = \"{key}\"\nthen wait for the next retry"
                ),
                Event::Disconnected { cause, retry_in } => {
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
                    stopped |= retry_in.is_none();
                    match retry_in {
                        Some(wait) => tracing::warn!(
                            %reason,
                            "disconnected; retrying in {:.1} s",
                            wait.as_secs_f64()
                        ),
                        None => {
                            tracing::error!(%reason, "disconnected; the config cannot be retried");
                        }
                    }
                }
            }
        }
        stopped
    });
    tracing::info!(%key, "client key");
    client.run(cancel, tx).await;
    let stopped = printer.await.into_diagnostic()?;
    Ok(if stopped {
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    })
}
