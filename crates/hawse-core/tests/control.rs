mod common;

use std::sync::Arc;
use std::time::Duration;

use hawse_core::control::Control;
use hawse_core::transport::{RecvHalf, SendHalf, Transport};
use hawse_proto::frame::{FrameError, MAX_FRAME};
use hawse_proto::msg::ClientMessage;
use tokio::io::AsyncWriteExt;

#[tokio::test]
async fn control_round_trips_a_hello() {
    let pair = common::quic_pair().await;
    let (cs, cr) = pair.client.open_bi().await.unwrap();
    let mut client = Control::new(SendHalf::Quic(cs), RecvHalf::Quic(cr));
    client
        .send(&ClientMessage::Hello {
            name: Some("a".into()),
            agent: "t".into(),
        })
        .await
        .unwrap();

    let (ss, sr) = pair.server.accept_bi().await.unwrap();
    let mut server = Control::new(SendHalf::Quic(ss), RecvHalf::Quic(sr));
    assert!(matches!(
        server.next::<ClientMessage>().await,
        Some(ClientMessage::Hello { .. })
    ));
}

/// Taking `self` here would not compile, which is the guard: consuming the `Control` drops an
/// unfinished `RecvStream`, and quinn's `Drop` then sends `STOP_SENDING` for a reply the peer is
/// still entitled to write.
#[tokio::test]
async fn finishing_the_send_half_leaves_the_peers_reply_readable() {
    let pair = common::quic_pair().await;
    let (cs, cr) = pair.client.open_bi().await.unwrap();
    let mut client = Control::new(SendHalf::Quic(cs), RecvHalf::Quic(cr));
    client
        .send(&ClientMessage::Ping { nonce: 7 })
        .await
        .unwrap();

    let (ss, sr) = pair.server.accept_bi().await.unwrap();
    let mut server = Control::new(SendHalf::Quic(ss), RecvHalf::Quic(sr));
    assert!(matches!(
        server.next::<ClientMessage>().await,
        Some(ClientMessage::Ping { .. })
    ));
    server.finish().await;

    client
        .send(&ClientMessage::Pong { nonce: 7 })
        .await
        .unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(5), server.next::<ClientMessage>())
        .await
        .expect("a reply within 5 s");
    assert!(
        matches!(reply, Some(ClientMessage::Pong { nonce: 7 })),
        "{reply:?}"
    );
}

/// What the server's `Control` reads next once the client has written `bytes` on a new stream and,
/// if `finished`, ended it there.
async fn reads(
    client: Arc<dyn Transport>,
    server: Arc<dyn Transport>,
    bytes: &[u8],
    finished: bool,
) -> Result<Option<ClientMessage>, FrameError> {
    let (mut send, _recv) = client.open_bi().await.unwrap();
    send.write_all(bytes).await.unwrap();
    send.flush().await.unwrap();
    if finished {
        send.finish().await;
    }
    let (ss, sr) = server.accept_bi().await.unwrap();
    let mut control = Control::new(ss, sr);
    tokio::time::timeout(Duration::from_secs(5), control.try_next())
        .await
        .expect("an answer within 5 s")
}

async fn a_length_prefix_over_the_limit_is_an_error(
    client: Arc<dyn Transport>,
    server: Arc<dyn Transport>,
) {
    let over = u32::try_from(MAX_FRAME + 1).unwrap();
    let read = reads(client, server, &over.to_le_bytes(), false).await;
    assert!(
        matches!(read, Err(FrameError::TooLarge(len)) if len == MAX_FRAME + 1),
        "{read:?}"
    );
}

#[tokio::test]
async fn a_length_prefix_over_the_limit_is_an_error_over_quic() {
    let pair = common::quic_pair().await;
    a_length_prefix_over_the_limit_is_an_error(pair.client_transport(), pair.server_transport())
        .await;
}

#[tokio::test]
async fn a_length_prefix_over_the_limit_is_an_error_over_tcp() {
    let pair = common::tcp_pair().await;
    a_length_prefix_over_the_limit_is_an_error(pair.client, pair.server).await;
}

async fn a_stream_that_ends_mid_frame_is_the_end(
    client: Arc<dyn Transport>,
    server: Arc<dyn Transport>,
) {
    // A prefix promising eight bytes, and two of them.
    let cut_short = [8, 0, 0, 0, 1, 2];
    let read = reads(client, server, &cut_short, true).await;
    assert!(matches!(read, Ok(None)), "{read:?}");
}

#[tokio::test]
async fn a_stream_that_ends_mid_frame_is_the_end_over_quic() {
    let pair = common::quic_pair().await;
    a_stream_that_ends_mid_frame_is_the_end(pair.client_transport(), pair.server_transport()).await;
}

#[tokio::test]
async fn a_stream_that_ends_mid_frame_is_the_end_over_tcp() {
    let pair = common::tcp_pair().await;
    a_stream_that_ends_mid_frame_is_the_end(pair.client, pair.server).await;
}
