use std::sync::Arc;
use std::time::Duration;

use hawse_proto::msg::StreamHeader;
use hawse_proto::port::Port;
use quinn::Connection;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use super::Shared;
use crate::error::chain;
use crate::frame::write_frame;
use crate::pump::pump;

/// Owns `port`'s claim on the allocator for as long as the socket is open.
pub async fn serve(
    conn: Connection,
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
            Err(err) => {
                tracing::warn!(err = %chain(&err), "accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let Ok(listener_addr) = socket.local_addr() else {
            continue;
        };
        let _ = socket.set_nodelay(true);
        let conn = conn.clone();
        tasks.spawn(async move {
            let (mut send, recv) = match conn.open_bi().await {
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
            if let Err(err) = write_frame(&mut send, &header).await {
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
