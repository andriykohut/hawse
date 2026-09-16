use std::sync::Arc;

use hawse_proto::msg::{StreamHeader, StreamOpen};
use hawse_proto::port::Port;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use super::{ACCEPT_BACKOFF, Shared, out_of_descriptors};
use crate::error::chain;
use crate::frame::write_frame;
use crate::pump::pump;
use crate::transport::Transport;

/// Owns `port`'s claim on the allocator for as long as the socket is open.
pub async fn serve(
    transport: Arc<dyn Transport>,
    listener: TcpListener,
    service_id: u16,
    port: Port,
    shared: Arc<Shared>,
    cancel: CancellationToken,
    tasks: TaskTracker,
) {
    let buffer = shared.buffer;
    loop {
        let accepted = tokio::select! {
            () = cancel.cancelled() => break,
            accepted = listener.accept() => accepted,
        };
        let (socket, visitor) = match accepted {
            Ok(accepted) => accepted,
            Err(err) if out_of_descriptors(&err) => {
                tracing::warn!(err = %chain(&err), "accept failed");
                // Every accept fails until something closes, so without this the loop spins on it.
                tokio::time::sleep(ACCEPT_BACKOFF).await;
                continue;
            }
            // Anything else is one visitor's doing — see `out_of_descriptors` — and this is the
            // published port, so pausing would hand a stranger its accept rate.
            Err(err) => {
                tracing::debug!(err = %chain(&err), "accept failed");
                continue;
            }
        };
        let Ok(listener_addr) = socket.local_addr() else {
            continue;
        };
        let _ = socket.set_nodelay(true);
        // Held by the task for the whole pump, not just for `open_bi`: on the TCP transport the
        // stream halves do not keep the connection alive, and the last `Arc` dropped ends it.
        let transport = Arc::clone(&transport);
        tasks.spawn(async move {
            let (mut send, recv) = match transport.open_bi().await {
                Ok(streams) => streams,
                Err(err) => {
                    tracing::debug!(%visitor, err = %chain(&err), "cannot open stream");
                    return;
                }
            };
            let header = StreamHeader {
                service_id,
                visitor,
                listener: listener_addr,
            };
            if let Err(err) = write_frame(&mut send, &StreamOpen::Visitor(header)).await {
                tracing::debug!(%visitor, err = %chain(&err), "header write failed");
                return;
            }
            tracing::debug!(service_id, %visitor, "visitor connected");
            match pump(socket, send, recv, buffer).await {
                Ok(stats) => {
                    tracing::debug!(%visitor, up = stats.to_stream, down = stats.to_socket, "visitor done");
                }
                Err(err) => tracing::debug!(%visitor, err = %chain(&err), "visitor ended"),
            }
        });
    }
    // Releasing before the socket is gone would let the next bind claim a port still in use.
    drop(listener);
    shared
        .ports
        .lock()
        .expect("port allocator lock")
        .release(port);
}
