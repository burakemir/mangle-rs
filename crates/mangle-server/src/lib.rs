//! mangle-server: HTTP server for Mangle query evaluation.
//!
//! Serves the Connect RPC API (`MangleService`, see `rpc`), which handles the
//! Connect, gRPC and gRPC-Web protocols on one port. See `RPC_DESIGN.md` in
//! this crate for the protocol design; the deprecated JSON HTTP API has been
//! removed.

pub mod app;
pub mod config;
pub mod mutations;
pub mod query;
pub mod rpc;
pub mod store;
