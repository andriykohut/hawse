mod common;

use std::future::poll_fn;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::FutureExt;
use hawse_core::transport::{CloseReason, RecvHalf, SendHalf, TransportError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;
use tokio_util::compat::{FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};

#[tokio::test]
async fn quic_transport_opens_a_bidirectional_stream() {
    let pair = common::quic_pair().await;
    let client = pair.client_transport();
    let server = pair.server_transport();

    let (mut cs, _cr) = client.open_bi().await.unwrap();
    cs.write_all(b"hi").await.unwrap();
    cs.finish().await;

    let (_ss, mut sr) = server.accept_bi().await.unwrap();
    let mut got = Vec::new();
    timeout(Duration::from_secs(5), sr.read_to_end(&mut got))
        .await
        .expect("the peer never saw end-of-stream")
        .unwrap();
    assert_eq!(got, b"hi");
}

#[tokio::test]
async fn shutting_down_a_send_half_gives_the_peer_eof() {
    let pair = common::quic_pair().await;
    let client = pair.client_transport();
    let server = pair.server_transport();

    let (mut cs, _cr) = client.open_bi().await.unwrap();
    cs.write_all(b"hi").await.unwrap();
    cs.shutdown().await.unwrap();

    let (_ss, mut sr) = server.accept_bi().await.unwrap();
    let mut got = Vec::new();
    timeout(Duration::from_secs(5), sr.read_to_end(&mut got))
        .await
        .expect("the peer never saw end-of-stream")
        .unwrap();
    assert_eq!(got, b"hi");
}

#[tokio::test]
async fn resetting_a_send_half_fails_the_peers_read_and_carries_the_code() {
    const CODE: u32 = 0x42;

    let pair = common::quic_pair().await;
    let client = pair.client_transport();
    let server = pair.server_transport();

    let (mut cs, _cr) = client.open_bi().await.unwrap();
    cs.write_all(b"hi").await.unwrap();
    let (_ss, mut sr) = server.accept_bi().await.unwrap();
    cs.reset(CODE);

    let mut got = Vec::new();
    let read = timeout(Duration::from_secs(5), sr.read_to_end(&mut got))
        .await
        .expect("the peer never saw the reset");
    let err = read.expect_err("a reset stream must fail the read, not end it cleanly");
    assert_eq!(
        err.get_ref()
            .and_then(|e| e.downcast_ref::<quinn::ReadError>()),
        Some(&quinn::ReadError::Reset(quinn::VarInt::from_u32(CODE))),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_datagram_reaches_the_peer() {
    let pair = common::quic_pair().await;
    let client = pair.client_transport();
    let server = pair.server_transport();

    client.send_datagram(Bytes::from_static(b"ping")).unwrap();
    let got = timeout(Duration::from_secs(5), server.recv_datagram())
        .await
        .expect("no datagram arrived")
        .unwrap();
    assert_eq!(got, Bytes::from_static(b"ping"));
}

#[tokio::test]
async fn quic_transport_reports_a_datagram_size() {
    let pair = common::quic_pair().await;
    assert!(pair.client_transport().max_datagram_size().is_some());
}

#[tokio::test]
async fn quic_transport_reports_the_address_it_dialed() {
    let pair = common::quic_pair().await;
    assert_eq!(
        pair.client_transport().remote_address(),
        pair.server_endpoint.local_addr().unwrap()
    );
}

#[tokio::test]
async fn closed_resolves_only_once_the_peer_has_closed() {
    let pair = common::quic_pair().await;
    let client = pair.client_transport();
    let server = pair.server_transport();

    assert!(
        server.closed().now_or_never().is_none(),
        "closed() resolved on a live connection"
    );

    client.close(CloseReason::Shutdown);
    timeout(Duration::from_secs(5), server.closed())
        .await
        .expect("the peer never saw the close");
}

#[tokio::test]
async fn tcp_transport_opens_a_bidirectional_stream() {
    let pair = common::tcp_pair().await;
    let (client, server) = (pair.client, pair.server);

    let (mut cs, _cr) = client.open_bi().await.unwrap();
    cs.write_all(b"hi").await.unwrap();
    cs.finish().await;

    let (_ss, mut sr) = server.accept_bi().await.unwrap();
    let mut got = Vec::new();
    timeout(Duration::from_secs(5), sr.read_to_end(&mut got))
        .await
        .expect("the peer never saw end-of-stream")
        .unwrap();
    assert_eq!(got, b"hi");
}

#[tokio::test]
async fn tcp_transport_has_no_datagrams() {
    let pair = common::tcp_pair().await;
    let client = &pair.client;

    assert!(client.max_datagram_size().is_none());
    assert!(matches!(
        client.send_datagram(Bytes::from_static(b"x")),
        Err(TransportError::NoDatagrams)
    ));
    assert!(matches!(
        client.recv_datagram().await,
        Err(TransportError::NoDatagrams)
    ));
}

#[tokio::test]
async fn tcp_transport_opens_a_stream_from_the_server_end() {
    let pair = common::tcp_pair().await;
    let (client, server) = (pair.client, pair.server);

    let (mut ss, _sr) = server.open_bi().await.unwrap();
    ss.write_all(b"down").await.unwrap();
    ss.finish().await;

    let (_cs, mut cr) = client.accept_bi().await.unwrap();
    let mut got = Vec::new();
    timeout(Duration::from_secs(5), cr.read_to_end(&mut got))
        .await
        .expect("the peer never saw end-of-stream")
        .unwrap();
    assert_eq!(got, b"down");
}

/// The last echoes land after both ends have left `open_bi` and `accept_bi`, so they can only be
/// moving because the driver polls the connection on its own.
#[tokio::test]
async fn tcp_transport_round_trips_many_streams_at_once() {
    const BURST: usize = 64;

    let pair = common::tcp_pair().await;
    let (client, server) = (pair.client, pair.server);

    // `server` has to outlive every echo: dropping the transport closes the connection under any
    // stream still running on it.
    let echoing = tokio::spawn(async move {
        let mut echoes = Vec::with_capacity(BURST);
        for _ in 0..BURST {
            let (mut send, mut recv) = server.accept_bi().await.unwrap();
            echoes.push(tokio::spawn(async move {
                let mut got = Vec::new();
                recv.read_to_end(&mut got).await.unwrap();
                send.write_all(&got).await.unwrap();
                send.finish().await;
            }));
        }
        for echo in echoes {
            echo.await.unwrap();
        }
    });

    let mut streams = Vec::with_capacity(BURST);
    for i in 0..BURST {
        let (mut send, recv) = client.open_bi().await.unwrap();
        let label = format!("stream {i}");
        send.write_all(label.as_bytes()).await.unwrap();
        send.finish().await;
        streams.push((label, recv));
    }

    for (label, mut recv) in streams {
        let mut got = Vec::new();
        timeout(Duration::from_secs(10), recv.read_to_end(&mut got))
            .await
            .expect("an echo never came back")
            .unwrap();
        assert_eq!(got, label.as_bytes());
    }
    echoing.await.unwrap();
}

/// yamux carries the SYN on a stream's first frame, so opening one costs no round trip — up to the
/// 256 unacknowledged streams yamux allows.
#[tokio::test]
async fn tcp_transport_opens_streams_the_peer_has_not_accepted() {
    const BURST: usize = 64;

    // The whole pair stays bound: dropping the server end would close the connection under the
    // opens this test is timing.
    let pair = common::tcp_pair().await;
    let client = &pair.client;

    let open = async {
        let mut streams = Vec::with_capacity(BURST);
        for _ in 0..BURST {
            streams.push(client.open_bi().await.unwrap());
        }
        streams
    };
    let opened = timeout(Duration::from_secs(5), open)
        .await
        .expect("open_bi stalled waiting on the peer");
    assert_eq!(opened.len(), BURST);
}

#[tokio::test]
async fn tcp_transport_reports_the_peer_key_and_address() {
    let pair = common::tcp_pair().await;

    assert_eq!(pair.client.peer_key(), Some(pair.server_key));
    assert_eq!(pair.server.peer_key(), Some(pair.client_key));
    assert_ne!(pair.client_key, pair.server_key);
    assert_eq!(pair.client.remote_address().ip(), Ipv4Addr::LOCALHOST);
    assert_eq!(pair.server.remote_address().ip(), Ipv4Addr::LOCALHOST);
}

/// Past 256 unacknowledged streams yamux parks `poll_new_outbound`, and the acknowledgement that
/// releases it arrives only through `poll_next_inbound`: a driver awaiting the two separately would
/// stop collecting acknowledgements exactly when it starts needing one, and stall here.
#[tokio::test]
async fn tcp_transport_opens_past_the_ack_backlog() {
    const BACKLOG: usize = 256;
    const BURST: usize = BACKLOG + 44;

    let pair = common::tcp_pair().await;
    let server = Arc::clone(&pair.server);

    let answering = tokio::spawn(async move {
        // The first wave is held unanswered so the client's 257th open really does park. A stream
        // is acknowledged by the peer's first frame on it, so accepting alone puts nothing on the
        // wire and drains nothing.
        let mut held = Vec::with_capacity(BACKLOG);
        for _ in 0..BACKLOG {
            held.push(server.accept_bi().await.unwrap());
        }
        for (send, _) in &mut held {
            send.write_all(b"ack").await.unwrap();
        }
        let mut answered = held;
        for _ in BACKLOG..BURST {
            let (mut send, recv) = server.accept_bi().await.unwrap();
            send.write_all(b"ack").await.unwrap();
            answered.push((send, recv));
        }
        answered
    });

    let opening = async {
        let mut opened = Vec::with_capacity(BURST);
        for _ in 0..BURST {
            let (mut send, recv) = pair.client.open_bi().await.unwrap();
            send.write_all(b"x").await.unwrap();
            opened.push((send, recv));
        }
        opened
    };
    let opened = timeout(Duration::from_secs(20), opening)
        .await
        .expect("open_bi stalled past the 256-stream acknowledgement backlog");
    assert_eq!(opened.len(), BURST);
    assert_eq!(answering.await.unwrap().len(), BURST);
}

#[tokio::test]
async fn tcp_closed_resolves_only_once_the_peer_has_closed() {
    let pair = common::tcp_pair().await;
    let (client, server) = (pair.client, pair.server);

    assert!(
        server.closed().now_or_never().is_none(),
        "closed() resolved on a live connection"
    );

    client.close(CloseReason::Shutdown);
    timeout(Duration::from_secs(5), server.closed())
        .await
        .expect("the peer never saw the close");
}

/// Both connections are driven by detached tasks: a `Connection` that stops being polled stalls
/// every stream on it. The server is handed its end through a channel rather than returned
/// alongside the client's, because yamux carries the SYN flag on a stream's first frame rather
/// than opening eagerly, so nothing arrives until the caller writes.
async fn yamux_pair() -> (yamux::Stream, mpsc::Receiver<yamux::Stream>) {
    let (a, b) = tokio::io::duplex(64 * 1024);
    let mut client =
        yamux::Connection::new(a.compat(), yamux::Config::default(), yamux::Mode::Client);
    let mut server =
        yamux::Connection::new(b.compat(), yamux::Config::default(), yamux::Mode::Server);

    let (inbound_tx, inbound_rx) = mpsc::channel(1);
    tokio::spawn(async move {
        while let Some(Ok(stream)) = poll_fn(|cx| server.poll_next_inbound(cx)).await {
            if inbound_tx.send(stream).await.is_err() {
                break;
            }
        }
    });

    let (stream_tx, stream_rx) = oneshot::channel();
    tokio::spawn(async move {
        let stream = poll_fn(|cx| client.poll_new_outbound(cx)).await.unwrap();
        stream_tx.send(stream).unwrap();
        while poll_fn(|cx| client.poll_next_inbound(cx)).await.is_some() {}
    });

    (stream_rx.await.unwrap(), inbound_rx)
}

fn halves(stream: yamux::Stream) -> (SendHalf, RecvHalf) {
    let (recv, send) = tokio::io::split(stream.compat());
    (SendHalf::Tcp(send), RecvHalf::Tcp(recv))
}

#[tokio::test]
async fn a_yamux_pair_round_trips_through_the_halves() {
    let (client, mut inbound) = yamux_pair().await;
    let (mut cs, mut cr) = halves(client);

    // The enum's own AsyncWrite here and write_bytes for the reply, so one round trip covers both
    // send paths.
    cs.write_all(b"ping").await.unwrap();
    cs.flush().await.unwrap();

    let server = inbound
        .recv()
        .await
        .expect("the server never saw the stream");
    let (mut ss, mut sr) = halves(server);
    let mut got = [0u8; 4];
    timeout(Duration::from_secs(5), sr.read_exact(&mut got))
        .await
        .expect("the request never arrived")
        .unwrap();
    assert_eq!(&got, b"ping");

    ss.write_bytes(Bytes::from_static(b"pong")).await.unwrap();
    ss.finish().await;

    let mut back = Vec::new();
    timeout(Duration::from_secs(5), cr.read_to_end(&mut back))
        .await
        .expect("the peer never saw end-of-stream")
        .unwrap();
    assert_eq!(back, b"pong");
}

#[tokio::test]
async fn resetting_a_yamux_send_half_leaves_the_peer_reading_a_clean_eof() {
    let (client, mut inbound) = yamux_pair().await;
    let (mut cs, _cr) = halves(client);

    cs.write_bytes(Bytes::from_static(b"hi")).await.unwrap();
    let server = inbound
        .recv()
        .await
        .expect("the server never saw the stream");
    let (_ss, mut sr) = halves(server);
    cs.reset(0x42);

    let mut got = Vec::new();
    timeout(Duration::from_secs(5), sr.read_to_end(&mut got))
        .await
        .expect("the peer never saw the stream end")
        .expect("yamux surfaces a reset as end-of-stream, so the read cannot fail");
    assert_eq!(got, b"hi");
}
