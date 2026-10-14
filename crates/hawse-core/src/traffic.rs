//! What a client's services carry while it runs: per service, the connections open through it
//! and the bytes each way, as running totals for whatever shows them. The pumps and the UDP path
//! add as they copy; a reader takes snapshots and works out rates from the difference.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use tokio::sync::Notify;

/// One service's totals as `Traffic::snapshot` reads them. `down` is bytes toward the local
/// service, `up` bytes from it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub open: u64,
    pub down: u64,
    pub up: u64,
}

/// Whether anything moved since the last `Traffic::take_moved`, and the wakeup for a reader
/// waiting on that.
#[derive(Debug, Default)]
struct Activity {
    moving: AtomicBool,
    wake: Notify,
}

impl Activity {
    /// Loads before it swaps, so a busy pump only reads a line every other pump shares, and
    /// writes it once per `take_moved`.
    fn touch(&self) {
        if !self.moving.load(Ordering::Relaxed) && !self.moving.swap(true, Ordering::Relaxed) {
            self.wake.notify_one();
        }
    }
}

/// One service's counters. `Default` gives one nothing reads, for a pump whose figures nobody
/// shows.
#[derive(Debug, Default)]
pub struct Meter {
    open: AtomicU64,
    down: AtomicU64,
    up: AtomicU64,
    activity: Arc<Activity>,
}

impl Meter {
    pub fn toward_local(&self, bytes: usize) {
        add(&self.down, bytes);
        self.activity.touch();
    }

    pub fn from_local(&self, bytes: usize) {
        add(&self.up, bytes);
        self.activity.touch();
    }

    /// Counts a connection open until the returned guard drops.
    pub fn open(self: &Arc<Self>) -> Open {
        self.open.fetch_add(1, Ordering::Relaxed);
        self.activity.touch();
        Open(Arc::clone(self))
    }

    pub(crate) fn counts(&self) -> Counts {
        Counts {
            open: self.open.load(Ordering::Relaxed),
            down: self.down.load(Ordering::Relaxed),
            up: self.up.load(Ordering::Relaxed),
        }
    }
}

fn add(counter: &AtomicU64, bytes: usize) {
    counter.fetch_add(
        u64::try_from(bytes).expect("usize fits u64"),
        Ordering::Relaxed,
    );
}

/// A connection `Meter::open` counted, until this drops.
#[derive(Debug)]
pub struct Open(Arc<Meter>);

impl Drop for Open {
    fn drop(&mut self) {
        self.0.open.fetch_sub(1, Ordering::Relaxed);
        self.0.activity.touch();
    }
}

/// Every service's meter for one `Client`, kept for its life: a service a reload takes away
/// keeps its totals.
#[derive(Debug, Default)]
pub struct Traffic {
    meters: RwLock<BTreeMap<String, Arc<Meter>>>,
    activity: Arc<Activity>,
}

impl Traffic {
    /// `service`'s meter, made the first time it is asked for.
    pub fn meter(&self, service: &str) -> Arc<Meter> {
        if let Some(meter) = self.meters.read().expect("traffic lock").get(service) {
            return Arc::clone(meter);
        }
        let mut meters = self.meters.write().expect("traffic lock");
        Arc::clone(meters.entry(service.to_owned()).or_insert_with(|| {
            Arc::new(Meter {
                activity: Arc::clone(&self.activity),
                ..Meter::default()
            })
        }))
    }

    /// Every service's totals so far, by name.
    pub fn snapshot(&self) -> BTreeMap<String, Counts> {
        self.meters
            .read()
            .expect("traffic lock")
            .iter()
            .map(|(service, meter)| (service.clone(), meter.counts()))
            .collect()
    }

    /// Resolves once bytes move or a connection opens or closes after the last `take_moved`, and
    /// at once if that has already happened.
    pub async fn moved(&self) {
        loop {
            // Made before the check, so a touch between the two is not missed.
            let woken = self.activity.wake.notified();
            if self.activity.moving.load(Ordering::Relaxed) {
                return;
            }
            woken.await;
        }
    }

    /// Whether anything moved since the last call, which starts the next interval.
    pub fn take_moved(&self) -> bool {
        self.activity.moving.swap(false, Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn a_meter_adds_up_and_counts_a_connection_open_until_it_closes() {
        let traffic = Traffic::default();
        let meter = traffic.meter("web");
        meter.toward_local(1000);
        meter.from_local(24);
        let (one, two) = (meter.open(), meter.open());
        let counts = |traffic: &Traffic| traffic.snapshot()["web"];
        assert_eq!(
            counts(&traffic),
            Counts {
                open: 2,
                down: 1000,
                up: 24
            }
        );
        drop((one, two));
        assert_eq!(counts(&traffic).open, 0);
        // Asked for again, it is the same meter.
        traffic.meter("web").toward_local(1);
        assert_eq!(counts(&traffic).down, 1001);
    }

    /// Whether `moved` is still waiting after 50 ms.
    async fn waits(traffic: &Traffic) -> bool {
        tokio::time::timeout(Duration::from_millis(50), traffic.moved())
            .await
            .is_err()
    }

    #[tokio::test]
    async fn moved_waits_for_the_first_touch_after_take_moved() {
        let traffic = Traffic::default();
        let meter = traffic.meter("web");
        assert!(!traffic.take_moved());
        assert!(waits(&traffic).await, "resolved with nothing moving");
        meter.from_local(1);
        assert!(!waits(&traffic).await, "still waiting after bytes moved");
        assert!(traffic.take_moved());
        assert!(!traffic.take_moved());
        // The wakeup the touch left behind is spent, and nothing moving waits again.
        assert!(waits(&traffic).await, "resolved on a spent wakeup");
    }
}
