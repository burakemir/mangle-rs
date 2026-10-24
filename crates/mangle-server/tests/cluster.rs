//! Phase 2 end-to-end tests: multi-node Raft consensus over real TCP
//! (ConnectRPC transport, `mangle.RaftService`).
//!
//! Covers the DISTRIBUTED_PLAN.md Phase 2 deliverables:
//! - 3-node static bootstrap, leader election, replicated writes visible
//!   on all nodes
//! - follower writes rejected with a leader hint
//! - leader failover: a new leader is elected and accepts writes
//! - cluster join: a 4th node joins via the Join admin RPC and catches up
//!   (snapshot or log replay)
//! - node restart catch-up

use std::sync::{Arc, RwLock};

use connectrpc::client::{ClientConfig, HttpClient};
use mangle_common::Value;
use mangle_proto::pb::mangle as pb;
use mangle_proto::pb::mangle::MangleServiceClient;
use mangle_proto::{Command, LoadProgramCommand, MutateCommand, MutationOp, MutationOpKind};
use mangle_raft::{ConnectNetworkFactory, Engine};
use mangle_server::fsm::FsmStore;
use mangle_server::store::{AppState, ProgramStore};
use mangle_wal::RaftLog;

const GRAPH_SOURCE: &str = "path(X, Y) :- edge(X, Y).\npath(X, Z) :- edge(X, Y), path(Y, Z).\n";

struct Node {
    url: String,
    /// Advertised raft address (same listener).
    raft_addr: String,
    state: AppState,
    engine: Engine,
    dir: std::path::PathBuf,
    server: tokio::task::JoinHandle<()>,
}

impl Node {
    /// Stop the HTTP server and the raft engine, releasing the port so a
    /// restarted node can rebind the same advertised address.
    async fn stop(self) {
        self.server.abort();
        self.engine.shutdown().await.unwrap();
    }
}

async fn start_node(id: u64, dir: std::path::PathBuf) -> Node {
    let state: AppState = Arc::new(RwLock::new(ProgramStore::new()));
    let fsm = FsmStore::new(state.clone(), &dir);
    let log = RaftLog::open(dir.join("raft.redb")).unwrap();
    let engine = Engine::with_network(
        id,
        test_config(),
        ConnectNetworkFactory::new(),
        log,
        fsm,
    )
    .await
    .unwrap();

    let app = mangle_server::app::build_app(state.clone(), Some(Arc::new(engine.clone())));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    Node {
        url: format!("http://{addr}"),
        raft_addr: addr.to_string(),
        state,
        engine,
        dir,
        server,
    }
}

fn test_config() -> openraft::Config {
    let mut config = Engine::raft_config(1024);
    // Faster elections for tests.
    config.election_timeout_min = 200;
    config.election_timeout_max = 400;
    config.heartbeat_interval = 50;
    config
}

async fn bootstrap_cluster(nodes: &[&Node]) {
    let mut members = std::collections::BTreeMap::new();
    for n in nodes {
        members.insert(n.engine.id(), openraft::BasicNode { addr: n.raft_addr.clone() });
    }
    nodes[0].engine.bootstrap(members).await.unwrap();
}

async fn wait_for_leader(nodes: &[&Node]) -> usize {
    for _ in 0..400 {
        for (i, n) in nodes.iter().enumerate() {
            if n.engine.is_leader().await {
                return i;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("no leader elected within 10s");
}

fn client(url: &str) -> MangleServiceClient<HttpClient> {
    MangleServiceClient::new(
        HttpClient::plaintext(),
        ClientConfig::new(url.parse().unwrap()),
    )
}

fn insert_op(relation: &str, tuple: Vec<Value>) -> MutationOp {
    MutationOp {
        kind: MutationOpKind::Insert,
        relation: relation.to_string(),
        tuple,
    }
}

fn facts(state: &AppState) -> Vec<Vec<Value>> {
    state
        .read()
        .unwrap()
        .execute_query("graph", "path(X, Y)")
        .unwrap_or_default()
}

async fn write_via_rpc(node: &Node, cmd: Command) -> mangle_proto::ApplyResult {
    let c = client(&node.url);
    match cmd {
        Command::LoadProgram(l) => c
            .load_program(pb::LoadProgramRequest {
                name: l.name,
                source: l.source,
                ..Default::default()
            })
            .await
            .map(|_| Ok(mangle_proto::ApplyOutcome::Loaded { name: String::new(), predicates: vec![] }))
            .unwrap(),
        Command::Mutate(m) => {
            let reqs: Vec<pb::MutationRequest> = m
                .ops
                .into_iter()
                .map(|op| pb::MutationRequest {
                    program: m.program.clone(),
                    payload: Some(pb::__buffa::oneof::mutation_request::Payload::Fact(Box::new(
                        mangle_proto::fact_to_proto(&op.relation, &op.tuple),
                    ))),
                    ..Default::default()
                })
                .collect();
            let summary = c.insert_facts(connectrpc::stream_iter(reqs)).await.unwrap().into_owned();
            Ok(mangle_proto::ApplyOutcome::Mutated {
                num_applied: summary.num_inserted,
                num_skipped: summary.num_skipped,
            })
        }
        other => panic!("unsupported in test helper: {other:?}"),
    }
}

#[tokio::test]
async fn three_node_cluster_replicates_and_fails_over() {
    let dirs: Vec<_> = (0..3)
        .map(|i| tempfile::tempdir().unwrap().into_path().join(format!("n{i}")))
        .collect();
    let mut nodes = Vec::new();
    for (i, dir) in dirs.iter().enumerate() {
        std::fs::create_dir_all(dir).unwrap();
        nodes.push(start_node(i as u64 + 1, dir.clone()).await);
    }
    let node_refs: Vec<&Node> = nodes.iter().collect();
    bootstrap_cluster(&node_refs).await;
    let leader = wait_for_leader(&node_refs).await;
    eprintln!("leader elected: node {}", leader + 1);

    // Load a program and write facts through the leader's RPC API.
    write_via_rpc(
        &nodes[leader],
        Command::LoadProgram(LoadProgramCommand {
            name: "graph".into(),
            source: GRAPH_SOURCE.into(),
        }),
    )
    .await
    .unwrap();

    let summary = write_via_rpc(
        &nodes[leader],
        Command::Mutate(MutateCommand {
            program: "graph".into(),
            ops: vec![
                insert_op("edge", vec![Value::Number(1), Value::Number(2)]),
                insert_op("edge", vec![Value::Number(2), Value::Number(3)]),
            ],
        }),
    )
    .await
    .unwrap();
    assert_eq!(
        summary,
        mangle_proto::ApplyOutcome::Mutated { num_applied: 2, num_skipped: 0 }
    );

    // The write replicates to all nodes (each applies to its own store).
    wait_until(60, || {
        node_refs
            .iter()
            .all(|n| facts(&n.state).len() == 3)
    })
    .await;
    for (i, n) in node_refs.iter().enumerate() {
        assert_eq!(facts(&n.state).len(), 3, "node {i} must have all facts");
    }

    // Follower writes are rejected with failed_precondition.
    let follower = (leader + 1) % 3;
    let c = client(&nodes[follower].url);
    let err = c
        .insert_facts(connectrpc::stream_iter(vec![pb::MutationRequest {
            program: "graph".into(),
            payload: Some(pb::__buffa::oneof::mutation_request::Payload::Fact(Box::new(
                mangle_proto::fact_to_proto("edge", &[Value::Number(9), Value::Number(9)]),
            ))),
            ..Default::default()
        }]))
        .await
        .unwrap_err();
    assert_eq!(err.code, connectrpc::ErrorCode::FailedPrecondition);
    assert!(err.message.as_deref().unwrap_or_default().contains("not leader"));

    // --- Leader failover -------------------------------------------------
    let old_dir = nodes[leader].dir.clone();
    let old_id = nodes[leader].engine.id();
    let old_addr = nodes[leader].raft_addr.clone();
    let old_state = nodes[leader].state.clone();
    let old = nodes.remove(leader);
    old.stop().await;
    eprintln!("stopped old leader node {old_id}");

    let remaining: Vec<&Node> = nodes.iter().collect();
    let new_leader_idx = wait_for_leader(&remaining).await;
    eprintln!("new leader elected: node at index {new_leader_idx}");

    // Write through the new leader; the old leader is gone but the
    // remaining two form a quorum.
    write_via_rpc(
        &remaining[new_leader_idx],
        Command::Mutate(MutateCommand {
            program: "graph".into(),
            ops: vec![insert_op("edge", vec![Value::Number(3), Value::Number(4)])],
        }),
    )
    .await
    .unwrap();

    wait_until(60, || {
        remaining
            .iter()
            .all(|n| facts(&n.state).len() == 6)
    })
    .await;

    // --- Restart the old leader: it catches up ---------------------------
    // Rebind the same advertised address so the cluster can reach it.
    let revived = restart_node(old_id, old_dir, old_addr, old_state).await;
    wait_until(120, || {
        let (_, applied, _) = revived.engine.progress();
        applied >= 4 // membership + load + 2 mutates (approx; just "caught up")
    })
    .await;
    wait_until(120, || facts(&revived.state).len() == 6).await;

    for n in nodes {
        n.stop().await;
    }
    revived.stop().await;
}

#[tokio::test]
async fn join_flow_adds_node_to_cluster() {
    let dirs: Vec<_> = (0..3)
        .map(|i| tempfile::tempdir().unwrap().into_path().join(format!("n{i}")))
        .collect();
    let mut nodes = Vec::new();
    for (i, dir) in dirs.iter().enumerate() {
        std::fs::create_dir_all(dir).unwrap();
        nodes.push(start_node(i as u64 + 1, dir.clone()).await);
    }
    let node_refs: Vec<&Node> = nodes.iter().collect();
    bootstrap_cluster(&node_refs[..2]).await; // 2-node cluster
    let leader = wait_for_leader(&node_refs[..2]).await;

    write_via_rpc(
        &nodes[leader],
        Command::LoadProgram(LoadProgramCommand {
            name: "graph".into(),
            source: GRAPH_SOURCE.into(),
        }),
    )
    .await
    .unwrap();
    write_via_rpc(
        &nodes[leader],
        Command::Mutate(MutateCommand {
            program: "graph".into(),
            ops: vec![
                insert_op("edge", vec![Value::Number(1), Value::Number(2)]),
                insert_op("edge", vec![Value::Number(2), Value::Number(3)]),
            ],
        }),
    )
    .await
    .unwrap();
    wait_until(60, || node_refs[..2].iter().all(|n| facts(&n.state).len() == 3)).await;

    // Node 3 joins via the Join admin RPC on the leader.
    let c = client(&nodes[leader].url);
    c.join(pb::JoinRequest {
        node_id: 3,
        addr: nodes[2].raft_addr.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        ..Default::default()
    })
    .await
    .unwrap();

    // It receives the state (log replay and/or snapshot) and becomes a
    // voting member with the same data.
    wait_until(240, || facts(&nodes[2].state).len() == 3).await;
    let (_, applied, _) = nodes[2].engine.progress();
    assert!(applied >= 3, "joiner must have applied the log");

    // It shows up in the leader's membership as a voter.
    let status = c.node_status(pb::NodeStatusRequest::default()).await.unwrap().into_owned();
    assert!(status.raft_enabled);
    let m3 = status.members.iter().any(|m| m.node_id == 3 && m.is_voter);
    assert!(m3, "node 3 must appear in membership");

    for n in nodes {
        n.stop().await;
    }
}

/// Restart a node on the same directory and advertised address, sharing
/// its state handle (so the test can observe the same store).
async fn restart_node(id: u64, dir: std::path::PathBuf, raft_addr: String, state: AppState) -> Node {
    let fsm = FsmStore::new(state.clone(), &dir);
    fsm.restore().unwrap();
    let log = RaftLog::open(dir.join("raft.redb")).unwrap();
    let engine = Engine::with_network(id, test_config(), ConnectNetworkFactory::new(), log, fsm)
        .await
        .unwrap();

    let app = mangle_server::app::build_app(state.clone(), Some(Arc::new(engine.clone())));
    let listener = tokio::net::TcpListener::bind(&raft_addr).await.unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    Node {
        url: format!("http://{raft_addr}"),
        raft_addr,
        state,
        engine,
        dir,
        server,
    }
}

/// Poll `cond` until true, `ticks` * 50ms total.
async fn wait_until<F: Fn() -> bool>(ticks: usize, cond: F) {
    for _ in 0..ticks {
        if cond() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("condition not met within timeout");
}


#[tokio::test]
async fn consistency_levels_none_weak_strong() {
    let dirs: Vec<_> = (0..3)
        .map(|i| tempfile::tempdir().unwrap().into_path().join(format!("n{i}")))
        .collect();
    let mut nodes = Vec::new();
    for (i, dir) in dirs.iter().enumerate() {
        std::fs::create_dir_all(dir).unwrap();
        nodes.push(start_node(i as u64 + 1, dir.clone()).await);
    }
    let node_refs: Vec<&Node> = nodes.iter().collect();
    bootstrap_cluster(&node_refs).await;
    let leader = wait_for_leader(&node_refs).await;
    let follower = (leader + 1) % 3;

    write_via_rpc(
        &nodes[leader],
        Command::LoadProgram(LoadProgramCommand {
            name: "graph".into(),
            source: GRAPH_SOURCE.into(),
        }),
    )
    .await
    .unwrap();
    write_via_rpc(
        &nodes[leader],
        Command::Mutate(MutateCommand {
            program: "graph".into(),
            ops: vec![insert_op("edge", vec![Value::Number(1), Value::Number(2)])],
        }),
    )
    .await
    .unwrap();
    wait_until(60, || node_refs.iter().all(|n| facts(&n.state).len() == 1)).await;

    let q = |consistency: i32| {
        pb::QueryRequest {
            program: "graph".into(),
            query: Some(pb::__buffa::oneof::query_request::Query::Text("path(X, Y)".into())),
            consistency: consistency.into(),
            ..Default::default()
        }
    };

    // NONE: any node answers.
    for (i, n) in node_refs.iter().enumerate() {
        let stream = client(&n.url).query(q(0)).await.unwrap();
        let rows = collect(stream).await;
        assert_eq!(rows.len(), 1, "NONE on node {i}");
    }

    // WEAK: leader answers, follower rejects with a hint.
    let stream = client(&nodes[leader].url).query(q(1)).await.unwrap();
    assert_eq!(collect(stream).await.len(), 1);
    expect_stream_error(
        client(&nodes[follower].url).query(q(1)).await.unwrap(),
        connectrpc::ErrorCode::FailedPrecondition,
        "not leader",
    )
    .await;

    // STRONG: the leader proposes a no-op barrier and answers; the
    // follower rejects (barriers are writes). The applied index advances.
    let applied_before = nodes[leader].engine.last_applied();
    let stream = client(&nodes[leader].url).query(q(2)).await.unwrap();
    assert_eq!(collect(stream).await.len(), 1);
    let applied_after = nodes[leader].engine.last_applied();
    assert!(
        applied_after > applied_before,
        "STRONG read must advance the applied index (barrier entry): {applied_before} -> {applied_after}"
    );
    expect_stream_error(
        client(&nodes[follower].url).query(q(2)).await.unwrap(),
        connectrpc::ErrorCode::FailedPrecondition,
        "not leader",
    )
    .await;

    // Strong reads are fresh: write, then STRONG-read on the leader must
    // observe it (trivially true on the leader, but pins the contract).
    write_via_rpc(
        &nodes[leader],
        Command::Mutate(MutateCommand {
            program: "graph".into(),
            ops: vec![insert_op("edge", vec![Value::Number(2), Value::Number(3)])],
        }),
    )
    .await
    .unwrap();
    let stream = client(&nodes[leader].url).query(q(2)).await.unwrap();
    assert_eq!(collect(stream).await.len(), 3);

    // NodeStatus reports matching fingerprints on all nodes (same
    // programs, same sources).
    let mut first_fp: Option<String> = None;
    for (i, n) in node_refs.iter().enumerate() {
        let status = client(&n.url)
            .node_status(pb::NodeStatusRequest::default())
            .await
            .unwrap()
            .into_owned();
        let fp = status.fingerprints.get("graph").expect("graph fingerprint");
        assert_eq!(fp.program_hash.len(), 64, "sha-256 hex");
        // No external EDB sources here: the fingerprint is the constant
        // hash of nothing, but critically it is IDENTICAL across nodes.
        if i == 0 {
            first_fp = Some(fp.edb_fingerprint.clone());
        } else {
            assert_eq!(
                fp.edb_fingerprint,
                first_fp.clone().unwrap(),
                "all nodes must agree on the EDB fingerprint"
            );
        }
    }

    for n in nodes {
        n.stop().await;
    }
}

/// Expect the stream to fail with the given Connect code (streaming RPCs
/// deliver errors on the first message, not at call time).
async fn expect_stream_error(
    stream: connectrpc::client::ServerStream<
        <HttpClient as connectrpc::client::ClientTransport>::ResponseBody,
        pb::QueryEventView<'static>,
    >,
    code: connectrpc::ErrorCode,
    needle: &str,
) {
    let mut stream = stream;
    let result = stream.message::<pb::QueryEvent>().await;
    let err = result.unwrap_err();
    assert_eq!(err.code, code, "unexpected error: {}", err.message.display_or_empty());
    assert!(err.message.as_deref().unwrap_or_default().contains(needle), "error message: {err:?}");
}

trait MsgDisplay {
    fn display_or_empty(&self) -> String;
}
impl MsgDisplay for Option<String> {
    fn display_or_empty(&self) -> String {
        self.clone().unwrap_or_default()
    }
}

/// Collect a Query stream's rows.
async fn collect(
    mut stream: connectrpc::client::ServerStream<
        <HttpClient as connectrpc::client::ClientTransport>::ResponseBody,
        pb::QueryEventView<'static>,
    >,
) -> Vec<Vec<Value>> {
    use mangle_proto::pb::mangle as pb0;
    use pb0::__buffa::view::oneof::query_event::Event;
    let mut rows = Vec::new();
    while let Some(msg) = stream.message::<pb::QueryEvent>().await.unwrap() {
        if let Event::Batch(b) = msg.event().unwrap() {
            rows.extend(mangle_proto::rows_from_batch_view(b).unwrap());
        }
    }
    rows
}
