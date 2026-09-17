mod common;

use std::sync::Arc;
use std::time::Duration;

use hawse_core::frame::{read_body, write_body};
use hawse_core::transport::{RecvHalf, SendHalf};
use hawse_core::udp::{BULK_QUEUE, Drops, Sender};
use hawse_proto::msg::DatagramHeader;
use hawse_proto::packet;

const HEADER: DatagramHeader = DatagramHeader {
    service_id: 3,
    session: 9,
};

/// Past any QUIC datagram on a loopback path, well inside one bulk frame.
const OVERSIZED: usize = 4000;

#[tokio::test]
async fn a_small_payload_travels_as_a_quic_datagram() {
    let pair = common::quic_pair().await;
    let (sender, mut bulk) = Sender::new(pair.server_transport(), Arc::default());

    sender.send(HEADER, b"ping");

    let packet = tokio::time::timeout(Duration::from_secs(2), pair.client.read_datagram())
        .await
        .expect("a datagram within 2 s")
        .unwrap();
    assert_eq!(packet::decode(&packet).unwrap(), (HEADER, &b"ping"[..]));
    assert!(bulk.try_recv().is_err());
}

#[tokio::test]
async fn an_oversized_payload_is_queued_for_the_bulk_stream_on_quic() {
    let pair = common::quic_pair().await;
    let (sender, mut bulk) = Sender::new(pair.server_transport(), Arc::default());
    let payload = vec![7u8; OVERSIZED];

    sender.send(HEADER, &payload);

    let packet = bulk.try_recv().expect("queued without waiting");
    assert_eq!(packet::decode(&packet).unwrap(), (HEADER, &payload[..]));
}

#[tokio::test]
async fn every_payload_is_queued_for_the_bulk_stream_on_tcp() {
    let pair = common::tcp_pair().await;
    let (sender, mut bulk) = Sender::new(pair.server, Arc::default());

    sender.send(HEADER, b"ping");

    let packet = bulk.try_recv().expect("queued without waiting");
    assert_eq!(packet::decode(&packet).unwrap(), (HEADER, &b"ping"[..]));
}

#[tokio::test]
async fn a_full_bulk_queue_drops_the_new_packet_and_counts_it() {
    let pair = common::tcp_pair().await;
    let drops = Arc::new(Drops::default());
    let (sender, mut bulk) = Sender::new(pair.server, Arc::clone(&drops));

    for n in 0..=BULK_QUEUE {
        sender.send(HEADER, &u32::try_from(n).unwrap().to_le_bytes());
    }

    assert_eq!(drops.snapshot().bulk_full, 1);
    let first = bulk.try_recv().unwrap();
    assert_eq!(packet::decode(&first).unwrap().1, &0u32.to_le_bytes()[..]);
}

#[tokio::test]
async fn a_body_the_size_of_the_largest_packet_crosses_a_stream() {
    let pair = common::quic_pair().await;
    let body = vec![5u8; 65535];
    let (send, _) = pair.client.open_bi().await.unwrap();
    let mut send = SendHalf::Quic(send);
    write_body(&mut send, &body).await.unwrap();

    let (_, recv) = pair.server.accept_bi().await.unwrap();
    let mut recv = RecvHalf::Quic(recv);
    assert_eq!(read_body(&mut recv).await.unwrap(), body);
}
