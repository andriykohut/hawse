mod common;

use std::time::Duration;

use hawse_core::control::Control;
use hawse_core::transport::{RecvHalf, SendHalf};
use hawse_proto::msg::ClientMessage;

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
