//! Phase 1 end-to-end tests: single-node Raft (WAL + FSM + snapshots).
//!
//! Covers the DISTRIBUTED_PLAN.md Phase 1 deliverables:
//! - acknowledged writes survive a restart (WAL replay; no snapshot yet)
//! - snapshot + restart: the FSM is restored from the snapshot and only
//!   post-snapshot log entries are replayed
//! - writes through the RPC API in replicated mode; Reload* disabled
//! - failed commands are deterministic (same command fails identically
//!   on replay, state unchanged)

use std::sync::{Arc, RwLock};

use connectrpc::client::{ClientConfig, HttpClient};
use mangle_common::Value;
use mangle_proto::pb::mangle as pb;
use mangle_proto::pb::mangle::MangleServiceClient;
use mangle_proto::{
    Command, DeleteProgramCommand, LoadProgramCommand, MutateCommand, MutationOp, MutationOpKind,
};
use mangle_raft::Engine;
use mangle_server::fsm::FsmStore;
use mangle_server::store::{AppState, ProgramStore};
use mangle_wal::RaftLog;

const GRAPH_SOURCE: &str = "path(X, Y) :- edge(X, Y).\npath(X, Z) :- edge(X, Y), path(Y, Z).\n";

fn insert_op(relation: &str, tuple: Vec<Value>) -> MutationOp {
    MutationOp {
        kind: MutationOpKind::Insert,
        relation: relation.to_string(),
        tuple,
    }
}

/// Start a fresh single-node cluster in `dir`, sharing `state`.
async fn start_node(state: AppState, dir: &std::path::Path) -> Engine {
    let fsm = FsmStore::new(state, dir);
    let log = RaftLog::open(dir.join("raft.redb")).unwrap();
    let engine = Engine::new(1, Engine::raft_config(1024), log, fsm)
        .await
        .unwrap();
    engine.bootstrap_single("127.0.0.1:0").await.unwrap();
    wait_leader(&engine).await;
    engine
}

async fn wait_leader(engine: &Engine) {
    for _ in 0..200 {
        if engine.is_leader().await {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("node did not become leader");
}

/// Restart: fresh ProgramStore + FsmStore + engine over the same dir.
async fn restart_node(dir: &std::path::Path) -> (AppState, Engine) {
    let state: AppState = Arc::new(RwLock::new(ProgramStore::new()));
    let fsm = FsmStore::new(state.clone(), dir);
    fsm.restore().unwrap();
    let log = RaftLog::open(dir.join("raft.redb")).unwrap();
    let engine = Engine::new(1, Engine::raft_config(1024), log, fsm)
        .await
        .unwrap();
    wait_leader(&engine).await;
    (state, engine)
}

fn graph_facts(state: &AppState) -> Vec<Vec<Value>> {
    state
        .read()
        .unwrap()
        .execute_query("graph", "path(X, Y)")
        .unwrap()
}

#[tokio::test]
async fn acknowledged_writes_survive_restart() {
    let dir = tempfile::tempdir().unwrap();
    {
        let state: AppState = Arc::new(RwLock::new(ProgramStore::new()));
        let engine = start_node(state.clone(), dir.path()).await;

        let (idx1, r) = engine
            .write(Command::LoadProgram(LoadProgramCommand {
                name: "graph".into(),
                source: GRAPH_SOURCE.into(),
            }))
            .await
            .unwrap();
        assert!(r.is_ok());
        assert_eq!(idx1, 2); // index 1 is the bootstrap membership entry

        let (_, r) = engine
            .write(Command::Mutate(MutateCommand {
                program: "graph".into(),
                ops: vec![
                    insert_op("edge", vec![Value::Number(1), Value::Number(2)]),
                    insert_op("edge", vec![Value::Number(2), Value::Number(3)]),
                    insert_op("edge", vec![Value::Number(3), Value::Number(4)]),
                ],
            }))
            .await
            .unwrap();
        assert_eq!(
            r.unwrap(),
            mangle_proto::ApplyOutcome::Mutated {
                num_applied: 3,
                num_skipped: 0
            }
        );

        // Derived facts visible before restart.
        assert_eq!(graph_facts(&state).len(), 3 + 2 + 1); // edges + 2-hop + 3-hop

        engine.shutdown().await.unwrap();
    }

    // Restart with a fresh store: the log replays and rebuilds state.
    let (state, _engine) = restart_node(dir.path()).await;
    let paths = graph_facts(&state);
    assert_eq!(paths.len(), 6, "all acknowledged writes must survive");
}

#[tokio::test]
async fn snapshot_and_restart_restores_state() {
    let dir = tempfile::tempdir().unwrap();
    let mut last_index;
    {
        let state: AppState = Arc::new(RwLock::new(ProgramStore::new()));
        let engine = start_node(state.clone(), dir.path()).await;

        engine
            .write(Command::LoadProgram(LoadProgramCommand {
                name: "graph".into(),
                source: GRAPH_SOURCE.into(),
            }))
            .await
            .unwrap()
            .1
            .unwrap();
        let (i, _) = engine
            .write(Command::Mutate(MutateCommand {
                program: "graph".into(),
                ops: vec![
                    insert_op("edge", vec![Value::Number(1), Value::Number(2)]),
                    insert_op("edge", vec![Value::Number(2), Value::Number(3)]),
                ],
            }))
            .await
            .unwrap();
        last_index = i;

        // Build a snapshot; openraft may purge log entries up to it.
        engine.trigger_snapshot().await.unwrap();
        // Let the snapshot/purge round-trip complete.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // More writes after the snapshot.
        let (i, _) = engine
            .write(Command::Mutate(MutateCommand {
                program: "graph".into(),
                ops: vec![insert_op("edge", vec![Value::Number(3), Value::Number(4)])],
            }))
            .await
            .unwrap();
        last_index = i;
        assert_eq!(graph_facts(&state).len(), 6);

        engine.shutdown().await.unwrap();
    }

    // Restart: snapshot restores pre-snapshot state, log replays the rest.
    let (state, _engine) = restart_node(dir.path()).await;
    assert_eq!(graph_facts(&state).len(), 6);

    // And the snapshot files exist for the next restart.
    assert!(dir.path().join("snapshot.data").is_file());
    assert!(dir.path().join("snapshot.meta").is_file());
    let _ = last_index;
}

#[tokio::test]
async fn failed_command_is_deterministic_and_replays_identically() {
    let dir = tempfile::tempdir().unwrap();
    let state: AppState = Arc::new(RwLock::new(ProgramStore::new()));
    let engine = start_node(state.clone(), dir.path()).await;

    // A mutate against an unknown program: committed to the log, fails
    // deterministically at apply.
    let (_, r) = engine
        .write(Command::Mutate(MutateCommand {
            program: "nope".into(),
            ops: vec![insert_op("edge", vec![Value::Number(1)])],
        }))
        .await
        .unwrap();
    let err = r.unwrap_err();
    assert!(err.contains("not found"));

    // A mutate against a source-defined relation: rejected deterministically.
    engine
        .write(Command::LoadProgram(LoadProgramCommand {
            name: "t".into(),
            source: "p(1).".into(),
        }))
        .await
        .unwrap();
    let (_, r) = engine
        .write(Command::Mutate(MutateCommand {
            program: "t".into(),
            ops: vec![insert_op("p", vec![Value::Number(2)])],
        }))
        .await
        .unwrap();
    assert!(r.unwrap_err().contains("defined by the program source"));

    engine.shutdown().await.unwrap();

    // Restart: replaying the failed commands fails identically and leaves
    // state unchanged (the log contains them, apply is idempotent-failing).
    let (state, engine2) = restart_node(dir.path()).await;
    assert!(state.read().unwrap().get("t").is_some());
    let ps = state
        .read()
        .unwrap()
        .execute_query("t", "p(X)")
        .unwrap();
    assert_eq!(ps.len(), 1, "failed command must not partially apply");

    // And a new write still works (the log continues).
    let (_, r) = engine2
        .write(Command::DeleteProgram(DeleteProgramCommand {
            name: "t".into(),
        }))
        .await
        .unwrap();
    assert_eq!(r.unwrap(), mangle_proto::ApplyOutcome::Deleted);
    engine2.shutdown().await.unwrap();
}

// --- FSM unit tests --------------------------------------------------------

#[test]
fn snapshot_state_roundtrip_includes_ledger() {
    use mangle_server::store::ProgramStore as _;

    let programs_dir = tempfile::tempdir().unwrap();
    let edb_dir = tempfile::tempdir().unwrap();

    std::fs::write(programs_dir.path().join("t.mg"), "q(X) :- p(X).\n").unwrap();
    let t_edb = edb_dir.path().join("t");
    std::fs::create_dir(&t_edb).unwrap();
    std::fs::write(t_edb.join("data.mg"), "p(1). p(2).\n").unwrap();

    // Build state: retract an external-source fact (ledger: retracted),
    // insert a replicated fact.
    let mut store = ProgramStore::new()
        .with_programs_dir(programs_dir.path().to_path_buf())
        .with_edb_dir(edb_dir.path().to_path_buf());
    store.reload("t").unwrap();
    store
        .apply(&Command::Mutate(MutateCommand {
            program: "t".into(),
            ops: vec![MutationOp {
                kind: MutationOpKind::Retract,
                relation: "p".into(),
                tuple: vec![Value::Number(1)],
            }],
        }))
        .unwrap();
    store
        .apply(&Command::Mutate(MutateCommand {
            program: "t".into(),
            ops: vec![insert_op("p", vec![Value::Number(3)])],
        }))
        .unwrap();

    let snap = store.snapshot_state();
    let expected = store.canonical_state();

    // Restore into a fresh store sharing the same external sources.
    let mut restored = ProgramStore::new()
        .with_programs_dir(programs_dir.path().to_path_buf())
        .with_edb_dir(edb_dir.path().to_path_buf());
    restored.install_snapshot_state(&snap).unwrap();
    assert_eq!(restored.canonical_state(), expected);

    // The derived relation reflects the ledger replay.
    let qs = restored.execute_query("t", "q(X)").unwrap();
    assert_eq!(qs, vec![vec![Value::Number(2)], vec![Value::Number(3)]]);
}

#[test]
fn mutations_to_ops_parses_legacy_log_format() {
    let ops = mangle_server::fsm::mutations_to_ops(
        "container(\"web\", \"running\").\n__retract__container(\"db\", \"stopped\").\n",
    )
    .unwrap();
    assert_eq!(ops.len(), 2);
    assert_eq!(ops[0].kind, MutationOpKind::Insert);
    assert_eq!(ops[0].relation, "container");
    assert_eq!(ops[1].kind, MutationOpKind::Retract);
    assert_eq!(ops[1].relation, "container");
    assert_eq!(ops[1].tuple.len(), 2);

    assert!(mangle_server::fsm::mutations_to_ops("").unwrap().is_empty());
}

// --- RPC level -------------------------------------------------------------

async fn spawn_raft_server() -> (String, AppState) {
    // A tempdir for the raft data; leaked intentionally so it outlives
    // the spawned server.
    let dir = tempfile::tempdir().unwrap().into_path();

    let state: AppState = Arc::new(RwLock::new(ProgramStore::new()));
    let engine = start_node(state.clone(), &dir).await;

    let app = mangle_server::app::build_app(state.clone(), Some(Arc::new(engine)));
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

#[tokio::test]
async fn rpc_writes_go_through_raft_and_reload_is_disabled() {
    let (url, state) = spawn_raft_server().await;
    let c = client(&url);

    // LoadProgram through the RPC: one raft round-trip.
    c.load_program(pb::LoadProgramRequest {
        name: "graph".into(),
        source: GRAPH_SOURCE.into(),
        ..Default::default()
    })
    .await
    .unwrap();

    // Mutations through the RPC.
    let summary = c
        .insert_facts(connectrpc::stream_iter(vec![
            pb::MutationRequest {
                program: "graph".into(),
                payload: Some(pb::__buffa::oneof::mutation_request::Payload::Fact(Box::new(
                    mangle_proto::fact_to_proto("edge", &[Value::Number(1), Value::Number(2)]),
                ))),
                ..Default::default()
            },
        ]))
        .await
        .unwrap()
        .into_owned();
    assert_eq!(summary.num_inserted, 1);

    // The fact is applied to the shared state.
    assert_eq!(graph_facts(&state).len(), 1);

    // Reload* is disabled in replicated mode.
    let err = c
        .reload_program(pb::ReloadProgramRequest {
            name: "graph".into(),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(err.code, connectrpc::ErrorCode::FailedPrecondition);
    assert!(err.message.as_deref().unwrap_or_default().contains("raft"));

    let err = c.reload_all(pb::ReloadAllRequest::default()).await.unwrap_err();
    assert_eq!(err.code, connectrpc::ErrorCode::FailedPrecondition);
}
