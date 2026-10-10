use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use hawse_core::config::ServerConfig;
use hawse_core::identity::Identity;
use hawse_core::server::policy::Policy;
use hawse_core::server::{Event, Server};
use miette::{IntoDiagnostic as _, WrapErr as _};
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::Reloads;
use crate::config_file::{self, LoadError};
use crate::paths::{self, Located, Role};
use crate::terminal::{self, Style};

/// Reads `file` and no other: a reload that looked the config up again could find one that has
/// since appeared somewhere the lookup tries first, which is not the one being watched.
fn load(file: &Path, listen: Option<SocketAddr>) -> Result<(ServerConfig, Located), LoadError> {
    let (mut cfg, located) = config_file::load_server(Some(file.to_owned()))?;
    if let Some(listen) = listen {
        cfg.listen = listen;
    }
    Ok((cfg, located))
}

/// Prints what the server tells of its sessions, in the terminal form. `names` is the width the
/// clients' names are padded to, and grows with a name a reload brings.
async fn show(mut events: broadcast::Receiver<Event>, mut names: usize, style: Style) {
    let mut shown = None;
    loop {
        let event = match events.recv().await {
            Ok(event) => event,
            // The rows are for a person watching, who loses nothing they would have read.
            Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => return,
        };
        if terminal::knocks_again(&mut shown, &event) {
            continue;
        }
        if let Event::Connected { client, .. } = &event {
            names = names.max(client.len());
        }
        terminal::print(&terminal::served(&event, names, style));
    }
}

pub async fn run(
    config: Option<PathBuf>,
    listen: Option<SocketAddr>,
    terminal: Option<Style>,
) -> miette::Result<()> {
    let located = paths::locate(Role::Server, config);
    let reloads = Reloads::new(located.file.clone());
    let (cfg, _) = load(&located.file, listen)?;
    let streams = cfg.limits.streams_in_effect();
    if streams < cfg.limits.streams_per_client {
        tracing::warn!(
            config = %located.file.display(),
            streams_per_client = cfg.limits.streams_per_client,
            in_effect = streams,
            "limits.streams_per_client is more than a client accepts, and the rest does nothing"
        );
    }
    let key_path = located.dir.join(&cfg.key);
    let (identity, created) = Identity::load_or_create(&key_path)
        .into_diagnostic()
        .wrap_err_with(|| format!("cannot load the server key at {}", key_path.display()))?;
    if created {
        tracing::info!(path = %key_path.display(), "created server key");
    }
    let server = Server::bind(&cfg, &identity)
        .into_diagnostic()
        .wrap_err("cannot start the server")?;
    let addr = server.local_addr();
    // Before the line that says the server is up, which is what a script waits for to signal it.
    let cancel = CancellationToken::new();
    tokio::spawn(super::shutdown_signal(cancel.clone()));
    if let Some(style) = terminal {
        let (key, unused) = (identity.public_key(), cfg.clients.is_empty());
        let tcp = cfg.transport.tcp_fallback;
        terminal::print(&terminal::server_header(addr, tcp, key, unused, style));
        let names = cfg.clients.keys().map(String::len).max().unwrap_or(0);
        tokio::spawn(show(server.events(), names, style));
    }
    tracing::info!(%addr, transports = "quic/udp, tcp", "listening");
    tracing::info!(key = %identity.public_key(), "server key");
    if cfg.clients.is_empty() && terminal.is_none() {
        tracing::warn!(config = %located.file.display(), "no clients are authorized yet; add a [clients.NAME] table with the client's key");
    }
    tracing::info!(
        "a client joins with: hawse join <this-host>:{} --server-key {}",
        addr.port(),
        identity.public_key()
    );
    let policy = server.policy();
    tokio::select! {
        () = server.serve(cancel) => {}
        () = reload(reloads, listen, &cfg, &located, &policy, terminal) => {}
    }
    Ok(())
}

/// Reads the config again each time it changes and puts its `[clients.*]` in force. `started` is
/// what the server is running on, and stays what every reload is compared with.
async fn reload(
    mut reloads: Reloads,
    listen: Option<SocketAddr>,
    started: &ServerConfig,
    located: &Located,
    policy: &watch::Sender<Policy>,
    terminal: Option<Style>,
) {
    loop {
        reloads.next().await;
        let cfg = match load(&located.file, listen) {
            // A server started from a file does not fall back to the defaults when the file goes:
            // they authorize nobody.
            Ok((_, found)) if located.exists && !found.exists => {
                tracing::error!(config = %located.file.display(), "config is gone, and the running one stays in force");
                continue;
            }
            Ok((cfg, _)) => cfg,
            Err(err) => {
                super::not_reloaded(&err.into());
                continue;
            }
        };
        if started.needs_restart(&cfg) {
            tracing::warn!(
                config = %located.file.display(),
                "a setting outside [clients.*] and `bind` changed, and takes effect at the next start"
            );
        }
        policy.send_replace(Policy::from_config(&cfg));
        tracing::info!(clients = cfg.clients.len(), "config reloaded");
        if let Some(style) = terminal {
            terminal::print(&terminal::note("config reloaded", style));
        }
    }
}
