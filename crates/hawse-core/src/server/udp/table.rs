use std::collections::{BTreeSet, HashMap};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

/// Which visitor each session id belongs to, for one UDP service. Does no I/O and reads no clock.
pub struct SessionTable {
    cap: usize,
    idle: Duration,
    next: u32,
    by_visitor: HashMap<SocketAddr, u32>,
    by_session: HashMap<u32, Entry>,
    /// Oldest first, so eviction and expiry both start at the front.
    by_age: BTreeSet<(Instant, u32)>,
}

struct Entry {
    visitor: SocketAddr,
    used: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Inbound {
    pub session: u32,
    pub fresh: bool,
    /// Another visitor's session was removed to make room for this one.
    pub evicted: bool,
}

impl SessionTable {
    pub fn new(cap: usize, idle: Duration) -> Self {
        Self {
            cap,
            idle,
            next: 0,
            by_visitor: HashMap::new(),
            by_session: HashMap::new(),
            by_age: BTreeSet::new(),
        }
    }

    pub fn inbound(&mut self, visitor: SocketAddr, now: Instant) -> Inbound {
        if let Some(&session) = self.by_visitor.get(&visitor) {
            self.touch(session, now);
            return Inbound {
                session,
                fresh: false,
                evicted: false,
            };
        }
        let evicted = self.by_session.len() >= self.cap && self.evict_oldest();
        let session = self.claim_id();
        self.by_visitor.insert(visitor, session);
        self.by_session
            .insert(session, Entry { visitor, used: now });
        self.by_age.insert((now, session));
        Inbound {
            session,
            fresh: true,
            evicted,
        }
    }

    pub fn outbound(&mut self, session: u32, now: Instant) -> Option<SocketAddr> {
        let visitor = self.by_session.get(&session)?.visitor;
        self.touch(session, now);
        Some(visitor)
    }

    pub fn sweep(&mut self, now: Instant) -> usize {
        let mut removed = 0;
        while let Some(&(used, session)) = self.by_age.first() {
            if now.saturating_duration_since(used) < self.idle {
                break;
            }
            self.remove(session);
            removed += 1;
        }
        removed
    }

    // Exercised only by the tests below; `is_empty` exists alongside it for `clippy::len_without_is_empty`.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.by_session.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.by_session.is_empty()
    }

    fn touch(&mut self, session: u32, now: Instant) {
        if let Some(entry) = self.by_session.get_mut(&session) {
            self.by_age.remove(&(entry.used, session));
            entry.used = now;
            self.by_age.insert((now, session));
        }
    }

    fn evict_oldest(&mut self) -> bool {
        let Some(&(_, session)) = self.by_age.first() else {
            return false;
        };
        self.remove(session);
        true
    }

    fn remove(&mut self, session: u32) {
        if let Some(entry) = self.by_session.remove(&session) {
            self.by_visitor.remove(&entry.visitor);
            self.by_age.remove(&(entry.used, session));
        }
    }

    /// Skips ids a live session holds: past a wrap the counter would otherwise hand a visitor an
    /// id that still routes replies to someone else.
    fn claim_id(&mut self) -> u32 {
        loop {
            let id = self.next;
            self.next = self.next.wrapping_add(1);
            if !self.by_session.contains_key(&id) {
                return id;
            }
        }
    }

    #[cfg(test)]
    fn check(&self) {
        assert!(self.by_session.len() <= self.cap.max(1));
        assert_eq!(self.by_session.len(), self.by_visitor.len());
        assert_eq!(self.by_session.len(), self.by_age.len());
        for (session, entry) in &self.by_session {
            assert_eq!(self.by_visitor.get(&entry.visitor), Some(session));
            assert!(self.by_age.contains(&(entry.used, *session)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const IDLE: Duration = Duration::from_secs(60);

    fn visitor(n: u16) -> SocketAddr {
        SocketAddr::from(([203, 0, 113, 1], 1000 + n))
    }

    #[test]
    fn a_visitor_keeps_its_session_and_a_reply_finds_the_visitor() {
        let now = Instant::now();
        let mut table = SessionTable::new(8, IDLE);
        let first = table.inbound(visitor(1), now);
        let again = table.inbound(visitor(1), now);
        assert!(first.fresh && !again.fresh);
        assert_eq!(first.session, again.session);
        assert_eq!(table.outbound(first.session, now), Some(visitor(1)));
    }

    #[test]
    fn the_least_recently_used_session_goes_at_the_cap() {
        let start = Instant::now();
        let at = |secs| start + Duration::from_secs(secs);
        let mut table = SessionTable::new(2, IDLE);
        let one = table.inbound(visitor(1), at(0)).session;
        let two = table.inbound(visitor(2), at(1)).session;
        assert_eq!(table.outbound(one, at(2)), Some(visitor(1)));

        let three = table.inbound(visitor(3), at(3));

        assert!(three.evicted);
        assert_eq!(table.len(), 2);
        assert_eq!(table.outbound(two, at(4)), None);
        assert_eq!(table.outbound(one, at(4)), Some(visitor(1)));
    }

    #[test]
    fn a_late_reply_to_an_evicted_session_finds_nobody() {
        let now = Instant::now();
        let mut table = SessionTable::new(1, IDLE);
        let gone = table.inbound(visitor(1), now).session;
        let kept = table.inbound(visitor(2), now).session;
        assert_ne!(gone, kept);
        assert_eq!(table.outbound(gone, now), None);
    }

    #[test]
    fn a_sweep_removes_only_sessions_idle_for_the_full_time() {
        let start = Instant::now();
        let mut table = SessionTable::new(8, IDLE);
        table.inbound(visitor(1), start);
        table.inbound(visitor(2), start + Duration::from_secs(30));

        assert_eq!(table.sweep(start + Duration::from_secs(59)), 0);
        assert_eq!(table.sweep(start + Duration::from_secs(60)), 1);
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn a_reply_keeps_a_session_from_expiring() {
        let start = Instant::now();
        let mut table = SessionTable::new(8, IDLE);
        let session = table.inbound(visitor(1), start).session;
        table.outbound(session, start + Duration::from_secs(50));
        assert_eq!(table.sweep(start + Duration::from_secs(100)), 0);
    }

    #[test]
    fn a_visitor_returning_after_expiry_gets_a_new_session() {
        let start = Instant::now();
        let later = start + Duration::from_secs(61);
        let mut table = SessionTable::new(8, IDLE);
        let before = table.inbound(visitor(1), start).session;
        table.sweep(later);
        let after = table.inbound(visitor(1), later);
        assert!(after.fresh);
        assert_ne!(after.session, before);
    }

    #[test]
    fn a_wrapped_counter_skips_an_id_still_in_use() {
        let now = Instant::now();
        let mut table = SessionTable::new(8, IDLE);
        table.next = u32::MAX;
        let last = table.inbound(visitor(1), now).session;
        table.next = u32::MAX;
        let next = table.inbound(visitor(2), now).session;
        assert_eq!((last, next), (u32::MAX, 0));
    }

    proptest! {
        #[test]
        fn the_three_indexes_never_disagree(
            ops in proptest::collection::vec((0u8..3, 0u16..12, 0u64..45), 1..300),
        ) {
            let mut now = Instant::now();
            let mut table = SessionTable::new(4, IDLE);
            let mut told: HashMap<u16, u32> = HashMap::new();
            for (op, who, wait) in ops {
                now += Duration::from_secs(wait);
                match op {
                    0 => {
                        let seen = table.inbound(visitor(who), now);
                        prop_assert_eq!(table.outbound(seen.session, now), Some(visitor(who)));
                        told.insert(who, seen.session);
                    }
                    1 => {
                        if let Some(&session) = told.get(&who) {
                            let reply = table.outbound(session, now);
                            prop_assert!(reply.is_none() || reply == Some(visitor(who)));
                        }
                    }
                    _ => {
                        table.sweep(now);
                    }
                }
                table.check();
            }
        }
    }
}
