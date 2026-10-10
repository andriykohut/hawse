//! What a client run by hand prints on a terminal, in place of its log lines.

use std::io::Write as _;

use hawse_core::client::Event;
use hawse_core::config::{ClientConfig, split_host_port};
use hawse_proto::port::{Kind, PortRequest};

use crate::commands::client::reason;

#[derive(Clone, Copy, Debug)]
pub struct Style {
    pub color: bool,
    pub unicode: bool,
}

impl Style {
    /// Unicode marks where the locale says the terminal reads UTF-8, which the first of these
    /// that is set decides.
    pub fn detect(color: bool) -> Self {
        let locale = ["LC_ALL", "LC_CTYPE", "LANG"]
            .into_iter()
            .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
            .unwrap_or_default()
            .to_ascii_lowercase();
        Self {
            color,
            unicode: locale.contains("utf-8") || locale.contains("utf8"),
        }
    }

    fn pick(self, unicode: &'static str, ascii: &'static str) -> &'static str {
        if self.unicode { unicode } else { ascii }
    }

    fn dim(self, text: &str) -> String {
        if self.color {
            format!("\x1b[2m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }
}

/// The widest a service's public port can come out, known before the server has answered.
fn port_width(request: PortRequest) -> usize {
    match request {
        PortRequest::Fixed(port) => port.to_string().len(),
        PortRequest::Any(Kind::Tcp) => "65535".len(),
        PortRequest::Any(Kind::Udp) => "65535/udp".len(),
    }
}

/// What `event` reads as. The columns are sized from `cfg`, so a row lines up with the ones
/// before it whatever order the server answers in.
pub fn event(event: &Event, cfg: &ClientConfig, style: Style) -> String {
    let (ok, no, again) = (
        style.pick("✓", "+"),
        style.pick("✗", "x"),
        style.pick("↻", "~"),
    );
    let dot = style.dim(style.pick("·", "|"));
    let names = cfg.expose.keys().map(String::len).max().unwrap_or(0);
    match event {
        Event::Connected { transport, .. } => {
            let transport = transport.to_string().to_uppercase();
            format!(
                "\n  hawse client  {dot}  {}  {dot}  {transport}\n",
                cfg.server
            )
        }
        Event::Bound { service, port } => {
            let host = split_host_port(&cfg.server)
                .map(|(host, _)| host)
                .unwrap_or_default();
            let ports = cfg.expose.values().map(|e| port_width(e.port)).max();
            let width = host.len() + 1 + ports.unwrap_or(0);
            let local = cfg.expose.get(service).map_or("", |e| e.local.as_str());
            let from = style.dim(&format!("{}  {local}", style.pick("←", "<-")));
            format!(
                "  {ok} {service:names$}  {:width$}  {from}",
                format!("{host}:{port}")
            )
        }
        Event::BindFailed { service, reason } => format!("  {no} {service:names$}  {reason}"),
        Event::Denied { key } => format!(
            "  {no} not authorized. on the server, add to server.toml:\n\n      [clients.NAME]\n      key = \"{key}\"\n\n    then wait for the next retry"
        ),
        Event::Disconnected { cause, retry_in } => {
            let reason = reason(cause.clone());
            match retry_in {
                Some(wait) => {
                    let retry = format!("retrying in {:.1} s", wait.as_secs_f64());
                    format!("  {again} {reason} {dot} {}", style.dim(&retry))
                }
                None => format!("  {no} {reason}; the config cannot be retried"),
            }
        }
    }
}

/// Whether `event` is a denial already on the screen: a denied key is retried for as long as the
/// client runs, and the table to paste is the same each time. `denied` carries that from one
/// event to the next.
pub fn repeats(denied: &mut bool, event: &Event) -> bool {
    let again = *denied && matches!(event, Event::Denied { .. });
    *denied = match event {
        Event::Denied { .. } => true,
        Event::Connected { .. } => false,
        _ => *denied,
    };
    again
}

/// Something that happened to the client which is not the server's doing, a reload for one.
pub fn note(text: &str, style: Style) -> String {
    format!("  {} {text}", style.pick("↻", "~"))
}

/// A line that cannot be written is dropped: `eprintln!` would panic on a stderr whose reader has
/// gone, and take the task that answers SIGTERM with it.
pub fn print(line: &str) {
    let _ = writeln!(std::io::stderr(), "{line}");
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use hawse_core::client::DisconnectCause;
    use hawse_core::transport::TransportKind;
    use hawse_proto::msg::BindFailure;
    use hawse_proto::port::Port;

    use super::*;
    use crate::config_file::parse;

    fn session(style: Style) -> String {
        let cfg: ClientConfig = parse(
            std::path::Path::new("client.toml"),
            r#"
server = "tunnel.example.com:4433"
server_key = "ed25519:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"

[expose.ssh]
local = "127.0.0.1:22"
port = 2222

[expose.dev]
local = "127.0.0.1:3000"

[expose.wireguard]
local = "127.0.0.1:51820"
port = "51820/udp"
"#,
        )
        .unwrap();
        let bound = |service: &str, number, kind| Event::Bound {
            service: service.to_owned(),
            port: Port { number, kind },
        };
        let events = [
            Event::Connected {
                remote: "203.0.113.7:4433".parse().unwrap(),
                transport: TransportKind::Quic,
                name: "laptop".to_owned(),
                agent: "hawse/0.6.0".to_owned(),
            },
            bound("ssh", 2222, Kind::Tcp),
            bound("dev", 40017, Kind::Tcp),
            Event::BindFailed {
                service: "wireguard".to_owned(),
                reason: BindFailure::NotGranted,
            },
            Event::Disconnected {
                cause: DisconnectCause::Unresponsive,
                retry_in: Some(Duration::from_secs(4)),
            },
            Event::Denied {
                key: cfg.server_key,
            },
            Event::Disconnected {
                cause: DisconnectCause::Config("server `` must be host or host:port".to_owned()),
                retry_in: None,
            },
        ];
        let mut out: Vec<String> = events.iter().map(|e| event(e, &cfg, style)).collect();
        out.push(note("config reloaded", style));
        out.join("\n")
    }

    #[test]
    fn a_session_reads_as_rows_under_a_header() {
        insta::assert_snapshot!(session(Style {
            color: false,
            unicode: true
        }));
    }

    #[test]
    fn a_session_without_unicode_keeps_to_ascii() {
        let out = session(Style {
            color: false,
            unicode: false,
        });
        assert!(out.is_ascii(), "{out}");
        insta::assert_snapshot!(out);
    }

    #[test]
    fn a_denial_is_shown_once_until_the_server_lets_the_client_in() {
        let key = "ed25519:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let denial = Event::Denied {
            key: key.parse().unwrap(),
        };
        let retry = Event::Disconnected {
            cause: DisconnectCause::Denied,
            retry_in: Some(Duration::from_secs(1)),
        };
        let connected = Event::Connected {
            remote: "203.0.113.7:4433".parse().unwrap(),
            transport: TransportKind::Tcp,
            name: "laptop".to_owned(),
            agent: "hawse/0.6.0".to_owned(),
        };
        let mut denied = false;
        let seen: Vec<bool> = [&denial, &retry, &denial, &connected, &denial]
            .into_iter()
            .map(|event| repeats(&mut denied, event))
            .collect();
        assert_eq!(seen, [false, false, true, false, false]);
    }

    #[test]
    fn colour_only_dims() {
        let out = session(Style {
            color: true,
            unicode: true,
        });
        let plain = out.replace("\x1b[2m", "").replace("\x1b[0m", "");
        assert!(out.len() > plain.len(), "nothing was dimmed");
        assert!(
            !plain.contains('\x1b'),
            "an escape other than dim: {plain:?}"
        );
    }
}
