mod common;

use std::time::Duration;

use common::{
    client_config, echo_server, expect_bound, fixed_port, raw_client, raw_hello, server_config,
    start, start_server,
};
use hawse_core::client::Client;
use hawse_core::identity::Identity;
use hawse_core::server::policy::Policy;
use hawse_proto::msg::{BindFailure, ServerMessage};
use hawse_proto::port::Kind;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

async fn echoes(port: u16) {
    let mut visitor = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    visitor.write_all(b"hello").await.unwrap();
    let mut buf = [0u8; 5];
    visitor.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"hello");
}

#[tokio::test]
async fn a_client_a_reload_no_longer_names_is_told_and_then_denied() {
    let (server_id, client_id) = (Identity::generate().unwrap(), Identity::generate().unwrap());
    let server = start_server(
        &server_config(&[("test", client_id.public_key(), &[])]),
        &server_id,
    );
    let mut client = raw_client(&server, &client_id).await;

    server
        .policy
        .send_replace(Policy::from_config(&server_config(&[])));

    let ServerMessage::Shutdown { reason } = client.reply().await else {
        panic!("a revoked session was not told it is over");
    };
    assert!(reason.contains("no longer authorized"), "{reason}");
    let mut again = raw_hello(&server, &client_id).await;
    assert!(matches!(again.reply().await, ServerMessage::Denied { .. }));
    server.cancel.cancel();
}

#[tokio::test]
async fn a_port_a_reload_takes_out_of_a_grant_is_unbound_and_the_session_goes_on() {
    let (server_id, client_id) = (Identity::generate().unwrap(), Identity::generate().unwrap());
    let granted = fixed_port(Kind::Udp);
    let grant = format!("{granted}/udp");
    let server = start_server(
        &server_config(&[("test", client_id.public_key(), &[&grant])]),
        &server_id,
    );
    let mut client = raw_client(&server, &client_id).await;
    assert!(matches!(
        client.bind_udp("fixed", Some(granted)).await,
        ServerMessage::Bound { .. }
    ));
    client.bound_udp("dynamic").await;

    server
        .policy
        .send_replace(Policy::from_config(&server_config(&[(
            "test",
            client_id.public_key(),
            &[],
        )])));

    let lost = ServerMessage::BindFailed {
        service: "fixed".to_owned(),
        reason: BindFailure::NotGranted,
    };
    assert_eq!(client.reply().await, lost);
    // The name is free again, the port is not this client's to ask for, and the dynamic service
    // was never under the grant.
    assert_eq!(client.bind_udp("fixed", Some(granted)).await, lost);
    assert_eq!(
        client.bind_udp("dynamic", None).await,
        ServerMessage::BindFailed {
            service: "dynamic".to_owned(),
            reason: BindFailure::InUse,
        }
    );
    server.cancel.cancel();
}

#[tokio::test]
async fn a_running_client_binds_a_service_a_reload_adds_and_unbinds_one_it_removes() {
    let (server_id, client_id) = (Identity::generate().unwrap(), Identity::generate().unwrap());
    let server = start_server(
        &server_config(&[("test", client_id.public_key(), &[])]),
        &server_id,
    );
    let echo = echo_server().await.to_string();
    let one = client_config(server.addr, server.key, &[("one", &echo, "any")]);
    let both = client_config(
        server.addr,
        server.key,
        &[("one", &echo, "any"), ("two", &echo, "any")],
    );
    let two = client_config(server.addr, server.key, &[("two", &echo, "any")]);

    let client = Client::new(one, client_id);
    let expose = client.expose();
    let mut client = start(client);
    let first = expect_bound(&mut client.events, "one").await.number;
    echoes(first).await;

    expose.send_replace(both.expose);
    let second = expect_bound(&mut client.events, "two").await.number;
    echoes(second).await;
    echoes(first).await;

    expose.send_replace(two.expose);
    let closed = async {
        while TcpStream::connect(("127.0.0.1", first)).await.is_ok() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), closed)
        .await
        .expect("the removed service's port closes within 5 s");
    echoes(second).await;

    client.cancel.cancel();
    assert!(client.task.await.unwrap().is_ok());
    server.cancel.cancel();
}
