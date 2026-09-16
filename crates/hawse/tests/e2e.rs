use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use hawse_core::config::ServerConfig;

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn hawse() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hawse"));
    cmd.stdin(Stdio::null()).stderr(Stdio::inherit());
    cmd
}

fn free_udp_port() -> u16 {
    UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// The server answers its listen port on UDP and on TCP, and the two port spaces are independent:
/// a port the kernel hands out as free on UDP can be taken on TCP, and the server then refuses to
/// start rather than come up without the fallback. It also refuses a listen port inside its
/// dynamic pool, and the default pool lies within Linux's ephemeral range.
fn free_listen_port() -> u16 {
    let pool = ServerConfig::default().dynamic_ports;
    for _ in 0..16 {
        let port = free_udp_port();
        if pool.contains_number(port) {
            continue;
        }
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
    panic!("no port free on both UDP and TCP, outside the dynamic pool, in 16 tries");
}

fn free_tcp_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn keygen(path: &std::path::Path) -> String {
    let out = hawse()
        .args(["keygen", "--out"])
        .arg(path)
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

#[test]
fn echo_round_trip_through_real_binaries() {
    let dir = tempfile::tempdir().unwrap();
    let server_key = keygen(&dir.path().join("server.key"));
    let client_key = keygen(&dir.path().join("client.key"));
    let server_port = free_listen_port();
    let public_port = free_tcp_port();

    let echo = TcpListener::bind("127.0.0.1:0").unwrap();
    let echo_addr = echo.local_addr().unwrap();
    std::thread::spawn(move || {
        for socket in echo.incoming().flatten() {
            std::thread::spawn(move || {
                let mut reader = socket.try_clone().unwrap();
                let mut writer = socket;
                let mut buf = [0u8; 4096];
                while let Ok(n) = reader.read(&mut buf) {
                    if n == 0 || writer.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            });
        }
    });

    fs::write(
        dir.path().join("server.toml"),
        format!("listen = \"127.0.0.1:{server_port}\"\n\n[clients.e2e]\nkey = \"{client_key}\"\nports = [\"{public_port}\"]\n"),
    )
    .unwrap();
    fs::write(
        dir.path().join("client.toml"),
        format!("server = \"127.0.0.1:{server_port}\"\nserver_key = \"{server_key}\"\nname = \"e2e\"\n\n[expose.echo]\nlocal = \"{echo_addr}\"\nport = {public_port}\n"),
    )
    .unwrap();

    let _server = Proc(
        hawse()
            .args(["server", "--config"])
            .arg(dir.path().join("server.toml"))
            .spawn()
            .unwrap(),
    );
    let _client = Proc(
        hawse()
            .args(["client", "--config"])
            .arg(dir.path().join("client.toml"))
            .spawn()
            .unwrap(),
    );

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut visitor = loop {
        if let Ok(s) = TcpStream::connect(("127.0.0.1", public_port)) {
            break s;
        }
        assert!(Instant::now() < deadline, "public port never opened");
        std::thread::sleep(Duration::from_millis(200));
    };
    visitor
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    visitor.write_all(b"ping").unwrap();
    let mut buf = [0u8; 4];
    visitor.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"ping");
}
