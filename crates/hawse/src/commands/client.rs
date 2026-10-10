use std::path::{Path, PathBuf};
use std::process::ExitCode;

use hawse_core::client::{Client, DisconnectCause, Event};
use hawse_core::config::{ClientConfig, split_host_port};
use hawse_core::identity::Identity;
use miette::{IntoDiagnostic as _, WrapErr as _};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use super::Reloads;
use crate::config_file;
use crate::paths::{self, Role};
use crate::terminal::{self, Style};

/// Reads `file` and no other: a reload that looked the config up again could find one that has
/// since appeared somewhere the lookup tries first, which is not the one being watched.
fn load(file: &Path, terminal: Option<Style>) -> miette::Result<(ClientConfig, Identity)> {
    let (cfg, located) = config_file::load_client(Some(file.to_owned()))?;
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
        if let Some(style) = terminal {
            let made = format!("created client key at {}", key_path.display());
            terminal::print(&terminal::note(&made, style));
        }
    }
    Ok((cfg, identity))
}

/// Why a session ended, as the log says it.
pub(crate) fn reason(cause: DisconnectCause) -> String {
    match cause {
        DisconnectCause::Shutdown(why) => format!("server ended the session: {why}"),
        DisconnectCause::Unresponsive => "server stopped answering".to_owned(),
        DisconnectCause::Denied => "this machine's key is not authorized on the server".to_owned(),
        DisconnectCause::Transport(reason) | DisconnectCause::Config(reason) => reason,
    }
}

/// Logs what the client reports, or prints it in the terminal form, and says whether it has
/// stopped for good. `cfg` is the config in force, which a service's addresses are named from.
fn report(event: Event, cfg: &ClientConfig, terminal: Option<Style>) -> bool {
    if let Some(style) = terminal {
        terminal::print(&terminal::event(&event, cfg, style));
        return matches!(event, Event::Disconnected { retry_in: None, .. });
    }
    let mut stopped = false;
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
            let host = split_host_port(&cfg.server)
                .map(|(host, _)| host)
                .unwrap_or_default();
            let local = cfg.expose.get(&service).map_or("", |e| e.local.as_str());
            tracing::info!("{service}  {host}:{port} <- {local}");
        }
        Event::BindFailed { service, reason } => tracing::warn!("{service}  {reason}"),
        Event::Denied { key } => tracing::warn!(
            "not authorized. on the server, add to server.toml:\n[clients.NAME]\nkey = \"{key}\"\nthen wait for the next retry"
        ),
        Event::Disconnected { cause, retry_in } => {
            let reason = reason(cause);
            stopped = retry_in.is_none();
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
    stopped
}

/// 2 when a config error stopped the client, the exit of a config that does not load: starting
/// it again will not fix either.
pub async fn run(config: Option<PathBuf>, terminal: Option<Style>) -> miette::Result<ExitCode> {
    let file = paths::locate(Role::Client, config).file;
    let mut reloads = Reloads::new(file.clone());
    let (mut cfg, mut identity) = load(&file, terminal)?;
    let cancel = CancellationToken::new();
    tokio::spawn(super::shutdown_signal(cancel.clone()));
    // What the printer names a service's addresses from, replaced by each reload.
    let (shown, showing) = watch::channel(cfg.clone());
    let (tx, mut events) = mpsc::channel(64);
    let printer = tokio::spawn(async move {
        let mut stopped = false;
        let mut denied = false;
        while let Some(event) = events.recv().await {
            if terminal.is_some() && terminal::repeats(&mut denied, &event) {
                continue;
            }
            stopped |= report(event, &showing.borrow(), terminal);
        }
        stopped
    });
    // One turn per connection: a reload that changes more than `[expose.*]` ends the client it
    // found and starts another on what it read.
    loop {
        tracing::info!(key = %identity.public_key(), "client key");
        let client = Client::new(cfg.clone(), identity);
        let session = cancel.child_token();
        let run = client.run(session.clone(), tx.clone());
        tokio::pin!(run);
        let next = loop {
            tokio::select! {
                () = &mut run => break None,
                () = reloads.next() => {}
            }
            let (new, identity) = match load(&file, terminal) {
                Ok(loaded) => loaded,
                Err(report) => {
                    super::not_reloaded(&report);
                    continue;
                }
            };
            shown.send_replace(new.clone());
            if cfg.needs_reconnect(&new) {
                tracing::info!("config reloaded; reconnecting, as more than [expose.*] changed");
                if let Some(style) = terminal {
                    terminal::print(&terminal::note("config reloaded; reconnecting", style));
                }
                session.cancel();
                run.await;
                break Some((new, identity));
            }
            tracing::info!(services = new.expose.len(), "config reloaded");
            if let Some(style) = terminal {
                terminal::print(&terminal::note("config reloaded", style));
            }
            client.expose().send_replace(new.expose.clone());
            cfg = new;
        };
        let Some(next) = next else { break };
        (cfg, identity) = next;
    }
    // The printer ends when the last sender does.
    drop(tx);
    let stopped = printer.await.into_diagnostic()?;
    Ok(if stopped {
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    })
}
