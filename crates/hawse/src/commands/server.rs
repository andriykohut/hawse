use std::net::SocketAddr;
use std::path::PathBuf;

use hawse_core::config::ServerConfig;
use hawse_core::identity::Identity;
use hawse_core::server::Server;
use hawse_core::server::policy::Policy;
use miette::{IntoDiagnostic as _, WrapErr as _};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::Reloads;
use crate::config_file::{self, LoadError};
use crate::paths::Located;

fn load(
    config: Option<PathBuf>,
    listen: Option<SocketAddr>,
) -> Result<(ServerConfig, Located), LoadError> {
    let (mut cfg, located) = config_file::load_server(config)?;
    if let Some(listen) = listen {
        cfg.listen = listen;
    }
    Ok((cfg, located))
}

pub async fn run(config: Option<PathBuf>, listen: Option<SocketAddr>) -> miette::Result<()> {
    let (cfg, located) = load(config.clone(), listen)?;
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
    tracing::info!(%addr, transports = "quic/udp, tcp", "listening");
    tracing::info!(key = %identity.public_key(), "server key");
    if cfg.clients.is_empty() {
        tracing::warn!(config = %located.file.display(), "no clients are authorized yet; add a [clients.NAME] table with the client's key");
    }
    tracing::info!(
        "clients connect with: server = \"<this-host>:{}\"  server_key = \"{}\"",
        addr.port(),
        identity.public_key()
    );
    let cancel = CancellationToken::new();
    tokio::spawn(super::shutdown_signal(cancel.clone()));
    let policy = server.policy();
    tokio::select! {
        () = server.serve(cancel) => {}
        () = reload(config, listen, &cfg, &located, &policy) => {}
    }
    Ok(())
}

/// Reads the config again each time it changes and puts its `[clients.*]` in force. `started` is
/// what the server is running on, and stays what every reload is compared with.
async fn reload(
    config: Option<PathBuf>,
    listen: Option<SocketAddr>,
    started: &ServerConfig,
    located: &Located,
    policy: &watch::Sender<Policy>,
) {
    let mut reloads = Reloads::new(located.file.clone());
    loop {
        reloads.next().await;
        let cfg = match load(config.clone(), listen) {
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
    }
}
