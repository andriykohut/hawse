use std::sync::Arc;
use std::time::{Duration, Instant};

use hawse_proto::msg::{StreamHeader, StreamOpen, reset};
use hawse_proto::port::Port;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use super::{ACCEPT_BACKOFF, Shared, out_of_descriptors};
use crate::allow::AllowList;
use crate::error::chain;
use crate::frame::write_frame;
use crate::net;
use crate::pump::{Edge, pump};
use crate::transport::Transport;

/// How often a listener says it is turning visitors away, so that a flood cannot fill the log.
const REPORT_EVERY: Duration = Duration::from_secs(60);

/// What one TCP listener serves.
pub struct Public {
    pub service_id: u16,
    pub port: Port,
    pub allow: AllowList,
}

/// Owns `port`'s claim on the allocator for as long as the socket is open.
///
/// A visitor takes one of the session's `streams` for as long as it is pumped, and is reset when
/// there is none to take.
pub async fn serve(
    transport: Arc<dyn Transport>,
    listener: TcpListener,
    public: Public,
    shared: Arc<Shared>,
    streams: Arc<Semaphore>,
    cancel: CancellationToken,
    tasks: TaskTracker,
) {
    let Public {
        service_id,
        port,
        allow,
    } = public;
    let buffer = shared.buffer;
    let mut refused = 0u64;
    let mut reported: Option<Instant> = None;
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
                tokio::select! {
                    () = cancel.cancelled() => break,
                    () = tokio::time::sleep(ACCEPT_BACKOFF) => continue,
                }
            }
            // Anything else is one visitor's doing — see `out_of_descriptors` — and this is the
            // published port, so pausing would hand a stranger its accept rate.
            Err(err) => {
                tracing::debug!(err = %chain(&err), "accept failed");
                continue;
            }
        };
        if !allow.permits(visitor.ip()) {
            tracing::debug!(%visitor, "visitor is not on the service's allow list");
            continue;
        }
        // Taken before the task is spawned: a visitor that waited for a stream would hold its
        // socket and a task while it did, with nothing bounding how many wait.
        let Ok(permit) = Arc::clone(&streams).try_acquire_owned() else {
            refused += 1;
            if reported.is_none_or(|at| at.elapsed() >= REPORT_EVERY) {
                tracing::warn!(
                    %port,
                    refused,
                    "refusing visitors: the session is at limits.streams_per_client"
                );
                reported = Some(Instant::now());
            }
            socket.abort();
            continue;
        };
        let Ok(listener_addr) = socket.local_addr() else {
            continue;
        };
        let _ = socket.set_nodelay(true);
        let _ = net::keepalive(&socket);
        // Held by the task for the whole pump, not just for `open_bi`: on the TCP transport the
        // stream halves do not keep the connection alive, and the last `Arc` dropped ends it.
        let transport = Arc::clone(&transport);
        tasks.spawn(async move {
            let _permit = permit;
            let (mut send, recv) = match transport.open_bi().await {
                Ok(streams) => streams,
                Err(err) => {
                    tracing::debug!(%visitor, err = %chain(&err), "cannot open stream");
                    socket.abort();
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
                socket.abort();
                return;
            }
            tracing::debug!(service_id, %visitor, "visitor connected");
            match pump(socket, send, recv, buffer).await {
                Ok(stats) => {
                    tracing::debug!(%visitor, up = stats.to_stream, down = stats.to_socket, "visitor done");
                }
                Err(err) => tracing::debug!(
                    %visitor,
                    reset = err.reset_code().map(reset::name),
                    err = %chain(&err),
                    "visitor ended"
                ),
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
