//! mangle-server: HTTP server for Mangle query evaluation.
//!
//! Serves the Connect RPC API (`MangleService`, see `rpc`) alongside the
//! deprecated JSON HTTP API (`handlers`) for one release of coexistence;
//! see `RPC_DESIGN.md` in this crate for the protocol design and the
//! deprecation plan.

pub mod app;
pub mod config;
pub mod handlers;
pub mod mutations;
pub mod query;
pub mod rpc;
pub mod store;
