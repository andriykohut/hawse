//! The client's panel on a terminal: what it knows of the connection and of each service, kept
//! from the client's events and sampled from its traffic, and drawn when that changes. It samples
//! once a second only while bytes move or a countdown runs, so a client with nothing moving wakes
//! for its events alone.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use hawse_core::client::{DisconnectCause, Event};
use hawse_core::config::{ClientConfig, Expose};
use hawse_core::traffic::{Counts, Traffic};
use hawse_proto::key::PublicKey;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{mpsc, watch};
use tokio::time::{Instant, MissedTickBehavior};

use crate::screen;
use crate::terminal::{self, Link, Row, Style};

const SAMPLE: Duration = Duration::from_secs(1);

/// What the panel names a service's addresses from and reads its figures from, replaced when a
/// reload brings a new config or a new client.
pub struct Shown {
    pub cfg: ClientConfig,
    pub traffic: Arc<Traffic>,
}

/// What a `Row::Bound` is drawn from: the server's answer, then the sampled figures.
#[derive(Default)]
struct State {
    link: Link,
    /// The server's answer to each service's bind, with the settings it was asked for.
    answers: BTreeMap<String, (Expose, Answer)>,
    /// Each service's open count and its bytes per second over the last sample.
    figures: BTreeMap<String, Counts>,
    /// The totals the last sample read, which the next one's rates count from.
    counted: BTreeMap<String, Counts>,
    /// The cause of the outage last printed: a retry that fails the same way prints nothing.
    outage: Option<DisconnectCause>,
    /// The key the not-authorized table was last printed for.
    denied: Option<PublicKey>,
}

#[derive(Clone, Copy)]
enum Answer {
    Bound(hawse_proto::port::Port),
    Refused(hawse_proto::msg::BindFailure),
}

impl State {
    /// Takes in `event`, and returns what to print above the panel for it, if anything. `cfg` is
    /// the config in force, whose settings a bind's answer is recorded against.
    fn apply(
        &mut self,
        event: &Event,
        now: Instant,
        style: Style,
        cfg: &ClientConfig,
    ) -> Option<String> {
        let mut answer = |service: &String, answer| {
            if let Some(expose) = cfg.expose.get(service) {
                self.answers
                    .insert(service.clone(), (expose.clone(), answer));
            }
        };
        let repeat = terminal::repeats(&mut self.denied, event);
        match event {
            Event::Connected {
                transport,
                round_trip,
                ..
            } => {
                self.link = Link::Up {
                    transport: *transport,
                    round_trip: *round_trip,
                    since: now,
                };
                self.outage = None;
                // A new session binds afresh, and the last one's ports may be gone.
                self.answers.clear();
            }
            Event::RoundTrip { round_trip: latest } => {
                if let Link::Up { round_trip, .. } = &mut self.link {
                    *round_trip = *latest;
                }
            }
            Event::Bound { service, port } => answer(service, Answer::Bound(*port)),
            Event::BindFailed { service, reason } => answer(service, Answer::Refused(*reason)),
            Event::Denied { .. } if repeat => return None,
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
                if retry_in.is_some() && self.outage.as_ref() == Some(cause) {
                    return None;
                }
                self.outage = Some(cause.clone());
            }
        }
        terminal::event(event, style)
    }

    /// Each service's figures from `totals`, read `elapsed` after the last sample. A total
    /// smaller than the last, from a client that started again, reads as no rate.
    fn sample(&mut self, totals: BTreeMap<String, Counts>, elapsed: Duration) {
        let millis = elapsed.as_millis().max(1);
        let per_second = |now: u64, then: u64| {
            u64::try_from(u128::from(now.saturating_sub(then)) * 1000 / millis).unwrap_or(u64::MAX)
        };
        self.figures = totals
            .iter()
            .map(|(service, now)| {
                let then = self.counted.get(service).copied().unwrap_or_default();
                let figures = Counts {
                    open: now.open,
                    down: per_second(now.down, then.down),
                    up: per_second(now.up, then.up),
                };
                (service.clone(), figures)
            })
            .collect();
        self.counted = totals;
    }

    /// The open counts from `totals`, leaving the rates and what they count from alone.
    fn recount(&mut self, totals: &BTreeMap<String, Counts>) {
        for (service, now) in totals {
            self.figures.entry(service.clone()).or_default().open = now.open;
        }
    }

    /// Each service's row, leaving out a service whose settings in `cfg` differ from those its
    /// answer was for: a reload has asked again, and the old answer no longer holds.
    fn rows(&self, cfg: &ClientConfig) -> BTreeMap<String, Row> {
        self.answers
            .iter()
            .filter(|(service, (expose, _))| cfg.expose.get(*service) == Some(expose))
            .map(|(service, (_, answer))| {
                let row = match *answer {
                    Answer::Refused(reason) => Row::Refused(reason),
                    Answer::Bound(port) => {
                        let Counts { open, down, up } =
                            self.figures.get(service).copied().unwrap_or_default();
                        Row::Bound {
                            port,
                            open,
                            down,
                            up,
                        }
                    }
                };
                (service.clone(), row)
            })
            .collect()
    }
}

/// Keeps the panel for the client whose events arrive on `events`, handing each frame and the
/// terminal's width to `draw`, until `events` closes. Returns whether the client stopped for
/// good.
pub async fn show(
    mut events: mpsc::Receiver<Event>,
    mut shown: watch::Receiver<Shown>,
    style: Style,
    mut draw: impl FnMut(Vec<String>, usize),
) -> bool {
    let mut state = State::default();
    let mut traffic = Arc::clone(&shown.borrow().traffic);
    let mut stopped = false;
    let mut ticking = false;
    let mut sampled = Instant::now();
    let mut tick = tokio::time::interval_at(Instant::now() + SAMPLE, SAMPLE);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut resized = signal(SignalKind::window_change()).expect("sigwinch handler");
    // Drawn at the top, so `connecting` shows before the first event and every change after
    // it is drawn before the next wait.
    loop {
        let latest = Arc::clone(&shown.borrow().traffic);
        // A new client, which has yet to connect, and whose totals start again.
        if !Arc::ptr_eq(&latest, &traffic) {
            traffic = latest;
            state.link = Link::Connecting;
            state.counted.clear();
            state.figures.clear();
        }
        let (width, height) = screen::size();
        let rows = state.rows(&shown.borrow().cfg);
        let frame = terminal::frame(
            state.link,
            &rows,
            &shown.borrow().cfg,
            style,
            Instant::now(),
            (width, height),
        );
        draw(frame, width);
        let counting_down =
            matches!(state.link, Link::Down { retry_at, .. } if retry_at > Instant::now());
        tokio::select! {
            // First, so a new client is taken in before any event it sends: the client sends
            // its first event only after the reload that made it.
            biased;
            Ok(()) = shown.changed() => {}
            event = events.recv() => {
                let Some(event) = event else { break };
                stopped |= matches!(event, Event::Disconnected { retry_in: None, .. });
                let line = state.apply(&event, Instant::now(), style, &shown.borrow().cfg);
                if let Some(line) = line {
                    screen::print(&line);
                }
            }
            _ = resized.recv() => {}
            () = traffic.moved(), if !ticking => {
                ticking = true;
                tick.reset();
                // The bytes that woke this are counted by the first tick, over the second
                // from here.
                sampled = Instant::now();
                state.recount(&traffic.snapshot());
            }
            _ = tick.tick(), if ticking || counting_down => {
                if ticking {
                    // Cleared before the totals are read: bytes that land between the two are
                    // in this sample and set the flag again, so the last sample drawn is never
                    // one that still has bytes in it.
                    ticking = traffic.take_moved();
                    state.sample(traffic.snapshot(), sampled.elapsed());
                    sampled = Instant::now();
                }
            }
        }
    }
    stopped
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use hawse_core::transport::TransportKind;
    use hawse_proto::port::{Kind, Port};

    use super::*;
    use crate::config_file::parse;

    const STYLE: Style = Style {
        color: false,
        unicode: true,
        accent: None,
    };

    fn cfg() -> ClientConfig {
        parse(
            std::path::Path::new("client.toml"),
            "server = \"tunnel.example.com:4433\"\nserver_key = \"ed25519:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\"\n\n[expose.web]\nlocal = \"127.0.0.1:3000\"\n",
        )
        .unwrap()
    }

    #[test]
    fn a_smaller_total_reads_as_no_rate() {
        let mut state = State::default();
        let counts = |down| {
            BTreeMap::from([(
                "web".to_owned(),
                Counts {
                    open: 1,
                    down,
                    up: 0,
                },
            )])
        };
        state.sample(counts(5000), Duration::from_secs(1));
        // A reload that reconnected: a new client, whose totals start again.
        state.sample(counts(10), Duration::from_secs(1));
        assert_eq!(
            state.figures["web"],
            Counts {
                open: 1,
                down: 0,
                up: 0
            }
        );
    }

    fn bound(number: u16) -> Event {
        Event::Bound {
            service: "web".to_owned(),
            port: Port {
                number,
                kind: Kind::Tcp,
            },
        }
    }

    #[test]
    fn a_service_a_reload_changed_has_no_row_until_its_new_bind_is_answered() {
        let mut state = State::default();
        let before = cfg();
        let mut after = cfg();
        after.expose.get_mut("web").unwrap().local = "127.0.0.1:4000".to_owned();
        state.apply(&bound(8080), Instant::now(), STYLE, &before);
        assert!(state.rows(&before).contains_key("web"));
        assert!(!state.rows(&after).contains_key("web"));
        state.apply(&bound(8081), Instant::now(), STYLE, &after);
        assert!(state.rows(&after).contains_key("web"));
    }

    #[test]
    fn a_new_session_shows_no_answer_from_the_last() {
        let mut state = State::default();
        state.apply(&bound(8080), Instant::now(), STYLE, &cfg());
        let connected = Event::Connected {
            remote: "203.0.113.7:4433".parse().unwrap(),
            transport: TransportKind::Quic,
            name: "laptop".to_owned(),
            agent: "hawse/0.8.0".to_owned(),
            round_trip: Duration::from_millis(24),
        };
        state.apply(&connected, Instant::now(), STYLE, &cfg());
        assert!(state.rows(&cfg()).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_reload_is_drawn_without_waiting_for_an_event() {
        let (tx, events) = mpsc::channel(8);
        let mut two = cfg();
        let web = two.expose["web"].clone();
        two.expose.insert("other".to_owned(), web);
        let (shown, showing) = watch::channel(Shown {
            cfg: two,
            traffic: Arc::default(),
        });
        let frames = Arc::new(std::sync::Mutex::new(Vec::new()));
        let kept = Arc::clone(&frames);
        let panel = tokio::spawn(show(events, showing, STYLE, move |rows, _| {
            kept.lock().unwrap().push(rows.join("\n"));
        }));
        tx.send(Event::Connected {
            remote: "203.0.113.7:4433".parse().unwrap(),
            transport: TransportKind::Quic,
            name: "laptop".to_owned(),
            agent: "hawse/0.8.0".to_owned(),
            round_trip: Duration::from_millis(24),
        })
        .await
        .unwrap();
        for (service, number) in [("web", 8080), ("other", 8081)] {
            tx.send(Event::Bound {
                service: service.to_owned(),
                port: Port {
                    number,
                    kind: Kind::Tcp,
                },
            })
            .await
            .unwrap();
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        let last = || frames.lock().unwrap().last().cloned().unwrap();
        assert!(last().contains("other"), "{}", last());

        shown.send_modify(|shown| {
            shown.cfg.expose.remove("other");
        });
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(!last().contains("other"), "{}", last());

        // A reload that reconnects: a new client, which has yet to connect.
        shown.send_modify(|shown| shown.traffic = Arc::default());
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(last().contains("connecting"), "{}", last());

        drop(tx);
        panel.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn with_nothing_moving_only_events_draw() {
        let (tx, events) = mpsc::channel(8);
        let traffic = Arc::new(Traffic::default());
        let (_shown, showing) = watch::channel(Shown {
            cfg: cfg(),
            traffic: Arc::clone(&traffic),
        });
        let draws = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&draws);
        let panel = tokio::spawn(show(events, showing, STYLE, move |_, _| {
            counted.fetch_add(1, Ordering::Relaxed);
        }));
        tx.send(Event::Connected {
            remote: "203.0.113.7:4433".parse().unwrap(),
            transport: TransportKind::Quic,
            name: "laptop".to_owned(),
            agent: "hawse/0.8.0".to_owned(),
            round_trip: Duration::from_millis(24),
        })
        .await
        .unwrap();
        tx.send(Event::Bound {
            service: "web".to_owned(),
            port: Port {
                number: 8080,
                kind: Kind::Tcp,
            },
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_secs(60)).await;
        // The first frame, then one for each event.
        assert_eq!(draws.load(Ordering::Relaxed), 3, "drew with nothing moving");

        traffic.meter("web").toward_local(1000);
        tokio::time::sleep(Duration::from_secs(10)).await;
        // The wakeup, the tick that saw the bytes, and the tick that saw none, which ends the
        // ticking.
        assert_eq!(draws.load(Ordering::Relaxed), 6);

        drop(tx);
        assert!(!panel.await.unwrap());
    }
}
