use std::collections::HashMap;

use std::net::IpAddr;

use hawse_proto::key::PublicKey;
use hawse_proto::port::{Port, PortRange};
use ipnet::IpNet;

use crate::config::ServerConfig;

#[derive(Clone, Debug)]
pub struct Grant {
    pub name: String,
    pub ports: Vec<PortRange>,
    pub bind: IpAddr,
    pub allow: Vec<IpNet>,
}

impl Grant {
    pub fn allows(&self, port: Port) -> bool {
        self.ports.iter().any(|range| range.contains(port))
    }
}

#[derive(Clone, Debug, Default)]
pub struct Policy {
    by_key: HashMap<PublicKey, Grant>,
}

impl Policy {
    pub fn from_config(cfg: &ServerConfig) -> Self {
        let by_key = cfg
            .clients
            .iter()
            .map(|(name, policy)| {
                (
                    policy.key,
                    Grant {
                        name: name.clone(),
                        ports: policy.ports.clone(),
                        bind: policy.bind.unwrap_or(cfg.bind),
                        allow: policy.allow.clone(),
                    },
                )
            })
            .collect();
        Self { by_key }
    }

    pub fn lookup(&self, key: &PublicKey) -> Option<&Grant> {
        self.by_key.get(key)
    }

    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ClientPolicy;
    use ipnet::IpNet;

    fn key(b: u8) -> PublicKey {
        PublicKey::from_bytes([b; 32])
    }

    #[test]
    fn a_grant_carries_its_clients_allow_ceiling() {
        let mut cfg = ServerConfig::default();
        let ceiling: Vec<IpNet> = vec!["192.0.2.0/24".parse().unwrap()];
        cfg.clients.insert(
            "nas".into(),
            ClientPolicy {
                key: key(4),
                ports: vec![],
                bind: None,
                allow: ceiling.clone(),
            },
        );
        let policy = Policy::from_config(&cfg);
        assert_eq!(policy.lookup(&key(4)).unwrap().allow, ceiling);
    }

    #[test]
    fn looks_up_grants_by_key() {
        let mut cfg = ServerConfig::default();
        cfg.clients.insert(
            "homelab".into(),
            ClientPolicy {
                key: key(1),
                ports: vec!["443".parse().unwrap(), "8000-8100/udp".parse().unwrap()],
                bind: None,
                allow: vec![],
            },
        );
        let policy = Policy::from_config(&cfg);
        let grant = policy.lookup(&key(1)).unwrap();
        assert_eq!(grant.name, "homelab");
        assert!(grant.allows("443".parse().unwrap()));
        assert!(grant.allows("8050/udp".parse().unwrap()));
        assert!(!grant.allows("8050".parse().unwrap()));
        assert!(!grant.allows("444".parse().unwrap()));
        assert!(policy.lookup(&key(2)).is_none());
    }

    #[test]
    fn a_client_without_ports_allows_nothing_fixed() {
        let mut cfg = ServerConfig::default();
        cfg.clients.insert(
            "laptop".into(),
            ClientPolicy {
                key: key(3),
                ports: vec![],
                bind: None,
                allow: vec![],
            },
        );
        let policy = Policy::from_config(&cfg);
        assert!(
            !policy
                .lookup(&key(3))
                .unwrap()
                .allows("80".parse().unwrap())
        );
    }
}
