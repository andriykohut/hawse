use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use hawse_core::config::ServerConfig;
use hawse_core::identity::Identity;
use hawse_core::server::policy::Policy;
use hawse_core::server::{Event, Server};
use miette::{IntoDiagnostic as _, WrapErr as _};
use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::sync::watch;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::Reloads;
use crate::config_file::{self, LoadError};
use crate::notify;
use crate::paths::{self, Located, Role};
use crate::socket;
use crate::status::{self, Status, server::Keeper};
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

/// The descriptor limit to set in place of `limit`: the hard limit as the soft one too, or `None`
/// when the soft one is that high already. On macOS the soft limit stops at `OPEN_MAX`, the most
/// it accepts.
fn raised(limit: Rlimit) -> Option<Rlimit> {
    const OPEN_MAX: u64 = 10240;
    let ceiling = if cfg!(target_os = "macos") {
        Some(limit.maximum.map_or(OPEN_MAX, |max| max.min(OPEN_MAX)))
    } else {
        limit.maximum
    };
    // `None` is unlimited.
    let most = |value: Option<u64>| value.unwrap_or(u64::MAX);
    (most(ceiling) > most(limit.current)).then_some(Rlimit {
        current: ceiling,
        maximum: limit.maximum,
    })
}

pub async fn run(
    config: Option<PathBuf>,
    listen: Option<SocketAddr>,
    terminal: Option<Style>,
) -> miette::Result<()> {
    // Each visitor and each connection on its way in holds a descriptor, and systemd starts a
    // service at 1024, which a few hundred idle visitors fill for every client at once.
    if let Some(limit) = raised(getrlimit(Resource::Nofile))
        && let Err(err) = setrlimit(Resource::Nofile, limit)
    {
        tracing::warn!(%err, "cannot raise the descriptor limit");
    }
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
    let key_path = paths::key_path(Role::Server, &located, cfg.key.as_deref());
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
    // Before systemd hears the unit is ready, so a status asked once it is finds the socket.
    let answering = socket::bind(Role::Server).map(|bound| {
        let (keeper, kept) = watch::channel(Keeper::default());
        tokio::spawn(status::server::keep(server.events(), keeper));
        let (policy, sessions) = (server.policy(), server.sessions());
        let (tcp, started) = (cfg.transport.tcp_fallback, Instant::now());
        tokio::spawn(bound.serve(move || {
            let live = sessions.live();
            let up = started.elapsed();
            let reply =
                kept.borrow()
                    .snapshot(&policy.borrow(), &live, addr, tcp, up, Instant::now());
            Status::Server(reply)
        }))
    });
    if let Some(socket) = notify::socket() {
        tokio::spawn(notify::serve(socket, addr, server.events()));
    }
    // Before the line that says the server is up, which is what a script waits for to signal it.
    let cancel = CancellationToken::new();
    tokio::spawn(super::shutdown_signal(cancel.clone()));
    let rows = terminal.map(|style| {
        let (key, unused) = (identity.public_key(), cfg.clients.is_empty());
        let tcp = cfg.transport.tcp_fallback;
        terminal::print(&terminal::server_header(addr, tcp, key, unused, style));
        let names = cfg.clients.keys().map(String::len).max().unwrap_or(0);
        tokio::spawn(show(server.events(), names, style))
    });
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
    // The status task's hold on the live sessions keeps the server's events open, and the
    // printer ends when they close.
    if let Some(answering) = answering {
        answering.abort();
    }
    // The sessions the stop ended have said so, and their rows may not be printed yet. The
    // printer ends with the last session; one that did not drain is not waited for.
    if let Some(rows) = rows {
        let _ = tokio::time::timeout(Duration::from_secs(1), rows).await;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn limit(current: Option<u64>, maximum: Option<u64>) -> Rlimit {
        Rlimit { current, maximum }
    }

    #[test]
    fn the_soft_descriptor_limit_rises_to_the_hard_one() {
        let hard = Some(524_288);
        let to = if cfg!(target_os = "macos") {
            Some(10240)
        } else {
            hard
        };
        assert_eq!(raised(limit(Some(1024), hard)), Some(limit(to, hard)));
        assert_eq!(raised(limit(Some(524_288), hard)), None);
        assert_eq!(raised(limit(Some(1024), Some(1024))), None);
        assert_eq!(raised(limit(None, None)), None);
    }

    #[test]
    fn an_unlimited_hard_limit_lifts_the_soft_one_as_far_as_the_system_takes() {
        let to = if cfg!(target_os = "macos") {
            Some(10240)
        } else {
            None
        };
        assert_eq!(raised(limit(Some(256), None)), Some(limit(to, None)));
    }
}
