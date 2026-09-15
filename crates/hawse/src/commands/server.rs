use std::net::SocketAddr;
use std::path::PathBuf;

use hawse_core::identity::Identity;
use hawse_core::server::Server;
use miette::{IntoDiagnostic as _, WrapErr as _};
use tokio_util::sync::CancellationToken;

use crate::config_file;

pub async fn run(config: Option<PathBuf>, listen: Option<SocketAddr>) -> miette::Result<()> {
    let (mut cfg, located) = config_file::load_server(config)?;
    if let Some(listen) = listen {
        cfg.listen = listen;
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
    tracing::info!(%addr, transport = "quic", "listening");
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
    server.serve(cancel).await;
    Ok(())
}
