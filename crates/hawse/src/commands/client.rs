use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hawse_core::client::{Client, DisconnectCause, Event};
use hawse_core::config::{ClientConfig, split_host_port};
use hawse_core::identity::Identity;
use hawse_core::traffic::Counts;
use miette::{IntoDiagnostic as _, WrapErr as _};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use super::Reloads;
use crate::config_file;
use crate::notify;
use crate::panel::{self, Shown};
use crate::paths::{self, Role};
use crate::screen;
use crate::terminal::{self, Style};

/// Reads `file` and no other: a reload that looked the config up again could find one that has
/// since appeared somewhere the lookup tries first, which is not the one being watched.
fn load(file: &Path, terminal: Option<Style>) -> miette::Result<(ClientConfig, Identity)> {
    let (cfg, located) = config_file::load_client(Some(file.to_owned()))?;
    let inert = cfg.transport.inert();
    if !inert.is_empty() {
        tracing::warn!(
            config = %located.file.display(),
            settings = inert.join(", "),
            "[transport] settings that tune QUIC do nothing under prefer = \"tcp\""
        );
    }
    let key_path = paths::key_path(Role::Client, &located, cfg.key.as_deref());
    let (identity, created) = Identity::load_or_create(&key_path)
        .into_diagnostic()
        .wrap_err_with(|| format!("cannot load the client key at {}", key_path.display()))?;
    if created {
        tracing::info!(path = %key_path.display(), key = %identity.public_key(), "created client key");
        if let Some(style) = terminal {
            let made = format!("created client key at {}", key_path.display());
            terminal::print(&terminal::note(&made, style));
        }
    }
    Ok((cfg, identity))
}

/// Why a session ended, as the log says it.
pub(crate) fn reason(cause: DisconnectCause) -> String {
    match cause {
        DisconnectCause::Shutdown(why) => format!("server ended the session: {why}"),
        DisconnectCause::Unresponsive => "server stopped answering".to_owned(),
        DisconnectCause::Denied => "this machine's key is not authorized on the server".to_owned(),
        DisconnectCause::Transport(reason) | DisconnectCause::Config(reason) => reason,
    }
}

/// `duration` in whole milliseconds as a number: a `u128` is written to JSON as a string.
fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Logs what the client reports, and says whether it has stopped for good. `cfg` is the config in
/// force, which a service's addresses are named from.
fn report(event: Event, cfg: &ClientConfig) -> bool {
    let mut stopped = false;
    match event {
        Event::Connected {
            remote,
            transport,
            name,
            agent,
            round_trip,
        } => {
            let round_trip_ms = millis(round_trip);
            tracing::info!(%remote, %transport, name = %name, server = %agent, round_trip_ms, "connected");
        }
        Event::Bound { service, port } => {
            let host = split_host_port(&cfg.server)
                .map(|(host, _)| host)
                .unwrap_or_default();
            let local = cfg.expose.get(&service).map_or("", |e| e.local.as_str());
            tracing::info!("{service}  {host}:{port} <- {local}");
        }
        Event::BindFailed { service, reason } => tracing::warn!("{service}  {reason}"),
        Event::RoundTrip { round_trip } => {
            tracing::debug!(round_trip_ms = millis(round_trip), "round trip");
        }
        Event::Denied { key } => tracing::warn!(
            "not authorized. on the server, add to server.toml:\n[clients.NAME]\nkey = \"{key}\"\nthen wait for the next retry"
        ),
        Event::Disconnected { cause, retry_in } => {
            let reason = reason(cause);
            stopped = retry_in.is_none();
            match retry_in {
                Some(wait) => tracing::warn!(
                    %reason,
                    "disconnected; retrying in {:.1} s",
                    wait.as_secs_f64()
                ),
                None => {
                    tracing::error!(%reason, "disconnected; the config cannot be retried");
                }
            }
        }
    }
    stopped
}

/// Logs what the client reports until `events` closes, keeping `status` to it when systemd
/// watches. Returns whether the client stopped for good, and how many times it connected.
async fn log(
    mut events: mpsc::Receiver<Event>,
    mut showing: watch::Receiver<Shown>,
    status: Option<notify::Status>,
) -> (bool, u64) {
    let (mut stopped, mut connects) = (false, 0);
    let mut status = status.map(|status| (status, notify::Client::default()));
    let mut traffic = Arc::clone(&showing.borrow().traffic);
    loop {
        tokio::select! {
            // First, so a new client is taken in before any event it sends, as the panel does.
            biased;
            // A reload can take a service away with no event to say so. One that reconnects
            // makes a new client, yet to connect, and the session it ended was cancelled, which
            // sends no event either.
            Ok(()) = showing.changed() => {
                let latest = Arc::clone(&showing.borrow().traffic);
                if !Arc::ptr_eq(&latest, &traffic) {
                    traffic = latest;
                    if let Some((_, client)) = &mut status {
                        *client = notify::Client::default();
                    }
                }
            }
            event = events.recv() => {
                let Some(event) = event else { break };
                connects += u64::from(matches!(event, Event::Connected { .. }));
                if let Some((_, client)) = &mut status {
                    client.apply(&event, &showing.borrow().cfg);
                }
                stopped |= report(event, &showing.borrow().cfg);
            }
        }
        if let Some((status, client)) = &mut status {
            status.show(client.line(&showing.borrow().cfg));
        }
    }
    (stopped, connects)
}

/// 2 when a config error stopped the client, the exit of a config that does not load: starting
/// it again will not fix either.
pub async fn run(config: Option<PathBuf>, terminal: Option<Style>) -> miette::Result<ExitCode> {
    let file = paths::locate(Role::Client, config).file;
    let mut reloads = Reloads::new(file.clone());
    let (mut cfg, mut identity) = load(&file, terminal)?;
    let started = Instant::now();
    // Every finished client's totals, summed by service: a reload that reconnects starts a new
    // client, whose totals start again.
    let mut carried = BTreeMap::<String, Counts>::new();
    let cancel = CancellationToken::new();
    tokio::spawn(super::shutdown_signal(cancel.clone()));
    // What the printer names a service's addresses from and reads its figures from, replaced by
    // each reload and each new client.
    let (shown, showing) = watch::channel(Shown {
        cfg: cfg.clone(),
        traffic: Arc::default(),
    });
    // Ready once the config is read, and not at the first connect, so a server that is down does
    // not fail the unit's start.
    // shortcut: only the log lines keep the status after this, as a unit's stderr is never a
    // terminal; one run on a terminal under systemd says `connecting` throughout.
    let status =
        notify::socket().map(|socket| notify::Status::ready(socket, "connecting".to_owned()));
    let (tx, events) = mpsc::channel(64);
    let printer = match terminal {
        Some(style) => tokio::spawn(panel::show(events, showing, style, screen::draw)),
        None => tokio::spawn(log(events, showing, status)),
    };
    // One turn per connection: a reload that changes more than `[expose.*]` ends the client it
    // found and starts another on what it read.
    loop {
        tracing::info!(key = %identity.public_key(), "client key");
        let client = Client::new(cfg.clone(), identity);
        shown.send_modify(|shown| shown.traffic = client.traffic());
        let session = cancel.child_token();
        let run = client.run(session.clone(), tx.clone());
        tokio::pin!(run);
        let next = loop {
            tokio::select! {
                () = &mut run => break None,
                () = reloads.next() => {}
            }
            let (new, identity) = match load(&file, terminal) {
                Ok(loaded) => loaded,
                Err(report) => {
                    super::not_reloaded(&report);
                    continue;
                }
            };
            shown.send_modify(|shown| shown.cfg = new.clone());
            if cfg.needs_reconnect(&new) {
                tracing::info!("config reloaded; reconnecting, as more than [expose.*] changed");
                if let Some(style) = terminal {
                    terminal::print(&terminal::note("config reloaded; reconnecting", style));
                }
                session.cancel();
                run.await;
                break Some((new, identity));
            }
            tracing::info!(services = new.expose.len(), "config reloaded");
            if let Some(style) = terminal {
                terminal::print(&terminal::note("config reloaded", style));
            }
            client.expose().send_replace(new.expose.clone());
            cfg = new;
        };
        for (service, counts) in client.traffic().snapshot() {
            *carried.entry(service).or_default() += counts;
        }
        let Some(next) = next else { break };
        (cfg, identity) = next;
    }
    // The printer ends when the last sender does.
    drop(tx);
    let (stopped, connects) = printer.await.into_diagnostic()?;
    let (ran, reconnects) = (started.elapsed(), connects.saturating_sub(1));
    tracing::info!(uptime_s = ran.as_secs(), reconnects, "stopped");
    for (service, counts) in &carried {
        tracing::info!(
            service,
            visitors = counts.visitors,
            down = counts.down,
            up = counts.up,
            datagrams_down = counts.datagrams_down,
            datagrams_up = counts.datagrams_up,
            dropped = counts.dropped,
            "service totals"
        );
    }
    if let Some(style) = terminal {
        screen::release();
        terminal::print(&terminal::summary(ran, reconnects, &carried, style));
    }
    Ok(if stopped {
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixDatagram;

    use hawse_core::transport::TransportKind;

    use super::*;

    /// The next line `systemd` is sent, waiting up to 2 s for it.
    async fn next_line(systemd: &UnixDatagram) -> String {
        let mut buf = [0u8; 512];
        for _ in 0..200 {
            if let Ok(len) = systemd.recv(&mut buf) {
                return String::from_utf8(buf[..len].to_vec()).unwrap();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("nothing sent within 2 s");
    }

    #[tokio::test]
    async fn a_reload_that_reconnects_says_connecting_until_the_new_client_has() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notify");
        let systemd = UnixDatagram::bind(&path).unwrap();
        systemd.set_nonblocking(true).unwrap();
        let cfg = config_file::parse(
            Path::new("client.toml"),
            r#"
server = "tunnel.example.com:4433"
server_key = "ed25519:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
"#,
        )
        .unwrap();
        let (shown, showing) = watch::channel(Shown {
            cfg,
            traffic: Arc::default(),
        });
        let (tx, events) = mpsc::channel(8);
        let status = notify::Status::ready(path.into_os_string(), "connecting".to_owned());
        let printer = tokio::spawn(log(events, showing, Some(status)));
        assert_eq!(next_line(&systemd).await, "READY=1\nSTATUS=connecting");
        let connected = Event::Connected {
            remote: "203.0.113.7:4433".parse().unwrap(),
            transport: TransportKind::Quic,
            name: "nas".to_owned(),
            agent: "hawse/0.8.0".to_owned(),
            round_trip: Duration::from_millis(24),
        };
        tx.send(connected.clone()).await.unwrap();
        assert_eq!(next_line(&systemd).await, "STATUS=connected over QUIC");

        // The session the reload ends is cancelled, which sends no event.
        shown.send_modify(|shown| shown.traffic = Arc::default());
        assert_eq!(next_line(&systemd).await, "STATUS=connecting");
        tx.send(connected).await.unwrap();
        assert_eq!(next_line(&systemd).await, "STATUS=connected over QUIC");

        drop(tx);
        printer.await.unwrap();
    }
}
