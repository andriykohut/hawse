//! What a server or a client run by hand prints on a terminal, in place of its log lines.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use clap::builder::styling::{self, Ansi256Color, Color, RgbColor};
use hawse_core::client::Event;
use hawse_core::config::{ClientConfig, split_host_port};
use hawse_core::server;
use hawse_core::traffic::Counts;
use hawse_core::transport::TransportKind;
use hawse_proto::key::PublicKey;
use hawse_proto::msg::BindFailure;
use hawse_proto::port::{Kind, Port, PortRequest};
use miette::{GraphicalTheme, ThemeCharacters, ThemeStyles};
use tokio::time::Instant;
use unicode_width::UnicodeWidthChar as _;

use crate::commands::client::reason;
use crate::status::{
    Answer, ClientStatus, LinkStatus, Presence, ServerStatus, ServiceStatus, Status,
};

#[derive(Clone, Copy, Debug)]
pub struct Style {
    pub color: bool,
    pub unicode: bool,
    /// The colour of the mark as the terminal can show it, `None` without colour or where it
    /// cannot.
    pub accent: Option<Color>,
    /// Whether a TCP service's public address is a link a terminal can open.
    pub links: bool,
}

impl Style {
    /// Unicode marks where the locale says the terminal reads UTF-8, which the first of these
    /// that is set decides, and with `color` the accent [`accent`] finds.
    pub fn detect(color: bool) -> Self {
        let locale = ["LC_ALL", "LC_CTYPE", "LANG"]
            .into_iter()
            .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
            .unwrap_or_default()
            .to_ascii_lowercase();
        Self {
            color,
            unicode: locale.contains("utf-8") || locale.contains("utf8"),
            accent: accent().filter(|_| color),
            links: true,
        }
    }

    /// The theme a report is drawn in: its mark red, what it points at in the accent and bold,
    /// its help dimmed, and the file it names underlined.
    pub fn theme(self) -> GraphicalTheme {
        let characters = if self.unicode {
            ThemeCharacters::unicode()
        } else {
            ThemeCharacters::ascii()
        };
        if !self.color {
            return GraphicalTheme {
                characters,
                styles: ThemeStyles::none(),
            };
        }
        let plain = owo_colors::Style::new();
        let pointed = match self.accent {
            Some(Color::Rgb(RgbColor(r, g, b))) => plain.truecolor(r, g, b),
            Some(Color::Ansi256(Ansi256Color(n))) => plain.color(owo_colors::XtermColors::from(n)),
            Some(Color::Ansi(_)) | None => plain,
        };
        GraphicalTheme {
            characters,
            styles: ThemeStyles {
                advice: plain.dimmed(),
                help: plain.dimmed(),
                link: plain.underline(),
                highlights: vec![pointed.bold()],
                ..ThemeStyles::ansi()
            },
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

    fn red(self, text: &str) -> String {
        if self.color {
            format!("\x1b[31m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }

    /// `text` as a link to `url` where links are on. A terminal that does not know the escape
    /// ignores it and shows the text alone.
    fn link(self, url: Option<String>, text: &str) -> String {
        match url {
            Some(url) if self.links => format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\"),
            _ => text.to_owned(),
        }
    }

    fn accent(self, text: &str) -> String {
        match self.accent {
            Some(color) => {
                let style = styling::Style::new().fg_color(Some(color));
                format!("{style}{text}{style:#}")
            }
            None => text.to_owned(),
        }
    }
}

/// The colour of the mark, `#e8632b`, where the terminal says it shows 24-bit colour, the nearest
/// of the 256 where it says it has those, and `None` where it has only the basic 16, none of
/// them orange.
pub fn accent() -> Option<Color> {
    let var = |name| std::env::var(name).unwrap_or_default();
    accent_for(&var("COLORTERM"), &var("TERM"))
}

fn accent_for(colorterm: &str, term: &str) -> Option<Color> {
    if colorterm == "truecolor" || colorterm == "24bit" {
        Some(Color::Rgb(RgbColor(0xe8, 0x63, 0x2b)))
    } else if term.contains("256color") {
        Some(Color::Ansi256(Ansi256Color(166)))
    } else {
        None
    }
}

/// The URL a browser opens a service public at `host` and `port` with: `https` when its `local`
/// port is 443 or 8443, `http` otherwise, and `None` for a local port of 22, which a browser
/// cannot open, or a UDP port.
fn url(host: &str, port: Port, local: &str) -> Option<String> {
    let scheme = match split_host_port(local).and_then(|(_, port)| port) {
        _ if port.kind == Kind::Udp => return None,
        Some(22) => return None,
        Some(443 | 8443) => "https",
        _ => "http",
    };
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    Some(format!("{scheme}://{host}:{}", port.number))
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

/// A rate in at most 6 columns, the bytes as [`bytes`] gives them and then `/s`.
pub fn rate(per_second: u64) -> String {
    format!("{}/s", bytes(per_second))
}

/// A byte count in at most 4 columns up to 999G: bytes under 1000, then K, M and G (binary),
/// with one decimal under 10. A figure that would round to 1000 of a unit reads as 1.0 of the
/// next.
pub fn bytes(count: u64) -> String {
    const UNITS: [&str; 3] = ["K", "M", "G"];
    if count < 1000 {
        return format!("{count}B");
    }
    // `count` in units of `scale`, to the nearest whole one and the nearest tenth.
    let whole = |scale: u128| (u128::from(count) * 2 + scale) / (scale * 2);
    let tenths = |scale: u128| (u128::from(count) * 20 + scale) / (scale * 2);
    let (mut scale, mut unit) = (1024u128, 0);
    while unit + 1 < UNITS.len() && whole(scale) >= 1000 {
        scale *= 1024;
        unit += 1;
    }
    let tenths = tenths(scale);
    if tenths < 100 {
        format!("{}.{}{}", tenths / 10, tenths % 10, UNITS[unit])
    } else {
        format!("{}{}", whole(scale), UNITS[unit])
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

/// How many columns `text` takes on a terminal, a wide character two of them, leaving out the
/// escapes a [`Style`] adds.
pub fn columns(text: &str) -> usize {
    plain(text).chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// `text` without the escapes a [`Style`] adds: colours, `ESC [ … m`, and links,
/// `ESC ] … ESC \`.
fn plain(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        if chars.next() == Some(']') {
            chars.by_ref().find(|&c| c == '\x1b');
            chars.next();
        } else {
            chars.by_ref().find(|&c| c == 'm');
        }
    }
    out
}

/// `fields` joined, dropping whole fields from the right until the line fits in `room` columns,
/// and cutting the first by characters when it does not fit alone, its colour and link dropped
/// with the cut. A `room` of 0 is a terminal that does not say how wide it is, and cuts nothing.
pub fn fit(fields: &[String], room: usize) -> String {
    let mut line = String::new();
    for field in fields {
        if room > 0 && !line.is_empty() && columns(&line) + columns(field) > room {
            break;
        }
        line.push_str(field);
    }
    if room > 0 && columns(&line) > room {
        let mut taken = 0;
        return plain(&line)
            .chars()
            .take_while(|c| {
                taken += c.width().unwrap_or(0);
                taken <= room
            })
            .collect();
    }
    line
}

/// Where the client's connection stands, as the panel's header says it.
#[derive(Clone, Copy, Debug, Default)]
pub enum Link {
    #[default]
    Connecting,
    Up {
        transport: TransportKind,
        round_trip: Duration,
        since: Instant,
    },
    Down {
        since: Instant,
        retry_at: Instant,
    },
    /// Retrying has ended: the config cannot be retried.
    Stopped,
}

/// One service's row: the server's answer to its bind, and once bound, its connections open
/// and its bytes per second each way over the last sample.
#[derive(Clone, Copy, Debug)]
pub enum Row {
    Bound {
        port: Port,
        open: u64,
        down: u64,
        up: u64,
    },
    Refused(BindFailure),
}

/// The header's fields: who this is, the server, and where the connection stands at `now`.
fn header(link: Link, cfg: &ClientConfig, style: Style, now: Instant) -> Vec<String> {
    let dot = || style.dim(style.pick("·", "|"));
    let mut header = vec![
        "  hawse client".to_owned(),
        format!("  {}  {}", dot(), cfg.server),
    ];
    match link {
        Link::Up {
            transport,
            round_trip,
            since,
        } => {
            header.push(format!(
                "  {}  {}",
                dot(),
                transport.to_string().to_uppercase()
            ));
            header.push(format!("  {}  {}", dot(), millis(round_trip)));
            header.push(format!("  {}  up {}", dot(), span(now - since)));
        }
        Link::Down { since, retry_at } if retry_at > now => {
            let left = retry_at - now;
            let secs = left.as_secs() + u64::from(left.subsec_nanos() > 0);
            header.push(format!("  {}  down {}", dot(), span(now - since)));
            header.push(format!("  {}  retrying in {secs} s", dot()));
        }
        Link::Connecting | Link::Down { .. } => header.push(format!("  {}  connecting", dot())),
        Link::Stopped => header.push(format!("  {}  stopped", dot())),
    }
    header
}

/// Each row's fields as one line in `room` columns, as [`fit`] makes them, but with a bound
/// row's second field, its local column, dropped from every row when any row is too wide.
fn fit_rows(rows: Vec<Vec<String>>, room: usize) -> Vec<String> {
    // The local column says nothing a redraw changes, so it goes before any figure does.
    let too_wide = room > 0
        && rows
            .iter()
            .any(|fields| fields.iter().map(|field| columns(field)).sum::<usize>() > room);
    rows.into_iter()
        .map(|mut fields| {
            if too_wide && fields.len() > 1 {
                fields.remove(1);
            }
            // The padding of a column with nothing after it.
            fit(&fields, room).trim_end().to_owned()
        })
        .collect()
}

/// How wide a bound row's columns are: the services' names, their public addresses, and their
/// local addresses.
#[derive(Clone, Copy)]
struct Widths {
    names: usize,
    public: usize,
    locals: usize,
}

/// A bound row's first two fields: `mark`, the service's name and its public address at `host`
/// and `port`, a link where a browser opens it; then the dim local column, `local`, padded to
/// `widths.locals`.
fn service_fields(
    mark: &str,
    service: &str,
    host: &str,
    port: Port,
    local: &str,
    widths: Widths,
    style: Style,
) -> Vec<String> {
    let Widths {
        names,
        public,
        locals,
    } = widths;
    // Every row's local column is as wide, so the figures after it line up.
    let pad = locals - local.len();
    let address = format!("{host}:{port}");
    let fill = public.saturating_sub(address.len());
    let address = style.link(url(host, port, local), &address);
    let local = style.dim(&format!("  {}  {local}", style.pick("←", "<-")));
    vec![
        format!("  {mark} {service:names$}  {address}{:fill$}", ""),
        format!("{local}{:pad$}", ""),
    ]
}

/// The client's panel: a blank line, a header for the connection, then a row for each service in
/// `rows` that `cfg` still exposes, in its order. Each line is cut to one column less than `width`, and the
/// panel folded to `height` lines; a `width` or `height` of 0 is a terminal that does not say.
pub fn frame(
    link: Link,
    rows: &BTreeMap<String, Row>,
    cfg: &ClientConfig,
    style: Style,
    now: Instant,
    (width, height): (usize, usize),
) -> Vec<String> {
    let room = width.saturating_sub(1);
    let no = style.red(style.pick("✗", "x"));
    let again = style.accent(style.pick("↻", "~"));
    let moving = style.accent(style.pick("●", "*"));
    let idle = style.accent(style.pick("○", "o"));
    let host = split_host_port(&cfg.server)
        .map(|(host, _)| host)
        .unwrap_or_default();
    let names = cfg.expose.keys().map(String::len).max().unwrap_or(0);
    let public = host.len()
        + 1
        + cfg
            .expose
            .values()
            .map(|e| port_width(e.port))
            .max()
            .unwrap_or(0);
    let locals = cfg
        .expose
        .values()
        .map(|e| e.local.len())
        .max()
        .unwrap_or(0);
    let widths = Widths {
        names,
        public,
        locals,
    };
    let counts = rows
        .values()
        .filter_map(|row| match row {
            Row::Bound { open, .. } => Some(open.to_string().len()),
            Row::Refused(_) => None,
        })
        .max()
        .unwrap_or(1);

    let mut rows_fields = Vec::new();
    for (service, expose) in &cfg.expose {
        let Some(row) = rows.get(service) else {
            continue;
        };
        let fields = match *row {
            Row::Refused(reason) => vec![format!("  {no} {service:names$}  {reason}")],
            Row::Bound {
                port,
                open,
                down,
                up,
            } => {
                let moved = down > 0 || up > 0;
                let mark = match link {
                    Link::Up { .. } if moved => &moving,
                    Link::Up { .. } => &idle,
                    Link::Stopped => &no,
                    Link::Connecting | Link::Down { .. } => &again,
                };
                let mut fields =
                    service_fields(mark, service, &host, port, &expose.local, widths, style);
                if matches!(link, Link::Up { .. }) {
                    fields.extend(figures(open, down, up, counts, style));
                }
                fields
            }
        };
        rows_fields.push(fields);
    }
    let body = fit_rows(rows_fields, room);

    // Set off from the command that started the client, as the server's header is.
    let mut lines = vec![String::new(), fit(&header(link, cfg, style, now), room)];
    if !body.is_empty() {
        lines.push(String::new());
        lines.extend(body);
    }
    fold(lines, height, style, room)
}

/// A bound row's figures as the panel draws them while the link is up, from its open count and
/// its bytes per second each way: `idle` with nothing open or moving, else the count padded to
/// `counts` columns, then the rates when bytes moved.
fn figures(open: u64, down: u64, up: u64, counts: usize, style: Style) -> Vec<String> {
    let moved = down > 0 || up > 0;
    if open == 0 && !moved {
        return vec![format!("  {}", style.dim("idle"))];
    }
    let mut fields = vec![format!("  {}", style.accent(&format!("{open:>counts$}")))];
    if moved {
        let down = style.accent(&format!("{} {:>6}", style.pick("↓", "v"), rate(down)));
        let up = style.accent(&format!("{} {:>6}", style.pick("↑", "^"), rate(up)));
        fields.push(format!("  {down}"));
        fields.push(format!("  {up}"));
    }
    fields
}

/// `lines` folded to `height` lines: a blank first line goes first, then the lines from the last
/// that fits on, said as `… N more` in `room` columns. A `height` of 0 is a terminal that does
/// not say, and folds nothing.
pub fn fold(mut lines: Vec<String>, height: usize, style: Style, room: usize) -> Vec<String> {
    if height > 0 && lines.len() > height && lines.first().is_some_and(String::is_empty) {
        // The blank line above the header goes before any row does.
        lines.remove(0);
    }
    if height == 1 {
        // A line taller would scroll the header off, and the next redraw could not erase it.
        lines.truncate(1);
    } else if height > 0 && lines.len() > height {
        let kept = height - 1;
        let more = lines[kept..].iter().filter(|line| !line.is_empty()).count();
        lines.truncate(kept);
        let summary = format!("  {} {more} more", style.pick("…", "..."));
        lines.push(fit(&[summary], room));
    }
    lines
}

/// A status reply as `hawse status` prints it: a header that opens with a mark and a word for
/// the state, then a client's services in the panel's rows, or a server's clients and the
/// unknown keys that knocked. With `rates`, each service's bytes per second each way by name, a
/// client's rows are the panel's, rates in place of totals. Each line is cut to one column less
/// than `width`, as [`frame`]'s are; a `width` of 0 cuts nothing.
pub fn status_lines(
    status: &Status,
    rates: Option<&BTreeMap<String, (u64, u64)>>,
    style: Style,
    width: usize,
) -> Vec<String> {
    let room = width.saturating_sub(1);
    match status {
        Status::Client(client) => client_status_lines(client, rates, style, room),
        Status::Server(server) => server_status_lines(server, style, room),
    }
}

/// A client's status header in `room` columns: a mark and a word for the link, the server, and
/// the link's figures, with a second line for why it is down.
fn client_status_header(status: &ClientStatus, style: Style, room: usize) -> Vec<String> {
    let dot = || style.dim(style.pick("·", "|"));
    let no = style.red(style.pick("✗", "x"));
    let secs = Duration::from_secs;
    let server = format!("  {}  {}", dot(), status.server);
    let mut lines = Vec::new();
    match &status.link {
        LinkStatus::Up {
            transport,
            round_trip_ms,
            up_s,
            ..
        } => {
            let header = [
                format!(
                    "{} hawse client  {}",
                    style.accent(style.pick("●", "*")),
                    style.accent("connected")
                ),
                server,
                format!("  {}  {}", dot(), transport.to_uppercase()),
                format!(
                    "  {}  {}",
                    dot(),
                    millis(Duration::from_millis(*round_trip_ms))
                ),
                format!("  {}  up {}", dot(), span(secs(*up_s))),
            ];
            lines.push(fit(&header, room));
        }
        LinkStatus::Down {
            down_s,
            retry_in_s,
            cause,
        } => {
            let down = style.red(&format!("down {}", span(secs(*down_s))));
            let retry = match retry_in_s {
                0 => "retrying now".to_owned(),
                secs => format!("retrying in {secs} s"),
            };
            let header = [
                format!("{no} hawse client  {down}"),
                server,
                format!("  {}  {retry}", dot()),
            ];
            lines.push(fit(&header, room));
            lines.push(fit(&[format!("  {no} {cause}")], room));
        }
        LinkStatus::Connecting => {
            let header = [
                format!(
                    "{} hawse client  {}",
                    style.accent(style.pick("↻", "~")),
                    style.accent("connecting")
                ),
                server,
            ];
            lines.push(fit(&header, room));
        }
    }
    lines
}

/// A client's status in `room` columns, with `rates` as [`status_lines`] takes them. A service the
/// server has not answered yet has no row, as on the panel.
fn client_status_lines(
    status: &ClientStatus,
    rates: Option<&BTreeMap<String, (u64, u64)>>,
    style: Style,
    room: usize,
) -> Vec<String> {
    let no = style.red(style.pick("✗", "x"));
    let again = style.accent(style.pick("↻", "~"));
    let busy = style.accent(style.pick("●", "*"));
    let idle = style.accent(style.pick("○", "o"));
    let (toward, from) = (style.pick("↓", "v"), style.pick("↑", "^"));
    let mut lines = client_status_header(status, style, room);
    let up = matches!(status.link, LinkStatus::Up { .. });
    let bound = |service: &ServiceStatus| match &service.answer {
        Answer::Bound { port } => port.parse::<Port>().ok(),
        Answer::Pending | Answer::Refused { .. } => None,
    };
    let host = split_host_port(&status.server)
        .map(|(host, _)| host)
        .unwrap_or_default();
    let services = &status.services;
    let widths = Widths {
        names: services.iter().map(|s| s.name.len()).max().unwrap_or(0),
        public: host.len()
            + 1
            + services
                .iter()
                .filter_map(bound)
                .map(|port| port.to_string().len())
                .max()
                .unwrap_or(0),
        locals: services.iter().map(|s| s.local.len()).max().unwrap_or(0),
    };
    let counts = services
        .iter()
        .filter(|s| bound(s).is_some())
        .map(|s| s.open.to_string().len())
        .max()
        .unwrap_or(1);
    let mut rows = Vec::new();
    for service in services {
        let (name, names) = (&service.name, widths.names);
        let fields = match (&service.answer, bound(service)) {
            (Answer::Refused { reason }, _) => vec![format!("  {no} {name:names$}  {reason}")],
            (_, Some(port)) => {
                let rate = rates.map(|rates| rates.get(name).copied().unwrap_or_default());
                // Without rates nothing says whether bytes move, so an open connection marks it.
                let busy_now = rate.map_or(service.open > 0, |(down, sent)| down > 0 || sent > 0);
                let mark = match (up, busy_now) {
                    (false, _) => &again,
                    (true, false) => &idle,
                    (true, true) => &busy,
                };
                let mut fields =
                    service_fields(mark, name, &host, port, &service.local, widths, style);
                match rate {
                    Some((down, sent)) if up => {
                        fields.extend(figures(service.open, down, sent, counts, style));
                    }
                    None if up => {
                        let figure = |text: String| format!("  {}", style.accent(&text));
                        fields.push(figure(format!("{:>counts$}", service.open)));
                        fields.push(figure(format!("{toward} {:>4}", bytes(service.down))));
                        fields.push(figure(format!("{from} {:>4}", bytes(service.up))));
                    }
                    _ => {}
                }
                fields
            }
            _ => continue,
        };
        rows.push(fields);
    }
    let body = fit_rows(rows, room);
    if !body.is_empty() {
        lines.push(String::new());
        lines.extend(body);
    }
    lines
}

/// A server's status in `room` columns: its header, a line for each client, with the services a
/// connected one has bound under it, and a line for each unknown key.
fn server_status_lines(status: &ServerStatus, style: Style, room: usize) -> Vec<String> {
    let dot = || style.dim(style.pick("·", "|"));
    let no = style.red(style.pick("✗", "x"));
    let secs = Duration::from_secs;
    let ip = |remote: &str| {
        remote
            .parse::<SocketAddr>()
            .map_or_else(|_| remote.to_owned(), |addr| addr.ip().to_string())
    };
    let clients = &status.clients;
    let connected = clients
        .iter()
        .filter(|c| matches!(c.presence, Presence::Connected { .. }))
        .count();
    let count = format!("{connected} of {} clients", clients.len());
    // No client authorized is how a new server starts, not a fault to show in red.
    let (mark, state) = if clients.is_empty() {
        let none = "no clients are authorized";
        (style.dim(style.pick("○", "o")), style.dim(none))
    } else if connected == 0 {
        (no.clone(), style.red(&count))
    } else {
        (style.accent(style.pick("●", "*")), style.accent(&count))
    };
    let header = [
        format!("{mark} hawse server  {state}"),
        format!("  {}  {}", dot(), status.listen),
        format!("  {}  up {}", dot(), span(secs(status.up_s))),
        format!("  {}  {}", dot(), status.version),
    ];
    let mut lines = vec![fit(&header, room)];

    if !clients.is_empty() {
        lines.push(String::new());
    }
    let names = clients.iter().map(|c| c.name.len()).max().unwrap_or(0);
    for client in clients {
        let name = &client.name;
        match &client.presence {
            Presence::Connected {
                remote,
                transport,
                agent,
                up_s,
                services,
            } => {
                let mark = style.accent(style.pick("●", "*"));
                let mut fields = vec![format!("  {mark} {name:names$}  {}", ip(remote))];
                fields.extend(
                    transport
                        .iter()
                        .map(|t| format!("  {}  {}", dot(), t.to_uppercase())),
                );
                fields.extend(up_s.map(|up| format!("  {}  up {}", dot(), span(secs(up)))));
                fields.extend(agent.as_deref().map(|agent| {
                    let version = agent.strip_prefix("hawse/").unwrap_or(agent);
                    format!("  {}  {version}", dot())
                }));
                lines.push(fit(&fields, room));
                let width = services.iter().map(|s| s.name.len()).max().unwrap_or(0);
                for service in services {
                    let at = match service.bind.parse::<IpAddr>() {
                        Ok(ip) if ip.is_unspecified() => String::new(),
                        Ok(IpAddr::V6(ip)) => format!("[{ip}]"),
                        _ => service.bind.clone(),
                    };
                    let line = format!("      {:width$}  {at}:{}", service.name, service.port);
                    lines.push(fit(&[line], room));
                }
            }
            Presence::NotConnected { last_s } => {
                let seen = last_s
                    .map(|last| format!(", last seen {} ago", span(secs(last))))
                    .unwrap_or_default();
                let mark = style.pick("○", "o");
                let line = style.dim(&format!("{mark} {name:names$}  not connected{seen}"));
                lines.push(fit(&[format!("  {line}")], room));
            }
        }
    }

    if !status.unknown_keys.is_empty() {
        lines.push(String::new());
    }
    for knock in &status.unknown_keys {
        // Fields, so a narrow terminal drops the counts before it cuts the key to be pasted.
        let times = if knock.times == 1 { "time" } else { "times" };
        let fields = [
            format!("  {no} unknown key {}", knock.key),
            format!(" from {}", ip(&knock.remote)),
            format!(
                "  {}  {} {times}, last {} ago",
                dot(),
                knock.times,
                span(secs(knock.last_s))
            ),
        ];
        lines.push(fit(&fields, room));
    }
    lines
}

/// The line to print above the panel for `event`, or `None` for one that only changes the panel.
pub fn event(event: &Event, style: Style) -> Option<String> {
    let no = style.red(style.pick("✗", "x"));
    let again = style.accent(style.pick("↻", "~"));
    match event {
        Event::Denied { key } => Some(format!(
            "  {no} not authorized. on the server, add to server.toml:\n\n      [clients.NAME]\n      key = \"{key}\"\n\n    then wait for the next retry"
        )),
        Event::Disconnected { cause, retry_in } => {
            let reason = reason(cause.clone());
            Some(match retry_in {
                Some(_) => format!("  {again} {reason}"),
                None => format!("  {no} {reason}; the config cannot be retried"),
            })
        }
        Event::Connected { .. }
        | Event::Bound { .. }
        | Event::BindFailed { .. }
        | Event::RoundTrip { .. } => None,
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
        style.accent(style.pick("✓", "+")),
        style.red(style.pick("✗", "x")),
        style.accent(style.pick("↻", "~")),
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
    format!("  {} {text}", style.accent(style.pick("↻", "~")))
}

/// What a client prints under its panel when it stops: how long it ran, how many times it
/// reconnected if it did, and a line for each service in `carried` with its visitors and bytes
/// each way, and its datagrams each way and dropped if it carried any.
pub fn summary(
    ran: Duration,
    reconnects: u64,
    carried: &BTreeMap<String, Counts>,
    style: Style,
) -> String {
    let again = match reconnects {
        0 => String::new(),
        1 => ", reconnected once".to_owned(),
        n => format!(", reconnected {n} times"),
    };
    let mut lines = vec![format!("\n  stopped after {}{again}", span(ran))];
    let names = carried.keys().map(String::len).max().unwrap_or(0);
    let visitors = carried
        .values()
        .map(|counts| counts.visitors.to_string().len())
        .max()
        .unwrap_or(0);
    let figure = |text: String| style.accent(&text);
    for (service, counts) in carried {
        let datagrams = if counts.datagrams_down + counts.datagrams_up + counts.dropped > 0 {
            format!(
                "  {} datagrams in, {} out, {} dropped",
                figure(counts.datagrams_down.to_string()),
                figure(counts.datagrams_up.to_string()),
                figure(counts.dropped.to_string()),
            )
        } else {
            String::new()
        };
        let noun = if counts.visitors == 1 {
            "visitor "
        } else {
            "visitors"
        };
        lines.push(format!(
            "    {service:names$}  {} {noun}  down {}  up {}{datagrams}",
            figure(format!("{:>visitors$}", counts.visitors)),
            figure(format!("{:>4}", bytes(counts.down))),
            figure(format!("{:>4}", bytes(counts.up))),
        ));
    }
    lines.join("\n")
}

/// Writes `line` to stderr through [`crate::screen::print`], above the client's panel when it is
/// up.
pub fn print(line: &str) {
    crate::screen::print(line);
}

#[cfg(test)]
mod tests {
    use hawse_core::client::DisconnectCause;
    use hawse_core::identity::Identity;
    use hawse_core::transport::TransportKind;
    use hawse_proto::msg::BindFailure;
    use hawse_proto::port::Port;
    use tokio::time::Instant;

    use super::*;
    use crate::config_file::parse;
    use crate::status::{Bind, ClientEntry, Knock};

    fn cfg() -> ClientConfig {
        parse(
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
        .unwrap()
    }

    fn session(style: Style) -> String {
        let cfg = cfg();
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
        let mut out: Vec<String> = events.iter().filter_map(|e| event(e, style)).collect();
        out.push(note("config reloaded", style));
        out.join("\n")
    }

    #[test]
    fn what_scrolls_above_the_panel() {
        insta::assert_snapshot!(session(Style {
            color: false,
            unicode: true,
            accent: None,
            links: false,
        }));
    }

    #[test]
    fn what_scrolls_above_the_panel_keeps_to_ascii() {
        let out = session(Style {
            color: false,
            unicode: false,
            accent: None,
            links: false,
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
            unicode: true,
            accent: None,
            links: false,
        }));
    }

    #[test]
    fn a_server_without_unicode_keeps_to_ascii() {
        let out = server_session(Style {
            color: false,
            unicode: false,
            accent: None,
            links: false,
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
    fn colour_and_links_only_add_escapes() {
        let render = |style| {
            format!(
                "{}\n{}\n{}",
                session(style),
                server_session(style),
                frames(style)
            )
        };
        let out = render(Style {
            color: true,
            unicode: true,
            accent: accent_for("truecolor", ""),
            links: true,
        });
        let uncoloured = render(Style {
            color: false,
            unicode: true,
            accent: None,
            links: false,
        });
        assert_eq!(plain(&out), uncoloured);
        assert!(out.contains("\x1b[38;2;232;99;43m"), "no accent: {out:?}");
        let dev =
            "\x1b]8;;http://tunnel.example.com:40017\x1b\\tunnel.example.com:40017\x1b]8;;\x1b\\";
        assert!(out.contains(dev), "no link for dev: {out:?}");
        assert_eq!(
            out.matches("\x1b]8;;http").count(),
            out.matches(dev).count(),
            "{out:?}"
        );
    }

    #[test]
    fn a_browser_opens_a_service_by_its_local_port() {
        let port = |number, kind| Port { number, kind };
        let at = |host, number, kind, local| url(host, port(number, kind), local);
        let host = "tunnel.example.com";
        assert_eq!(
            at(host, 40017, Kind::Tcp, "127.0.0.1:3000").as_deref(),
            Some("http://tunnel.example.com:40017")
        );
        assert_eq!(
            at(host, 9443, Kind::Tcp, "127.0.0.1:443").as_deref(),
            Some("https://tunnel.example.com:9443")
        );
        assert_eq!(
            at("::1", 8443, Kind::Tcp, "[::1]:8443").as_deref(),
            Some("https://[::1]:8443")
        );
        assert_eq!(at(host, 2222, Kind::Tcp, "127.0.0.1:22"), None);
        assert_eq!(at(host, 51820, Kind::Udp, "127.0.0.1:51820"), None);
    }

    #[test]
    fn the_accent_is_what_the_terminal_says_it_shows() {
        let orange = Some(Color::Rgb(RgbColor(232, 99, 43)));
        assert_eq!(accent_for("truecolor", "xterm-256color"), orange);
        assert_eq!(accent_for("24bit", "xterm"), orange);
        let nearest = Some(Color::Ansi256(Ansi256Color(166)));
        assert_eq!(accent_for("", "xterm-256color"), nearest);
        assert_eq!(accent_for("", "tmux-256color"), nearest);
        assert_eq!(accent_for("", "xterm"), None);
        assert_eq!(accent_for("", "dumb"), None);
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
    fn a_byte_count_reads_as_a_rate_does_without_the_per_second() {
        assert_eq!(bytes(999), "999B");
        assert_eq!(bytes(1_258_291), "1.2M");
        assert_eq!(rate(1_258_291), "1.2M/s");
    }

    #[test]
    fn a_client_that_stops_sums_up_its_run() {
        let carried = BTreeMap::from([
            (
                "dns".to_owned(),
                Counts {
                    visitors: 37,
                    down: 1_153_434,
                    up: 1_468_006,
                    datagrams_down: 9104,
                    datagrams_up: 9101,
                    dropped: 3,
                    ..Counts::default()
                },
            ),
            (
                "jellyfin".to_owned(),
                Counts {
                    visitors: 412,
                    down: 19_541_180_006,
                    up: 325_058_560,
                    ..Counts::default()
                },
            ),
            ("ssh".to_owned(), Counts::default()),
        ]);
        let style = Style {
            color: false,
            unicode: true,
            accent: None,
            links: false,
        };
        let ran = Duration::from_secs(3 * 86_400 + 4 * 3600 + 12 * 60);
        insta::assert_snapshot!(summary(ran, 2, &carried, style));
        let short = Duration::from_secs(30);
        let header = |reconnects| summary(short, reconnects, &BTreeMap::new(), style);
        assert_eq!(header(0), "\n  stopped after <1 min");
        assert_eq!(header(1), "\n  stopped after <1 min, reconnected once");
        let one = BTreeMap::from([(
            "web".to_owned(),
            Counts {
                visitors: 1,
                ..Counts::default()
            },
        )]);
        let one = summary(short, 0, &one, style);
        assert!(
            one.ends_with("web  1 visitor   down   0B  up   0B"),
            "{one}"
        );
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

    #[test]
    fn a_wide_character_takes_two_columns_and_is_never_cut_in_half() {
        assert_eq!(columns("例え.jp"), 7);
        let cut = fit(&["  例え例え".to_owned()], 5);
        assert_eq!(cut, "  例");
        assert!(columns(&cut) <= 5);
    }

    #[test]
    fn a_folded_panel_fits_its_terminal() {
        let now = Instant::now();
        let up = Link::Up {
            transport: TransportKind::Quic,
            round_trip: Duration::from_millis(24),
            since: now,
        };
        let row = |number| Row::Bound {
            port: Port {
                number,
                kind: Kind::Tcp,
            },
            open: 1,
            down: 1024,
            up: 1024,
        };
        let rows = BTreeMap::from([
            ("dev".to_owned(), row(40017)),
            ("ssh".to_owned(), row(2222)),
            ("wireguard".to_owned(), row(51820)),
        ]);
        let style = Style {
            color: false,
            unicode: true,
            accent: None,
            links: false,
        };
        for (width, height) in [(0, 1), (0, 2), (12, 2), (12, 3), (12, 4)] {
            let lines = frame(up, &rows, &cfg(), style, now, (width, height));
            assert!(lines.len() <= height, "{width}x{height}: {lines:?}");
            if width > 0 {
                let wide = lines.iter().find(|line| columns(line) >= width);
                assert_eq!(wide, None, "{width}x{height}: {lines:?}");
            }
        }
        // The header, then a summary that counts rows and not the blank line between.
        let lines = frame(up, &rows, &cfg(), style, now, (0, 2));
        assert_eq!(lines[1], "  … 3 more");
    }

    #[test]
    fn a_busy_row_gives_up_the_local_column_before_a_rate() {
        let now = Instant::now();
        let up = Link::Up {
            transport: TransportKind::Quic,
            round_trip: Duration::from_millis(24),
            since: now,
        };
        let rows = BTreeMap::from([(
            "dev".to_owned(),
            Row::Bound {
                port: Port {
                    number: 40017,
                    kind: Kind::Tcp,
                },
                open: 2,
                down: 308 * 1024,
                up: 1_258_291,
            },
        )]);
        let style = Style {
            color: false,
            unicode: true,
            accent: None,
            links: false,
        };
        let lines = frame(up, &rows, &cfg(), style, now, (80, 0));
        let row = lines.last().unwrap();
        assert!(row.ends_with("↑ 1.2M/s"), "{row}");
        assert!(!row.contains('←'), "{row}");
    }

    fn frames(style: Style) -> String {
        let cfg = cfg();
        let now = Instant::now();
        let up = Link::Up {
            transport: TransportKind::Quic,
            round_trip: Duration::from_millis(24),
            since: now - Duration::from_secs(2 * 3600 + 14 * 60),
        };
        let port = |number, kind| Port { number, kind };
        let bound = |number, kind, open, down, up| Row::Bound {
            port: port(number, kind),
            open,
            down,
            up,
        };
        let busy = BTreeMap::from([
            (
                "ssh".to_owned(),
                bound(2222, Kind::Tcp, 1, 2 * 1024, 9 * 1024),
            ),
            (
                "dev".to_owned(),
                bound(40017, Kind::Tcp, 2, 308 * 1024, 1_258_291),
            ),
            ("wireguard".to_owned(), bound(51820, Kind::Udp, 0, 0, 0)),
        ]);
        let mut quiet = busy.clone();
        quiet.insert("ssh".to_owned(), bound(2222, Kind::Tcp, 1, 0, 0));
        let mut refused = busy.clone();
        refused.insert(
            "wireguard".to_owned(),
            Row::Refused(BindFailure::NotGranted),
        );
        let down = Link::Down {
            since: now - Duration::from_secs(180),
            retry_at: now + Duration::from_millis(3200),
        };
        let none = BTreeMap::new();
        let shown = [
            ("busy", frame(up, &busy, &cfg, style, now, (0, 0))),
            (
                "open with nothing moving",
                frame(up, &quiet, &cfg, style, now, (0, 0)),
            ),
            (
                "a refused bind",
                frame(up, &refused, &cfg, style, now, (0, 0)),
            ),
            ("down", frame(down, &busy, &cfg, style, now, (0, 0))),
            (
                "connecting",
                frame(Link::Connecting, &none, &cfg, style, now, (0, 0)),
            ),
            (
                "stopped",
                frame(Link::Stopped, &busy, &cfg, style, now, (0, 0)),
            ),
            (
                "cut at 60 columns",
                frame(up, &busy, &cfg, style, now, (61, 0)),
            ),
            (
                "folded to 4 lines",
                frame(up, &busy, &cfg, style, now, (0, 4)),
            ),
        ];
        shown
            .iter()
            .map(|(name, lines)| format!("{name}:\n{}", lines.join("\n")))
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    #[test]
    fn a_panel_reads_as_a_header_over_a_row_per_service() {
        insta::assert_snapshot!(frames(Style {
            color: false,
            unicode: true,
            accent: None,
            links: false,
        }));
    }

    #[test]
    fn a_panel_without_unicode_keeps_to_ascii() {
        let out = frames(Style {
            color: false,
            unicode: false,
            accent: None,
            links: false,
        });
        assert!(out.is_ascii(), "{out}");
        insta::assert_snapshot!(out);
    }

    #[test]
    fn a_service_the_config_no_longer_has_has_no_row() {
        let rows = BTreeMap::from([(
            "gone".to_owned(),
            Row::Bound {
                port: Port {
                    number: 9,
                    kind: Kind::Tcp,
                },
                open: 1,
                down: 0,
                up: 0,
            },
        )]);
        let now = Instant::now();
        let up = Link::Up {
            transport: TransportKind::Tcp,
            round_trip: Duration::from_millis(5),
            since: now,
        };
        let lines = frame(
            up,
            &rows,
            &cfg(),
            Style {
                color: false,
                unicode: true,
                accent: None,
                links: false,
            },
            now,
            (0, 0),
        );
        // A blank line, then the header, with no row under it.
        assert_eq!(lines.len(), 2, "{lines:?}");
    }

    fn uncoloured(unicode: bool) -> Style {
        Style {
            color: false,
            unicode,
            accent: None,
            links: false,
        }
    }

    fn coloured() -> Style {
        Style {
            color: true,
            unicode: true,
            accent: accent_for("truecolor", ""),
            links: true,
        }
    }

    fn connected() -> LinkStatus {
        LinkStatus::Up {
            transport: "quic".to_owned(),
            round_trip_ms: 38,
            up_s: 2 * 3600 + 10 * 60,
            agent: "hawse/0.11.0".to_owned(),
        }
    }

    fn down() -> LinkStatus {
        LinkStatus::Down {
            down_s: 180,
            retry_in_s: 8,
            cause: "server stopped answering".to_owned(),
        }
    }

    /// The client of the spec's mockups: jellyfin bound with two open, wg bound with none, and
    /// ssh refused.
    fn client_status(link: LinkStatus) -> ClientStatus {
        let service =
            |name: &str, local: &str, port: &str, answer, (open, down, up)| ServiceStatus {
                name: name.to_owned(),
                local: local.to_owned(),
                port: port.to_owned(),
                answer,
                open,
                visitors: 0,
                down,
                up,
                datagrams_down: 0,
                datagrams_up: 0,
                dropped: 0,
            };
        let bound = |port: &str| Answer::Bound {
            port: port.to_owned(),
        };
        ClientStatus {
            version: "0.11.0".to_owned(),
            server: "tunnel.example.com:4433".to_owned(),
            up_s: 3 * 3600,
            link,
            services: vec![
                service(
                    "jellyfin",
                    "127.0.0.1:8096",
                    "8096",
                    bound("8096"),
                    (2, 15_247_000_000, 220_200_960),
                ),
                service(
                    "wg",
                    "127.0.0.1:51820",
                    "51820/udp",
                    bound("51820/udp"),
                    (0, 1_181_116_006, 100_663_296),
                ),
                service(
                    "ssh",
                    "127.0.0.1:22",
                    "2222",
                    Answer::Refused {
                        reason: "port is not granted to this client".to_owned(),
                    },
                    (0, 0, 0),
                ),
            ],
        }
    }

    fn nas_connected() -> ClientEntry {
        let bind = |name: &str, bind: &str, port: &str| Bind {
            name: name.to_owned(),
            bind: bind.to_owned(),
            port: port.to_owned(),
        };
        ClientEntry {
            name: "nas".to_owned(),
            presence: Presence::Connected {
                remote: "203.0.113.7:51234".to_owned(),
                transport: Some("quic".to_owned()),
                agent: Some("hawse/0.11.0".to_owned()),
                up_s: Some(2 * 3600 + 10 * 60),
                services: vec![
                    bind("jellyfin", "0.0.0.0", "8096"),
                    bind("wg", "::", "51820/udp"),
                ],
            },
        }
    }

    fn not_connected(name: &str, last_s: Option<u64>) -> ClientEntry {
        ClientEntry {
            name: name.to_owned(),
            presence: Presence::NotConnected { last_s },
        }
    }

    /// The server of the spec's mockup, with `clients` and one unknown key.
    fn server_status(clients: Vec<ClientEntry>) -> Status {
        Status::Server(ServerStatus {
            version: "0.11.0".to_owned(),
            listen: "0.0.0.0:4433".to_owned(),
            tcp_fallback: false,
            up_s: (3 * 24 + 4) * 3600,
            clients,
            unknown_keys: vec![Knock {
                key: "ed25519:Qm9vYmFyYmF6cXV4cXV1eHF1dXhxdXV4cXV1eHgzUms".to_owned(),
                remote: "198.51.100.4:40122".to_owned(),
                times: 3,
                last_s: 12 * 60,
            }],
        })
    }

    fn drawn(status: &Status, style: Style, width: usize) -> String {
        status_lines(status, None, style, width).join("\n")
    }

    #[test]
    fn a_connected_client_reads_as_its_panel_at_rest() {
        let status = Status::Client(client_status(connected()));
        insta::assert_snapshot!(drawn(&status, uncoloured(true), 0));
    }

    #[test]
    fn a_watched_client_reads_as_its_panel() {
        let mut client = client_status(connected());
        // wg has a session open and quiet; jellyfin moves bytes.
        client.services[1].open = 1;
        let rates = BTreeMap::from([("jellyfin".to_owned(), (1_258_291, 36_045))]);
        let lines = status_lines(&Status::Client(client), Some(&rates), uncoloured(true), 0);
        insta::assert_snapshot!(lines.join("\n"));
    }

    #[test]
    fn a_fold_keeps_a_first_line_that_is_not_blank() {
        let lines = ["head", "", "a", "b"].map(str::to_owned).to_vec();
        assert_eq!(
            fold(lines, 3, uncoloured(true), 0),
            ["head", "", "  … 2 more"]
        );
    }

    #[test]
    fn a_connected_client_status_keeps_to_ascii() {
        let status = Status::Client(client_status(connected()));
        let out = drawn(&status, uncoloured(false), 0);
        assert!(out.is_ascii(), "{out}");
        insta::assert_snapshot!(out);
    }

    #[test]
    fn a_client_that_is_down_says_so_first() {
        let mut status = client_status(down());
        // The last session's answers stay until the next session, and ssh's had not come.
        status.services[2].answer = Answer::Pending;
        insta::assert_snapshot!(drawn(&Status::Client(status), uncoloured(true), 0));

        let due = client_status(LinkStatus::Down {
            down_s: 180,
            retry_in_s: 0,
            cause: "server stopped answering".to_owned(),
        });
        let lines = status_lines(&Status::Client(due), None, uncoloured(true), 0);
        assert!(lines[0].ends_with("  ·  retrying now"), "{lines:?}");
    }

    #[test]
    fn a_connecting_client_says_so_first() {
        let mut status = client_status(LinkStatus::Connecting);
        for service in &mut status.services {
            service.answer = Answer::Pending;
        }
        insta::assert_snapshot!(drawn(&Status::Client(status), uncoloured(true), 0));
    }

    #[test]
    fn a_down_client_starts_with_a_red_mark() {
        let lines = status_lines(&Status::Client(client_status(down())), None, coloured(), 0);
        assert!(lines[0].starts_with("\x1b[31m✗"), "{lines:?}");
    }

    #[test]
    fn a_status_narrower_than_its_rows_drops_the_local_column_first() {
        let status = Status::Client(client_status(connected()));
        let out = drawn(&status, uncoloured(true), 60);
        assert!(out.lines().all(|line| columns(line) < 60), "{out}");
        insta::assert_snapshot!(out);
    }

    #[test]
    fn a_server_with_one_of_two_clients_connected() {
        let status = server_status(vec![nas_connected(), not_connected("laptop", None)]);
        insta::assert_snapshot!(drawn(&status, uncoloured(true), 0));

        let mut nas = nas_connected();
        let Presence::Connected { services, .. } = &mut nas.presence else {
            unreachable!()
        };
        services[0].bind = "127.0.0.1".to_owned();
        services[1].bind = "::1".to_owned();
        let out = drawn(&server_status(vec![nas]), uncoloured(true), 0);
        assert!(out.contains("jellyfin  127.0.0.1:8096\n"), "{out}");
        assert!(out.contains("wg        [::1]:51820/udp\n"), "{out}");
    }

    #[test]
    fn a_server_with_no_client_connected_says_when_each_was_last_seen() {
        let status = server_status(vec![
            not_connected("nas", Some(180)),
            not_connected("laptop", None),
        ]);
        insta::assert_snapshot!(drawn(&status, uncoloured(true), 0));
        let lines = status_lines(&status, None, coloured(), 0);
        assert!(lines[0].starts_with("\x1b[31m✗"), "{lines:?}");

        let Status::Server(mut once) = status else {
            unreachable!()
        };
        once.unknown_keys[0].times = 1;
        let out = drawn(&Status::Server(once), uncoloured(true), 0);
        assert!(out.ends_with("  ·  1 time, last 12 min ago"), "{out}");
    }

    #[test]
    fn a_server_with_no_clients_authorized_does_not_alarm() {
        let status = server_status(Vec::new());
        let out = drawn(&status, uncoloured(true), 0);
        assert!(
            out.lines()
                .next()
                .unwrap()
                .contains("no clients are authorized"),
            "{out}"
        );
        insta::assert_snapshot!(out);
        let lines = status_lines(&status, None, coloured(), 0);
        assert!(!lines[0].contains("\x1b[31m"), "{lines:?}");
    }
}
