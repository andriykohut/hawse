mod common;

use std::time::Duration;

use common::{
    RunningClient, RunningServer, client_config, client_config_over, expect_bound,
    free_udp_port_outside_pool, server_config, start_client, start_server, udp_ask,
    udp_echo_server, udp_recv, udp_replier, udp_visitor,
};
use hawse_core::config::Prefer;
use hawse_core::identity::Identity;
use tokio::net::UdpSocket;

struct Tunnel {
    server: RunningServer,
    client: RunningClient,
    ports: Vec<u16>,
}

/// One dynamic UDP service per `(name, local)`, `ports` in the same order.
async fn tunnel(prefer: Prefer, services: &[(&str, &str)]) -> Tunnel {
    let server_id = Identity::generate().unwrap();
    let client_id = Identity::generate().unwrap();
    let server = start_server(
        &server_config(&[("test", client_id.public_key(), &[])]),
        &server_id,
    );
    let exposes: Vec<(&str, &str, &str)> = services
        .iter()
        .map(|(name, local)| (*name, *local, "any/udp"))
        .collect();
    let mut client = start_client(
        client_config_over(server.addr, server.key, &exposes, prefer),
        client_id,
    );
    let mut ports = Vec::new();
    for (name, _) in services {
        ports.push(expect_bound(&mut client.events, name).await.number);
    }
    Tunnel {
        server,
        client,
        ports,
    }
}

impl Tunnel {
    async fn close(self) {
        self.client.cancel.cancel();
        assert!(self.client.task.await.unwrap().is_ok());
        self.server.cancel.cancel();
        self.server.task.await.unwrap();
    }
}

async fn echoes_a_datagram(prefer: Prefer) {
    let echo = udp_echo_server().await.to_string();
    let tunnel = tunnel(prefer, &[("echo", &echo)]).await;
    let visitor = udp_visitor().await;
    assert_eq!(udp_ask(&visitor, tunnel.ports[0], b"hello").await, b"hello");
    tunnel.close().await;
}

#[tokio::test]
async fn echoes_a_datagram_over_quic() {
    echoes_a_datagram(Prefer::Quic).await;
}

#[tokio::test]
async fn echoes_a_datagram_over_tcp() {
    echoes_a_datagram(Prefer::Tcp).await;
}

async fn keeps_two_visitors_apart(prefer: Prefer) {
    let echo = udp_echo_server().await.to_string();
    let tunnel = tunnel(prefer, &[("echo", &echo)]).await;
    let (one, two) = (udp_visitor().await, udp_visitor().await);

    assert_eq!(
        udp_ask(&one, tunnel.ports[0], b"from one").await,
        b"from one"
    );
    assert_eq!(
        udp_ask(&two, tunnel.ports[0], b"from two").await,
        b"from two"
    );

    assert_eq!(udp_recv(&one).await, None);
    assert_eq!(udp_recv(&two).await, None);
    tunnel.close().await;
}

#[tokio::test]
async fn keeps_two_visitors_apart_over_quic() {
    keeps_two_visitors_apart(Prefer::Quic).await;
}

#[tokio::test]
async fn keeps_two_visitors_apart_over_tcp() {
    keeps_two_visitors_apart(Prefer::Tcp).await;
}

/// macOS refuses to send a datagram over `net.inet.udp.maxdgram`, 9216 bytes by default.
#[cfg(target_os = "linux")]
const PAYLOADS: &[usize] = &[1472, 9000, 65507];
#[cfg(not(target_os = "linux"))]
const PAYLOADS: &[usize] = &[1472, 9000];

async fn echoes_payloads_no_datagram_holds(prefer: Prefer) {
    let echo = udp_echo_server().await.to_string();
    let tunnel = tunnel(prefer, &[("echo", &echo)]).await;
    let visitor = udp_visitor().await;
    for &len in PAYLOADS {
        let payload: Vec<u8> = (0..len).map(|n| u8::try_from(n % 251).unwrap()).collect();
        let reply = udp_ask(&visitor, tunnel.ports[0], &payload).await;
        assert!(reply == payload, "{len} bytes came back as {}", reply.len());
    }
    tunnel.close().await;
}

#[tokio::test]
async fn echoes_payloads_no_datagram_holds_over_quic() {
    echoes_payloads_no_datagram_holds(Prefer::Quic).await;
}

#[tokio::test]
async fn echoes_payloads_no_datagram_holds_over_tcp() {
    echoes_payloads_no_datagram_holds(Prefer::Tcp).await;
}

async fn a_small_request_gets_its_oversized_reply(prefer: Prefer) {
    let replier = udp_replier(4000).await.to_string();
    let tunnel = tunnel(prefer, &[("big", &replier)]).await;
    let visitor = udp_visitor().await;
    assert_eq!(
        udp_ask(&visitor, tunnel.ports[0], b"?").await,
        vec![0xAB; 4000]
    );
    tunnel.close().await;
}

#[tokio::test]
async fn a_small_request_gets_its_oversized_reply_over_quic() {
    a_small_request_gets_its_oversized_reply(Prefer::Quic).await;
}

#[tokio::test]
async fn a_small_request_gets_its_oversized_reply_over_tcp() {
    a_small_request_gets_its_oversized_reply(Prefer::Tcp).await;
}

async fn a_silent_local_service_costs_only_silence(prefer: Prefer) {
    let closed = UdpSocket::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .to_string();
    let echo = udp_echo_server().await.to_string();
    let tunnel = tunnel(prefer, &[("dead", &closed), ("echo", &echo)]).await;
    let visitor = udp_visitor().await;

    for _ in 0..3 {
        visitor
            .send_to(b"anyone?", ("127.0.0.1", tunnel.ports[0]))
            .await
            .unwrap();
    }
    assert_eq!(udp_recv(&visitor).await, None);

    assert_eq!(udp_ask(&visitor, tunnel.ports[1], b"hello").await, b"hello");
    assert!(!tunnel.client.task.is_finished());
    tunnel.close().await;
}

#[tokio::test]
async fn a_silent_local_service_costs_only_silence_over_quic() {
    a_silent_local_service_costs_only_silence(Prefer::Quic).await;
}

#[tokio::test]
async fn a_silent_local_service_costs_only_silence_over_tcp() {
    a_silent_local_service_costs_only_silence(Prefer::Tcp).await;
}

async fn a_fixed_udp_port_is_free_for_the_next_session(prefer: Prefer) {
    let server_id = Identity::generate().unwrap();
    let client_id = Identity::generate().unwrap();
    let granted = free_udp_port_outside_pool().await;
    let grant = format!("{granted}/udp");
    let server = start_server(
        &server_config(&[("test", client_id.public_key(), &[&grant])]),
        &server_id,
    );
    let echo = udp_echo_server().await.to_string();
    let cfg = client_config_over(server.addr, server.key, &[("echo", &echo, &grant)], prefer);
    let visitor = udp_visitor().await;

    let twin = Identity::from_pem(&client_id.to_pem()).unwrap();
    let mut first = start_client(cfg.clone(), twin);
    assert_eq!(
        expect_bound(&mut first.events, "echo").await.number,
        granted
    );
    assert_eq!(udp_ask(&visitor, granted, b"first").await, b"first");
    first.cancel.cancel();
    assert!(first.task.await.unwrap().is_ok());

    let mut second = start_client(cfg, client_id);
    assert_eq!(
        expect_bound(&mut second.events, "echo").await.number,
        granted
    );
    assert_eq!(udp_ask(&visitor, granted, b"second").await, b"second");

    second.cancel.cancel();
    assert!(second.task.await.unwrap().is_ok());
    server.cancel.cancel();
    server.task.await.unwrap();
}

#[tokio::test]
async fn a_fixed_udp_port_is_free_for_the_next_session_over_quic() {
    a_fixed_udp_port_is_free_for_the_next_session(Prefer::Quic).await;
}

#[tokio::test]
async fn a_fixed_udp_port_is_free_for_the_next_session_over_tcp() {
    a_fixed_udp_port_is_free_for_the_next_session(Prefer::Tcp).await;
}

/// No retries here, on purpose: each session's first packet is oversized, so it rides the bulk
/// stream the server opened around the time it sent `Bound`, in whichever order QUIC delivered
/// the two. A lost first packet is the failure this looks for.
#[tokio::test]
async fn the_first_oversized_packet_of_twenty_fresh_sessions_arrives() {
    let echo = udp_echo_server().await.to_string();
    let payload = vec![9u8; 4000];
    for round in 0..20 {
        let tunnel = tunnel(Prefer::Quic, &[("echo", &echo)]).await;
        let visitor = udp_visitor().await;
        visitor
            .send_to(&payload, ("127.0.0.1", tunnel.ports[0]))
            .await
            .unwrap();
        let reply = udp_recv(&visitor).await;
        assert!(
            reply.as_deref() == Some(&payload[..]),
            "round {round} lost its first packet"
        );
        tunnel.close().await;
    }
}

mod rogue {
    use super::*;

    use futures_util::{SinkExt, StreamExt};
    use hawse_core::frame::{write_body, write_frame};
    use hawse_core::tls;
    use hawse_core::transport::SendHalf;
    use hawse_core::transport::quic::{self, Tuning};
    use hawse_proto::frame::{codec, decode, encode};
    use hawse_proto::msg::{ClientMessage, DatagramHeader, ServerMessage, StreamOpen, reset};
    use hawse_proto::packet;
    use quinn::{Connection, RecvStream, SendStream};
    use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};

    struct Session {
        conn: Connection,
        control: FramedWrite<SendStream, LengthDelimitedCodec>,
        _control_rx: FramedRead<RecvStream, LengthDelimitedCodec>,
        client: RunningClient,
        _endpoint: quinn::Endpoint,
    }

    /// A server of our own that has welcomed the client and read its one `Bind`, and has not yet
    /// answered it.
    async fn welcomed(local: &str) -> Session {
        let server_id = Identity::generate().unwrap();
        let client_id = Identity::generate().unwrap();
        let (cert, key) = server_id.certificate().unwrap();
        let endpoint = quic::listen(
            "127.0.0.1:0".parse().unwrap(),
            tls::server_config(cert, key, tls::provider()).unwrap(),
            Tuning::SERVER,
        )
        .unwrap();
        let client = start_client(
            client_config(
                endpoint.local_addr().unwrap(),
                server_id.public_key(),
                &[("svc", local, "any/udp")],
            ),
            client_id,
        );
        let conn = endpoint.accept().await.unwrap().await.unwrap();
        let (send, recv) = conn.accept_bi().await.unwrap();
        let mut control = FramedWrite::new(send, codec());
        let mut control_rx = FramedRead::new(recv, codec());
        assert!(matches!(
            decode::<ClientMessage>(&control_rx.next().await.unwrap().unwrap()).unwrap(),
            ClientMessage::Hello { .. }
        ));
        let welcome = ServerMessage::Welcome {
            agent: "rogue".into(),
            client_name: "test".into(),
        };
        control.send(encode(&welcome).unwrap()).await.unwrap();
        assert!(matches!(
            decode::<ClientMessage>(&control_rx.next().await.unwrap().unwrap()).unwrap(),
            ClientMessage::Bind { .. }
        ));
        Session {
            conn,
            control,
            _control_rx: control_rx,
            client,
            _endpoint: endpoint,
        }
    }

    #[tokio::test]
    async fn a_bulk_stream_that_beats_bound_still_finds_its_service() {
        let echo = udp_echo_server().await.to_string();
        let mut session = welcomed(&echo).await;

        let (send, _recv) = session.conn.open_bi().await.unwrap();
        let mut send = SendHalf::Quic(send);
        write_frame(&mut send, &StreamOpen::Bulk { service_id: 7 })
            .await
            .unwrap();
        let header = DatagramHeader {
            service_id: 7,
            session: 1,
        };
        write_body(&mut send, &packet::encode(header, b"early").unwrap())
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(200)).await;
        let bound = ServerMessage::Bound {
            service: "svc".into(),
            service_id: 7,
            port: 40000,
        };
        session.control.send(encode(&bound).unwrap()).await.unwrap();

        // The echo is five bytes, so it comes back as a datagram, not on the bulk stream.
        let reply = tokio::time::timeout(Duration::from_secs(2), session.conn.read_datagram())
            .await
            .expect("the early packet's echo within 2 s")
            .unwrap();
        assert_eq!(packet::decode(&reply).unwrap(), (header, &b"early"[..]));
        session.client.cancel.cancel();
    }

    #[tokio::test]
    async fn a_bulk_stream_for_a_service_never_bound_is_reset() {
        let echo = udp_echo_server().await.to_string();
        let session = welcomed(&echo).await;

        let (send, mut recv) = session.conn.open_bi().await.unwrap();
        let mut send = SendHalf::Quic(send);
        write_frame(&mut send, &StreamOpen::Bulk { service_id: 99 })
            .await
            .unwrap();

        let refusal = tokio::time::timeout(Duration::from_secs(8), recv.read_to_end(16)).await;
        assert!(
            matches!(
                &refusal,
                Ok(Err(quinn::ReadToEndError::Read(quinn::ReadError::Reset(code))))
                    if *code == quinn::VarInt::from_u32(reset::UNKNOWN_SERVICE)
            ),
            "client did not reset the stream as an unknown service: {refusal:?}"
        );
        session.client.cancel.cancel();
    }
}
