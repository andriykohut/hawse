use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use hawse_proto::msg::{StreamHeader, reset};
use hawse_proto::proxy;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use super::Target;
use crate::error::chain;
use crate::net;
use crate::pump::pump;
use crate::transport::{RecvHalf, SendHalf};
use crate::udp::FINISH_WAIT;

/// The matching `stop` is what keeps the reason on both halves over QUIC: dropping `recv` would
/// send the server `STOP_SENDING(0)` instead. On the TCP transport the code travels in a record,
/// so this waits for it to leave before the stream is dropped; the wait is bounded because a
/// stalled link would otherwise hold a visitor's task.
pub(super) async fn refuse(send: &mut SendHalf, recv: &mut RecvHalf, code: u32) {
    let _ = tokio::time::timeout(FINISH_WAIT, send.reset_flushed(code)).await;
    recv.stop(code);
}

/// Dials only the `local` address recorded for `service_id` at `Bound` time; nothing in the header can name an address.
pub async fn serve(
    header: StreamHeader,
    mut send: SendHalf,
    mut recv: RecvHalf,
    targets: Arc<RwLock<HashMap<u16, Target>>>,
    buffer: usize,
) {
    let target = targets
        .read()
        .expect("targets lock")
        .get(&header.service_id)
        .cloned();
    let Some(target) = target else {
        tracing::warn!(
            service_id = header.service_id,
            visitor = %header.visitor,
            "stream for a service we never bound"
        );
        refuse(&mut send, &mut recv, reset::UNKNOWN_SERVICE).await;
        return;
    };
    let mut socket = match TcpStream::connect(&target.local).await {
        Ok(socket) => socket,
        Err(err) => {
            tracing::warn!(
                service = target.service,
                local = target.local,
                err = %chain(&err),
                "local service refused the connection"
            );
            refuse(&mut send, &mut recv, reset::LOCAL_REFUSED).await;
            return;
        }
    };
    let _ = socket.set_nodelay(true);
    let _ = net::keepalive(&socket);
    if target.proxy_protocol {
        let preamble = proxy::v2_tcp(header.visitor, header.listener);
        if let Err(err) = socket.write_all(&preamble).await {
            tracing::warn!(
                service = target.service,
                local = target.local,
                err = %chain(&err),
                "local service closed before the PROXY header"
            );
            refuse(&mut send, &mut recv, reset::LOCAL_REFUSED).await;
            return;
        }
    }
    tracing::debug!(service = target.service, visitor = %header.visitor, "visitor connected");
    match pump(socket, send, recv, buffer).await {
        Ok(stats) => tracing::debug!(
            service = target.service,
            up = stats.to_socket,
            down = stats.to_stream,
            "visitor done"
        ),
        Err(err) => tracing::debug!(
            service = target.service,
            reset = err.reset_code().map(reset::name),
            err = %chain(&err),
            "visitor ended"
        ),
    }
}
