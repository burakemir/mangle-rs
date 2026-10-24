//! Connect RPC application: the `MangleService` mounted as the sole service,
//! serving the Connect, gRPC and gRPC-Web protocols on one port.
//!
//! The JSON HTTP API that used to live here was deprecated (RFC 8594
//! `Deprecation`/`Sunset` headers) and has been removed. See
//! `RPC_DESIGN.md` for the protocol design and the deprecation history.

use axum::Router;
use connectrpc::Router as ConnectRouter;
use std::sync::Arc;

use crate::raft_service::RaftServiceImpl;
use crate::rpc::MangleServiceImpl;
use crate::store::AppState;

/// Build the application: the Connect RPC service, plus the inter-node
/// `RaftService` when running in replicated mode.
pub fn build_app(state: AppState, engine: Option<Arc<mangle_raft::Engine>>) -> Router {
    let connect = match &engine {
        Some(engine) => ConnectRouter::new()
            .add_service(Arc::new(MangleServiceImpl {
                state,
                engine: Some(Arc::clone(engine)),
            }))
            .add_service(Arc::new(RaftServiceImpl {
                raft: engine.raft().clone(),
            })),
        None => ConnectRouter::new().add_service(Arc::new(MangleServiceImpl {
            state,
            engine: None,
        })),
    };

    Router::new().fallback_service(connect.into_axum_service())
}
