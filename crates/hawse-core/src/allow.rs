use std::net::IpAddr;

use ipnet::IpNet;

/// The visitor addresses one bound service admits. Empty admits everyone.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AllowList(Vec<IpNet>);

impl AllowList {
    /// The client's list narrowed by the server's ceiling for that client. An empty side places no
    /// limit; `None` when both are set and share no address.
    pub fn effective(client: &[IpNet], ceiling: &[IpNet]) -> Option<Self> {
        let client: Vec<IpNet> = client.iter().map(IpNet::trunc).collect();
        let ceiling: Vec<IpNet> = ceiling.iter().map(IpNet::trunc).collect();
        if client.is_empty() {
            return Some(Self(ceiling));
        }
        if ceiling.is_empty() {
            return Some(Self(client));
        }
        // Two networks either nest or share nothing, so an overlapping pair leaves the narrower.
        let mut both: Vec<IpNet> = client
            .iter()
            .flat_map(|a| {
                ceiling.iter().filter_map(move |b| {
                    if a.contains(b) {
                        Some(*b)
                    } else if b.contains(a) {
                        Some(*a)
                    } else {
                        None
                    }
                })
            })
            .collect();
        both.sort_unstable();
        both.dedup();
        (!both.is_empty()).then_some(Self(both))
    }

    /// A dual-stack socket reports an IPv4 visitor as `::ffff:a.b.c.d`, so the comparison is on
    /// the canonical form.
    pub fn permits(&self, ip: IpAddr) -> bool {
        let ip = ip.to_canonical();
        self.0.is_empty() || self.0.iter().any(|net| net.contains(&ip))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nets(list: &[&str]) -> Vec<IpNet> {
        list.iter().map(|net| net.parse().unwrap()).collect()
    }

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn no_list_on_either_side_admits_everyone() {
        let allow = AllowList::effective(&[], &[]).unwrap();
        assert!(allow.permits(ip("198.51.100.7")));
        assert!(allow.permits(ip("2001:db8::1")));
    }

    #[test]
    fn the_ceiling_applies_when_the_client_names_none() {
        let allow = AllowList::effective(&[], &nets(&["192.0.2.0/24"])).unwrap();
        assert!(allow.permits(ip("192.0.2.9")));
        assert!(!allow.permits(ip("198.51.100.7")));
    }

    #[test]
    fn the_clients_list_applies_without_a_ceiling() {
        let allow = AllowList::effective(&nets(&["192.0.2.0/24"]), &[]).unwrap();
        assert!(allow.permits(ip("192.0.2.9")));
        assert!(!allow.permits(ip("198.51.100.7")));
    }

    #[test]
    fn nested_networks_intersect_to_the_narrower_either_way() {
        let wide = nets(&["10.0.0.0/8"]);
        let narrow = nets(&["10.1.0.0/16"]);
        assert_eq!(AllowList::effective(&wide, &narrow).unwrap().0, narrow);
        assert_eq!(AllowList::effective(&narrow, &wide).unwrap().0, narrow);
    }

    #[test]
    fn a_partial_overlap_keeps_only_what_both_cover() {
        let client = nets(&["127.0.0.0/8", "192.0.2.0/24"]);
        let ceiling = nets(&["127.0.0.1/32"]);
        let allow = AllowList::effective(&client, &ceiling).unwrap();
        assert!(allow.permits(ip("127.0.0.1")));
        assert!(!allow.permits(ip("127.0.0.2")));
        assert!(!allow.permits(ip("192.0.2.9")));
    }

    #[test]
    fn disjoint_lists_have_no_intersection() {
        assert_eq!(
            AllowList::effective(&nets(&["192.0.2.0/24"]), &nets(&["198.51.100.0/24"])),
            None
        );
    }

    #[test]
    fn host_bits_in_an_entry_are_ignored() {
        let allow = AllowList::effective(&nets(&["203.0.113.5/24"]), &[]).unwrap();
        assert!(allow.permits(ip("203.0.113.200")));
    }

    #[test]
    fn an_ipv4_visitor_on_a_dual_stack_socket_matches_an_ipv4_entry() {
        let allow = AllowList::effective(&nets(&["203.0.113.0/24"]), &[]).unwrap();
        assert!(allow.permits(ip("::ffff:203.0.113.7")));
    }

    #[test]
    fn an_ipv4_entry_never_matches_an_ipv6_visitor() {
        let allow = AllowList::effective(&nets(&["0.0.0.0/0"]), &[]).unwrap();
        assert!(!allow.permits(ip("2001:db8::1")));
    }
}
