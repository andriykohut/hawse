use std::future::Future;
use std::time::Duration;

/// How a `Prefer::Auto` dial ended.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Raced<T, E> {
    Quic(T),
    /// `quic` is why QUIC lost when it had already failed, and `None` while it was still dialing.
    Tcp {
        dialed: T,
        quic: Option<E>,
    },
    Failed {
        quic: E,
        tcp: E,
    },
}

/// Dials QUIC, and starts the TCP dial beside it once `after` has passed or QUIC has failed. The
/// first to connect wins and the other is dropped where it stands; a dial that fails leaves the
/// other running. `tcp` is a closure so the fallback is not dialed until it is wanted.
///
/// A race and not a deadline: a QUIC handshake slower than `after` still wins if it lands first,
/// and a tie goes to it.
pub(super) async fn race<T, E, Q, F, C>(quic: Q, tcp: F, after: Duration) -> Raced<T, E>
where
    Q: Future<Output = Result<T, E>>,
    F: FnOnce() -> C,
    C: Future<Output = Result<T, E>>,
{
    tokio::pin!(quic);
    let early = match tokio::time::timeout(after, &mut quic).await {
        Ok(Ok(dialed)) => return Raced::Quic(dialed),
        Ok(Err(err)) => Some(err),
        Err(_) => None,
    };
    let tcp = tcp();
    tokio::pin!(tcp);
    if let Some(quic_err) = early {
        return match tcp.await {
            Ok(dialed) => Raced::Tcp {
                dialed,
                quic: Some(quic_err),
            },
            Err(tcp_err) => Raced::Failed {
                quic: quic_err,
                tcp: tcp_err,
            },
        };
    }
    tokio::select! {
        biased;
        result = &mut quic => match result {
            Ok(dialed) => Raced::Quic(dialed),
            Err(quic_err) => match tcp.await {
                Ok(dialed) => Raced::Tcp { dialed, quic: Some(quic_err) },
                Err(tcp_err) => Raced::Failed { quic: quic_err, tcp: tcp_err },
            },
        },
        result = &mut tcp => match result {
            Ok(dialed) => Raced::Tcp { dialed, quic: None },
            Err(tcp_err) => match quic.await {
                Ok(dialed) => Raced::Quic(dialed),
                Err(quic_err) => Raced::Failed { quic: quic_err, tcp: tcp_err },
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use tokio::time::Instant;

    use super::*;

    const AFTER: Duration = Duration::from_secs(3);

    type Dialed = Result<&'static str, &'static str>;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    async fn dial(takes: Duration, result: Dialed) -> Dialed {
        tokio::time::sleep(takes).await;
        result
    }

    #[tokio::test(start_paused = true)]
    async fn quic_inside_the_delay_wins_and_tcp_never_starts() {
        let started = Cell::new(false);
        let tcp = || {
            started.set(true);
            dial(secs(0), Ok("tcp"))
        };
        assert_eq!(
            race(dial(secs(1), Ok("quic")), tcp, AFTER).await,
            Raced::Quic("quic")
        );
        assert!(!started.get());
    }

    #[tokio::test(start_paused = true)]
    async fn quic_failing_early_starts_tcp_at_once() {
        let begun = Instant::now();
        let raced = race(
            dial(secs(1), Err("refused")),
            || dial(secs(1), Ok("tcp")),
            AFTER,
        )
        .await;
        assert_eq!(
            raced,
            Raced::Tcp {
                dialed: "tcp",
                quic: Some("refused")
            }
        );
        assert_eq!(begun.elapsed(), secs(2), "tcp must not wait out the delay");
    }

    #[tokio::test(start_paused = true)]
    async fn tcp_starts_beside_a_slow_quic_and_wins() {
        let begun = Instant::now();
        let raced = race(
            dial(secs(60), Ok("quic")),
            || dial(secs(1), Ok("tcp")),
            AFTER,
        )
        .await;
        assert_eq!(
            raced,
            Raced::Tcp {
                dialed: "tcp",
                quic: None
            }
        );
        assert_eq!(begun.elapsed(), AFTER + secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_quic_still_wins_if_it_connects_first() {
        let raced = race(
            dial(AFTER + secs(1), Ok("quic")),
            || dial(secs(5), Ok("tcp")),
            AFTER,
        )
        .await;
        assert_eq!(raced, Raced::Quic("quic"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_tie_goes_to_quic() {
        let raced = race(
            dial(AFTER + secs(1), Ok("quic")),
            || dial(secs(1), Ok("tcp")),
            AFTER,
        )
        .await;
        assert_eq!(raced, Raced::Quic("quic"));
    }

    #[tokio::test(start_paused = true)]
    async fn tcp_failing_leaves_quic_running() {
        let raced = race(
            dial(AFTER + secs(5), Ok("quic")),
            || dial(secs(1), Err("refused")),
            AFTER,
        )
        .await;
        assert_eq!(raced, Raced::Quic("quic"));
    }

    #[tokio::test(start_paused = true)]
    async fn quic_failing_late_leaves_tcp_running() {
        let raced = race(
            dial(AFTER + secs(1), Err("timed out")),
            || dial(secs(5), Ok("tcp")),
            AFTER,
        )
        .await;
        assert_eq!(
            raced,
            Raced::Tcp {
                dialed: "tcp",
                quic: Some("timed out")
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn both_failing_reports_both_whichever_fails_first() {
        let failed = Raced::Failed {
            quic: "no quic",
            tcp: "no tcp",
        };
        let early: Raced<&str, &str> = race(
            dial(secs(1), Err("no quic")),
            || dial(secs(1), Err("no tcp")),
            AFTER,
        )
        .await;
        assert_eq!(early, failed);
        let quic_last: Raced<&str, &str> = race(
            dial(AFTER + secs(9), Err("no quic")),
            || dial(secs(1), Err("no tcp")),
            AFTER,
        )
        .await;
        assert_eq!(quic_last, failed);
        let tcp_last: Raced<&str, &str> = race(
            dial(AFTER + secs(1), Err("no quic")),
            || dial(secs(9), Err("no tcp")),
            AFTER,
        )
        .await;
        assert_eq!(tcp_last, failed);
    }
}
