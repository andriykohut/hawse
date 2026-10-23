//! What `hawse status` reads from a running server or client: the reply to a status request,
//! as one JSON document. Times are whole seconds, rounded down, taken when the reply is made.

use serde::{Deserialize, Serialize};

pub mod client;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Status {
    Client(ClientStatus),
    Server(ServerStatus),
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ClientStatus {
    pub version: String,
    pub server: String,
    /// How long the process has run.
    pub up_s: u64,
    pub link: LinkStatus,
    /// In the order of the config's `[expose.*]` tables.
    pub services: Vec<ServiceStatus>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum LinkStatus {
    Connecting,
    Up {
        transport: String,
        round_trip_ms: u64,
        up_s: u64,
        /// The server's own name and version.
        agent: String,
    },
    Down {
        down_s: u64,
        retry_in_s: u64,
        cause: String,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ServiceStatus {
    pub name: String,
    pub local: String,
    /// The port asked for, as the config spells it.
    pub port: String,
    pub answer: Answer,
    pub open: u64,
    pub visitors: u64,
    pub down: u64,
    pub up: u64,
    pub datagrams_down: u64,
    pub datagrams_up: u64,
    pub dropped: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum Answer {
    /// The server has not answered this session's bind yet.
    Pending,
    Bound {
        port: String,
    },
    Refused {
        reason: String,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ServerStatus {
    pub version: String,
    pub listen: String,
    pub tcp_fallback: bool,
    pub up_s: u64,
    pub clients: Vec<ClientEntry>,
    pub unknown_keys: Vec<Knock>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ClientEntry {
    pub name: String,
    #[serde(flatten)]
    pub presence: Presence,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Presence {
    Connected {
        remote: String,
        /// Absent for a session the server did not see connect.
        transport: Option<String>,
        agent: Option<String>,
        up_s: Option<u64>,
        services: Vec<Bind>,
    },
    NotConnected {
        /// Seconds since the client was last connected; absent if it has not been since the
        /// server started.
        last_s: Option<u64>,
    },
}

/// One service a client has bound: the address the public port listens on, and the port.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[allow(
    clippy::struct_field_names,
    reason = "`bind` is the key the JSON carries"
)]
pub struct Bind {
    pub name: String,
    pub bind: String,
    pub port: String,
}

/// A key that no `[clients.*]` table names, and how often it has tried.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Knock {
    pub key: String,
    pub remote: String,
    pub times: u64,
    pub last_s: u64,
}
