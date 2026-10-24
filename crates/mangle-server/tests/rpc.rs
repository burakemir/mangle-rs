//! End-to-end tests for the Connect RPC API (see RPC_DESIGN.md).

use std::sync::{Arc, RwLock};

use connectrpc::client::{ClientConfig, HttpClient};
use mangle_common::Value;
use mangle_proto::pb::mangle as pb;
use mangle_proto::pb::mangle::MangleServiceClient;
use mangle_proto::{rows_from_batch_view, value_to_proto};
use mangle_server::app::build_app;
use mangle_server::store::{AppState, ProgramStore};

use pb::__buffa::view::oneof::query_event::Event;

const SOURCE: &str = r#"
    route("GET", "/api", "api_handler").
    route("POST", "/api", "api_post").
    route("GET", "/home", "home_handler").
    method(M) :- route(M, _, _).
"#;

async fn spawn_server() -> (String, AppState) {
    let mut store = ProgramStore::new();
    store.load("routes", SOURCE).unwrap();
    let state: AppState = Arc::new(RwLock::new(store));
    let app = build_app(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), state)
}

fn client(url: &str) -> MangleServiceClient<HttpClient> {
    MangleServiceClient::new(
        HttpClient::plaintext(),
        ClientConfig::new(url.parse().unwrap()),
    )
}

/// Collect a server-streaming Query/Eval result: (relation, rows) per
/// batch, plus the total fact count from the terminal stats event.
async fn collect_stream(
    mut stream: connectrpc::client::ServerStream<
        <HttpClient as connectrpc::client::ClientTransport>::ResponseBody,
        pb::QueryEventView<'static>,
    >,
) -> (Vec<(String, Vec<Vec<Value>>)>, u64) {
    let mut batches = Vec::new();
    let mut total = 0u64;
    while let Some(msg) = stream
        .message::<pb::QueryEvent>()
        .await
        .expect("stream error")
    {
        match msg.event().expect("event is required") {
            Event::Batch(b) => {
                let rows = rows_from_batch_view(b).expect("batch decodes");
                total += rows.len() as u64;
                batches.push((b.relation.to_string(), rows));
            }
            Event::Done(s) => {
                assert_eq!(s.num_facts, total, "done stats must match streamed facts");
            }
        }
    }
    (batches, total)
}

fn query_request(
    program: &str,
    text: &str,
    columnar: bool,
    batch_size: u32,
    limit: u64,
) -> pb::QueryRequest {
    pb::QueryRequest {
        program: program.into(),
        query: Some(pb::__buffa::oneof::query_request::Query::Text(text.into())),
        encoding: if columnar {
            pb::ResultEncoding::COLUMNAR.into()
        } else {
            pb::ResultEncoding::ROWS.into()
        },
        batch_size,
        limit,
        ..Default::default()
    }
}

fn mutation_fact(program: &str, relation: &str, tuple: &[Value]) -> pb::MutationRequest {
    pb::MutationRequest {
        program: program.into(),
        payload: Some(pb::__buffa::oneof::mutation_request::Payload::Fact(
            Box::new(mangle_proto::fact_to_proto(relation, tuple)),
        )),
        ..Default::default()
    }
}

fn mutation_batch(program: &str, relation: &str, rows: Vec<Vec<Value>>) -> pb::MutationRequest {
    pb::MutationRequest {
        program: program.into(),
        payload: Some(pb::__buffa::oneof::mutation_request::Payload::Batch(
            Box::new(mangle_proto::batch_from_rows(relation, &rows)),
        )),
        ..Default::default()
    }
}

#[tokio::test]
async fn query_text_columnar() {
    let (url, _) = spawn_server().await;
    let c = client(&url);

    let stream = c
        .query(query_request("routes", "route(M, P, H)", true, 2, 0))
        .await
        .unwrap();
    let (batches, total) = collect_stream(stream).await;

    // batch_size=2 over 3 facts -> 2 batches.
    let route_batches: Vec<_> = batches.iter().filter(|(r, _)| r == "route").collect();
    assert_eq!(route_batches.len(), 2);
    assert_eq!(total, 3);
}

#[tokio::test]
async fn query_text_row_encoding() {
    let (url, _) = spawn_server().await;
    let c = client(&url);

    let stream = c
        .query(query_request(
            "routes",
            r#"route("GET", P, H)"#,
            false,
            4096,
            0,
        ))
        .await
        .unwrap();
    let (batches, total) = collect_stream(stream).await;
    assert_eq!(total, 2);
    let rows: Vec<&Vec<Value>> = batches.iter().flat_map(|(_, rows)| rows).collect();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r[0] == Value::String("GET".into())));
}

#[tokio::test]
async fn query_structured() {
    let (url, _) = spawn_server().await;
    let c = client(&url);

    let req = pb::QueryRequest {
        program: "routes".into(),
        query: Some(pb::__buffa::oneof::query_request::Query::Structured(
            pb::StructuredQuery {
                predicate: "route".into(),
                args: vec![
                    pb::ArgPattern {
                        pattern: Some(pb::__buffa::oneof::arg_pattern::Pattern::Const(Box::new(
                            value_to_proto(&Value::String("POST".into())),
                        ))),
                        ..Default::default()
                    },
                    pb::ArgPattern {
                        pattern: Some(
                            pb::__buffa::oneof::arg_pattern::Pattern::Any(Box::default()),
                        ),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }
            .into(),
        )),
        encoding: pb::ResultEncoding::COLUMNAR.into(),
        ..Default::default()
    };

    let stream = c.query(req).await.unwrap();
    let (batches, total) = collect_stream(stream).await;
    assert_eq!(total, 1);
    let rows: Vec<&Vec<Value>> = batches.iter().flat_map(|(_, rows)| rows).collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][1], Value::String("/api".into()));
}

#[tokio::test]
async fn query_limit() {
    let (url, _) = spawn_server().await;
    let c = client(&url);
    let stream = c
        .query(query_request("routes", "method(M)", true, 4096, 2))
        .await
        .unwrap();
    let (_, total) = collect_stream(stream).await;
    assert_eq!(total, 2);
}

#[tokio::test]
async fn query_unknown_program_is_not_found() {
    let (url, _) = spawn_server().await;
    let c = client(&url);
    // For server-streaming RPCs the error arrives as the stream's terminal
    // record, before any facts are streamed.
    let mut stream = c
        .query(query_request("nope", "p(X)", true, 0, 0))
        .await
        .unwrap();
    let err = stream
        .message::<pb::QueryEvent>()
        .await
        .expect_err("unknown program must fail the stream");
    assert_eq!(err.code, connectrpc::ErrorCode::NotFound);
}

#[tokio::test]
async fn eval_stream() {
    let (url, _) = spawn_server().await;
    let c = client(&url);

    let req = pb::EvalRequest {
        source: "p(1). p(2). p(3).".into(),
        query: "p(X)".into(),
        encoding: pb::ResultEncoding::COLUMNAR.into(),
        ..Default::default()
    };
    let stream = c.eval(req).await.unwrap();
    let (batches, total) = collect_stream(stream).await;
    assert_eq!(total, 3);
    assert!(batches.iter().any(|(r, rows)| r == "p" && rows.len() == 3));
}

#[tokio::test]
async fn eval_all_relations() {
    let (url, _) = spawn_server().await;
    let c = client(&url);

    let req = pb::EvalRequest {
        source: "p(1). q(2).".into(),
        encoding: pb::ResultEncoding::COLUMNAR.into(),
        ..Default::default()
    };
    let stream = c.eval(req).await.unwrap();
    let (batches, total) = collect_stream(stream).await;
    assert_eq!(total, 2);
    let relations: Vec<&str> = batches.iter().map(|(r, _)| r.as_str()).collect();
    assert!(relations.contains(&"p"));
    assert!(relations.contains(&"q"));
}

#[tokio::test]
async fn insert_and_retract_facts() {
    let (url, _) = spawn_server().await;
    let c = client(&url);

    // Load a program with an empty EDB relation and a derived rule.
    c.load_program(pb::LoadProgramRequest {
        name: "test".into(),
        source: "q(X) :- p(X).".into(),
        ..Default::default()
    })
    .await
    .unwrap();

    // Stream: one row-encoded Fact, then a columnar FactBatch with two rows.
    let summary = c
        .insert_facts(connectrpc::stream_iter(vec![
            mutation_fact("test", "p", &[Value::Number(1)]),
            mutation_batch(
                "test",
                "p",
                vec![vec![Value::Number(2)], vec![Value::Number(3)]],
            ),
        ]))
        .await
        .unwrap()
        .into_owned();
    assert_eq!(summary.num_inserted, 3);

    let stream = c
        .query(query_request("test", "q(X)", true, 0, 0))
        .await
        .unwrap();
    let (_, total) = collect_stream(stream).await;
    assert_eq!(total, 3);

    // Retract two facts, streaming row-encoded.
    let summary = c
        .retract_facts(connectrpc::stream_iter(vec![
            mutation_fact("test", "p", &[Value::Number(1)]),
            mutation_fact("test", "p", &[Value::Number(2)]),
        ]))
        .await
        .unwrap()
        .into_owned();
    assert_eq!(summary.num_retracted, 2);

    let stream = c
        .query(query_request("test", "q(X)", true, 0, 0))
        .await
        .unwrap();
    let (_, total) = collect_stream(stream).await;
    assert_eq!(total, 1);
}

#[tokio::test]
async fn mutation_stream_reports_skipped_duplicates() {
    let (url, _) = spawn_server().await;
    let c = client(&url);

    c.load_program(pb::LoadProgramRequest {
        name: "test".into(),
        source: "q(X) :- p(X).".into(),
        ..Default::default()
    })
    .await
    .unwrap();

    // The same fact twice in one stream: one applied, one skipped.
    let summary = c
        .insert_facts(connectrpc::stream_iter(vec![
            mutation_fact("test", "p", &[Value::Number(1)]),
            mutation_fact("test", "p", &[Value::Number(1)]),
        ]))
        .await
        .unwrap()
        .into_owned();
    assert_eq!(summary.num_inserted, 1);
    assert_eq!(summary.num_skipped, 1);

    // Retract of an absent fact is a non-error no-op, counted skipped.
    let summary = c
        .retract_facts(connectrpc::stream_iter(vec![mutation_fact(
            "test",
            "p",
            &[Value::Number(42)],
        )]))
        .await
        .unwrap()
        .into_owned();
    assert_eq!(summary.num_retracted, 0);
    assert_eq!(summary.num_skipped, 1);
}

#[tokio::test]
async fn mutation_stream_mixed_programs_is_rejected() {
    let (url, _) = spawn_server().await;
    let c = client(&url);

    for name in ["a", "b"] {
        c.load_program(pb::LoadProgramRequest {
            name: name.into(),
            source: "q(X) :- p(X).".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    }

    // One command per stream (§7.2): a stream spanning two programs is a
    // client error, and nothing is applied.
    let err = c
        .insert_facts(connectrpc::stream_iter(vec![
            mutation_fact("a", "p", &[Value::Number(1)]),
            mutation_fact("b", "p", &[Value::Number(2)]),
        ]))
        .await
        .unwrap_err();
    assert_eq!(err.code, connectrpc::ErrorCode::InvalidArgument);
    assert!(err.message.as_deref().is_some_and(|m| m.contains("single program")));

    for name in ["a", "b"] {
        let stream = c
            .query(query_request(name, "q(X)", true, 0, 0))
            .await
            .unwrap();
        let (_, total) = collect_stream(stream).await;
        assert_eq!(total, 0, "nothing must be applied for program {name}");
    }
}

#[tokio::test]
async fn mutation_of_source_defined_relation_is_rejected() {
    let (url, _) = spawn_server().await;
    let c = client(&url);

    // `p` is defined by a fact in the program source: mutations cannot
    // persist (recompute re-derives p(1)), so they are rejected instead
    // of silently lost.
    c.load_program(pb::LoadProgramRequest {
        name: "test".into(),
        source: "p(1). q(X) :- p(X).".into(),
        ..Default::default()
    })
    .await
    .unwrap();

    let err = c
        .insert_facts(connectrpc::stream_iter(vec![mutation_fact(
            "test",
            "p",
            &[Value::Number(3)],
        )]))
        .await
        .unwrap_err();
    assert_eq!(err.code, connectrpc::ErrorCode::InvalidArgument);
    assert!(err
        .message
        .as_deref()
        .is_some_and(|m| m.contains("defined by the program source")));

    // State unchanged.
    let stream = c
        .query(query_request("test", "p(X)", true, 0, 0))
        .await
        .unwrap();
    let (_, total) = collect_stream(stream).await;
    assert_eq!(total, 1);
}

#[tokio::test]
async fn program_management() {
    let (url, _) = spawn_server().await;
    let c = client(&url);

    // LoadProgram
    let info = c
        .load_program(pb::LoadProgramRequest {
            name: "demo".into(),
            source: "p(1).".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_owned();
    assert_eq!(info.name, "demo");
    assert!(info.predicates.iter().any(|p| p == "p"));

    // ListPrograms
    let list = c
        .list_programs(pb::ListProgramsRequest::default())
        .await
        .unwrap()
        .into_owned();
    assert!(list.programs.iter().any(|p| p.name == "demo"));

    // GetProgram returns the source.
    let detail = c
        .get_program(pb::GetProgramRequest {
            name: "demo".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_owned();
    assert_eq!(detail.source, "p(1).");

    // DeleteProgram, then GetProgram fails with not_found.
    c.delete_program(pb::DeleteProgramRequest {
        name: "demo".into(),
        ..Default::default()
    })
    .await
    .unwrap();

    let err = c
        .get_program(pb::GetProgramRequest {
            name: "demo".into(),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(err.code, connectrpc::ErrorCode::NotFound);
}

#[tokio::test]
async fn json_api_routes_are_gone() {
    use tower::ServiceExt;

    let state: AppState = Arc::new(RwLock::new(ProgramStore::new()));
    state.write().unwrap().load("routes", SOURCE).unwrap();
    let app = build_app(state);

    for (method, uri) in [
        ("POST", "/query"),
        ("POST", "/eval"),
        ("GET", "/programs"),
        ("POST", "/programs"),
        ("GET", "/programs/routes"),
        ("DELETE", "/programs/routes"),
        ("POST", "/programs/routes/reload"),
        ("POST", "/programs/routes/insert"),
        ("POST", "/programs/routes/retract"),
        ("POST", "/admin/reload-all"),
    ] {
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // The Connect fallback answers bare requests with 415 or 404;
        // either way, the old JSON handlers are gone (no 200s, no Deprecation
        // headers).
        assert!(
            !response.status().is_success(),
            "removed JSON route {method} {uri} must not succeed, got {}",
            response.status()
        );
        assert!(response.headers().get("deprecation").is_none());
    }
}
