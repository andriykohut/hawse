//! What hawse tells systemd when a `Type=notify` unit runs it: that it is ready, and the status
//! line `systemctl status` shows, kept to how the server or the client stands. systemd names the
//! socket to tell in `$NOTIFY_SOCKET`; without it nothing is sent and nothing is kept.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::net::SocketAddr;
use std::os::unix::net::UnixDatagram;

use hawse_core::client::Event;
use hawse_core::config::ClientConfig;
use hawse_core::server;
use hawse_core::transport::TransportKind;
use tokio::sync::broadcast::{self, error::RecvError};

use crate::commands::client::reason;

/// Where systemd listens for this service's state, `None` outside a `Type=notify` unit.
pub fn socket() -> Option<OsString> {
    std::env::var_os("NOTIFY_SOCKET")
}

/// Sends `state`, `KEY=value` lines, to `socket`: a path, or on Linux an abstract name after
/// `@`. A failure is logged and goes no further, since systemd then only shows a stale status or
/// times out the start.
pub fn send(socket: &OsStr, state: &str) {
    if let Err(err) = try_send(socket, state) {
        tracing::warn!(socket = ?socket, %err, "cannot tell systemd how the service stands");
    }
}

fn try_send(socket: &OsStr, state: &str) -> io::Result<()> {
    let sender = UnixDatagram::unbound()?;
    #[cfg(target_os = "linux")]
    if let Some(name) = socket.as_encoded_bytes().strip_prefix(b"@") {
        use std::os::linux::net::SocketAddrExt as _;
        let addr = std::os::unix::net::SocketAddr::from_abstract_name(name)?;
        sender.send_to_addr(state.as_bytes(), &addr)?;
        return Ok(());
    }
    sender.send_to(state.as_bytes(), socket)?;
    Ok(())
}

/// The status line last sent, so that only a change is sent again.
pub struct Status {
    socket: OsString,
    sent: String,
}

impl Status {
    /// Tells systemd the service is ready, with `line` as its status.
    pub fn ready(socket: OsString, line: String) -> Self {
        send(&socket, &format!("READY=1\nSTATUS={line}"));
        Self { socket, sent: line }
    }

    pub fn show(&mut self, line: String) {
        if line != self.sent {
            send(&self.socket, &format!("STATUS={line}"));
            self.sent = line;
        }
    }
}

/// How a client stands, as its status line says it, kept from its events.
#[derive(Default)]
pub struct Client {
    link: Link,
    /// Whether the server bound each service the session asked for, by name, since the last
    /// connect.
    answers: BTreeMap<String, bool>,
}

#[derive(Default)]
enum Link {
    #[default]
    Connecting,
    Up(TransportKind),
    Down(String),
    Stopped(String),
}

impl Client {
    pub fn apply(&mut self, event: &Event) {
        match event {
            Event::Connected { transport, .. } => {
                self.link = Link::Up(*transport);
                self.answers.clear();
            }
            Event::Bound { service, .. } => {
                self.answers.insert(service.clone(), true);
            }
            Event::BindFailed { service, .. } => {
                self.answers.insert(service.clone(), false);
            }
            Event::Disconnected { cause, retry_in } => {
                let why = reason(cause.clone());
                self.link = match retry_in {
                    Some(_) => Link::Down(why),
                    None => Link::Stopped(why),
                };
            }
            Event::Denied { .. } | Event::RoundTrip { .. } => {}
        }
    }

    /// The status line, counting only the services `cfg` still exposes.
    pub fn line(&self, cfg: &ClientConfig) -> String {
        match &self.link {
            Link::Connecting => "connecting".to_owned(),
            Link::Up(transport) => {
                let up = format!("connected over {}", transport.to_string().to_uppercase());
                let answered: Vec<bool> = self
                    .answers
                    .iter()
                    .filter(|(service, _)| cfg.expose.contains_key(*service))
                    .map(|(_, bound)| *bound)
                    .collect();
                let bound = answered.iter().filter(|bound| **bound).count();
                let noun = if answered.len() == 1 {
                    "service"
                } else {
                    "services"
                };
                match answered.len() {
                    0 => up,
                    all if all == bound => format!("{up}, {all} {noun} bound"),
                    all => format!("{up}, {bound} of {all} {noun} bound"),
                }
            }
            Link::Down(why) => format!("disconnected: {why}, retrying"),
            Link::Stopped(why) => format!("stopped: {why}"),
        }
    }
}

/// The clients connected to a server, by name, as its status line says them.
pub struct Server {
    listen: SocketAddr,
    /// Sessions per client: a client's next session can connect before its last has ended.
    sessions: BTreeMap<String, usize>,
}

impl Server {
    pub fn new(listen: SocketAddr) -> Self {
        Self {
            listen,
            sessions: BTreeMap::new(),
        }
    }

    pub fn apply(&mut self, event: &server::Event) {
        match event {
            server::Event::Connected { client, .. } => {
                *self.sessions.entry(client.clone()).or_default() += 1;
            }
            server::Event::Ended { client, .. } => {
                if let Some(left) = self.sessions.get_mut(client) {
                    *left -= 1;
                    if *left == 0 {
                        self.sessions.remove(client);
                    }
                }
            }
            server::Event::Bound { .. }
            | server::Event::Unbound { .. }
            | server::Event::Denied { .. } => {}
        }
    }

    /// The status line, naming the first three clients by name and counting the rest.
    pub fn line(&self) -> String {
        const NAMED: usize = 3;
        let clients = match self.sessions.len() {
            0 => "no clients connected".to_owned(),
            count => {
                let names: Vec<&str> = self
                    .sessions
                    .keys()
                    .take(NAMED)
                    .map(String::as_str)
                    .collect();
                let noun = if count == 1 { "client" } else { "clients" };
                let more = match count.saturating_sub(NAMED) {
                    0 => String::new(),
                    more => format!(" and {more} more"),
                };
                format!("{count} {noun}: {}{more}", names.join(", "))
            }
        };
        format!("listening on {}, {clients}", self.listen)
    }
}

/// Tells systemd the server listening on `listen` is ready, then keeps its status to the clients
/// `events` reports until the server stops.
pub async fn serve(
    socket: OsString,
    listen: SocketAddr,
    mut events: broadcast::Receiver<server::Event>,
) {
    let mut server = Server::new(listen);
    let mut status = Status::ready(socket, server.line());
    loop {
        match events.recv().await {
            Ok(event) => server.apply(&event),
            // shortcut: a lagged receiver misses connects and ends, and the names stay off until
            // the server restarts; 256 events behind is a server busier than this line is for.
            Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => return,
        }
        status.show(server.line());
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use hawse_core::client::DisconnectCause;
    use hawse_proto::msg::BindFailure;
    use hawse_proto::port::{Kind, Port};

    use super::*;
    use crate::config_file::parse;

    fn listener(dir: &tempfile::TempDir) -> (UnixDatagram, OsString) {
        let path = dir.path().join("notify");
        let socket = UnixDatagram::bind(&path).unwrap();
        socket.set_nonblocking(true).unwrap();
        (socket, path.into_os_string())
    }

    /// What `socket` has been sent, oldest first.
    fn heard(socket: &UnixDatagram) -> Vec<String> {
        let mut buf = [0u8; 512];
        let mut heard = Vec::new();
        while let Ok(len) = socket.recv(&mut buf) {
            heard.push(String::from_utf8(buf[..len].to_vec()).unwrap());
        }
        heard
    }

    #[test]
    fn a_status_is_sent_once_ready_and_again_only_when_it_changes() {
        let dir = tempfile::tempdir().unwrap();
        let (systemd, path) = listener(&dir);
        let mut status = Status::ready(path, "connecting".to_owned());
        status.show("connecting".to_owned());
        status.show("connected over QUIC".to_owned());
        assert_eq!(
            heard(&systemd),
            ["READY=1\nSTATUS=connecting", "STATUS=connected over QUIC"]
        );
    }

    #[test]
    fn a_socket_nothing_listens_on_costs_nothing_but_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        send(dir.path().join("gone").as_os_str(), "READY=1");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_abstract_socket_is_named_after_an_at() {
        use std::os::linux::net::SocketAddrExt as _;
        let name = format!("hawse-notify-test-{}", std::process::id());
        let addr = std::os::unix::net::SocketAddr::from_abstract_name(&name).unwrap();
        let systemd = UnixDatagram::bind_addr(&addr).unwrap();
        systemd.set_nonblocking(true).unwrap();
        send(OsStr::new(&format!("@{name}")), "READY=1");
        assert_eq!(heard(&systemd), ["READY=1"]);
    }

    fn cfg() -> ClientConfig {
        parse(
            std::path::Path::new("client.toml"),
            r#"
server = "tunnel.example.com:4433"
server_key = "ed25519:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"

[expose.ssh]
local = "127.0.0.1:22"
port = 2222

[expose.web]
local = "127.0.0.1:3000"

[expose.wireguard]
local = "127.0.0.1:51820"
port = "51820/udp"
"#,
        )
        .unwrap()
    }

    fn connected(transport: TransportKind) -> Event {
        Event::Connected {
            remote: "203.0.113.7:4433".parse().unwrap(),
            transport,
            name: "nas".to_owned(),
            agent: "hawse/0.8.0".to_owned(),
            round_trip: Duration::from_millis(24),
        }
    }

    fn bound(service: &str) -> Event {
        Event::Bound {
            service: service.to_owned(),
            port: Port {
                number: 40017,
                kind: Kind::Tcp,
            },
        }
    }

    #[test]
    fn a_client_says_how_it_is_connected_and_how_many_services_are_bound() {
        let mut cfg = cfg();
        let mut client = Client::default();
        assert_eq!(client.line(&cfg), "connecting");
        client.apply(&connected(TransportKind::Quic));
        assert_eq!(client.line(&cfg), "connected over QUIC");
        client.apply(&bound("ssh"));
        assert_eq!(client.line(&cfg), "connected over QUIC, 1 service bound");
        client.apply(&bound("web"));
        client.apply(&Event::BindFailed {
            service: "wireguard".to_owned(),
            reason: BindFailure::NotGranted,
        });
        assert_eq!(
            client.line(&cfg),
            "connected over QUIC, 2 of 3 services bound"
        );
        // A reload that takes a service away sends no event of its own.
        cfg.expose.remove("wireguard");
        assert_eq!(client.line(&cfg), "connected over QUIC, 2 services bound");

        client.apply(&Event::Disconnected {
            cause: DisconnectCause::Unresponsive,
            retry_in: Some(Duration::from_secs(8)),
        });
        assert_eq!(
            client.line(&cfg),
            "disconnected: server stopped answering, retrying"
        );
        // The next session starts its answers over.
        client.apply(&connected(TransportKind::Tcp));
        assert_eq!(client.line(&cfg), "connected over TCP");

        client.apply(&Event::Disconnected {
            cause: DisconnectCause::Config("no such service".to_owned()),
            retry_in: None,
        });
        assert_eq!(client.line(&cfg), "stopped: no such service");
    }

    fn session(client: &str) -> server::Event {
        server::Event::Connected {
            client: client.to_owned(),
            remote: "198.51.100.4:50000".parse().unwrap(),
            transport: TransportKind::Quic,
            agent: "hawse/0.8.0".to_owned(),
        }
    }

    fn ended(client: &str) -> server::Event {
        server::Event::Ended {
            client: client.to_owned(),
            reason: "client closed the connection",
        }
    }

    #[test]
    fn a_server_names_its_first_three_clients_and_counts_the_rest() {
        let mut server = Server::new("[::]:4433".parse().unwrap());
        assert_eq!(
            server.line(),
            "listening on [::]:4433, no clients connected"
        );
        server.apply(&session("nas"));
        assert_eq!(server.line(), "listening on [::]:4433, 1 client: nas");
        for name in ["office", "laptop", "phone", "desk"] {
            server.apply(&session(name));
        }
        assert_eq!(
            server.line(),
            "listening on [::]:4433, 5 clients: desk, laptop, nas and 2 more"
        );
        for name in ["office", "laptop", "phone", "desk"] {
            server.apply(&ended(name));
        }
        assert_eq!(server.line(), "listening on [::]:4433, 1 client: nas");
    }

    #[test]
    fn a_client_whose_last_session_ends_after_its_next_connects_stays_connected() {
        let mut server = Server::new("[::]:4433".parse().unwrap());
        server.apply(&session("nas"));
        server.apply(&session("nas"));
        server.apply(&ended("nas"));
        assert_eq!(server.line(), "listening on [::]:4433, 1 client: nas");
        server.apply(&ended("nas"));
        assert_eq!(
            server.line(),
            "listening on [::]:4433, no clients connected"
        );
    }
}
