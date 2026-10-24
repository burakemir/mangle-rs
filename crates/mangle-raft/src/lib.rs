//! openraft glue for mangle-server: the [`Engine`] facade and the
//! inter-node network. See `README.md` and `DISTRIBUTED_PLAN.md` at the
//! workspace root.

pub mod engine;
pub mod network;
pub mod transport;
pub mod wire;

pub use engine::{Engine, WriteError};
pub use network::NoNetwork;
pub use transport::{ConnectNetwork, ConnectNetworkFactory, join_member, remove_member};

pub use mangle_wal::TypeConfig;
