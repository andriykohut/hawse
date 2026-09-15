pub mod client;
pub mod keygen;
pub mod server;

use tokio_util::sync::CancellationToken;

/// Resolves on SIGINT or SIGTERM and cancels the token.
pub async fn shutdown_signal(cancel: CancellationToken) {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("sigterm handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    tracing::info!("shutting down");
    cancel.cancel();
}
