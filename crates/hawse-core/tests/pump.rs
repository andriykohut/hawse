mod common;

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use hawse_core::pump::{Edge, PumpError, pump};
use hawse_core::transport::{RecvHalf, SendHalf};
use hawse_proto::msg::reset;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf};
use tokio::net::{TcpListener, TcpStream};

fn pattern(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x & 0xff) as u8
        })
        .collect()
}

/// A pipe that remembers whether `pump` aborted it.
struct Watched {
    pipe: DuplexStream,
    aborted: Arc<AtomicBool>,
}

fn watched(pipe: DuplexStream) -> (Watched, Arc<AtomicBool>) {
    let aborted = Arc::new(AtomicBool::new(false));
    (
        Watched {
            pipe,
            aborted: Arc::clone(&aborted),
        },
        aborted,
    )
}

impl AsyncRead for Watched {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().pipe).poll_read(cx, buf)
    }
}

impl AsyncWrite for Watched {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().pipe).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().pipe).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().pipe).poll_shutdown(cx)
    }
}

impl Edge for Watched {
    fn abort(&self) {
        self.aborted.store(true, Ordering::Relaxed);
    }
}

#[tokio::test]
async fn half_close_lets_the_other_direction_finish() {
    let pair = common::quic_pair().await;
    let (send, recv) = pair.client.open_bi().await.unwrap();
    let (near, far) = tokio::io::duplex(64 * 1024);
    let pumped = tokio::spawn(pump(
        near,
        SendHalf::Quic(send),
        RecvHalf::Quic(recv),
        16 * 1024,
    ));
    let (mut far_rd, mut far_wr) = tokio::io::split(far);

    far_wr.write_all(b"ping").await.unwrap();
    far_wr.shutdown().await.unwrap();

    let (mut peer_send, mut peer_recv) = pair.server.accept_bi().await.unwrap();
    assert_eq!(peer_recv.read_to_end(64).await.unwrap(), b"ping");

    peer_send.write_all(b"pong").await.unwrap();
    peer_send.finish().unwrap();
    let mut got = Vec::new();
    far_rd.read_to_end(&mut got).await.unwrap();
    assert_eq!(got, b"pong");

    let stats = pumped.await.unwrap().unwrap();
    assert_eq!((stats.to_stream, stats.to_socket), (4, 4));
}

#[tokio::test]
async fn moves_large_payloads_intact_both_ways() {
    const LEN: usize = 8 * 1024 * 1024;
    let pair = common::quic_pair().await;
    let (send, recv) = pair.client.open_bi().await.unwrap();
    let (near, far) = tokio::io::duplex(256 * 1024);
    let pumped = tokio::spawn(pump(
        near,
        SendHalf::Quic(send),
        RecvHalf::Quic(recv),
        16 * 1024,
    ));
    let (mut far_rd, mut far_wr) = tokio::io::split(far);

    let up = pattern(LEN, 1);
    let down = pattern(LEN, 2);
    let up_expected = up.clone();
    let down_expected = down.clone();

    // Started before accept_bi: quinn only reveals a stream to the peer once a frame carries
    // data for it, so accept_bi would hang forever waiting on a stream nothing has written to.
    let writer_far = tokio::spawn(async move {
        far_wr.write_all(&up).await.unwrap();
        far_wr.shutdown().await.unwrap();
    });
    let reader_far = tokio::spawn(async move {
        let mut got = Vec::with_capacity(LEN);
        far_rd.read_to_end(&mut got).await.unwrap();
        got
    });

    let (mut peer_send, mut peer_recv) = pair.server.accept_bi().await.unwrap();
    let writer_peer = tokio::spawn(async move {
        peer_send.write_all(&down).await.unwrap();
        peer_send.finish().unwrap();
        peer_send.stopped().await.ok();
    });
    let reader_peer = tokio::spawn(async move { peer_recv.read_to_end(LEN).await.unwrap() });

    writer_far.await.unwrap();
    writer_peer.await.unwrap();
    assert_eq!(reader_far.await.unwrap(), down_expected);
    assert_eq!(reader_peer.await.unwrap(), up_expected);
    let stats = pumped.await.unwrap().unwrap();
    assert_eq!((stats.to_stream, stats.to_socket), (LEN as u64, LEN as u64));
}

#[tokio::test]
async fn frames_round_trip_on_raw_streams() {
    use hawse_core::frame::{read_frame, write_frame};
    use hawse_proto::msg::StreamHeader;
    let pair = common::quic_pair().await;
    let (send, _recv) = pair.client.open_bi().await.unwrap();
    let mut send = SendHalf::Quic(send);
    let header = StreamHeader {
        service_id: 9,
        visitor: "203.0.113.5:5555".parse().unwrap(),
        listener: "[::]:443".parse().unwrap(),
    };
    write_frame(&mut send, &header).await.unwrap();
    send.write_all(b"payload").await.unwrap();
    send.finish().await;
    let (_send, recv) = pair.server.accept_bi().await.unwrap();
    let mut recv = RecvHalf::Quic(recv);
    assert_eq!(read_frame::<StreamHeader>(&mut recv).await.unwrap(), header);
    let mut rest = Vec::new();
    recv.read_to_end(&mut rest).await.unwrap();
    assert_eq!(rest, b"payload");
}

/// Fails every read deterministically, so `pump`'s socket-to-stream half always aborts there.
struct BoomOnRead(Arc<AtomicBool>);

impl Edge for BoomOnRead {
    fn abort(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

impl AsyncRead for BoomOnRead {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Poll::Ready(Err(io::Error::other("boom")))
    }
}

impl AsyncWrite for BoomOnRead {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn abort_resets_the_stream_for_the_peer() {
    let pair = common::quic_pair().await;
    let (send, recv) = pair.client.open_bi().await.unwrap();
    let mut send = SendHalf::Quic(send);
    // One byte so the server's accept_bi sees the stream before the pump aborts it.
    send.write_all(b"x").await.unwrap();

    let accepted = tokio::spawn(async move {
        let (_peer_send, mut peer_recv) = pair.server.accept_bi().await.unwrap();
        loop {
            match peer_recv.read_chunk(64, true).await {
                Ok(Some(_)) => {}
                Ok(None) => panic!("stream finished cleanly instead of being reset"),
                Err(err) => break err,
            }
        }
    });

    let aborted = Arc::new(AtomicBool::new(false));
    let result = pump(
        BoomOnRead(Arc::clone(&aborted)),
        send,
        RecvHalf::Quic(recv),
        16 * 1024,
    )
    .await;
    assert!(result.is_err());
    assert!(
        aborted.load(Ordering::Relaxed),
        "a socket that failed is aborted too"
    );

    let peer_err = tokio::time::timeout(Duration::from_secs(5), accepted)
        .await
        .expect("peer never observed the reset")
        .unwrap();
    assert!(matches!(peer_err, quinn::ReadError::Reset(_)));
}

#[tokio::test]
async fn a_transfer_finished_both_ways_leaves_the_socket_alone() {
    let pair = common::quic_pair().await;
    let (send, recv) = pair.client.open_bi().await.unwrap();
    let (near, far) = tokio::io::duplex(64 * 1024);
    let (near, aborted) = watched(near);
    let pumped = tokio::spawn(pump(
        near,
        SendHalf::Quic(send),
        RecvHalf::Quic(recv),
        16 * 1024,
    ));
    let (mut far_rd, mut far_wr) = tokio::io::split(far);

    far_wr.write_all(b"ping").await.unwrap();
    far_wr.shutdown().await.unwrap();
    let (mut peer_send, mut peer_recv) = pair.server.accept_bi().await.unwrap();
    assert_eq!(peer_recv.read_to_end(64).await.unwrap(), b"ping");
    peer_send.write_all(b"pong").await.unwrap();
    peer_send.finish().unwrap();
    let mut got = Vec::new();
    far_rd.read_to_end(&mut got).await.unwrap();
    assert_eq!(got, b"pong");

    pumped.await.unwrap().unwrap();
    assert!(!aborted.load(Ordering::Relaxed));
}

#[tokio::test]
async fn a_stream_the_peer_resets_aborts_the_socket() {
    let pair = common::quic_pair().await;
    let (send, recv) = pair.client.open_bi().await.unwrap();
    let (near, far) = tokio::io::duplex(64 * 1024);
    let (near, aborted) = watched(near);
    let pumped = tokio::spawn(pump(
        near,
        SendHalf::Quic(send),
        RecvHalf::Quic(recv),
        16 * 1024,
    ));
    let (_far_rd, mut far_wr) = tokio::io::split(far);

    // One byte so the server's accept_bi sees the stream.
    far_wr.write_all(b"x").await.unwrap();
    let (mut peer_send, _peer_recv) = pair.server.accept_bi().await.unwrap();
    peer_send
        .reset(quinn::VarInt::from_u32(reset::LOCAL_REFUSED))
        .unwrap();

    let err = tokio::time::timeout(Duration::from_secs(5), pumped)
        .await
        .expect("the pump ends once its stream is reset")
        .unwrap()
        .unwrap_err();
    assert!(matches!(err, PumpError::Stream(_)), "{err:?}");
    assert_eq!(err.reset_code(), Some(reset::LOCAL_REFUSED));
    assert!(aborted.load(Ordering::Relaxed));
}

/// The other half: the peer stops the stream and leaves its own send side open, so the pump's
/// read stays pending and only its next write can meet the stop.
#[tokio::test]
async fn a_stream_the_peer_stops_aborts_the_socket() {
    let pair = common::quic_pair().await;
    let (send, recv) = pair.client.open_bi().await.unwrap();
    let (near, far) = tokio::io::duplex(64 * 1024);
    let (near, aborted) = watched(near);
    let pumped = tokio::spawn(pump(
        near,
        SendHalf::Quic(send),
        RecvHalf::Quic(recv),
        16 * 1024,
    ));
    let (_far_rd, mut far_wr) = tokio::io::split(far);

    far_wr.write_all(b"x").await.unwrap();
    let (_peer_send, mut peer_recv) = pair.server.accept_bi().await.unwrap();
    peer_recv
        .stop(quinn::VarInt::from_u32(reset::LOCAL_REFUSED))
        .unwrap();
    // Keeps the pump writing; these writes fail once the pump has ended and let go of the pipe.
    let sending = tokio::spawn(async move {
        while far_wr.write_all(&[0u8; 1024]).await.is_ok() {
            tokio::task::yield_now().await;
        }
    });

    let err = tokio::time::timeout(Duration::from_secs(5), pumped)
        .await
        .expect("the pump ends once its stream is stopped")
        .unwrap()
        .unwrap_err();
    assert!(matches!(err, PumpError::Stream(_)), "{err:?}");
    assert_eq!(err.reset_code(), Some(reset::LOCAL_REFUSED));
    assert!(aborted.load(Ordering::Relaxed));
    sending.abort();
}

#[tokio::test]
async fn a_pump_dropped_mid_transfer_aborts_the_socket() {
    let pair = common::quic_pair().await;
    let (send, recv) = pair.client.open_bi().await.unwrap();
    let (near, far) = tokio::io::duplex(64 * 1024);
    let (near, aborted) = watched(near);
    let pumped = tokio::spawn(pump(
        near,
        SendHalf::Quic(send),
        RecvHalf::Quic(recv),
        16 * 1024,
    ));
    let (_far_rd, mut far_wr) = tokio::io::split(far);

    far_wr.write_all(b"ping").await.unwrap();
    let (_peer_send, mut peer_recv) = pair.server.accept_bi().await.unwrap();
    // Read through the pump, so it is known to be running when it is dropped.
    let mut buf = [0u8; 4];
    peer_recv.read_exact(&mut buf).await.unwrap();

    pumped.abort();
    assert!(pumped.await.unwrap_err().is_cancelled());
    assert!(aborted.load(Ordering::Relaxed));
}

/// The one test over a real socket: what `abort` does to a `TcpStream` is the point of it.
#[tokio::test]
async fn a_tcp_socket_is_reset_when_its_stream_is() {
    let pair = common::quic_pair().await;
    let (send, recv) = pair.client.open_bi().await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut far = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (near, _) = listener.accept().await.unwrap();
    let pumped = tokio::spawn(pump(
        near,
        SendHalf::Quic(send),
        RecvHalf::Quic(recv),
        16 * 1024,
    ));

    far.write_all(b"x").await.unwrap();
    let (mut peer_send, _peer_recv) = pair.server.accept_bi().await.unwrap();
    peer_send
        .reset(quinn::VarInt::from_u32(reset::ABORTED))
        .unwrap();

    let mut buf = [0u8; 8];
    let read = tokio::time::timeout(Duration::from_secs(5), far.read(&mut buf))
        .await
        .expect("the socket ends once its stream is reset");
    let err = read.expect_err("a reset stream must not close the socket cleanly");
    assert_eq!(err.kind(), io::ErrorKind::ConnectionReset, "{err:?}");
    assert!(pumped.await.unwrap().is_err());
}
