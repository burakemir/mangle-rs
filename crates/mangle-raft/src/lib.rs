//! openraft glue for mangle-server: the [`Engine`] facade and the
//! inter-node network. See `README.md` and `DISTRIBUTED_PLAN.md` at the
//! workspace root.

pub mod engine;
pub mod network;

pub use engine::Engine;
pub use network::NoNetwork;

pub use mangle_wal::TypeConfig;
