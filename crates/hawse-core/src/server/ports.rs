use std::collections::HashSet;

use hawse_proto::msg::BindFailure;
use hawse_proto::port::{Kind, Port, PortSpan};

pub struct PortAllocator {
    span: PortSpan,
    cursor: u16,
    in_use: HashSet<Port>,
}

impl PortAllocator {
    pub fn new(span: PortSpan) -> Self {
        Self {
            span,
            cursor: span.first,
            in_use: HashSet::new(),
        }
    }

    pub fn claim(&mut self, port: Port) -> Result<(), BindFailure> {
        if self.in_use.insert(port) {
            Ok(())
        } else {
            Err(BindFailure::InUse)
        }
    }

    /// Continues from where the last claim stopped, so a port freed by a crashed client is not reissued to the next stranger.
    pub fn claim_dynamic(&mut self, kind: Kind) -> Option<Port> {
        for _ in 0..self.span.len() {
            let port = Port {
                number: self.cursor,
                kind,
            };
            self.cursor = if self.cursor == self.span.last {
                self.span.first
            } else {
                self.cursor + 1
            };
            if self.in_use.insert(port) {
                return Some(port);
            }
        }
        None
    }

    pub fn release(&mut self, port: Port) -> bool {
        self.in_use.remove(&port)
    }

    /// Marks a port number as never-available for dynamic assignment, both TCP and UDP,
    /// so the tunnel's own listen port cannot be handed to a visitor listener.
    pub fn reserve(&mut self, number: u16) {
        self.in_use.insert(Port {
            number,
            kind: Kind::Tcp,
        });
        self.in_use.insert(Port {
            number,
            kind: Kind::Udp,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tcp(n: u16) -> Port {
        Port {
            number: n,
            kind: Kind::Tcp,
        }
    }

    #[test]
    fn fixed_ports_are_exclusive_per_kind() {
        let mut a = PortAllocator::new(PortSpan {
            first: 40000,
            last: 40002,
        });
        a.claim(tcp(443)).unwrap();
        assert_eq!(a.claim(tcp(443)), Err(BindFailure::InUse));
        a.claim(Port {
            number: 443,
            kind: Kind::Udp,
        })
        .unwrap();
        assert!(a.release(tcp(443)));
        assert!(!a.release(tcp(443)));
        a.claim(tcp(443)).unwrap();
    }

    #[test]
    fn dynamic_ports_walk_the_span_and_exhaust() {
        let mut a = PortAllocator::new(PortSpan {
            first: 40000,
            last: 40002,
        });
        let first = a.claim_dynamic(Kind::Tcp).unwrap();
        let second = a.claim_dynamic(Kind::Tcp).unwrap();
        let third = a.claim_dynamic(Kind::Tcp).unwrap();
        assert_eq!(
            [first.number, second.number, third.number],
            [40000, 40001, 40002]
        );
        assert_eq!(a.claim_dynamic(Kind::Tcp), None);
        assert!(a.release(second));
        assert_eq!(a.claim_dynamic(Kind::Tcp), Some(tcp(40001)));
    }

    #[test]
    fn a_released_port_is_not_handed_straight_back() {
        let mut a = PortAllocator::new(PortSpan {
            first: 40000,
            last: 40002,
        });
        let p = a.claim_dynamic(Kind::Tcp).unwrap();
        a.release(p);
        assert_ne!(a.claim_dynamic(Kind::Tcp), Some(p));
    }

    #[test]
    fn dynamic_skips_fixed_claims_inside_the_span() {
        let mut a = PortAllocator::new(PortSpan {
            first: 40000,
            last: 40001,
        });
        a.claim(tcp(40000)).unwrap();
        assert_eq!(a.claim_dynamic(Kind::Tcp), Some(tcp(40001)));
    }

    #[test]
    fn a_reserved_port_is_never_handed_out_dynamically() {
        let mut alloc = PortAllocator::new("40000-40002".parse().unwrap());
        alloc.reserve(40001);
        let mut handed = std::collections::HashSet::new();
        while let Some(p) = alloc.claim_dynamic(Kind::Tcp) {
            handed.insert(p.number);
        }
        assert!(!handed.contains(&40001));
        assert_eq!(handed, std::collections::HashSet::from([40000, 40002]));
    }
}
