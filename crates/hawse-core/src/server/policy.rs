use std::collections::HashMap;

use hawse_proto::key::PublicKey;
use hawse_proto::port::{Port, PortRange};

use crate::config::ServerConfig;

#[derive(Clone, Debug)]
pub struct Grant {
    pub name: String,
    pub ports: Vec<PortRange>,
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

    fn key(b: u8) -> PublicKey {
        PublicKey::from_bytes([b; 32])
    }

    #[test]
    fn looks_up_grants_by_key() {
        let mut cfg = ServerConfig::default();
        cfg.clients.insert(
            "homelab".into(),
            ClientPolicy {
                key: key(1),
                ports: vec!["443".parse().unwrap(), "8000-8100/udp".parse().unwrap()],
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
