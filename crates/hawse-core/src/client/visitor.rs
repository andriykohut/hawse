use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use hawse_proto::msg::{StreamOpen, reset};
use quinn::{RecvStream, SendStream, VarInt};
use tokio::net::TcpStream;

use super::Target;
use crate::error::chain;
use crate::frame::read_frame;
use crate::pump::pump;

/// Without the matching `stop`, dropping `recv` sends the server `STOP_SENDING(0)` and the reason
/// reaches only one half.
fn refuse(send: &mut SendStream, recv: &mut RecvStream, code: u32) {
    let _ = send.reset(VarInt::from_u32(code));
    let _ = recv.stop(VarInt::from_u32(code));
}

/// Dials only the `local` address recorded for `service_id` at `Bound` time; nothing in the header can name an address.
pub async fn serve(
    mut send: SendStream,
    mut recv: RecvStream,
    targets: Arc<RwLock<HashMap<u16, Target>>>,
    buffer: usize,
) {
    let header = match read_frame::<StreamOpen>(&mut recv).await {
        Ok(StreamOpen::Visitor(header)) => header,
        Err(err) => {
            tracing::debug!(err = %chain(&err), "bad stream header");
            return;
        }
    };
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
        refuse(&mut send, &mut recv, reset::UNKNOWN_SERVICE);
        return;
    };
    let socket = match TcpStream::connect(&target.local).await {
        Ok(socket) => socket,
        Err(err) => {
            tracing::warn!(
                service = target.service,
                local = target.local,
                err = %chain(&err),
                "local service refused the connection"
            );
            refuse(&mut send, &mut recv, reset::LOCAL_REFUSED);
            return;
        }
    };
    let _ = socket.set_nodelay(true);
    tracing::debug!(service = target.service, visitor = %header.visitor, "visitor connected");
    match pump(socket, send, recv, buffer).await {
        Ok(stats) => tracing::debug!(
            service = target.service,
            up = stats.to_socket,
            down = stats.to_stream,
            "visitor done"
        ),
        Err(err) => tracing::debug!(service = target.service, err = %chain(&err), "visitor ended"),
    }
}
