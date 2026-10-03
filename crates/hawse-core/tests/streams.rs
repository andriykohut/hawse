mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{raw_client, server_config, start_server};
use hawse_core::control::Control;
use hawse_core::identity::Identity;
use hawse_core::tls;
use hawse_core::transport::quic::Tuning;
use hawse_core::transport::{Transport, reset_code, tcp};
use hawse_proto::msg::{ClientMessage, ServerMessage, reset};
use quinn::VarInt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn a_stream_the_client_opens_is_reset_and_the_session_lives_on() {
    let server_id = Identity::generate().unwrap();
    let client_id = Identity::generate().unwrap();
    let server = start_server(
        &server_config(&[("test", client_id.public_key(), &[])]),
        &server_id,
    );
    let mut raw = raw_client(&server, &client_id).await;

    let (mut send, mut recv) = raw.conn.open_bi().await.unwrap();
    send.write_all(b"extra").await.unwrap();
    let mut buf = [0u8; 8];
    let read = tokio::time::timeout(Duration::from_secs(5), recv.read(&mut buf))
        .await
        .expect("an answer within 5 s");
    assert!(
        matches!(read, Err(quinn::ReadError::Reset(code)) if code == VarInt::from_u32(reset::UNEXPECTED_STREAM)),
        "{read:?}"
    );

    assert!(matches!(
        raw.bind_udp("dns", None).await,
        ServerMessage::Bound { .. }
    ));
    server.cancel.cancel();
}

#[tokio::test]
async fn a_stream_the_client_opens_over_tcp_is_reset_and_the_session_lives_on() {
    let server_id = Identity::generate().unwrap();
    let client_id = Identity::generate().unwrap();
    let server = start_server(
        &server_config(&[("test", client_id.public_key(), &[])]),
        &server_id,
    );
    let (cert, key) = client_id.certificate().unwrap();
    let client_tls = tls::client_config(cert, key, server.key, tls::provider()).unwrap();
    let transport: Arc<dyn Transport> = Arc::new(
        tcp::connect(server.addr, client_tls, Tuning::CLIENT)
            .await
            .unwrap(),
    );
    let (send, recv) = transport.open_bi().await.unwrap();
    let mut control = Control::new(send, recv);
    control
        .send(&ClientMessage::Hello {
            name: None,
            agent: "raw".to_owned(),
        })
        .await
        .unwrap();
    assert!(matches!(
        control.next::<ServerMessage>().await,
        Some(ServerMessage::Welcome { .. })
    ));

    let (mut send, mut recv) = transport.open_bi().await.unwrap();
    send.write_all(b"extra").await.unwrap();
    let mut buf = [0u8; 8];
    let read = tokio::time::timeout(Duration::from_secs(5), recv.read(&mut buf))
        .await
        .expect("an answer within 5 s");
    let err = read.expect_err("a refused stream must not read as a clean end");
    assert_eq!(reset_code(&err), Some(reset::UNEXPECTED_STREAM), "{err:?}");

    control
        .send(&ClientMessage::Ping { nonce: 7 })
        .await
        .unwrap();
    loop {
        let reply = tokio::time::timeout(Duration::from_secs(5), control.next::<ServerMessage>())
            .await
            .expect("a pong within 5 s");
        match reply {
            Some(ServerMessage::Pong { nonce: 7 }) => break,
            Some(ServerMessage::Ping { .. }) => {}
            other => panic!("expected Pong, got {other:?}"),
        }
    }
    server.cancel.cancel();
}

#[tokio::test]
async fn a_server_shutting_down_is_not_held_by_the_stream_refuser() {
    let server_id = Identity::generate().unwrap();
    let client_id = Identity::generate().unwrap();
    let server = start_server(
        &server_config(&[("test", client_id.public_key(), &[])]),
        &server_id,
    );
    let _raw = raw_client(&server, &client_id).await;

    server.cancel.cancel();
    // The session's own drain is 4 s; a refuser that ignored the shutdown would hold it that long.
    // The bound tells the two apart because a stalled refuser costs that whole drain plus the 2 s
    // shutdown linger, while a cancelled one costs only the linger.
    tokio::time::timeout(Duration::from_secs(4), server.task)
        .await
        .expect("the server stops within 4 s")
        .unwrap();
}
