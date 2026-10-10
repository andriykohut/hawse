mod common;

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use common::{client_config_over, expect_bound, server_config, start_client, start_server};
use hawse_core::config::Prefer;
use hawse_core::identity::Identity;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::oneshot;

/// Stands between a client and `server` so the server sees the client's packets come from one
/// address and, once `switch` fires, from another, as it would when the client changes network.
/// The first socket is dropped at the switch, so a server that kept answering the old address
/// would reach nobody.
async fn relay(server: SocketAddr, mut switch: oneshot::Receiver<()>) -> SocketAddr {
    let loopback: IpAddr = if server.is_ipv4() {
        Ipv4Addr::LOCALHOST.into()
    } else {
        Ipv6Addr::LOCALHOST.into()
    };
    let front = UdpSocket::bind((loopback, 0)).await.unwrap();
    let addr = front.local_addr().unwrap();
    let mut out = UdpSocket::bind((loopback, 0)).await.unwrap();
    tokio::spawn(async move {
        let mut client = None;
        let mut switched = false;
        let (mut up, mut down) = ([0u8; 2048], [0u8; 2048]);
        loop {
            tokio::select! {
                Ok(()) = &mut switch, if !switched => {
                    out = UdpSocket::bind((loopback, 0)).await.unwrap();
                    switched = true;
                }
                Ok((n, from)) = front.recv_from(&mut up) => {
                    client = Some(from);
                    let _ = out.send_to(&up[..n], server).await;
                }
                Ok((n, _)) = out.recv_from(&mut down) => {
                    if let Some(client) = client {
                        let _ = front.send_to(&down[..n], client).await;
                    }
                }
            }
        }
    });
    addr
}

/// A local service that speaks unasked, a byte every 10 ms. The server learns a client's new
/// address from the client's next packet, so only a service with something to send shows the move
/// at once: behind a quiet one that packet is the client's next ping, up to 15 s later.
async fn ticker() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut conn, _)) = listener.accept().await {
            tokio::spawn(async move {
                while conn.write_all(b"x").await.is_ok() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            });
        }
    });
    addr
}

async fn ticks(visitor: &mut TcpStream, n: usize) {
    let mut buf = vec![0u8; n];
    tokio::time::timeout(Duration::from_secs(5), visitor.read_exact(&mut buf))
        .await
        .expect("the ticks within 5 s")
        .unwrap();
}

#[tokio::test]
async fn a_visitor_stream_outlives_a_change_of_the_clients_address() {
    let (server_id, client_id) = (Identity::generate().unwrap(), Identity::generate().unwrap());
    let server = start_server(
        &server_config(&[("test", client_id.public_key(), &[])]),
        &server_id,
    );
    let local = ticker().await;
    let (switch, switched) = oneshot::channel();
    let via = relay(server.addr, switched).await;
    let mut client = start_client(
        client_config_over(
            via,
            server.key,
            &[("ticker", &local.to_string(), "any")],
            Prefer::Quic,
        ),
        client_id,
    );
    let port = expect_bound(&mut client.events, "ticker").await;

    let mut visitor = TcpStream::connect(("127.0.0.1", port.number))
        .await
        .unwrap();
    ticks(&mut visitor, 1).await;
    switch.send(()).unwrap();
    // Half a second of ticks, far more than was in flight at the switch.
    ticks(&mut visitor, 50).await;

    // The same session carried both: a reconnect would have reset the visitor and said so here.
    assert!(
        client.events.try_recv().is_err(),
        "the client reported an event"
    );
    client.cancel.cancel();
    server.cancel.cancel();
}
