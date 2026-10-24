//! Determinism test for the command path (DISTRIBUTED_PLAN.md §4.1).
//!
//! A replicated state machine requires that applying the same command
//! sequence to two independent stores produces identical state *and*
//! identical apply outcomes. This test replays a pseudo-random command
//! sequence — mutations (including duplicates, retracts of absent facts,
//! interleaved insert/retract), program loads, deletes, and no-ops —
//! against two `ProgramStore`s built from the same configuration, and
//! asserts outcome equality per command and canonical-state equality at
//! checkpoints and at the end.
//!
//! The RNG is a plain LCG (deterministic across platforms); the programs
//! exercise recursion (`path`), negation (`not_known`), and mixed value
//! types (numbers, strings, floats, names).

use std::collections::BTreeMap;

use mangle_common::Value;
use mangle_proto::{
    Command, DeleteProgramCommand, LoadProgramCommand, MutateCommand, MutationOp, MutationOpKind,
};
use mangle_server::store::{ApplyOutcome, CanonicalProgramState, ProgramStore};

/// Small deterministic LCG (Numerical Recipes constants).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 16
    }

    /// Uniform integer in `0..n`.
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const GRAPH_SOURCE_A: &str = r#"
    path(X, Y) :- edge(X, Y).
    path(X, Z) :- edge(X, Y), path(Y, Z).
"#;

const GRAPH_SOURCE_B: &str = r#"
    path(X, Y) :- edge(X, Y).
    path(X, Z) :- edge(X, Y), path(Y, Z).
    hop2(X, Z) :- edge(X, Y), edge(Y, Z).
"#;

const SOCIAL_SOURCE: &str = r#"
    not_known(X, Y) :- person(X), person(Y), !knows(X, Y).
    knows_either(X, Y) :- knows(X, Y).
    knows_either(X, Y) :- knows(Y, X).
"#;

fn build_store() -> ProgramStore {
    let mut store = ProgramStore::new();
    store
        .apply(&Command::LoadProgram(LoadProgramCommand {
            name: "graph".into(),
            source: GRAPH_SOURCE_A.into(),
        }))
        .unwrap();
    store
        .apply(&Command::LoadProgram(LoadProgramCommand {
            name: "social".into(),
            source: SOCIAL_SOURCE.into(),
        }))
        .unwrap();
    store
}

fn random_value(rng: &mut Lcg) -> Value {
    match rng.below(4) {
        0 => Value::Number(rng.below(8) as i64),
        1 => Value::String(format!("s{}", rng.below(5))),
        2 => Value::Float(f64::from(rng.below(4) as u32) + 0.5),
        _ => Value::Name(format!("/n{}", rng.below(3))),
    }
}

fn random_tuple(rng: &mut Lcg, arity: usize) -> Vec<Value> {
    (0..arity).map(|_| random_value(rng)).collect()
}

/// Generates the next command. `social_loaded` tracks the (deterministic)
/// generator state so delete/reload pairs stay valid.
fn random_command(rng: &mut Lcg, social_loaded: &mut bool) -> Command {
    match rng.below(20) {
        // 12/20: mutate graph.edge (arity 2, numbers only for real collisions)
        0..=11 => {
            let n_ops = 1 + rng.below(4) as usize;
            let ops = (0..n_ops)
                .map(|_| {
                    let kind = if rng.below(3) == 0 {
                        MutationOpKind::Retract
                    } else {
                        MutationOpKind::Insert
                    };
                    MutationOp {
                        kind,
                        relation: "edge".into(),
                        tuple: vec![
                            Value::Number(rng.below(6) as i64),
                            Value::Number(rng.below(6) as i64),
                        ],
                    }
                })
                .collect();
            Command::Mutate(MutateCommand {
                program: "graph".into(),
                ops,
            })
        }
        // 4/20: mutate social (mixed types, mixed relations, mixed ops)
        12..=15 => {
            let n_ops = 1 + rng.below(3) as usize;
            let ops = (0..n_ops)
                .map(|_| {
                    let relation = if rng.below(2) == 0 { "knows" } else { "person" };
                    let arity = if relation == "knows" { 2 } else { 1 };
                    MutationOp {
                        kind: if rng.below(2) == 0 {
                            MutationOpKind::Insert
                        } else {
                            MutationOpKind::Retract
                        },
                        relation: relation.into(),
                        tuple: random_tuple(rng, arity),
                    }
                })
                .collect();
            Command::Mutate(MutateCommand {
                program: "social".into(),
                ops,
            })
        }
        // 2/20: re-load graph with either source variant
        16..=17 => Command::LoadProgram(LoadProgramCommand {
            name: "graph".into(),
            source: if rng.below(2) == 0 {
                GRAPH_SOURCE_A.into()
            } else {
                GRAPH_SOURCE_B.into()
            },
        }),
        // 1/20: delete + reload social (a paired two-command cycle is
        // issued by the caller via the social_loaded flag)
        18 => {
            if *social_loaded {
                *social_loaded = false;
                Command::DeleteProgram(DeleteProgramCommand {
                    name: "social".into(),
                })
            } else {
                *social_loaded = true;
                Command::LoadProgram(LoadProgramCommand {
                    name: "social".into(),
                    source: SOCIAL_SOURCE.into(),
                })
            }
        }
        // 1/20: no-op
        _ => Command::Noop,
    }
}

/// Apply one command to both stores and assert identical results
/// (outcome or error message).
fn apply_both(a: &mut ProgramStore, b: &mut ProgramStore, cmd: &Command, step: usize) {
    let ra = a.apply(cmd).map_err(|e| e.to_string());
    let rb = b.apply(cmd).map_err(|e| e.to_string());
    assert_eq!(
        ra, rb,
        "divergent apply result at step {step} for command {cmd:?}"
    );
}

fn assert_states_equal(
    a: &ProgramStore,
    b: &ProgramStore,
    step: usize,
) -> BTreeMap<String, CanonicalProgramState> {
    let (sa, sb) = (a.canonical_state(), b.canonical_state());
    assert_eq!(sa, sb, "divergent canonical state at step {step}");
    sa
}

#[test]
fn replay_equivalence_two_stores() {
    let mut a = build_store();
    let mut b = build_store();

    // Sanity: identical construction.
    assert_states_equal(&a, &b, 0);

    let mut rng = Lcg(0x5EED_CAFE_F00D_D00D);
    let mut social_loaded = true;
    const STEPS: usize = 400;

    for step in 1..=STEPS {
        let cmd = random_command(&mut rng, &mut social_loaded);
        apply_both(&mut a, &mut b, &cmd, step);

        // Periodic state comparison (every 50 steps).
        if step % 50 == 0 {
            assert_states_equal(&a, &b, step);
        }
    }

    // Close the delete/reload cycle if the sequence ended mid-cycle.
    if !social_loaded {
        let cmd = Command::LoadProgram(LoadProgramCommand {
            name: "social".into(),
            source: SOCIAL_SOURCE.into(),
        });
        apply_both(&mut a, &mut b, &cmd, STEPS + 1);
    }

    // Final: identical full state (registry, all relations, ledgers).
    let state = assert_states_equal(&a, &b, STEPS);

    // The exercise must have been meaningful: mutations happened, both
    // programs are present, and derived relations were recomputed.
    assert!(state.contains_key("graph") && state.contains_key("social"));
    let graph = &state["graph"];
    assert!(
        !graph.relations.get("edge").is_none_or(|e| e.is_empty()),
        "expected some edge facts after the replay"
    );
    assert!(
        graph.relations.contains_key("path"),
        "expected derived path relation"
    );
    let social = &state["social"];
    assert!(social.relations.contains_key("knows_either"));
}

#[test]
fn replay_equivalence_second_seed() {
    // A second, shorter run with a different seed: cross-checks that
    // equality isn't an artifact of one particular sequence.
    let mut a = build_store();
    let mut b = build_store();

    let mut rng = Lcg(0xABCD_1234_5678_EF90);
    let mut social_loaded = true;
    for step in 1..=200 {
        let cmd = random_command(&mut rng, &mut social_loaded);
        apply_both(&mut a, &mut b, &cmd, step);
    }
    assert_states_equal(&a, &b, 200);
}
