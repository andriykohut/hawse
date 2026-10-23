//! The client's side of a status reply: what the client's events say of its link and of each
//! service's bind, kept so that a request can be answered without the client having to be asked.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use hawse_core::client::Event;
use hawse_core::config::ClientConfig;
use hawse_core::traffic::Counts;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use super::{Answer, ClientStatus, LinkStatus, ServiceStatus};
use crate::commands::client::reason;
use crate::panel::Shown;
use crate::terminal::Link;

/// What the client's events have said so far. A new client starts from [`Model::reset`].
#[derive(Default)]
pub struct Model {
    link: Link,
    /// The server's name and version, from the session's welcome.
    agent: String,
    /// Why the last session ended, as the log says it.
    cause: String,
    answers: BTreeMap<String, Answer>,
}

impl Model {
    /// Takes in `event`, moving the link as the panel's does.
    pub fn apply(&mut self, event: &Event, now: Instant) {
        match event {
            Event::Connected {
                transport,
                round_trip,
                agent,
                ..
            } => {
                self.link = Link::Up {
                    transport: *transport,
                    round_trip: *round_trip,
                    since: now,
                };
                self.agent.clone_from(agent);
                // A new session binds afresh, and the last one's ports may be gone.
                self.answers.clear();
            }
            Event::RoundTrip { round_trip: latest } => {
                if let Link::Up { round_trip, .. } = &mut self.link {
                    *round_trip = *latest;
                }
            }
            Event::Bound { service, port } => {
                let port = port.to_string();
                self.answers.insert(service.clone(), Answer::Bound { port });
            }
            Event::BindFailed { service, reason } => {
                let reason = reason.to_string();
                self.answers
                    .insert(service.clone(), Answer::Refused { reason });
            }
            Event::Denied { .. } => {}
            Event::Disconnected { cause, retry_in } => {
                self.link = match retry_in {
                    Some(wait) => Link::Down {
                        since: match self.link {
                            Link::Down { since, .. } => since,
                            _ => now,
                        },
                        retry_at: now + *wait,
                    },
                    None => Link::Stopped,
                };
                self.cause = reason(cause.clone());
            }
        }
    }

    /// Becomes a new client's model, yet to connect.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// The reply for a client that has run for `up`. Services come in `cfg`'s order, each with
    /// its figures from `totals` (zero when absent) and its answer, `Pending` when it has none.
    /// A client that has stopped for good reads as `Down` with no retry; it has no start time
    /// for the outage, so `down_s` is 0.
    pub fn snapshot(
        &self,
        cfg: &ClientConfig,
        totals: &BTreeMap<String, Counts>,
        up: Duration,
        now: Instant,
    ) -> ClientStatus {
        let link = match self.link {
            Link::Connecting => LinkStatus::Connecting,
            Link::Up {
                transport,
                round_trip,
                since,
            } => LinkStatus::Up {
                transport: transport.to_string(),
                round_trip_ms: u64::try_from(round_trip.as_millis()).unwrap_or(u64::MAX),
                up_s: now.saturating_duration_since(since).as_secs(),
                agent: self.agent.clone(),
            },
            Link::Down { since, retry_at } => LinkStatus::Down {
                down_s: now.saturating_duration_since(since).as_secs(),
                retry_in_s: retry_at.saturating_duration_since(now).as_secs(),
                cause: self.cause.clone(),
            },
            Link::Stopped => LinkStatus::Down {
                down_s: 0,
                retry_in_s: 0,
                cause: self.cause.clone(),
            },
        };
        let services = cfg
            .expose
            .iter()
            .map(|(name, expose)| {
                let counts = totals.get(name).copied().unwrap_or_default();
                ServiceStatus {
                    name: name.clone(),
                    local: expose.local.clone(),
                    port: expose.port.to_string(),
                    answer: self.answers.get(name).cloned().unwrap_or(Answer::Pending),
                    open: counts.open,
                    visitors: counts.visitors,
                    down: counts.down,
                    up: counts.up,
                    datagrams_down: counts.datagrams_down,
                    datagrams_up: counts.datagrams_up,
                    dropped: counts.dropped,
                }
            })
            .collect();
        ClientStatus {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            server: cfg.server.clone(),
            up_s: up.as_secs(),
            link,
            services,
        }
    }
}

/// Keeps `model` from the client whose events arrive on `events`, passing each on to `forward`
/// in order, until `events` closes. A reload that makes a new client resets the model before
/// any event that client sends.
pub async fn keep(
    mut events: mpsc::Receiver<Event>,
    forward: mpsc::Sender<Event>,
    mut showing: watch::Receiver<Shown>,
    model: watch::Sender<Model>,
) {
    let mut traffic = Arc::clone(&showing.borrow().traffic);
    loop {
        tokio::select! {
            // First, so a new client is taken in before any event it sends, as the panel does.
            biased;
            Ok(()) = showing.changed() => {
                let latest = Arc::clone(&showing.borrow().traffic);
                if !Arc::ptr_eq(&latest, &traffic) {
                    traffic = latest;
                    model.send_modify(Model::reset);
                }
            }
            event = events.recv() => {
                let Some(event) = event else { break };
                model.send_modify(|model| model.apply(&event, Instant::now()));
                // A printer that has gone leaves the model still worth keeping.
                let _ = forward.send(event).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use hawse_core::client::{DisconnectCause, Event};
    use hawse_core::config::ClientConfig;
    use hawse_core::traffic::{Counts, Traffic};
    use hawse_core::transport::TransportKind;
    use hawse_proto::msg::BindFailure;
    use hawse_proto::port::{Kind, Port};
    use tokio::sync::{mpsc, watch};
    use tokio::time::Instant;

    use super::*;
    use crate::config_file::parse;
    use crate::panel::Shown;
    use crate::status::{Answer, ClientStatus, LinkStatus, ServiceStatus, Status};
    use crate::terminal::Link;

    fn cfg() -> ClientConfig {
        parse(
            std::path::Path::new("client.toml"),
            "server = \"tunnel.example.com:4433\"\nserver_key = \"ed25519:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\"\n\n[expose.jellyfin]\nlocal = \"127.0.0.1:8096\"\nport = 8096\n\n[expose.ssh]\nlocal = \"127.0.0.1:22\"\nport = 2222\n\n[expose.wg]\nlocal = \"127.0.0.1:51820\"\nport = \"51820/udp\"\n",
        )
        .unwrap()
    }

    fn connected() -> Event {
        Event::Connected {
            remote: "203.0.113.7:4433".parse().unwrap(),
            transport: TransportKind::Quic,
            name: "laptop".to_owned(),
            agent: "hawse/0.11.0".to_owned(),
            round_trip: Duration::from_millis(38),
        }
    }

    fn bound(service: &str, number: u16) -> Event {
        Event::Bound {
            service: service.to_owned(),
            port: Port {
                number,
                kind: Kind::Tcp,
            },
        }
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn snapshot(model: &Model, now: Instant) -> ClientStatus {
        model.snapshot(&cfg(), &BTreeMap::new(), Duration::ZERO, now)
    }

    #[test]
    fn a_connected_client_reports_its_link_and_each_services_answer() {
        let t0 = Instant::now();
        let mut model = Model::default();
        model.apply(&connected(), t0);
        model.apply(&bound("jellyfin", 8096), t0);
        model.apply(
            &Event::BindFailed {
                service: "ssh".to_owned(),
                reason: BindFailure::NotGranted,
            },
            t0,
        );
        let totals = BTreeMap::from([(
            "jellyfin".to_owned(),
            Counts {
                open: 2,
                down: 14_200_000,
                up: 210_000,
                ..Counts::default()
            },
        )]);
        let got = model.snapshot(&cfg(), &totals, secs(7800), t0 + secs(7800));
        let service = |name: &str, local: &str, port: &str, answer, counts: Counts| ServiceStatus {
            name: name.to_owned(),
            local: local.to_owned(),
            port: port.to_owned(),
            answer,
            open: counts.open,
            visitors: 0,
            down: counts.down,
            up: counts.up,
            datagrams_down: 0,
            datagrams_up: 0,
            dropped: 0,
        };
        assert_eq!(
            got,
            ClientStatus {
                version: env!("CARGO_PKG_VERSION").into(),
                server: "tunnel.example.com:4433".into(),
                up_s: 7800,
                link: LinkStatus::Up {
                    transport: "quic".into(),
                    round_trip_ms: 38,
                    up_s: 7800,
                    agent: "hawse/0.11.0".into()
                },
                services: vec![
                    service(
                        "jellyfin",
                        "127.0.0.1:8096",
                        "8096",
                        Answer::Bound {
                            port: "8096".into()
                        },
                        Counts {
                            open: 2,
                            down: 14_200_000,
                            up: 210_000,
                            ..Counts::default()
                        }
                    ),
                    service(
                        "ssh",
                        "127.0.0.1:22",
                        "2222",
                        Answer::Refused {
                            reason: "port is not granted to this client".into()
                        },
                        Counts::default()
                    ),
                    service(
                        "wg",
                        "127.0.0.1:51820",
                        "51820/udp",
                        Answer::Pending,
                        Counts::default()
                    ),
                ],
            }
        );
    }

    #[test]
    fn a_ping_updates_the_round_trip() {
        let t0 = Instant::now();
        let mut model = Model::default();
        model.apply(&connected(), t0);
        model.apply(
            &Event::RoundTrip {
                round_trip: Duration::from_millis(52),
            },
            t0,
        );
        let LinkStatus::Up { round_trip_ms, .. } = snapshot(&model, t0).link else {
            panic!("not up");
        };
        assert_eq!(round_trip_ms, 52);
    }

    #[test]
    fn a_client_that_lost_its_server_is_down_since_the_first_failure() {
        let t0 = Instant::now();
        let mut model = Model::default();
        let lost = Event::Disconnected {
            cause: DisconnectCause::Unresponsive,
            retry_in: Some(secs(8)),
        };
        model.apply(&lost, t0);
        model.apply(&lost, t0 + secs(60));
        assert_eq!(
            snapshot(&model, t0 + secs(62)).link,
            LinkStatus::Down {
                down_s: 62,
                retry_in_s: 6,
                cause: "server stopped answering".into()
            }
        );
    }

    #[test]
    fn a_reconnect_starts_the_answers_again() {
        let t0 = Instant::now();
        let mut model = Model::default();
        model.apply(&connected(), t0);
        model.apply(&bound("jellyfin", 8096), t0);
        model.apply(&connected(), t0);
        let status = snapshot(&model, t0);
        assert!(status.services.iter().all(|s| s.answer == Answer::Pending));
    }

    #[tokio::test]
    async fn keep_forwards_every_event_in_order_and_resets_on_a_new_client() {
        let (tx, events) = mpsc::channel(8);
        let (forward, mut forwarded) = mpsc::channel(8);
        let (shown, showing) = watch::channel(Shown {
            cfg: cfg(),
            traffic: Arc::new(Traffic::default()),
        });
        let (model, mut modelled) = watch::channel(Model::default());
        let kept = tokio::spawn(keep(events, forward, showing, model));
        tx.send(connected()).await.unwrap();
        tx.send(bound("jellyfin", 8096)).await.unwrap();
        assert!(matches!(
            forwarded.recv().await,
            Some(Event::Connected { .. })
        ));
        assert!(matches!(forwarded.recv().await, Some(Event::Bound { .. })));
        // Marks the model seen, so `changed` below waits for the reset.
        assert!(matches!(modelled.borrow_and_update().link, Link::Up { .. }));

        shown.send_modify(|shown| shown.traffic = Arc::new(Traffic::default()));
        modelled.changed().await.unwrap();
        assert!(matches!(modelled.borrow().link, Link::Connecting));

        drop(tx);
        kept.await.unwrap();
        assert!(forwarded.recv().await.is_none());
    }

    #[test]
    fn a_status_survives_a_round_trip_through_json() {
        let t0 = Instant::now();
        let mut model = Model::default();
        model.apply(&connected(), t0);
        let status = Status::Client(snapshot(&model, t0));
        let text = serde_json::to_string(&status).unwrap();
        assert_eq!(serde_json::from_str::<Status>(&text).unwrap(), status);
        assert!(text.contains(r#""role":"client""#), "{text}");
        assert!(text.contains(r#""state":"up""#), "{text}");
    }
}
