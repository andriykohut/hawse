//! The server's side of a status reply: what the server's events say of each client's session and
//! services, and of the keys that were turned away. Who is connected is not kept here; the
//! server's live sessions say it at the moment of the request.

use std::collections::{BTreeMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use hawse_core::server::Event;
use hawse_core::server::policy::Policy;
use hawse_core::transport::TransportKind;
use hawse_proto::key::PublicKey;
use hawse_proto::port::Port;
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::sync::watch;
use tokio::time::Instant;

use super::{Bind, ClientEntry, Knock, Presence, ServerStatus};

/// Unknown keys kept, newest first.
const KNOCKS: usize = 16;

/// What the server's events have said so far.
#[derive(Default)]
pub struct Keeper {
    /// The latest session each client name connected with, which may have ended or been replaced.
    sessions: BTreeMap<String, Session>,
    /// When the latest session to end for each name did.
    ended: BTreeMap<String, Instant>,
    knocks: VecDeque<Attempt>,
}

struct Session {
    transport: TransportKind,
    agent: String,
    since: Instant,
    services: BTreeMap<String, (IpAddr, Port)>,
}

struct Attempt {
    key: PublicKey,
    remote: SocketAddr,
    times: u64,
    last: Instant,
}

impl Keeper {
    /// Takes in `event`. An `Ended` leaves the session record in place: a newer session of the
    /// same client may already own it.
    pub fn apply(&mut self, event: &Event, now: Instant) {
        match event {
            Event::Connected {
                client,
                transport,
                agent,
                ..
            } => {
                self.sessions.insert(
                    client.clone(),
                    Session {
                        transport: *transport,
                        agent: agent.clone(),
                        since: now,
                        services: BTreeMap::new(),
                    },
                );
            }
            Event::Bound {
                client,
                service,
                bind,
                port,
            } => {
                if let Some(session) = self.sessions.get_mut(client) {
                    session.services.insert(service.clone(), (*bind, *port));
                }
            }
            Event::Unbound {
                client, service, ..
            } => {
                if let Some(session) = self.sessions.get_mut(client) {
                    session.services.remove(service);
                }
            }
            Event::Ended { client, .. } => {
                self.ended.insert(client.clone(), now);
            }
            Event::Denied { key, remote } => {
                let times = self
                    .knocks
                    .iter()
                    .position(|attempt| attempt.key == *key)
                    .and_then(|at| self.knocks.remove(at))
                    .map_or(0, |attempt| attempt.times);
                self.knocks.push_front(Attempt {
                    key: *key,
                    remote: *remote,
                    times: times + 1,
                    last: now,
                });
                self.knocks.truncate(KNOCKS);
            }
        }
    }

    /// The reply for a server that has run for `up`: one entry per grant in `policy`, by name,
    /// connected exactly when its key is among the `live` sessions. A live session this keeper
    /// did not see connect has the live remote and no details.
    pub fn snapshot(
        &self,
        policy: &Policy,
        live: &[(PublicKey, SocketAddr)],
        listen: SocketAddr,
        tcp_fallback: bool,
        up: Duration,
        now: Instant,
    ) -> ServerStatus {
        let mut clients: Vec<ClientEntry> = policy
            .grants()
            .map(|(key, grant)| {
                let name = &grant.name;
                let presence = match live.iter().find(|(live_key, _)| live_key == key) {
                    Some((_, remote)) => {
                        let session = self.sessions.get(name);
                        Presence::Connected {
                            remote: remote.to_string(),
                            transport: session.map(|s| s.transport.to_string()),
                            agent: session.map(|s| s.agent.clone()),
                            up_s: session.map(|s| now.saturating_duration_since(s.since).as_secs()),
                            services: session.map_or_else(Vec::new, |s| {
                                s.services
                                    .iter()
                                    .map(|(name, (bind, port))| Bind {
                                        name: name.clone(),
                                        bind: bind.to_string(),
                                        port: port.to_string(),
                                    })
                                    .collect()
                            }),
                        }
                    }
                    None => Presence::NotConnected {
                        last_s: self
                            .ended
                            .get(name)
                            .map(|at| now.saturating_duration_since(*at).as_secs()),
                    },
                };
                ClientEntry {
                    name: name.clone(),
                    presence,
                }
            })
            .collect();
        clients.sort_by(|a, b| a.name.cmp(&b.name));
        ServerStatus {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            listen: listen.to_string(),
            tcp_fallback,
            up_s: up.as_secs(),
            clients,
            unknown_keys: self
                .knocks
                .iter()
                .map(|attempt| Knock {
                    key: attempt.key.to_string(),
                    remote: attempt.remote.to_string(),
                    times: attempt.times,
                    last_s: now.saturating_duration_since(attempt.last).as_secs(),
                })
                .collect(),
        }
    }
}

/// Keeps `keeper` from the server whose events arrive on `events`, until `events` closes.
pub async fn keep(mut events: broadcast::Receiver<Event>, keeper: watch::Sender<Keeper>) {
    loop {
        match events.recv().await {
            Ok(event) => keeper.send_modify(|keeper| keeper.apply(&event, Instant::now())),
            // shortcut: a lag loses a knock, and one that drops a reconnect's Connected leaves the
            // previous session's transport, agent, up time and services showing as current; only
            // who is connected comes from live sessions and stays right. Rebuild from the core if
            // those details go stale in practice.
            Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::net::SocketAddr;
    use std::time::Duration;

    use hawse_core::config::ServerConfig;
    use hawse_core::server::Event;
    use hawse_core::server::policy::Policy;
    use hawse_core::transport::TransportKind;
    use hawse_proto::key::PublicKey;
    use tokio::sync::{broadcast, watch};
    use tokio::time::Instant;

    use super::*;
    use crate::config_file::parse;
    use crate::status::{Bind, ClientEntry, Presence, ServerStatus, Status};

    const LAPTOP: PublicKey = PublicKey::from_bytes([1; 32]);
    const NAS: PublicKey = PublicKey::from_bytes([2; 32]);

    fn policy(names: &[(&str, PublicKey)]) -> Policy {
        let mut text = String::new();
        for (name, key) in names {
            writeln!(
                text,
                "[clients.{name}]\nkey = \"{key}\"\nports = [\"1-65535\"]"
            )
            .unwrap();
        }
        let cfg: ServerConfig = parse(std::path::Path::new("server.toml"), &text).unwrap();
        Policy::from_config(&cfg)
    }

    fn both() -> Policy {
        policy(&[("laptop", LAPTOP), ("nas", NAS)])
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn addr(text: &str) -> SocketAddr {
        text.parse().unwrap()
    }

    fn connected(client: &str, remote: &str) -> Event {
        Event::Connected {
            client: client.to_owned(),
            remote: addr(remote),
            transport: TransportKind::Quic,
            agent: "hawse/0.11.0".to_owned(),
        }
    }

    fn bound(client: &str, service: &str, port: &str) -> Event {
        Event::Bound {
            client: client.to_owned(),
            service: service.to_owned(),
            bind: "0.0.0.0".parse().unwrap(),
            port: port.parse().unwrap(),
        }
    }

    fn ended(client: &str) -> Event {
        Event::Ended {
            client: client.to_owned(),
            reason: "closed",
        }
    }

    fn denied(n: u8) -> Event {
        Event::Denied {
            key: PublicKey::from_bytes([n; 32]),
            remote: addr("198.51.100.9:40000"),
        }
    }

    fn snapshot(
        keeper: &Keeper,
        policy: &Policy,
        live: &[(PublicKey, SocketAddr)],
        now: Instant,
    ) -> ServerStatus {
        keeper.snapshot(policy, live, addr("[::]:4433"), true, Duration::ZERO, now)
    }

    #[test]
    fn a_connected_client_reports_its_session_and_services() {
        let t0 = Instant::now();
        let mut keeper = Keeper::default();
        keeper.apply(&connected("nas", "203.0.113.7:51234"), t0);
        keeper.apply(&bound("nas", "jellyfin", "8096"), t0);
        keeper.apply(&bound("nas", "wg", "51820/udp"), t0);
        let live = [(NAS, addr("203.0.113.7:51234"))];
        let got = keeper.snapshot(
            &both(),
            &live,
            addr("[::]:4433"),
            true,
            secs(9000),
            t0 + secs(7800),
        );
        let bind = |name: &str, port: &str| Bind {
            name: name.to_owned(),
            bind: "0.0.0.0".to_owned(),
            port: port.to_owned(),
        };
        assert_eq!(
            got,
            ServerStatus {
                version: env!("CARGO_PKG_VERSION").into(),
                listen: "[::]:4433".into(),
                tcp_fallback: true,
                up_s: 9000,
                clients: vec![
                    ClientEntry {
                        name: "laptop".into(),
                        presence: Presence::NotConnected { last_s: None },
                    },
                    ClientEntry {
                        name: "nas".into(),
                        presence: Presence::Connected {
                            remote: "203.0.113.7:51234".into(),
                            transport: Some("quic".into()),
                            agent: Some("hawse/0.11.0".into()),
                            up_s: Some(7800),
                            services: vec![bind("jellyfin", "8096"), bind("wg", "51820/udp")],
                        },
                    },
                ],
                unknown_keys: vec![],
            }
        );
    }

    #[test]
    fn a_client_whose_session_ended_says_when_it_was_last_seen() {
        let t0 = Instant::now();
        let mut keeper = Keeper::default();
        keeper.apply(&connected("nas", "203.0.113.7:51234"), t0);
        keeper.apply(&ended("nas"), t0);
        let got = snapshot(&keeper, &both(), &[], t0 + secs(180));
        assert_eq!(
            got.clients[1].presence,
            Presence::NotConnected { last_s: Some(180) }
        );
    }

    #[test]
    fn a_superseded_session_that_ends_after_its_successor_connects_leaves_it_connected() {
        let t0 = Instant::now();
        let mut keeper = Keeper::default();
        keeper.apply(&connected("nas", "203.0.113.7:1111"), t0);
        keeper.apply(&connected("nas", "203.0.113.8:2222"), t0);
        keeper.apply(&ended("nas"), t0);
        let live = [(NAS, addr("203.0.113.8:2222"))];
        let got = snapshot(&keeper, &both(), &live, t0);
        let Presence::Connected { remote, .. } = &got.clients[1].presence else {
            panic!("not connected");
        };
        assert_eq!(remote, "203.0.113.8:2222");
    }

    #[test]
    fn who_is_connected_follows_the_live_sessions() {
        let t0 = Instant::now();
        let mut keeper = Keeper::default();
        keeper.apply(&connected("nas", "203.0.113.7:51234"), t0);
        let live = [(LAPTOP, addr("192.0.2.4:5000"))];
        let got = snapshot(&keeper, &both(), &live, t0);
        assert_eq!(
            got.clients[0].presence,
            Presence::Connected {
                remote: "192.0.2.4:5000".into(),
                transport: None,
                agent: None,
                up_s: None,
                services: vec![],
            }
        );
        assert_eq!(
            got.clients[1].presence,
            Presence::NotConnected { last_s: None }
        );
    }

    #[test]
    fn unknown_keys_keep_the_16_latest_newest_first() {
        let t0 = Instant::now();
        let mut keeper = Keeper::default();
        for n in 1..=17 {
            keeper.apply(&denied(n), t0);
        }
        keeper.apply(&denied(5), t0 + secs(30));
        let got = snapshot(&keeper, &both(), &[], t0 + secs(40)).unknown_keys;
        let key = |n: u8| PublicKey::from_bytes([n; 32]).to_string();
        assert_eq!(got.len(), KNOCKS);
        assert_eq!(got[0].key, key(5));
        assert_eq!(got[0].times, 2);
        assert_eq!(got[0].last_s, 10);
        assert_eq!(got[1].key, key(17));
        assert!(got.iter().all(|knock| knock.key != key(1)));
    }

    #[test]
    fn a_client_removed_by_a_reload_drops_out_of_the_list() {
        let t0 = Instant::now();
        let mut keeper = Keeper::default();
        keeper.apply(&connected("nas", "203.0.113.7:51234"), t0);
        let got = snapshot(&keeper, &policy(&[("laptop", LAPTOP)]), &[], t0);
        let names: Vec<_> = got.clients.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["laptop"]);
    }

    #[test]
    fn a_server_status_survives_a_round_trip_through_json() {
        let t0 = Instant::now();
        let mut keeper = Keeper::default();
        keeper.apply(&connected("nas", "203.0.113.7:51234"), t0);
        keeper.apply(&bound("nas", "jellyfin", "8096"), t0);
        keeper.apply(&denied(9), t0);
        let live = [(NAS, addr("203.0.113.7:51234"))];
        let status = Status::Server(snapshot(&keeper, &both(), &live, t0));
        let text = serde_json::to_string(&status).unwrap();
        assert_eq!(serde_json::from_str::<Status>(&text).unwrap(), status);
        assert!(text.contains(r#""role":"server""#), "{text}");
        assert!(text.contains(r#""state":"connected""#), "{text}");
        assert!(text.contains(r#""state":"not_connected""#), "{text}");
    }

    #[tokio::test]
    async fn keep_applies_events_until_the_channel_closes() {
        let (tx, events) = broadcast::channel(8);
        let (keeper, kept) = watch::channel(Keeper::default());
        let task = tokio::spawn(keep(events, keeper));
        tx.send(denied(3)).unwrap();
        drop(tx);
        task.await.unwrap();
        assert_eq!(kept.borrow().knocks.len(), 1);
    }
}
