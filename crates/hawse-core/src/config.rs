use std::collections::BTreeMap;
use std::net::SocketAddr;
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
}

impl ServerConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.listen.port() == 0 {
            return Err(ConfigError::ListenPort);
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
