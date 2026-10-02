use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr};
use std::time::{Duration, Instant};

const WINDOW: Duration = Duration::from_secs(60);
const MAX_TRACKED: usize = 16384;

#[derive(Debug, PartialEq, Eq)]
pub enum Failure {
    Counted,
    /// This failure emptied the bucket, which had been full since the last time it did.
    NowLimited,
    /// Every slot is held by a bucket still refilling, so this address goes unlimited.
    Untracked,
    Disabled,
}

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    at: Instant,
    warned: bool,
}

impl Bucket {
    fn refill(&mut self, capacity: f64, now: Instant) {
        let elapsed = now.saturating_duration_since(self.at).as_secs_f64();
        self.tokens = (self.tokens + capacity * elapsed / WINDOW.as_secs_f64()).min(capacity);
        self.at = now;
        if self.tokens >= capacity {
            self.warned = false;
        }
    }
}

/// Failed authentications per address: a token bucket holding `per_minute` and refilling that
/// many a minute. Only the caller knows which failures prove their source address, so it decides
/// what to report.
#[derive(Debug)]
pub struct Limiter {
    per_minute: u32,
    buckets: HashMap<IpAddr, Bucket>,
    full_warned: bool,
}

impl Limiter {
    pub fn new(per_minute: u32) -> Self {
        Self {
            per_minute,
            buckets: HashMap::new(),
            full_warned: false,
        }
    }

    pub fn permits(&mut self, ip: IpAddr, now: Instant) -> bool {
        let capacity = f64::from(self.per_minute);
        match self.buckets.get_mut(&key(ip)) {
            Some(bucket) => {
                bucket.refill(capacity, now);
                bucket.tokens >= 1.0
            }
            None => true,
        }
    }

    pub fn failed(&mut self, ip: IpAddr, now: Instant) -> Failure {
        if self.per_minute == 0 {
            return Failure::Disabled;
        }
        let per_minute = self.per_minute;
        let capacity = f64::from(per_minute);
        let key = key(ip);
        if !self.buckets.contains_key(&key) && self.buckets.len() >= MAX_TRACKED {
            self.sweep(capacity, now);
            if self.buckets.len() >= MAX_TRACKED {
                if !self.full_warned {
                    self.full_warned = true;
                    tracing::warn!(
                        tracked = MAX_TRACKED,
                        "too many addresses are failing to authenticate; new ones go unlimited"
                    );
                }
                return Failure::Untracked;
            }
        }
        let bucket = self.buckets.entry(key).or_insert(Bucket {
            tokens: capacity,
            at: now,
            warned: false,
        });
        bucket.refill(capacity, now);
        bucket.tokens = (bucket.tokens - 1.0).max(0.0);
        if bucket.tokens >= 1.0 || bucket.warned {
            return Failure::Counted;
        }
        bucket.warned = true;
        tracing::warn!(
            address = %key,
            per_minute,
            "too many failed authentications; refusing this address for now"
        );
        Failure::NowLimited
    }

    /// A bucket that has refilled completely holds nothing a fresh one would not.
    fn sweep(&mut self, capacity: f64, now: Instant) {
        self.buckets.retain(|_, bucket| {
            bucket.refill(capacity, now);
            bucket.tokens < capacity
        });
        if self.buckets.len() < MAX_TRACKED {
            self.full_warned = false;
        }
    }
}

/// An IPv4 address, or the /64 an IPv6 one sits in: a host is handed a whole /64.
fn key(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V6(v6) => IpAddr::V6(Ipv6Addr::from(u128::from(v6) & (u128::MAX << 64))),
        v4 @ IpAddr::V4(_) => v4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};
    use std::time::{Duration, Instant};

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn an_address_is_refused_once_its_failures_reach_the_limit() {
        let now = Instant::now();
        let mut limiter = Limiter::new(3);
        let addr = ip("192.0.2.1");
        assert_eq!(limiter.failed(addr, now), Failure::Counted);
        assert_eq!(limiter.failed(addr, now), Failure::Counted);
        assert!(limiter.permits(addr, now));
        assert_eq!(limiter.failed(addr, now), Failure::NowLimited);
        assert!(!limiter.permits(addr, now));
        assert!(limiter.permits(ip("192.0.2.2"), now));
    }

    #[test]
    fn the_bucket_refills_over_the_minute() {
        let now = Instant::now();
        let mut limiter = Limiter::new(3);
        let addr = ip("192.0.2.1");
        for _ in 0..3 {
            limiter.failed(addr, now);
        }
        // Three a minute is one every 20 s. The second check is past 20 s because the first one
        // has already refilled the bucket to 0.95, and 0.95 + 0.05 need not reach 1.0 in floats.
        assert!(!limiter.permits(addr, now + Duration::from_secs(19)));
        assert!(limiter.permits(addr, now + Duration::from_secs(21)));
    }

    #[test]
    fn addresses_in_one_ipv6_64_share_a_bucket() {
        let now = Instant::now();
        let mut limiter = Limiter::new(1);
        limiter.failed(ip("2001:db8::1"), now);
        assert!(!limiter.permits(ip("2001:db8::ffff"), now));
        assert!(limiter.permits(ip("2001:db8:0:1::1"), now));
    }

    #[test]
    fn an_ipv4_mapped_address_shares_its_ipv4_bucket() {
        let now = Instant::now();
        let mut limiter = Limiter::new(1);
        limiter.failed(ip("::ffff:192.0.2.1"), now);
        assert!(!limiter.permits(ip("192.0.2.1"), now));
    }

    #[test]
    fn it_reports_the_limit_once_until_the_bucket_is_full_again() {
        let now = Instant::now();
        let mut limiter = Limiter::new(2);
        let addr = ip("192.0.2.1");
        limiter.failed(addr, now);
        assert_eq!(limiter.failed(addr, now), Failure::NowLimited);
        let half = now + Duration::from_secs(30);
        assert!(limiter.permits(addr, half));
        assert_eq!(limiter.failed(addr, half), Failure::Counted);
        let full = half + Duration::from_secs(60);
        assert_eq!(limiter.failed(addr, full), Failure::Counted);
        assert_eq!(limiter.failed(addr, full), Failure::NowLimited);
    }

    #[test]
    fn a_full_limiter_sweeps_refilled_buckets_and_otherwise_stops_tracking() {
        let now = Instant::now();
        let mut limiter = Limiter::new(1);
        for i in 0..MAX_TRACKED {
            let addr = IpAddr::V4(Ipv4Addr::from(u32::try_from(i).unwrap()));
            limiter.failed(addr, now);
        }
        let newcomer = ip("192.0.2.1");
        assert_eq!(limiter.failed(newcomer, now), Failure::Untracked);
        assert!(limiter.permits(newcomer, now));
        let later = now + Duration::from_secs(60);
        assert_eq!(limiter.failed(newcomer, later), Failure::NowLimited);
    }

    #[test]
    fn zero_disables_it() {
        let now = Instant::now();
        let mut limiter = Limiter::new(0);
        let addr = ip("192.0.2.1");
        for _ in 0..100 {
            assert_eq!(limiter.failed(addr, now), Failure::Disabled);
        }
        assert!(limiter.permits(addr, now));
    }
}
