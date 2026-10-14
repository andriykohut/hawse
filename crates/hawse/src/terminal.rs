//! What a server or a client run by hand prints on a terminal, in place of its log lines.

use std::io::Write as _;
use std::net::SocketAddr;
use std::time::Duration;

use hawse_core::client::Event;
use hawse_core::config::{ClientConfig, split_host_port};
use hawse_core::server;
use hawse_proto::key::PublicKey;
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

/// `round_trip` in whole milliseconds, with anything shorter than one said as that rather than
/// as none.
fn millis(round_trip: Duration) -> String {
    match round_trip.as_millis() {
        0 => "<1 ms".to_owned(),
        ms => format!("{ms} ms"),
    }
}

/// A rate in at most 6 columns: bytes under 1000, then K, M and G (binary), with one decimal
/// under 10. A figure that would round to 1000 of a unit reads as 1.0 of the next.
pub fn rate(per_second: u64) -> String {
    const UNITS: [&str; 3] = ["K", "M", "G"];
    if per_second < 1000 {
        return format!("{per_second}B/s");
    }
    // `per_second` in units of `scale`, to the nearest whole one and the nearest tenth.
    let whole = |scale: u128| (u128::from(per_second) * 2 + scale) / (scale * 2);
    let tenths = |scale: u128| (u128::from(per_second) * 20 + scale) / (scale * 2);
    let (mut scale, mut unit) = (1024u128, 0);
    while unit + 1 < UNITS.len() && whole(scale) >= 1000 {
        scale *= 1024;
        unit += 1;
    }
    let tenths = tenths(scale);
    if tenths < 100 {
        format!("{}.{}{}/s", tenths / 10, tenths % 10, UNITS[unit])
    } else {
        format!("{}{}/s", whole(scale), UNITS[unit])
    }
}

/// `elapsed` to the minute: `<1 min`, `14 min`, `2 h 14 m`, `3 d 2 h`.
pub fn span(elapsed: Duration) -> String {
    let minutes = elapsed.as_secs() / 60;
    match minutes {
        0 => "<1 min".to_owned(),
        1..60 => format!("{minutes} min"),
        60..1440 => format!("{} h {} m", minutes / 60, minutes % 60),
        _ => format!("{} d {} h", minutes / 1440, minutes % 1440 / 60),
    }
}

/// How many columns `text` takes on a terminal, leaving out the escapes `Style::dim` adds.
pub fn columns(text: &str) -> usize {
    let mut escape = false;
    text.chars()
        .filter(|&c| {
            if escape {
                escape = c != 'm';
                false
            } else {
                escape = c == '\x1b';
                !escape
            }
        })
        .count()
}

/// `fields` joined, dropping whole fields from the right until the line fits in `room` columns,
/// and cutting the first by characters when it does not fit alone, its dimming dropped with the
/// cut. A `room` of 0 is a terminal that does not say how wide it is, and cuts nothing.
pub fn fit(fields: &[String], room: usize) -> String {
    let mut line = String::new();
    for field in fields {
        if room > 0 && !line.is_empty() && columns(&line) + columns(field) > room {
            break;
        }
        line.push_str(field);
    }
    if room > 0 && columns(&line) > room {
        let plain: String = line.replace("\x1b[2m", "").replace("\x1b[0m", "");
        return plain.chars().take(room).collect();
    }
    line
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
        Event::Connected {
            transport,
            round_trip,
            ..
        } => {
            let transport = transport.to_string().to_uppercase();
            format!(
                "\n  hawse client  {dot}  {}  {dot}  {transport}  {dot}  {}\n",
                cfg.server,
                millis(*round_trip)
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
        Event::RoundTrip { .. } => {
            unreachable!("`report` prints nothing for a round trip")
        }
    }
}

/// Whether `event` is a denial already on the screen: a denied key is retried for as long as the
/// client runs, and the table to paste is the same each time. `shown` is the key the table was
/// last printed for, carried from one event to the next, so a reload onto another key that is
/// denied as well prints its own.
pub fn repeats(shown: &mut Option<PublicKey>, event: &Event) -> bool {
    match event {
        Event::Denied { key } => shown.replace(*key) == Some(*key),
        Event::Connected { .. } => {
            *shown = None;
            false
        }
        _ => false,
    }
}

/// What a server says once it is up: where it listens, its key, and the command a client joins
/// with. `unused` is a server no client is authorized on yet.
pub fn server_header(
    addr: SocketAddr,
    tcp: bool,
    key: PublicKey,
    unused: bool,
    style: Style,
) -> String {
    let dot = style.dim(style.pick("·", "|"));
    let transports = if tcp { "QUIC + TCP" } else { "QUIC" };
    let (key_is, join_is) = (style.dim("key "), style.dim("join"));
    let port = addr.port();
    let mut out = format!(
        "\n  hawse server  {dot}  {addr}  {dot}  {transports}\n  {key_is}  {key}\n  {join_is}  hawse join <this-host>:{port} --server-key {key}\n"
    );
    if unused {
        out.push_str("\n  no clients are authorized yet: one that joins is shown here with the table to add\n");
    }
    out
}

/// What `event` reads as, with clients' names padded to `names`.
pub fn served(event: &server::Event, names: usize, style: Style) -> String {
    use server::Event;
    let (ok, no, again) = (
        style.pick("✓", "+"),
        style.pick("✗", "x"),
        style.pick("↻", "~"),
    );
    match event {
        Event::Connected {
            client,
            remote,
            transport,
            ..
        } => {
            let dot = style.dim(style.pick("·", "|"));
            let transport = transport.to_string().to_uppercase();
            format!("  {ok} {client:names$}  connected from {remote} {dot} {transport}")
        }
        Event::Bound {
            client,
            service,
            bind,
            port,
        } => {
            let at = SocketAddr::new(*bind, port.number);
            let udp = if port.kind == Kind::Udp { "/udp" } else { "" };
            format!("  {ok} {client:names$}  {service}  {at}{udp}")
        }
        Event::Unbound {
            client,
            service,
            port,
        } => format!("  {again} {client:names$}  {service}  unbound from port {port}"),
        Event::Ended { client, reason } => {
            format!("  {again} {client:names$}  session ended: {reason}")
        }
        Event::Denied { key, remote } => format!(
            "  {no} unknown key from {remote}. to authorize it, add to server.toml:\n\n      [clients.NAME]\n      key = \"{key}\"\n"
        ),
    }
}

/// Whether `event` is a denial already on the screen, as `repeats` says for a client: the client
/// a server denies knocks again for as long as it runs.
// ponytail: remembers one key, so two unknown keys knocking in turn each print every time. A set
// of keys would stop that, and has to be bounded against strangers before it is kept.
pub fn knocks_again(shown: &mut Option<PublicKey>, event: &server::Event) -> bool {
    match event {
        server::Event::Denied { key, .. } => shown.replace(*key) == Some(*key),
        server::Event::Connected { .. } => {
            *shown = None;
            false
        }
        _ => false,
    }
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
    use hawse_core::client::DisconnectCause;
    use hawse_core::identity::Identity;
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
                round_trip: Duration::from_millis(24),
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
    fn a_round_trip_under_a_millisecond_is_not_shown_as_none() {
        assert_eq!(millis(Duration::from_micros(999)), "<1 ms");
        assert_eq!(millis(Duration::from_micros(1999)), "1 ms");
    }

    #[test]
    fn a_denial_is_shown_once_for_a_key_until_the_server_lets_the_client_in() {
        let denial = |identity: &Identity| Event::Denied {
            key: identity.public_key(),
        };
        let (first, second) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let retry = Event::Disconnected {
            cause: DisconnectCause::Denied,
            retry_in: Some(Duration::from_secs(1)),
        };
        let connected = Event::Connected {
            remote: "203.0.113.7:4433".parse().unwrap(),
            transport: TransportKind::Tcp,
            name: "laptop".to_owned(),
            agent: "hawse/0.6.0".to_owned(),
            round_trip: Duration::from_millis(24),
        };
        let events = [
            (denial(&first), false),
            (retry, false),
            (denial(&first), true),
            // A reload onto another key, with no connect in between.
            (denial(&second), false),
            (denial(&second), true),
            (connected, false),
            (denial(&second), false),
        ];
        let mut shown = None;
        for (n, (event, repeat)) in events.iter().enumerate() {
            assert_eq!(repeats(&mut shown, event), *repeat, "event {n}: {event:?}");
        }
    }

    fn server_session(style: Style) -> String {
        use server::Event;
        let key: PublicKey = "ed25519:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
            .parse()
            .unwrap();
        let client = || "laptop".to_owned();
        let port = |number, kind| Port { number, kind };
        let events = [
            Event::Denied {
                key,
                remote: "198.51.100.4:40122".parse().unwrap(),
            },
            Event::Connected {
                client: client(),
                remote: "203.0.113.9:51808".parse().unwrap(),
                transport: TransportKind::Quic,
                agent: "hawse/0.6.0".to_owned(),
            },
            Event::Bound {
                client: client(),
                service: "web".to_owned(),
                bind: "127.0.0.1".parse().unwrap(),
                port: port(8080, Kind::Tcp),
            },
            Event::Bound {
                client: "nas".to_owned(),
                service: "wireguard".to_owned(),
                bind: "::".parse().unwrap(),
                port: port(51820, Kind::Udp),
            },
            Event::Unbound {
                client: client(),
                service: "web".to_owned(),
                port: port(8080, Kind::Tcp),
            },
            Event::Ended {
                client: client(),
                reason: "peer left",
            },
        ];
        let addr = "[::]:4433".parse().unwrap();
        let mut out = vec![server_header(addr, true, key, true, style)];
        out.extend(events.iter().map(|e| served(e, "laptop".len(), style)));
        out.join("\n")
    }

    #[test]
    fn a_server_reads_as_rows_under_a_header() {
        insta::assert_snapshot!(server_session(Style {
            color: false,
            unicode: true
        }));
    }

    #[test]
    fn a_server_without_unicode_keeps_to_ascii() {
        let out = server_session(Style {
            color: false,
            unicode: false,
        });
        assert!(out.is_ascii(), "{out}");
        insta::assert_snapshot!(out);
    }

    #[test]
    fn an_unknown_key_is_shown_once_while_it_knocks() {
        use server::Event;
        let denial = |identity: &Identity| Event::Denied {
            key: identity.public_key(),
            remote: "198.51.100.4:40122".parse().unwrap(),
        };
        let (first, second) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let connected = Event::Connected {
            client: "laptop".to_owned(),
            remote: "203.0.113.9:51808".parse().unwrap(),
            transport: TransportKind::Quic,
            agent: "hawse/0.6.0".to_owned(),
        };
        let events = [
            (denial(&first), false),
            (denial(&first), true),
            (denial(&second), false),
            (connected, false),
            (denial(&second), false),
        ];
        let mut shown = None;
        for (n, (event, again)) in events.iter().enumerate() {
            assert_eq!(
                knocks_again(&mut shown, event),
                *again,
                "event {n}: {event:?}"
            );
        }
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

    #[test]
    fn a_rate_reads_in_at_most_six_columns() {
        let cases = [
            (0, "0B/s"),
            (999, "999B/s"),
            (1000, "1.0K/s"),
            (1023, "1.0K/s"),
            (2457, "2.4K/s"),
            (10_188, "9.9K/s"),
            (10_189, "10K/s"),
            (315_392, "308K/s"),
            // 999.5 K, the first figure that rounds to 1000 of a unit, and the one below it.
            (1_023_487, "999K/s"),
            (1_023_488, "1.0M/s"),
            (1_258_291, "1.2M/s"),
        ];
        for (per_second, want) in cases {
            assert_eq!(rate(per_second), want, "{per_second}");
        }
        for shift in 0..40 {
            for per_second in [(1u64 << shift) - 1, 1 << shift, (1 << shift) * 3 / 2] {
                let text = rate(per_second);
                assert!(text.len() <= 6, "{per_second}: {text}");
            }
        }
    }

    #[test]
    fn a_span_reads_to_the_minute() {
        let mins = |m: u64| Duration::from_secs(m * 60);
        assert_eq!(span(Duration::from_secs(59)), "<1 min");
        assert_eq!(span(mins(14)), "14 min");
        assert_eq!(span(mins(2 * 60 + 14)), "2 h 14 m");
        assert_eq!(span(mins(3 * 1440 + 2 * 60 + 5)), "3 d 2 h");
    }

    #[test]
    fn a_line_drops_whole_fields_from_the_right_to_fit() {
        let fields = [
            "  head".to_owned(),
            "  ·  one".to_owned(),
            "  ·  two".to_owned(),
        ];
        assert_eq!(fit(&fields, 0), "  head  ·  one  ·  two");
        assert_eq!(fit(&fields, 15), "  head  ·  one");
        assert_eq!(fit(&fields, 4), "  he");
        // What `Style::dim` adds takes no columns.
        let dim = ["\x1b[2mab\x1b[0m".to_owned(), "cd".to_owned()];
        assert_eq!(columns(&dim[0]), 2);
        assert_eq!(fit(&dim, 4), "\x1b[2mab\x1b[0mcd");
    }
}
