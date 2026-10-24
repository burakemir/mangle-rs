//! Connect RPC implementation of `MangleService`, backed by `ProgramStore`.
//!
//! See `RPC_DESIGN.md` in this crate for the design. The JSON HTTP handlers
//! that preceded this API have been removed.

use anyhow::Result;
use buffa::Enumeration as _;
use connectrpc::{
    ConnectError, RequestContext, Response, ServiceRequest, ServiceResult, ServiceStream,
};
use mangle_common::Value;
use mangle_proto::pb::mangle as pb;
use pb::{
    Column, ColumnType, Empty, FactBatch, ListProgramsRequest, ListProgramsResponse,
    LoadProgramRequest, MutationSummary, ProgramInfo, QueryEvent, QueryRequest, QueryStats,
    ResultEncoding,
};
use tokio_stream::StreamExt as _;

use crate::query::QueryArg;
use crate::store::{
    AppState, ApplyOutcome, ProgramStore, eval_source_relations, parse_query_lenient,
};
use mangle_proto::{Command, MutateCommand, MutationOpKind};
use mangle_raft::Engine;

use pb::__buffa::oneof::query_event::Event;
use pb::__buffa::view::oneof::arg_pattern::Pattern;
use pb::__buffa::view::oneof::mutation_request::Payload;
use pb::__buffa::view::oneof::query_request::Query;

/// Default number of facts per streamed FactBatch.
const DEFAULT_BATCH_SIZE: usize = 4096;

/// Cap on ops per MutateCommand (§7.2): a command is fully materialized
/// before it is applied (and proposed to the Raft log in replicated
/// mode), so an unbounded client stream must not become an unbounded
/// command. Bulk loads should batch per request.
const MAX_MUTATIONS_PER_COMMAND: usize = 1_048_576;

pub struct MangleServiceImpl {
    pub state: AppState,
    /// The Raft engine handle. `Some` in replicated mode: writes are
    /// proposed through Raft (WAL fsync → commit → apply) before the RPC
    /// responds. `None` (no `raft_dir` configured): writes apply directly,
    /// pre-replication behavior.
    pub engine: Option<std::sync::Arc<Engine>>,
}

// --- Error classification -------------------------------------------------
//
// TODO(RPC_DESIGN.md §5): replace message sniffing with a typed error enum
// in ProgramStore; this centralizes the classification in one place until
// then.
fn classify(e: anyhow::Error) -> ConnectError {
    let msg = e.to_string();
    if msg.contains("not found") || msg.contains("cannot read") {
        ConnectError::not_found(msg)
    } else if msg.contains("invalid query")
        || msg.contains("cannot extract predicate")
        || msg.contains("parse")
        || msg.contains("column ")
        || msg.contains("unsupported")
        || msg.contains("defined by the program source")
        || msg.contains("mutation stream")
    {
        ConnectError::invalid_argument(msg)
    } else {
        ConnectError::internal(msg)
    }
}

// --- Helpers ---------------------------------------------------------------

/// A `failed_precondition` error carrying the leader hint.
fn not_leader(leader_id: Option<u64>, leader_addr: Option<String>) -> ConnectError {
    ConnectError::failed_precondition(format!(
        "not leader; {}",
        match (leader_id, leader_addr) {
            (Some(id), Some(addr)) => format!("leader is node {id} at {addr}"),
            (Some(id), None) => format!("leader is node {id}"),
            _ => "leader unknown".to_string(),
        }
    ))
}

/// Lowercase hex of a byte slice (fingerprints on the wire).
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Build the stream of QueryEvents for one relation's result rows.
///
/// `columnar` selects the batch encoding: typed packed columns vs. all-ANY
/// columns carrying row-encoded values (the `ROWS` result encoding).
fn relation_events(
    relation: &str,
    rows: Vec<Vec<Value>>,
    batch_size: usize,
    columnar: bool,
    stats: &mut QueryStatsAccum,
) -> Vec<QueryEvent> {
    let mut events = Vec::new();
    for chunk in rows.chunks(batch_size.max(1)) {
        let batch = if columnar {
            mangle_proto::batch_from_rows(relation, chunk)
        } else {
            row_encoded_batch(relation, chunk)
        };
        stats.num_facts += chunk.len() as u64;
        events.push(batch_event(batch));
    }
    events
}

/// A `FactBatch` whose columns are all `ANY` (row-encoded values) — the
/// wire form of the `ROWS` result encoding.
fn row_encoded_batch(relation: &str, rows: &[Vec<Value>]) -> FactBatch {
    let arity = rows.first().map_or(0, |r| r.len());
    FactBatch {
        relation: relation.to_string(),
        num_rows: rows.len() as u32,
        columns: (0..arity)
            .map(|j| Column {
                r#type: ColumnType::ANY.into(),
                values: rows
                    .iter()
                    .map(|r| mangle_proto::value_to_proto(&r[j]))
                    .collect(),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}

fn batch_event(batch: FactBatch) -> QueryEvent {
    QueryEvent {
        event: Some(Event::Batch(Box::new(batch))),
        ..Default::default()
    }
}

struct QueryStatsAccum {
    num_facts: u64,
    elapsed_us: u64,
}

/// A parsed query: predicate name plus per-column patterns
/// (`None` = wildcard, `Some(v)` = constant that must match).
struct ResolvedQuery {
    predicate: String,
    patterns: Vec<Option<Value>>,
}

/// Convert a structured query's argument patterns to internal values.
fn patterns_from_view(
    args: &pb::__buffa::view::StructuredQueryView<'_>,
) -> Result<Vec<Option<Value>>> {
    let mut patterns = Vec::with_capacity(args.args.len());
    for a in args.args.iter() {
        let p = match a.pattern.as_ref() {
            None | Some(Pattern::Any(_)) => None,
            Some(Pattern::Const(v)) => Some(mangle_proto::value_from_view(v)?),
        };
        patterns.push(p);
    }
    Ok(patterns)
}

/// Resolve a textual query to predicate + patterns. The constant types
/// recognized in query text match the (deprecated) JSON API: strings,
/// names, integers.
fn resolve_text_query(text: &str) -> Result<ResolvedQuery> {
    let parsed = parse_query_lenient(text)?;
    let patterns = parsed
        .args
        .iter()
        .map(|a| match a {
            QueryArg::Variable => None,
            QueryArg::StringConst(s) => Some(Value::String(s.clone())),
            QueryArg::NameConst(s) => Some(Value::Name(s.clone())),
            QueryArg::NumberConst(n) => Some(Value::Number(*n)),
        })
        .collect();
    Ok(ResolvedQuery {
        predicate: parsed.predicate,
        patterns,
    })
}

fn program_info(name: &str, predicates: Vec<String>, source: Option<String>) -> ProgramInfo {
    ProgramInfo {
        name: name.to_string(),
        predicates,
        source: source.unwrap_or_default(),
        ..Default::default()
    }
}

// --- Service implementation ------------------------------------------------

// `use<>` capture lists narrow the trait's captures (our responses are all
// owned data); that is intentional, but the lint fires on every handler.
#[allow(refining_impl_trait)]
impl pb::MangleService for MangleServiceImpl {
    async fn query(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, QueryRequest>,
    ) -> ServiceResult<ServiceStream<impl connectrpc::Encodable<QueryEvent> + Send + use<>>> {
        let program = request.program.to_string();
        let columnar = request.encoding.to_i32() == ResultEncoding::COLUMNAR.to_i32();
        let batch_size = if request.batch_size == 0 {
            DEFAULT_BATCH_SIZE
        } else {
            request.batch_size as usize
        };
        let limit = request.limit as usize;

        // Read consistency (plan §3.7). NONE (default): any node answers
        // from local state. WEAK: leader-only. STRONG: propose a no-op and
        // wait for it to apply — the local state then reflects everything
        // committed before this read. Outside replicated mode all levels
        // are equivalent (local state is the only state).
        let consistency = request
            .consistency
            .as_known()
            .unwrap_or(pb::ConsistencyLevel::NONE);
        if let Some(engine) = &self.engine {
            match consistency {
                pb::ConsistencyLevel::WEAK => {
                    if !engine.is_leader().await {
                        let (leader_id, leader_addr) = engine.leader_hint();
                        return Err(not_leader(leader_id, leader_addr));
                    }
                }
                pb::ConsistencyLevel::STRONG => {
                    engine.read_barrier().await.map_err(|e| match e {
                        mangle_raft::WriteError::NotLeader {
                            leader_id,
                            leader_addr,
                        } => not_leader(leader_id, leader_addr),
                        mangle_raft::WriteError::Other(msg) => {
                            ConnectError::unavailable(msg)
                        }
                    })?;
                }
                _ => {}
            }
        }

        let resolved = match request.query.as_ref() {
            Some(Query::Text(t)) => resolve_text_query(t).map_err(classify)?,
            Some(Query::Structured(s)) => ResolvedQuery {
                predicate: s.predicate.to_string(),
                patterns: patterns_from_view(s).map_err(classify)?,
            },
            None => {
                return Err(ConnectError::invalid_argument(
                    "query is required (text or structured)",
                ));
            }
        };

        let predicate_name = resolved.predicate.clone();
        let state = self.state.clone();
        let started = std::time::Instant::now();
        // Evaluation is CPU-bound and synchronous; keep it off the async
        // executor. The result is materialized today — true incremental
        // streaming is a follow-up (RPC_DESIGN.md §5).
        let rows = tokio::task::spawn_blocking(move || -> Result<Vec<Vec<Value>>> {
            let store: &ProgramStore = &state.read().unwrap();
            store.execute_query_patterns(&program, &resolved.predicate, &resolved.patterns)
        })
        .await
        .map_err(|e| ConnectError::internal(format!("query task failed: {e}")))?
        .map_err(classify)?;

        let rows: Vec<_> = if limit > 0 {
            rows.into_iter().take(limit).collect()
        } else {
            rows
        };

        let mut accum = QueryStatsAccum {
            num_facts: 0,
            elapsed_us: started.elapsed().as_micros() as u64,
        };
        let events = relation_events(&predicate_name, rows, batch_size, columnar, &mut accum);

        // TODO: populate `variables` with the variable names bound by a
        // textual query once query parsing exposes them.
        let done = QueryEvent {
            event: Some(Event::Done(Box::new(QueryStats {
                num_facts: accum.num_facts,
                elapsed_us: accum.elapsed_us,
                ..Default::default()
            }))),
            ..Default::default()
        };

        let mut all = events;
        all.push(done);
        let stream: ServiceStream<QueryEvent> = Box::pin(tokio_stream::iter(
            all.into_iter().map(Ok::<_, ConnectError>),
        ));
        Response::ok(stream)
    }

    async fn eval(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, pb::EvalRequest>,
    ) -> ServiceResult<ServiceStream<impl connectrpc::Encodable<QueryEvent> + Send + use<>>> {
        let source = request.source.to_string();
        let libs: Vec<String> = request.libs.iter().map(|s| (*s).to_string()).collect();
        let query = request.query.to_string();
        let query_opt = if query.is_empty() { None } else { Some(query) };
        let columnar = request.encoding.to_i32() == ResultEncoding::COLUMNAR.to_i32();
        let batch_size = if request.batch_size == 0 {
            DEFAULT_BATCH_SIZE
        } else {
            request.batch_size as usize
        };
        let limit = request.limit as usize;

        let started = std::time::Instant::now();
        let relations = tokio::task::spawn_blocking(move || {
            let mut sources = vec![source];
            sources.extend(libs);
            let sources: Vec<&str> = sources.iter().map(|s| s.as_str()).collect();
            eval_source_relations(&sources, query_opt.as_deref())
        })
        .await
        .map_err(|e| ConnectError::internal(format!("eval task failed: {e}")))?
        .map_err(classify)?;

        let mut accum = QueryStatsAccum {
            num_facts: 0,
            elapsed_us: started.elapsed().as_micros() as u64,
        };
        let mut all = Vec::new();
        for (relation, rows) in relations {
            let rows: Vec<_> = if limit > 0 {
                let remaining = (limit as u64).saturating_sub(accum.num_facts);
                rows.into_iter().take(remaining as usize).collect()
            } else {
                rows
            };
            if rows.is_empty() {
                continue;
            }
            all.extend(relation_events(
                &relation, rows, batch_size, columnar, &mut accum,
            ));
        }
        all.push(QueryEvent {
            event: Some(Event::Done(Box::new(QueryStats {
                num_facts: accum.num_facts,
                elapsed_us: accum.elapsed_us,
                ..Default::default()
            }))),
            ..Default::default()
        });

        let stream: ServiceStream<QueryEvent> = Box::pin(tokio_stream::iter(
            all.into_iter().map(Ok::<_, ConnectError>),
        ));
        Response::ok(stream)
    }

    async fn insert_facts(
        &self,
        _ctx: RequestContext,
        requests: connectrpc::InboundStream<pb::MutationRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<MutationSummary> + Send> {
        self.apply_mutations(requests, true).await
    }

    async fn retract_facts(
        &self,
        _ctx: RequestContext,
        requests: connectrpc::InboundStream<pb::MutationRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<MutationSummary> + Send> {
        self.apply_mutations(requests, false).await
    }

    async fn list_programs(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, ListProgramsRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<ListProgramsResponse> + Send + use<>> {
        let store = self.state.read().unwrap();
        let programs = store
            .list()
            .into_iter()
            .map(|p| program_info(&p.name, p.predicates, None))
            .collect();
        Response::ok(ListProgramsResponse {
            programs,
            ..Default::default()
        })
    }

    async fn load_program(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, LoadProgramRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<ProgramInfo> + Send + use<>> {
        // Writes go through the command path (one log entry in
        // replicated mode).
        let cmd = Command::LoadProgram(mangle_proto::LoadProgramCommand {
            name: request.name.to_string(),
            source: request.source.to_string(),
        });
        match self.execute_command(cmd).await? {
            ApplyOutcome::Loaded { name, predicates } => {
                Response::ok(program_info(&name, predicates, None))
            }
            _ => Err(ConnectError::internal(
                "unexpected apply outcome for load-program command",
            )),
        }
    }

    async fn get_program(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, pb::GetProgramRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<ProgramInfo> + Send + use<>> {
        let name = request.name.to_string();
        let store = self.state.read().unwrap();
        let prog = store
            .get(&name)
            .ok_or_else(|| ConnectError::not_found(format!("program '{name}' not found")))?;
        Response::ok(program_info(
            &name,
            prog.predicates.clone(),
            Some(prog.source.clone()),
        ))
    }

    async fn delete_program(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, pb::DeleteProgramRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<Empty> + Send + use<>> {
        // Writes go through the command path (one log entry in
        // replicated mode).
        let cmd = Command::DeleteProgram(mangle_proto::DeleteProgramCommand {
            name: request.name.to_string(),
        });
        match self.execute_command(cmd).await? {
            ApplyOutcome::Deleted => Response::ok(Empty::default()),
            _ => Err(ConnectError::internal(
                "unexpected apply outcome for delete-program command",
            )),
        }
    }

    async fn reload_program(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, pb::ReloadProgramRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<ProgramInfo> + Send + use<>> {
        // Reload reads the node's local programs_dir; in replicated mode
        // that would mutate one node's state outside the log and diverge
        // the cluster (DISTRIBUTED_PLAN.md §3.1). Use LoadProgram with the
        // new source instead.
        if self.engine.is_some() {
            return Err(ConnectError::failed_precondition(
                "ReloadProgram is disabled in replicated (raft) mode: it reads the local \
                 programs_dir. Use LoadProgram with the new source.",
            ));
        }
        let name = request.name.to_string();
        let mut store = self.state.write().unwrap();
        let info = store.reload(&name).map_err(classify)?;
        Response::ok(program_info(&info.name, info.predicates, None))
    }

    async fn reload_all(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, pb::ReloadAllRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<ListProgramsResponse> + Send + use<>> {
        // See reload_program: local-disk reads cannot be replicated.
        if self.engine.is_some() {
            return Err(ConnectError::failed_precondition(
                "ReloadAll is disabled in replicated (raft) mode: it reads the local \
                 programs_dir. Use LoadProgram for each program instead.",
            ));
        }
        let mut store = self.state.write().unwrap();
        let loaded = store.reload_all().map_err(classify)?;
        let programs = loaded
            .into_iter()
            .map(|p| program_info(&p.name, p.predicates, None))
            .collect();
        Response::ok(ListProgramsResponse {
            programs,
            ..Default::default()
        })
    }

    // --- Cluster administration ----------------------------------------

    async fn join(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, pb::JoinRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<pb::JoinResponse> + Send + use<>> {
        let Some(engine) = &self.engine else {
            return Err(ConnectError::failed_precondition(
                "Join is only available in replicated (raft) mode",
            ));
        };
        if !engine.is_leader().await {
            let (leader_id, leader_addr) = engine.leader_hint();
            return Err(ConnectError::failed_precondition(format!(
                "not leader; {}",
                match (leader_id, leader_addr) {
                    (Some(id), Some(addr)) => format!("leader is node {id} at {addr}"),
                    (Some(id), None) => format!("leader is node {id}"),
                    _ => "leader unknown".to_string(),
                }
            )));
        }

        // Mixed-version clusters are unsupported (plan §7.5).
        let caller_version = request.version.to_string();
        if caller_version != env!("CARGO_PKG_VERSION") {
            return Err(ConnectError::failed_precondition(format!(
                "version mismatch: joining node is {}, this node is {}",
                caller_version,
                env!("CARGO_PKG_VERSION")
            )));
        }

        let node_id = request.node_id;
        let addr = request.addr.to_string();
        mangle_raft::join_member(engine.raft(), node_id, addr)
            .await
            .map_err(|e| ConnectError::internal(format!("join failed: {e}")))?;

        Response::ok(pb::JoinResponse {
            leader_id: engine.id(),
            ..Default::default()
        })
    }

    async fn remove_node(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, pb::RemoveNodeRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<Empty> + Send + use<>> {
        let Some(engine) = &self.engine else {
            return Err(ConnectError::failed_precondition(
                "RemoveNode is only available in replicated (raft) mode",
            ));
        };
        if !engine.is_leader().await {
            let (leader_id, leader_addr) = engine.leader_hint();
            return Err(ConnectError::failed_precondition(format!(
                "not leader; {}",
                match (leader_id, leader_addr) {
                    (Some(id), Some(addr)) => format!("leader is node {id} at {addr}"),
                    (Some(id), None) => format!("leader is node {id}"),
                    _ => "leader unknown".to_string(),
                }
            )));
        }
        mangle_raft::remove_member(engine.raft(), request.node_id)
            .await
            .map_err(|e| ConnectError::internal(format!("remove-node failed: {e}")))?;
        Response::ok(Empty::default())
    }

    async fn node_status(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, pb::NodeStatusRequest>,
    ) -> ServiceResult<impl connectrpc::Encodable<pb::NodeStatusResponse> + Send + use<>> {
        let Some(engine) = &self.engine else {
            return Response::ok(pb::NodeStatusResponse {
                raft_enabled: false,
                version: env!("CARGO_PKG_VERSION").to_string(),
                fingerprints: {
                    let store = self.state.read().unwrap();
                    store
                        .edb_fingerprints()
                        .into_iter()
                        .map(|(name, (program_hash, edb_fp))| {
                            (
                                name,
                                pb::ProgramFingerprint {
                                    program_hash: hex(&program_hash),
                                    edb_fingerprint: edb_fp.as_deref().map(hex).unwrap_or_default(),
                                    ..Default::default()
                                },
                            )
                        })
                        .collect()
                },
                ..Default::default()
            });
        };
        let (term, last_applied, last_log) = engine.progress();
        let (leader_id, _) = engine.leader_hint();
        let members = engine
            .membership_summary()
            .into_iter()
            .map(|(node_id, addr, is_voter)| pb::MemberInfo {
                node_id,
                addr,
                is_voter,
                is_leader: Some(node_id) == leader_id,
                ..Default::default()
            })
            .collect();

        // Per-program fingerprints for cross-node comparison (plan §4.4).
        let fingerprints = {
            let store = self.state.read().unwrap();
            store
                .edb_fingerprints()
                .into_iter()
                .map(|(name, (program_hash, edb_fp))| {
                    (
                        name,
                        pb::ProgramFingerprint {
                            program_hash: hex(&program_hash),
                            edb_fingerprint: edb_fp.as_deref().map(hex).unwrap_or_default(),
                            ..Default::default()
                        },
                    )
                })
                .collect()
        };

        Response::ok(pb::NodeStatusResponse {
            node_id: engine.id(),
            raft_enabled: true,
            term,
            leader_id: leader_id.unwrap_or(0),
            last_applied_index: last_applied,
            last_log_index: last_log,
            members,
            version: env!("CARGO_PKG_VERSION").to_string(),
            fingerprints,
            ..Default::default()
        })
    }
}

impl MangleServiceImpl {
    /// Execute one command: through Raft when the engine is present
    /// (append to the WAL, fsync, commit, apply — then respond), else
    /// directly against the store. The apply result is deterministic in
    /// both modes. Follower writes fail with `failed_precondition` and a
    /// leader hint.
    async fn execute_command(&self, cmd: Command) -> Result<ApplyOutcome, ConnectError> {
        match &self.engine {
            Some(engine) => {
                let (_index, result) = engine.write(cmd).await.map_err(|e| match e {
                    mangle_raft::WriteError::NotLeader {
                        leader_id,
                        leader_addr,
                    } => ConnectError::failed_precondition(format!(
                        "not leader; {}",
                        match (leader_id, leader_addr) {
                            (Some(id), Some(addr)) => format!("leader is node {id} at {addr}"),
                            (Some(id), None) => format!("leader is node {id}"),
                            _ => "leader unknown".to_string(),
                        }
                    )),
                    mangle_raft::WriteError::Other(msg) => ConnectError::unavailable(msg),
                })?;
                result.map_err(|e| classify(anyhow::anyhow!(e)))
            }
            None => {
                let mut store = self.state.write().unwrap();
                store.apply(&cmd).map_err(classify)
            }
        }
    }

    /// Shared body of InsertFacts / RetractFacts: decode each streamed
    /// mutation (row or batch) from its zero-copy view and apply it.
    async fn apply_mutations(
        &self,
        mut requests: connectrpc::InboundStream<pb::MutationRequest>,
        insert: bool,
    ) -> ServiceResult<MutationSummary> {
        // Aggregate the entire client stream into ONE MutateCommand, then
        // apply it in a single batch: one command = one future log entry =
        // one IDB recompute, with all-or-nothing semantics (§3.1, §7.2).
        // An invalid fact anywhere in the stream therefore fails the whole
        // command rather than partially applying.
        let mut program: Option<String> = None;
        let mut ops: Vec<mangle_proto::MutationOp> = Vec::new();
        let kind = if insert {
            MutationOpKind::Insert
        } else {
            MutationOpKind::Retract
        };

        while let Some(item) = requests.next().await {
            let msg = item.map_err(|e| ConnectError::internal(e.to_string()))?;
            let msg_program = msg.program().to_string();
            match &program {
                None => program = Some(msg_program),
                Some(p) if p == &msg_program => {}
                Some(p) => {
                    return Err(ConnectError::invalid_argument(format!(
                        "a mutation stream must target a single program (saw '{}' and '{}'); \
                         split the stream per program",
                        p, msg_program
                    )));
                }
            }

            // Decode (relation, rows) from the zero-copy view. Strings are
            // copied exactly once, into the internal Values.
            let decoded: Vec<(String, Vec<Vec<Value>>)> = match msg.payload() {
                Some(Payload::Fact(f)) => {
                    let (relation, tuple) = mangle_proto::fact_from_view(f).map_err(classify)?;
                    vec![(relation, vec![tuple])]
                }
                Some(Payload::Batch(b)) => {
                    let relation = b.relation.to_string();
                    let rows = mangle_proto::rows_from_batch_view(b).map_err(classify)?;
                    vec![(relation, rows)]
                }
                None => {
                    return Err(ConnectError::invalid_argument(
                        "payload is required (fact or batch)",
                    ));
                }
            };

            for (relation, rows) in decoded {
                for row in rows {
                    ops.push(mangle_proto::MutationOp {
                        kind,
                        relation: relation.clone(),
                        tuple: row,
                    });
                    if ops.len() > MAX_MUTATIONS_PER_COMMAND {
                        return Err(ConnectError::resource_exhausted(format!(
                            "mutation command exceeds the cap of {} ops; \
                             split into multiple requests",
                            MAX_MUTATIONS_PER_COMMAND
                        )));
                    }
                }
            }
        }

        let program = program.ok_or_else(|| {
            ConnectError::invalid_argument("mutation stream is empty: program is required")
        })?;

        let cmd = Command::Mutate(MutateCommand { program, ops });
        let outcome = self.execute_command(cmd).await?;

        let (num_applied, num_skipped) = match outcome {
            ApplyOutcome::Mutated {
                num_applied,
                num_skipped,
            } => (num_applied, num_skipped),
            _ => {
                return Err(ConnectError::internal(
                    "unexpected apply outcome for mutate command",
                ));
            }
        };

        let summary = if insert {
            MutationSummary {
                num_inserted: num_applied,
                num_skipped,
                ..Default::default()
            }
        } else {
            MutationSummary {
                num_retracted: num_applied,
                num_skipped,
                ..Default::default()
            }
        };
        Response::ok(summary)
    }
}
