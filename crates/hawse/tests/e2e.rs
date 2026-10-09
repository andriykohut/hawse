use std::fs;
use std::hash::{BuildHasher, RandomState};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv6Addr, TcpListener, TcpStream, UdpSocket};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use hawse_core::config::ServerConfig;
use hawse_core::net;
use hawse_proto::port::Kind;

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
    // Tests read what a child logs, so it logs at its defaults whatever the runner's filter is.
    cmd.env_remove("HAWSE_LOG");
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

/// A public port nothing holds on any interface, from below the range the kernel draws from for a
/// bind to port 0 and for an outgoing connection, which also keeps it out of the default dynamic
/// pool. A port from that range is free only until the next socket on the machine draws the same
/// number, and the server binds this one some time after it is picked.
fn free_public_port(kind: Kind) -> u16 {
    const FIRST: u16 = 20000;
    const COUNT: u16 = 10000;
    // The probe is the server's own bind, and its sockets register with a runtime.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let every_interface = Ipv6Addr::UNSPECIFIED.into();
    // Each process starts somewhere else in the range, so two runs side by side do not probe the
    // same ports in the same order.
    let start = RandomState::new().hash_one(()) % u64::from(COUNT);
    let start = u16::try_from(start).expect("below the count");
    (0..COUNT)
        .map(|step| FIRST + (start + step) % COUNT)
        .find(|&port| match kind {
            Kind::Tcp => net::bind_tcp(every_interface, port).is_ok(),
            Kind::Udp => net::bind_udp(every_interface, port).is_ok(),
        })
        .expect("a free port below the ephemeral range")
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
    let public_port = free_public_port(Kind::Tcp);

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

/// Whether a visitor to `port` gets its bytes back, tried until `open` is what it sees.
fn wait_for_echo(port: u16, open: bool, what: &str) {
    let echoed = || {
        let mut visitor = TcpStream::connect(("127.0.0.1", port)).ok()?;
        visitor
            .set_read_timeout(Some(Duration::from_secs(5)))
            .ok()?;
        visitor.write_all(b"ping").ok()?;
        let mut buf = [0u8; 4];
        visitor.read_exact(&mut buf).ok()?;
        (&buf == b"ping").then_some(())
    };
    let deadline = Instant::now() + Duration::from_secs(15);
    while echoed().is_some() != open {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn a_config_edited_under_running_binaries_takes_effect_without_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let server_key = keygen(&dir.path().join("server.key"));
    let client_key = keygen(&dir.path().join("client.key"));
    let server_port = free_listen_port();
    let first = free_public_port(Kind::Tcp);
    let second = (first + 1..30000)
        .find(|&port| TcpListener::bind(("::", port)).is_ok())
        .unwrap();

    let echo = TcpListener::bind("127.0.0.1:0").unwrap();
    let echo_addr = echo.local_addr().unwrap();
    std::thread::spawn(move || {
        for socket in echo.incoming().flatten() {
            std::thread::spawn(move || {
                let mut reader = socket.try_clone().unwrap();
                let mut writer = socket;
                let _ = std::io::copy(&mut reader, &mut writer);
            });
        }
    });

    let server_toml = dir.path().join("server.toml");
    let client_toml = dir.path().join("client.toml");
    let listen = format!("listen = \"127.0.0.1:{server_port}\"\n");
    let granted = format!(
        "{listen}\n[clients.e2e]\nkey = \"{client_key}\"\nports = [\"{first}\", \"{second}\"]\n"
    );
    let one = format!(
        "server = \"127.0.0.1:{server_port}\"\nserver_key = \"{server_key}\"\n\n[expose.one]\nlocal = \"{echo_addr}\"\nport = {first}\n"
    );
    fs::write(&server_toml, &granted).unwrap();
    fs::write(&client_toml, &one).unwrap();
    let _server = Proc(
        hawse()
            .args(["server", "--config"])
            .arg(&server_toml)
            .spawn()
            .unwrap(),
    );
    let _client = Proc(
        hawse()
            .args(["client", "--config"])
            .arg(&client_toml)
            .spawn()
            .unwrap(),
    );
    wait_for_echo(first, true, "the first service never opened");

    let two = format!("{one}\n[expose.two]\nlocal = \"{echo_addr}\"\nport = {second}\n");
    fs::write(&client_toml, two).unwrap();
    wait_for_echo(
        second,
        true,
        "the service added to client.toml never opened",
    );

    // A config that does not load changes nothing, however long it is left there.
    fs::write(&server_toml, format!("{granted}key = 7\n")).unwrap();
    std::thread::sleep(Duration::from_secs(1));
    wait_for_echo(
        first,
        true,
        "a server.toml that does not load took the tunnel down",
    );

    fs::write(&server_toml, listen).unwrap();
    wait_for_echo(
        first,
        false,
        "a client removed from server.toml kept its port",
    );
}

#[test]
fn udp_echo_round_trip_through_real_binaries() {
    let dir = tempfile::tempdir().unwrap();
    let server_key = keygen(&dir.path().join("server.key"));
    let client_key = keygen(&dir.path().join("client.key"));
    let server_port = free_listen_port();
    let public_port = free_public_port(Kind::Udp);

    let echo = UdpSocket::bind("127.0.0.1:0").unwrap();
    let echo_addr = echo.local_addr().unwrap();
    std::thread::spawn(move || {
        let mut buf = [0u8; 2048];
        while let Ok((len, peer)) = echo.recv_from(&mut buf) {
            let _ = echo.send_to(&buf[..len], peer);
        }
    });

    fs::write(
        dir.path().join("server.toml"),
        format!("listen = \"127.0.0.1:{server_port}\"\n\n[clients.e2e]\nkey = \"{client_key}\"\nports = [\"{public_port}/udp\"]\n"),
    )
    .unwrap();
    fs::write(
        dir.path().join("client.toml"),
        format!("server = \"127.0.0.1:{server_port}\"\nserver_key = \"{server_key}\"\nname = \"e2e\"\n\n[expose.echo]\nlocal = \"{echo_addr}\"\nport = \"{public_port}/udp\"\n"),
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

    let visitor = UdpSocket::bind("127.0.0.1:0").unwrap();
    visitor
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut buf = [0u8; 16];
    // Until the service is bound nothing answers, so keep asking.
    let len = loop {
        visitor
            .send_to(b"ping", ("127.0.0.1", public_port))
            .unwrap();
        if let Ok((len, _)) = visitor.recv_from(&mut buf) {
            break len;
        }
        assert!(Instant::now() < deadline, "no UDP reply through the tunnel");
    };
    assert_eq!(&buf[..len], b"ping");
}

/// Runs `role` on a config holding `text`, and returns its exit code and what it wrote to stderr.
fn run_with_config(role: &str, text: &str) -> (Option<i32>, String) {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("hawse.toml");
    fs::write(&config, text).unwrap();
    let out = hawse()
        .args([role, "--config"])
        .arg(&config)
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    (out.status.code(), String::from_utf8(out.stderr).unwrap())
}

#[test]
fn a_server_config_that_does_not_parse_exits_2_with_its_diagnostic() {
    let (code, stderr) = run_with_config("server", "listne = \"[::]:4433\"\n");
    assert!(stderr.contains("listne"), "{stderr}");
    assert_eq!(code, Some(2), "{stderr}");
}

#[test]
fn a_client_config_that_does_not_validate_exits_2_with_its_diagnostic() {
    let text = "server = \"tunnel.example.com:4433\"\nserver_key = \"ed25519:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\"\n\n[expose.Web]\nlocal = \"127.0.0.1:8080\"\n";
    let (code, stderr) = run_with_config("client", text);
    assert!(stderr.contains("expose `Web`"), "{stderr}");
    assert_eq!(code, Some(2), "{stderr}");
}

#[test]
fn check_exits_2_on_a_config_that_does_not_validate() {
    let text = "server = \"tunnel.example.com:4433\"\nserver_key = \"ed25519:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\"\n\n[expose.Web]\nlocal = \"127.0.0.1:8080\"\n";
    let (code, stderr) = run_with_config("check", text);
    assert!(stderr.contains("expose `Web`"), "{stderr}");
    assert_eq!(code, Some(2), "{stderr}");
}

#[test]
fn check_tells_the_roles_apart_and_creates_no_key() {
    let client = "server = \"tunnel.example.com\"\nserver_key = \"ed25519:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\"\n";
    for (role, text) in [("server", "listen = \"[::]:4433\"\n"), ("client", client)] {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("hawse.toml");
        fs::write(&config, text).unwrap();
        let out = hawse()
            .args(["check", "--config"])
            .arg(&config)
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert!(stderr.contains(&format!("valid {role} config")), "{stderr}");
        assert_eq!(out.status.code(), Some(0), "{stderr}");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1, "{role}");
    }
}

/// `/etc/hawse` is the host's and cannot be emptied from here, so this asserts only what a config
/// there would not change: a role's config in the user's directory is the one read.
#[test]
fn check_with_no_config_named_reads_each_role_it_finds() {
    let dir = tempfile::tempdir().unwrap();
    let configs = dir.path().join("hawse");
    fs::create_dir(&configs).unwrap();
    fs::write(configs.join("server.toml"), "").unwrap();
    fs::write(
        configs.join("client.toml"),
        "server = \"tunnel.example.com\"\nserver_key = \"ed25519:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\"\n",
    )
    .unwrap();
    let out = hawse()
        .arg("check")
        .env("XDG_CONFIG_HOME", dir.path())
        .env_remove("HAWSE_CONFIG")
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    for role in ["server", "client"] {
        let config = configs.join(format!("{role}.toml"));
        assert!(stderr.contains(&format!("valid {role} config")), "{stderr}");
        assert!(stderr.contains(config.to_str().unwrap()), "{stderr}");
    }
    assert_eq!(out.status.code(), Some(0), "{stderr}");
}

#[test]
fn check_connect_reports_the_transport_a_server_answered_on() {
    let dir = tempfile::tempdir().unwrap();
    let server_key = keygen(&dir.path().join("server.key"));
    keygen(&dir.path().join("client.key"));
    let server_port = free_listen_port();
    fs::write(
        dir.path().join("server.toml"),
        format!("listen = \"127.0.0.1:{server_port}\"\n"),
    )
    .unwrap();
    let config = dir.path().join("client.toml");
    fs::write(
        &config,
        // A QUIC dial nobody answers runs to the idle timeout, and the first check below is one.
        format!("server = \"127.0.0.1:{server_port}\"\nserver_key = \"{server_key}\"\n\n[transport]\nidle_timeout = \"1s\"\n"),
    )
    .unwrap();
    let check = || {
        let out = hawse()
            .args(["check", "--connect", "--config"])
            .arg(&config)
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        (out.status.code(), String::from_utf8(out.stderr).unwrap())
    };

    let (code, stderr) = check();
    assert!(stderr.contains("cannot connect to"), "{stderr}");
    assert_eq!(code, Some(1), "{stderr}");

    let _server = Proc(
        hawse()
            .args(["server", "--config"])
            .arg(dir.path().join("server.toml"))
            .spawn()
            .unwrap(),
    );
    // The server is listening some time after it is spawned, and until then nothing answers.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let (code, stderr) = check();
        if code == Some(0) {
            assert!(stderr.contains("quic"), "{stderr}");
            break;
        }
        assert!(Instant::now() < deadline, "no answer in 15 s: {stderr}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_server_that_cannot_bind_its_listen_port_exits_1() {
    let pool = ServerConfig::default().dynamic_ports;
    // A listen port inside the pool is a config error, which is not what this is about.
    let held = loop {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        if !pool.contains_number(socket.local_addr().unwrap().port()) {
            break socket;
        }
    };
    let port = held.local_addr().unwrap().port();
    let (code, stderr) = run_with_config("server", &format!("listen = \"127.0.0.1:{port}\"\n"));
    assert!(stderr.contains("cannot start the server"), "{stderr}");
    assert_eq!(code, Some(1), "{stderr}");
}

/// Runs `keygen` with `HAWSE_CONFIG` naming `config`.
fn keygen_under(config: &std::path::Path) {
    let out = hawse()
        .arg("keygen")
        .env("HAWSE_CONFIG", config)
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(out.status.success(), "{stderr}");
}

#[test]
fn keygen_writes_beside_the_config_the_environment_names() {
    let dir = tempfile::tempdir().unwrap();
    // The config is not written yet, which is the order the README gives.
    keygen_under(&dir.path().join("client.toml"));
    assert!(dir.path().join("client.key").is_file());
}

#[test]
fn keygen_writes_the_key_the_config_names() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("client.toml");
    fs::write(
        &config,
        "server = \"tunnel.example.com:4433\"\nserver_key = \"ed25519:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\"\nkey = \"laptop.key\"\n",
    )
    .unwrap();
    keygen_under(&config);
    assert!(dir.path().join("laptop.key").is_file());
    assert!(!dir.path().join("client.key").exists());
}

#[test]
fn a_client_that_stops_on_a_config_error_exits_2() {
    let dir = tempfile::tempdir().unwrap();
    let server_key = keygen(&dir.path().join("server.key"));
    let client_key = keygen(&dir.path().join("client.key"));
    let server_port = free_listen_port();
    // The config loads, and the one bind that carries this list does not fit in a frame: a
    // mistake the client only meets once a server has welcomed it.
    let allow: Vec<String> = (0..5000)
        .map(|n| format!("\"2001:db8::{n:x}/128\""))
        .collect();
    fs::write(
        dir.path().join("server.toml"),
        format!("listen = \"127.0.0.1:{server_port}\"\n\n[clients.e2e]\nkey = \"{client_key}\"\n"),
    )
    .unwrap();
    fs::write(
        dir.path().join("client.toml"),
        format!("server = \"127.0.0.1:{server_port}\"\nserver_key = \"{server_key}\"\nname = \"e2e\"\n\n[expose.web]\nlocal = \"127.0.0.1:8080\"\nallow = [{}]\n", allow.join(", ")),
    )
    .unwrap();
    let _server = Proc(
        hawse()
            .args(["server", "--config"])
            .arg(dir.path().join("server.toml"))
            .spawn()
            .unwrap(),
    );
    let out = hawse()
        .args(["client", "--config"])
        .arg(dir.path().join("client.toml"))
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("cannot be retried"), "{stderr}");
    assert_eq!(out.status.code(), Some(2), "{stderr}");
}

#[test]
fn a_client_warns_at_startup_about_a_quic_setting_under_prefer_tcp() {
    let dir = tempfile::tempdir().unwrap();
    let server_key = keygen(&dir.path().join("server.key"));
    let config = dir.path().join("client.toml");
    fs::write(
        &config,
        format!("server = \"127.0.0.1:{}\"\nserver_key = \"{server_key}\"\n\n[transport]\nprefer = \"tcp\"\nstream_window = \"1MiB\"\n", free_tcp_port()),
    )
    .unwrap();
    let mut client = Proc(
        hawse()
            .args(["client", "--config"])
            .arg(&config)
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stderr = BufReader::new(client.0.stderr.take().unwrap());
    // A read of the pipe blocks for as long as the client says nothing, so the lines come through
    // a channel, which can be waited on with a deadline.
    let (tx, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in stderr.lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut startup = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let line = lines
            .recv_timeout(left)
            .unwrap_or_else(|err| panic!("no warning, {err}: {startup:#?}"));
        if line.contains("WARN") && line.contains("stream_window") {
            break;
        }
        // Nothing listens on the port, so the first dial is refused at once, and by then startup
        // is over.
        assert!(
            !line.contains("disconnected"),
            "the first dial ended with no warning before it: {startup:#?}"
        );
        startup.push(line);
    }
}

#[test]
fn a_client_whose_stderr_reader_went_away_stops_on_sigterm() {
    let dir = tempfile::tempdir().unwrap();
    let server_key = keygen(&dir.path().join("server.key"));
    let config = dir.path().join("client.toml");
    fs::write(
        &config,
        format!("server = \"127.0.0.1:{}\"\nserver_key = \"{server_key}\"\n\n[transport]\nprefer = \"tcp\"\n", free_tcp_port()),
    )
    .unwrap();
    let mut client = Proc(
        hawse()
            .args(["client", "--config"])
            .arg(&config)
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut stderr = BufReader::new(client.0.stderr.take().unwrap());
    // Nothing listens on the port, so the first dial is refused at once. By then the client has
    // installed its signal handler, and a SIGTERM before that would end it the default way.
    // A read of the pipe blocks for as long as the client says nothing, so a thread does the
    // reading and hands the pipe back, which can be waited on with a deadline.
    let (tx, started) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        while !line.contains("disconnected") {
            line.clear();
            if stderr.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
        }
        let _ = tx.send(stderr);
    });
    let stderr = started
        .recv_timeout(Duration::from_secs(15))
        .expect("the client logs its first disconnect");
    // From here every log write fails, the one on the way out included.
    drop(stderr);
    let killed = Command::new("kill")
        .args(["-TERM", &client.0.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = client.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "still running 5 s after SIGTERM");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(status.code(), Some(0));
}
