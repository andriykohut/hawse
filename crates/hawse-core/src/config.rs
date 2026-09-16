use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use hawse_proto::key::PublicKey;
use hawse_proto::name::{self, NameError};
use hawse_proto::port::{PortRange, PortRequest, PortSpan};
use ipnet::IpNet;
use serde::Deserialize;

pub mod units;

use units::ByteSize;

pub const DEFAULT_PORT: u16 = 4433;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServerConfig {
    pub listen: SocketAddr,
    pub bind: IpAddr,
    pub key: PathBuf,
    pub dynamic_ports: PortSpan,
    pub quic_retry: bool,
    pub limits: Limits,
    pub transport: ServerTransport,
    pub clients: BTreeMap<String, ClientPolicy>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([0u16; 8], DEFAULT_PORT)),
            bind: IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            key: PathBuf::from("server.key"),
            dynamic_ports: PortSpan {
                first: 40000,
                last: 41000,
            },
            quic_retry: false,
            limits: Limits::default(),
            transport: ServerTransport::default(),
            clients: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientPolicy {
    pub key: PublicKey,
    #[serde(default)]
    pub ports: Vec<PortRange>,
    /// Overrides the server-wide `bind` for this client's public ports.
    #[serde(default)]
    pub bind: Option<IpAddr>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Limits {
    pub auth_failures_per_minute: u32,
    pub streams_per_client: u32,
    pub udp_sessions_per_service: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            auth_failures_per_minute: 30,
            streams_per_client: 4096,
            udp_sessions_per_service: 4096,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Congestion {
    Cubic,
    Bbr,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServerTransport {
    #[serde(with = "units::duration")]
    pub idle_timeout: Duration,
    pub stream_window: ByteSize,
    pub connection_window: ByteSize,
    pub congestion: Congestion,
    pub buffer: ByteSize,
}

impl Default for ServerTransport {
    fn default() -> Self {
        Self {
            idle_timeout: Duration::from_secs(30),
            stream_window: ByteSize(8 << 20),
            connection_window: ByteSize(64 << 20),
            congestion: Congestion::Cubic,
            buffer: ByteSize(16 << 10),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientConfig {
    pub server: String,
    pub server_key: PublicKey,
    #[serde(default = "default_client_key")]
    pub key: PathBuf,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub expose: BTreeMap<String, Expose>,
    #[serde(default)]
    pub transport: ClientTransport,
}

fn default_client_key() -> PathBuf {
    PathBuf::from("client.key")
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Expose {
    pub local: String,
    #[serde(default)]
    pub port: PortRequest,
    #[serde(default)]
    pub allow: Vec<IpNet>,
    #[serde(default)]
    pub proxy_protocol: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Prefer {
    Auto,
    Quic,
    Tcp,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ClientTransport {
    pub prefer: Prefer,
    pub congestion: Congestion,
    #[serde(with = "units::duration")]
    pub idle_timeout: Duration,
    pub stream_window: ByteSize,
    pub connection_window: ByteSize,
    pub buffer: ByteSize,
}

impl Default for ClientTransport {
    fn default() -> Self {
        Self {
            prefer: Prefer::Auto,
            congestion: Congestion::Cubic,
            idle_timeout: Duration::from_secs(30),
            stream_window: ByteSize(2 << 20),
            connection_window: ByteSize(16 << 20),
            buffer: ByteSize(16 << 10),
        }
    }
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("client `{0}`: {1}")]
    ClientName(String, NameError),
    #[error("clients `{0}` and `{1}` have the same key")]
    DuplicateKey(String, String),
    #[error("expose `{0}`: {1}")]
    ServiceName(String, NameError),
    #[error("expose `{0}`: local `{1}` must be host:port")]
    LocalAddr(String, String),
    #[error("server `{0}` must be host or host:port")]
    ServerAddr(String),
    #[error("name: {0}")]
    Name(NameError),
    #[error("listen port cannot be 0")]
    ListenPort,
    #[error("listen port {0} falls inside dynamic_ports; move the pool or the port")]
    ListenInPool(u16),
    #[error("client `{0}` is granted the listen port {1}; a grant cannot name it")]
    ListenGranted(String, u16),
    #[error("transport buffer must be between 1 KiB and 1 GiB, not {0} bytes")]
    Buffer(u64),
    #[error("transport stream_window must be at least 1 byte and under 4 GiB, not {0} bytes")]
    StreamWindow(u64),
    #[error("transport connection_window cannot be 0")]
    ConnectionWindow,
    #[error("transport stream_window of {0} bytes is larger than connection_window of {1} bytes")]
    Windows(u64, u64),
    #[error(
        "transport idle_timeout must be at least 1s, not {0:?}; the TCP fallback sets its keepalive in whole seconds, and anything shorter rounds down to a zero the kernel rejects"
    )]
    IdleTimeout(Duration),
    #[error("limits streams_per_client cannot be 0")]
    StreamsPerClient,
}

fn validate_transport(
    buffer: ByteSize,
    stream_window: ByteSize,
    connection_window: ByteSize,
    idle_timeout: Duration,
) -> Result<(), ConfigError> {
    // A zero buffer reads nothing at all: `BytesMut` never gains capacity and the pump takes the
    // empty read for end of stream.
    if !(1024..=1 << 30).contains(&buffer.0) {
        return Err(ConfigError::Buffer(buffer.0));
    }
    if idle_timeout < Duration::from_secs(1) {
        return Err(ConfigError::IdleTimeout(idle_timeout));
    }
    if stream_window.0 == 0 || u32::try_from(stream_window.0).is_err() {
        return Err(ConfigError::StreamWindow(stream_window.0));
    }
    if connection_window.0 == 0 {
        return Err(ConfigError::ConnectionWindow);
    }
    if stream_window.0 > connection_window.0 {
        return Err(ConfigError::Windows(stream_window.0, connection_window.0));
    }
    Ok(())
}

impl ServerConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.listen.port() == 0 {
            return Err(ConfigError::ListenPort);
        }
        let listen = self.listen.port();
        if self.dynamic_ports.contains_number(listen) {
            return Err(ConfigError::ListenInPool(listen));
        }
        for (name, policy) in &self.clients {
            if policy
                .ports
                .iter()
                .any(|r| r.first <= listen && listen <= r.last)
            {
                return Err(ConfigError::ListenGranted(name.clone(), listen));
            }
        }
        validate_transport(
            self.transport.buffer,
            self.transport.stream_window,
            self.transport.connection_window,
            self.transport.idle_timeout,
        )?;
        // A zero budget is not "no limit": yamux answers the first stream over the cap by tearing
        // the connection down.
        if self.limits.streams_per_client == 0 {
            return Err(ConfigError::StreamsPerClient);
        }
        let mut seen: BTreeMap<PublicKey, &str> = BTreeMap::new();
        for (client, policy) in &self.clients {
            name::validate(client).map_err(|e| ConfigError::ClientName(client.clone(), e))?;
            if let Some(other) = seen.insert(policy.key, client) {
                return Err(ConfigError::DuplicateKey(other.to_owned(), client.clone()));
            }
        }
        Ok(())
    }
}

impl ClientConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        match split_host_port(&self.server) {
            Some(_) => {}
            None => return Err(ConfigError::ServerAddr(self.server.clone())),
        }
        if let Some(name) = &self.name {
            name::validate(name).map_err(ConfigError::Name)?;
        }
        validate_transport(
            self.transport.buffer,
            self.transport.stream_window,
            self.transport.connection_window,
            self.transport.idle_timeout,
        )?;
        for (service, expose) in &self.expose {
            name::validate(service).map_err(|e| ConfigError::ServiceName(service.clone(), e))?;
            match split_host_port(&expose.local) {
                Some((_, Some(_))) => {}
                _ => {
                    return Err(ConfigError::LocalAddr(
                        service.clone(),
                        expose.local.clone(),
                    ));
                }
            }
        }
        Ok(())
    }
}

/// `None` port means the text had no port; a port of 0 or an empty host is rejected.
pub fn split_host_port(text: &str) -> Option<(String, Option<u16>)> {
    if text.is_empty() {
        return None;
    }
    if let Some(rest) = text.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        if host.is_empty() {
            return None;
        }
        return match tail {
            "" => Some((host.to_owned(), None)),
            tail => Some((host.to_owned(), Some(parse_port(tail.strip_prefix(':')?)?))),
        };
    }
    if text.matches(':').count() > 1 {
        return Some((text.to_owned(), None));
    }
    match text.split_once(':') {
        None => Some((text.to_owned(), None)),
        Some((host, port)) => {
            if host.is_empty() {
                return None;
            }
            Some((host.to_owned(), Some(parse_port(port)?)))
        }
    }
}

fn parse_port(text: &str) -> Option<u16> {
    text.parse::<u16>().ok().filter(|p| *p != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hawse_proto::port::{Kind, Port};
    use std::net::Ipv4Addr;
    use std::time::Duration;

    const SERVER: &str = r#"
listen = "0.0.0.0:4433"
key = "server.key"
dynamic_ports = "40000-41000"

[limits]
auth_failures_per_minute = 10

[transport]
idle_timeout = "20s"
stream_window = "4MiB"
congestion = "bbr"

[clients.homelab]
key = "ed25519:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
ports = ["443", "2222", "51820/udp", "8000-8100"]

[clients.laptop]
key = "ed25519:BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBA"
"#;

    const CLIENT: &str = r#"
server = "tunnel.example.com:4433"
server_key = "ed25519:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
name = "homelab"

[expose.web]
local = "127.0.0.1:8443"
port = 443

[expose.ssh]
local = "127.0.0.1:22"
port = 2222
allow = ["203.0.113.0/24"]
proxy_protocol = true

[expose.wireguard]
local = "127.0.0.1:51820"
port = "51820/udp"

[expose.dev]
local = "localhost:3000"

[transport]
prefer = "tcp"
"#;

    fn sample_key() -> PublicKey {
        PublicKey::from_bytes([7u8; 32])
    }

    #[test]
    fn bind_defaults_to_every_interface_and_a_client_can_override_it() {
        let cfg: ServerConfig = toml::from_str(SERVER).unwrap();
        assert_eq!(cfg.bind, IpAddr::V6(Ipv6Addr::UNSPECIFIED));
        assert_eq!(cfg.clients["homelab"].bind, None);

        let text = format!("bind = \"127.0.0.1\"\n{SERVER}");
        let cfg: ServerConfig = toml::from_str(&text).unwrap();
        assert_eq!(cfg.bind, IpAddr::V4(Ipv4Addr::LOCALHOST));

        let text = format!("{SERVER}bind = \"127.0.0.1\"\n");
        let cfg: ServerConfig = toml::from_str(&text).unwrap();
        assert_eq!(cfg.bind, IpAddr::V6(Ipv6Addr::UNSPECIFIED));
        assert_eq!(
            cfg.clients["laptop"].bind,
            Some(IpAddr::V4(Ipv4Addr::LOCALHOST))
        );
    }

    #[test]
    fn server_example_parses() {
        let cfg: ServerConfig = toml::from_str(SERVER).unwrap();
        assert_eq!(cfg.listen, "0.0.0.0:4433".parse().unwrap());
        assert_eq!(
            cfg.dynamic_ports,
            PortSpan {
                first: 40000,
                last: 41000
            }
        );
        assert_eq!(cfg.limits.auth_failures_per_minute, 10);
        assert_eq!(cfg.limits.streams_per_client, 4096);
        assert_eq!(cfg.transport.idle_timeout, Duration::from_secs(20));
        assert_eq!(cfg.transport.stream_window, units::ByteSize(4 << 20));
        assert_eq!(cfg.transport.connection_window, units::ByteSize(64 << 20));
        assert_eq!(cfg.transport.congestion, Congestion::Bbr);
        assert_eq!(cfg.clients.len(), 2);
        assert_eq!(cfg.clients["homelab"].ports.len(), 4);
        assert!(cfg.clients["laptop"].ports.is_empty());
        cfg.validate().unwrap();
    }

    #[test]
    fn empty_server_config_is_all_defaults() {
        let cfg: ServerConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.listen, "[::]:4433".parse().unwrap());
        assert_eq!(cfg.key, PathBuf::from("server.key"));
        assert_eq!(
            cfg.dynamic_ports,
            PortSpan {
                first: 40000,
                last: 41000
            }
        );
        assert!(!cfg.quic_retry);
        assert_eq!(cfg.transport.buffer, units::ByteSize(16 * 1024));
        assert!(cfg.clients.is_empty());
        cfg.validate().unwrap();
    }

    #[test]
    fn unknown_fields_are_rejected() {
        assert!(toml::from_str::<ServerConfig>("listne = \"[::]:1\"").is_err());
        assert!(toml::from_str::<ClientConfig>("server = \"x\"\nserver_key = \"ed25519:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\"\nbogus = 1").is_err());
    }

    #[test]
    fn client_example_parses() {
        let cfg: ClientConfig = toml::from_str(CLIENT).unwrap();
        assert_eq!(cfg.name.as_deref(), Some("homelab"));
        assert_eq!(cfg.key, PathBuf::from("client.key"));
        assert_eq!(cfg.transport.prefer, Prefer::Tcp);
        assert_eq!(cfg.transport.stream_window, units::ByteSize(2 << 20));
        assert_eq!(
            cfg.expose["web"].port,
            PortRequest::Fixed(Port {
                number: 443,
                kind: Kind::Tcp
            })
        );
        assert_eq!(
            cfg.expose["wireguard"].port,
            PortRequest::Fixed(Port {
                number: 51820,
                kind: Kind::Udp
            })
        );
        assert_eq!(cfg.expose["dev"].port, PortRequest::Any(Kind::Tcp));
        assert!(cfg.expose["ssh"].proxy_protocol);
        assert_eq!(cfg.expose["ssh"].allow.len(), 1);
        cfg.validate().unwrap();
    }

    #[test]
    fn listen_port_inside_the_dynamic_pool_is_rejected() {
        let cfg = ServerConfig {
            listen: "127.0.0.1:40500".parse().unwrap(),
            dynamic_ports: "40000-41000".parse().unwrap(),
            ..Default::default()
        };
        assert_eq!(cfg.validate(), Err(ConfigError::ListenInPool(40500)));
    }

    #[test]
    fn a_grant_naming_the_listen_port_is_rejected() {
        let mut cfg = ServerConfig {
            listen: "127.0.0.1:4433".parse().unwrap(),
            ..Default::default()
        };
        cfg.clients.insert(
            "laptop".into(),
            ClientPolicy {
                key: sample_key(),
                ports: vec!["4433".parse().unwrap()],
                bind: None,
            },
        );
        assert_eq!(
            cfg.validate(),
            Err(ConfigError::ListenGranted("laptop".into(), 4433))
        );
    }

    #[test]
    fn validation_catches_names_keys_and_addresses() {
        let mut cfg: ServerConfig = toml::from_str(SERVER).unwrap();
        let dup = cfg.clients["homelab"].clone();
        cfg.clients.insert("copy".into(), dup);
        assert!(matches!(
            cfg.validate(),
            Err(ConfigError::DuplicateKey(_, _))
        ));

        let mut cfg: ServerConfig = toml::from_str(SERVER).unwrap();
        let entry = cfg.clients.remove("laptop").unwrap();
        cfg.clients.insert("Bad Name".into(), entry);
        assert!(matches!(cfg.validate(), Err(ConfigError::ClientName(_, _))));

        let mut cfg: ClientConfig = toml::from_str(CLIENT).unwrap();
        cfg.expose.get_mut("dev").unwrap().local = "localhost".into();
        assert!(matches!(cfg.validate(), Err(ConfigError::LocalAddr(_, _))));

        let mut cfg: ClientConfig = toml::from_str(CLIENT).unwrap();
        cfg.server = ":4433".into();
        assert!(matches!(cfg.validate(), Err(ConfigError::ServerAddr(_))));

        let mut cfg: ClientConfig = toml::from_str(CLIENT).unwrap();
        cfg.name = Some("Home Lab".into());
        assert!(matches!(cfg.validate(), Err(ConfigError::Name(_))));
    }

    #[test]
    fn a_zero_buffer_is_rejected() {
        let mut cfg: ServerConfig = toml::from_str(SERVER).unwrap();
        cfg.transport.buffer = ByteSize(0);
        assert_eq!(cfg.validate(), Err(ConfigError::Buffer(0)));

        let mut cfg: ClientConfig = toml::from_str(CLIENT).unwrap();
        cfg.transport.buffer = ByteSize(0);
        assert_eq!(cfg.validate(), Err(ConfigError::Buffer(0)));
    }

    #[test]
    fn a_stream_window_larger_than_the_connection_window_is_rejected() {
        let mut cfg: ServerConfig = toml::from_str(SERVER).unwrap();
        cfg.transport.stream_window = ByteSize(2 << 20);
        cfg.transport.connection_window = ByteSize(1 << 20);
        assert_eq!(cfg.validate(), Err(ConfigError::Windows(2 << 20, 1 << 20)));

        let mut cfg: ClientConfig = toml::from_str(CLIENT).unwrap();
        cfg.transport.stream_window = ByteSize(2 << 20);
        cfg.transport.connection_window = ByteSize(1 << 20);
        assert_eq!(cfg.validate(), Err(ConfigError::Windows(2 << 20, 1 << 20)));
    }

    #[test]
    fn a_sub_second_idle_timeout_is_rejected() {
        let short = Duration::from_millis(500);

        let mut cfg: ServerConfig = toml::from_str(SERVER).unwrap();
        cfg.transport.idle_timeout = short;
        assert_eq!(cfg.validate(), Err(ConfigError::IdleTimeout(short)));

        let mut cfg: ClientConfig = toml::from_str(CLIENT).unwrap();
        cfg.transport.idle_timeout = short;
        assert_eq!(cfg.validate(), Err(ConfigError::IdleTimeout(short)));
    }

    #[test]
    fn a_zero_stream_budget_is_rejected() {
        let mut cfg: ServerConfig = toml::from_str(SERVER).unwrap();
        cfg.limits.streams_per_client = 0;
        assert_eq!(cfg.validate(), Err(ConfigError::StreamsPerClient));
    }

    #[test]
    fn splits_host_and_port() {
        assert_eq!(
            split_host_port("example.com:4433"),
            Some(("example.com".into(), Some(4433)))
        );
        assert_eq!(
            split_host_port("example.com"),
            Some(("example.com".into(), None))
        );
        assert_eq!(
            split_host_port("[::1]:4433"),
            Some(("::1".into(), Some(4433)))
        );
        assert_eq!(split_host_port("[::1]"), Some(("::1".into(), None)));
        assert_eq!(split_host_port("::1"), Some(("::1".into(), None)));
        assert_eq!(
            split_host_port("10.0.0.1:22"),
            Some(("10.0.0.1".into(), Some(22)))
        );
        assert_eq!(split_host_port(":4433"), None);
        assert_eq!(split_host_port("host:"), None);
        assert_eq!(split_host_port("host:0"), None);
        assert_eq!(split_host_port(""), None);
    }
}
