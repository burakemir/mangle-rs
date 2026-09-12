//! HTTP application assembly: the deprecated JSON routes and the Connect
//! RPC service (mounted as the fallback, serving Connect, gRPC and
//! gRPC-Web protocols on the same port).

use axum::Router;
use axum::http::HeaderValue;
use axum::middleware::{self, Next};
use axum::response::Response as AxumResponse;
use axum::routing::{get, post};
use connectrpc::Router as ConnectRouter;
use std::sync::Arc;

use crate::handlers::{
    AppState, delete_program_handler, eval_handler, get_program_handler, insert_handler,
    list_programs_handler, load_program_handler, query_handler, reload_all_handler,
    reload_program_handler, retract_handler,
};
use crate::rpc::MangleServiceImpl;

/// The JSON HTTP API is deprecated: it coexists with the Connect RPC API
/// (mounted at `/mangle.MangleService/...`) for one release and is removed
/// in the release after that. See RPC_DESIGN.md. The sunset date is set to
/// the planned removal release; adjust it when that release is scheduled.
const SUNSET_HTTP_DATE: &str = "Fri, 01 Jan 2027 00:00:00 GMT";

/// Middleware that marks deprecated JSON HTTP API responses with
/// `Deprecation: true` (RFC 8594) and a `Sunset` date.
async fn deprecation_notice(req: axum::extract::Request, next: Next) -> AxumResponse {
    let mut res = next.run(req).await;
    let headers = res.headers_mut();
    headers.insert(
        axum::http::HeaderName::from_static("deprecation"),
        HeaderValue::from_static("true"),
    );
    headers.insert(
        axum::http::HeaderName::from_static("sunset"),
        HeaderValue::from_static(SUNSET_HTTP_DATE),
    );
    eprintln!(
        "[deprecated] JSON HTTP API request; migrate to the Connect RPC API (see RPC_DESIGN.md)"
    );
    res
}

/// Build the full application: JSON routes (deprecated) with the Connect
/// RPC service as fallback.
pub fn build_app(state: AppState) -> Router {
    let connect = ConnectRouter::new().add_service(Arc::new(MangleServiceImpl {
        state: state.clone(),
    }));

    Router::new()
        .route("/query", post(query_handler))
        .route(
            "/programs",
            get(list_programs_handler).post(load_program_handler),
        )
        .route(
            "/programs/{name}",
            get(get_program_handler).delete(delete_program_handler),
        )
        .route("/programs/{name}/reload", post(reload_program_handler))
        .route("/programs/{name}/insert", post(insert_handler))
        .route("/programs/{name}/retract", post(retract_handler))
        .route("/admin/reload-all", post(reload_all_handler))
        .route("/eval", post(eval_handler))
        .with_state(state)
        .layer(middleware::from_fn(deprecation_notice))
        .fallback_service(connect.into_axum_service())
}
