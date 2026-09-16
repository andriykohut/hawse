mod common;

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
