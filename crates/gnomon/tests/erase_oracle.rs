//! Seam 1, the oracle test for Erasure (ADR-0003, ADR-0007, ADR-0020). An
//! independent oracle picks each property's winner straight from the
//! Contributions that remain: under ranked merge the highest Authority, then
//! the latest Reference time, then the latest Recorded time, then the later
//! write, and without it the same order with no Authority.
//!
//! Each case writes random sequences from a few Episodes, some batched in one
//! transaction, to three Nodes in two Scopes, joined by three Edges, two of
//! which cross from one Scope to the other. It then erases every Episode in
//! turn, with up to three Erasures of a Node, an Edge or a Scope among them. A write remains while its
//! Episode, its Node and their Scopes do, and an Edge remains while it, the
//! Episode that wrote it, both its endpoints and their Scopes do. After each
//! Erasure the oracle is compared with As-of reads of every Node at every
//! known-at instant the sequence produced and at points across the
//! Valid-time axis, and with which Edges exist. The no-residue check of
//! `support/erasure.rs` then reads every table past RLS: no row names an
//! Episode, Node, Edge or Version the Erasure removed, and the token and name
//! rows of each Version it recomputed are those its text as it stands gives.
//! The BM25 counts of every Scope must be those its token rows give.
//! The expected number of disagreements is zero, and proptest shrinks a
//! failing sequence to a short one.

mod support;

use gnomon::{
    AsOf, Bound, EdgeId, EpisodeId, KindOf, MergeRule, NewEdge, NewEpisode, NewKind, NodeId, Patch,
    ROOT_EDGE_KIND, ROOT_NODE_KIND, Scope, Store, TenantId, ValidTime,
};
use jiff::{SignedDuration, Timestamp};
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};
use serde_json::{Map, Value, json};
use support::TestDb;
use support::erasure::{Residue, assert_no_residue, assert_search_counts_match_the_token_rows};
use tokio_postgres::types::Type;
use uuid::Uuid;

const SCOPES: [&str; 2] = ["support", "billing"];
const RANKED_KIND: &str = "ranked_person";

/// The Scope of each Node, by index.
const NODE_SCOPES: [usize; 3] = [0, 0, 1];

/// An Edge of the case: its Scope, its source Node and its target Node, each
/// by index.
#[derive(Debug, Clone, Copy)]
struct EdgeAt {
    scope: usize,
    source: usize,
    target: usize,
}

/// The first Edge crosses from support to billing's Node, the second from
/// billing to support's, and the third stays in support.
const EDGES: [EdgeAt; 3] = [
    EdgeAt {
        scope: 0,
        source: 0,
        target: 2,
    },
    EdgeAt {
        scope: 1,
        source: 2,
        target: 1,
    },
    EdgeAt {
        scope: 0,
        source: 0,
        target: 1,
    },
];

/// Day `d` of the Valid-time axis, which may be fractional.
fn at(d: f64) -> Timestamp {
    "2024-01-01T00:00:00Z".parse::<Timestamp>().unwrap()
        + SignedDuration::from_mins((d * 1440.0) as i64)
}

/// What the oracle needs of one Episode: its Scope, its Reference time in
/// whole days and its Authority.
#[derive(Debug, Clone, Copy)]
struct EpisodeTraits {
    scope: usize,
    reference_day: u32,
    authority: i32,
}

fn traits(scope: usize, reference_day: u32, authority: i32) -> EpisodeTraits {
    EpisodeTraits {
        scope,
        reference_day,
        authority,
    }
}

/// One write: the Node it writes and the Episode it cites, both by index, a
/// Valid time in whole days (`None` for an unknown bound), and a value or a
/// null for each property it states.
#[derive(Debug, Clone)]
struct Write {
    node: usize,
    episode: usize,
    from: Option<u32>,
    to: Option<u32>,
    name: Option<Option<&'static str>>,
    aliases: Option<Option<&'static str>>,
}

impl Write {
    fn values(&self) -> Map<String, Value> {
        let mut m = Map::new();
        if let Some(v) = self.name {
            m.insert("name".into(), v.map_or(Value::Null, |s| json!(s)));
        }
        if let Some(v) = self.aliases {
            m.insert("aliases".into(), v.map_or(Value::Null, |s| json!([s])));
        }
        m
    }

    fn covers(&self, d: f64) -> bool {
        self.from.is_none_or(|f| f64::from(f) <= d) && self.to.is_none_or(|t| d < f64::from(t))
    }
}

/// One Erasure, of the Episode, Node, Edge or Scope of that index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Erasure {
    Episode(usize),
    Node(usize),
    Edge(usize),
    Scope(usize),
}

#[derive(Debug, Clone)]
struct Case {
    ranked: bool,
    /// The first Episode of each Scope writes that Scope's Edges.
    episodes: Vec<EpisodeTraits>,
    /// Transactions of one to three writes each.
    sequence: Vec<Vec<Write>>,
    /// The Erasures, in the order they run: one of every Episode, and up to
    /// three of a Node, an Edge or a Scope.
    erasures: Vec<Erasure>,
}

/// What the Erasures so far have erased.
#[derive(Debug, Default)]
struct Erased {
    episodes: Vec<usize>,
    nodes: Vec<usize>,
    edges: Vec<usize>,
    scopes: Vec<usize>,
}

impl Erased {
    fn is_episode_erased(&self, case: &Case, e: usize) -> bool {
        self.episodes.contains(&e) || self.scopes.contains(&case.episodes[e].scope)
    }

    fn is_node_erased(&self, n: usize) -> bool {
        self.nodes.contains(&n) || self.scopes.contains(&NODE_SCOPES[n])
    }

    /// Whether the Edge is gone: erased, its Scope erased, an endpoint
    /// erased, or the Episode that wrote it erased.
    fn is_edge_erased(&self, case: &Case, k: usize) -> bool {
        let EdgeAt {
            scope,
            source,
            target,
        } = EDGES[k];
        self.edges.contains(&k)
            || self.scopes.contains(&scope)
            || self.is_node_erased(source)
            || self.is_node_erased(target)
            || self.is_episode_erased(case, first_episode(case, scope))
    }
}

/// The first Episode of a Scope, which wrote that Scope's Edges.
fn first_episode(case: &Case, scope: usize) -> usize {
    case.episodes.iter().position(|e| e.scope == scope).unwrap()
}

fn case() -> impl Strategy<Value = Case> {
    (
        any::<bool>(),
        proptest::collection::vec((1..4u32, 0..3i32), 1..3),
        proptest::collection::vec((1..4u32, 0..3i32), 1..3),
    )
        .prop_flat_map(|(ranked, support, billing)| {
            let traits: Vec<EpisodeTraits> = support
                .into_iter()
                .map(|t| (0, t))
                .chain(billing.into_iter().map(|t| (1, t)))
                .map(|(scope, (reference_day, authority))| EpisodeTraits {
                    scope,
                    reference_day,
                    authority,
                })
                .collect();
            let n = traits.len();
            let value = |xs: &'static [&'static str]| {
                prop_oneof![
                    2 => Just(None),
                    1 => Just(Some(None)),
                    4 => proptest::sample::select(xs).prop_map(|s| Some(Some(s))),
                ]
            };
            let in_scope = traits.clone();
            let write = (
                0..NODE_SCOPES.len(),
                0..2usize,
                proptest::option::weighted(0.8, 1..25u32),
                proptest::option::weighted(0.8, 1..8u32),
                value(&["a", "b", "c"]),
                value(&["x", "y"]),
            )
                .prop_filter_map(
                    "a write states something",
                    move |(node, slot, from, len, name, aliases)| {
                        if name.is_none() && aliases.is_none() {
                            return None;
                        }
                        // An Episode of the Node's Scope.
                        let mine: Vec<usize> = (0..in_scope.len())
                            .filter(|&e| in_scope[e].scope == NODE_SCOPES[node])
                            .collect();
                        let e = mine[slot % mine.len()];
                        Some(Write {
                            node,
                            episode: e,
                            from,
                            to: len.map(|l| from.unwrap_or(1) + l),
                            name,
                            aliases,
                        })
                    },
                );
            // Every Episode is erased, in some order, among up to three
            // Erasures of a Node, an Edge or a Scope.
            let graph = prop_oneof![
                (0..NODE_SCOPES.len()).prop_map(Erasure::Node),
                (0..EDGES.len()).prop_map(Erasure::Edge),
                (0..SCOPES.len()).prop_map(Erasure::Scope),
            ];
            let erasures = proptest::collection::vec(graph, 0..4).prop_flat_map(move |graph| {
                Just(
                    (0..n)
                        .map(Erasure::Episode)
                        .chain(graph)
                        .collect::<Vec<_>>(),
                )
                .prop_shuffle()
            });
            (
                Just(ranked),
                Just(traits),
                proptest::collection::vec(proptest::collection::vec(write, 1..4), 1..6),
                erasures,
            )
        })
        .prop_map(|(ranked, episodes, sequence, erasures)| Case {
            ranked,
            episodes,
            sequence,
            erasures,
        })
}

/// The values the oracle expects of Node `node` at valid-at `d` after the
/// first `txs` transactions, from the Contributions that remain.
fn oracle(
    case: &Case,
    node: usize,
    txs: usize,
    erased: &Erased,
    d: f64,
) -> Option<Map<String, Value>> {
    if erased.is_node_erased(node) {
        return None;
    }
    let mut out = Map::new();
    for property in ["name", "aliases"] {
        // (Authority under ranked merge, Reference time, Recorded time as the
        // transaction's index, the write's index within it): the greatest
        // wins.
        let winner = case.sequence[..txs]
            .iter()
            .enumerate()
            .flat_map(|(t, tx)| tx.iter().enumerate().map(move |(i, w)| (t, i, w)))
            .filter(|(_, _, w)| {
                w.node == node && !erased.is_episode_erased(case, w.episode) && w.covers(d)
            })
            .filter_map(|(t, i, w)| {
                let s = case.episodes[w.episode];
                let authority = if case.ranked { s.authority } else { 0 };
                w.values()
                    .get(property)
                    .cloned()
                    .map(|v| ((authority, s.reference_day, t, i), v))
            })
            .max_by_key(|(k, _)| *k);
        if let Some((_, v)) = winner
            && !v.is_null()
        {
            out.insert(property.into(), v);
        }
    }
    (!out.is_empty()).then_some(out)
}

/// A database with the ranked Kind registered, and a Store whose worker may
/// register Kinds and erase.
async fn setup() -> (TestDb, Store, TenantId) {
    let db = TestDb::migrated().await;
    db.grant_kind_registration("gnomon_worker").await;
    db.grant_erasure("gnomon_worker").await;
    let store = Store::connect(&db.login_url(), &support::connect_options())
        .await
        .unwrap();
    let tenant = support::tenant();
    let caller = store.as_worker(tenant, Scope::new(SCOPES[0]), None);
    caller
        .register_kind(&NewKind {
            tenant: Some(tenant),
            name: RANKED_KIND.into(),
            of: KindOf::Node,
            parent: None,
            properties: json!({ "type": "object", "properties": {} }),
            search: vec![],
        })
        .await
        .unwrap();
    caller
        .set_merge_rule(tenant, RANKED_KIND, MergeRule::Ranked)
        .await
        .unwrap();
    (db, store, tenant)
}

#[test]
fn as_of_reads_after_every_erasure_agree_with_an_oracle_over_the_remaining_contributions() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (db, store, tenant) = rt.block_on(setup());
    let mut runner = TestRunner::new(Config {
        cases: 32,
        failure_persistence: None,
        ..Config::default()
    });
    let reads = std::cell::Cell::new(0);
    let counts = std::cell::Cell::new([0usize; 4]);
    let result = runner.run(&case(), |case| {
        let (wrong, n, ran) = rt.block_on(check(&db, &store, tenant, &case));
        reads.set(reads.get() + n);
        let mut c = counts.get();
        for (total, more) in c.iter_mut().zip(ran) {
            *total += more;
        }
        counts.set(c);
        prop_assert!(wrong.is_empty(), "{:#?}", wrong);
        Ok(())
    });
    drop(db);
    result.unwrap();
    let [episodes, nodes, edges, scopes] = counts.get();
    println!(
        "{} Erasures ({episodes} of Episodes, {nodes} of Nodes, {edges} of Edges, {scopes} of \
         Scopes), {} reads after them, 0 disagreements",
        episodes + nodes + edges + scopes,
        reads.get()
    );
}

/// The case the random sequences shrank to with `PROPTEST_RNG_SEED=1`. Node
/// 2's Version of "a" from day 10 on is current when Episode 3 states a null
/// Alias before day 2, and that write leaves it open. Erasing Episode 3 once
/// deleted the Version's token and name rows and indexed it again nowhere, so
/// neither full-text Search nor Resolution found the Node.
#[tokio::test]
async fn erasing_an_episode_keeps_the_search_text_of_a_current_version_it_never_closed() {
    let (db, store, tenant) = setup().await;
    let case = Case {
        ranked: false,
        episodes: vec![
            traits(0, 1, 2),
            traits(0, 1, 2),
            traits(1, 3, 0),
            traits(1, 3, 1),
        ],
        sequence: vec![
            vec![Write {
                node: 2,
                episode: 2,
                from: Some(10),
                to: None,
                name: Some(Some("a")),
                aliases: None,
            }],
            vec![Write {
                node: 2,
                episode: 3,
                from: None,
                to: Some(2),
                name: None,
                aliases: Some(None),
            }],
        ],
        erasures: vec![
            Erasure::Episode(3),
            Erasure::Episode(2),
            Erasure::Episode(1),
            Erasure::Episode(0),
        ],
    };
    let (wrong, _, ran) = check(&db, &store, tenant, &case).await;
    assert!(wrong.is_empty(), "{wrong:#?}");
    assert_eq!(ran, [4, 0, 0, 0]);
}

/// The case of issue #99, which the random sequences shrank to. One of its
/// Erasures adds texts to the net count of the Scope `support` and removes
/// words from it. The commit once proposed that net change as a new row of
/// the Scope's totals, which the row's check refused although the row exists.
#[tokio::test]
async fn an_erasure_that_adds_texts_and_removes_words_commits() {
    let (db, store, tenant) = setup().await;
    let write = |episode, from, to, name, aliases| Write {
        node: 1,
        episode,
        from,
        to,
        name,
        aliases: Some(Some(aliases)),
    };
    let case = Case {
        ranked: false,
        episodes: vec![
            traits(0, 3, 1),
            traits(0, 3, 0),
            traits(1, 3, 0),
            traits(1, 2, 0),
        ],
        sequence: vec![
            vec![write(0, None, None, None, "y")],
            vec![
                write(0, Some(6), Some(7), None, "x"),
                write(1, Some(1), None, Some(Some("a")), "x"),
            ],
        ],
        erasures: vec![
            Erasure::Episode(1),
            Erasure::Episode(0),
            Erasure::Episode(3),
            Erasure::Episode(2),
        ],
    };
    let (wrong, _, ran) = check(&db, &store, tenant, &case).await;
    assert!(wrong.is_empty(), "{wrong:#?}");
    assert_eq!(ran, [4, 0, 0, 0]);
}

/// A user of both Scopes, who writes and reads every Node and Edge.
fn user_of_both_scopes(store: &Store, tenant: TenantId) -> gnomon::Caller<'_> {
    let claims = json!({
        "role": "authenticated",
        "app_metadata": { "tenant_id": tenant.0.to_string(), "scopes": SCOPES }
    });
    match claims {
        Value::Object(m) => store.as_user(m),
        _ => unreachable!(),
    }
}

/// Whether a Node or Edge row exists, read past RLS.
async fn exists(db: &TestDb, table: &str, id: Uuid) -> bool {
    db.admin()
        .await
        .query_typed(
            &format!("select from gnomon.{table} where id = $1"),
            &[(&id, Type::UUID)],
        )
        .await
        .unwrap()
        .len()
        == 1
}

/// The IDs of every Episode, Node, Edge and Version of the Tenant, read past
/// RLS, in that order.
async fn ids(db: &TestDb, tenant: TenantId) -> [Vec<Uuid>; 4] {
    let admin = db.admin().await;
    let mut out: [Vec<Uuid>; 4] = Default::default();
    for (i, table) in ["episode", "node", "edge", "version"].iter().enumerate() {
        out[i] = admin
            .query_typed(
                &format!("select id from gnomon.{table} where tenant_id = $1"),
                &[(&tenant.0, Type::UUID)],
            )
            .await
            .unwrap()
            .iter()
            .map(|r| r.get(0))
            .collect();
    }
    out
}

/// Checks that an Erasure left no residue. What it removed comes from the
/// rows before and after it, because the Erasure of a Node names no Edge of
/// a Scope its caller cannot see. The Versions it recomputed are those that
/// survive of the Nodes and Edges it reports recomputing.
async fn assert_erasure_left_no_residue(
    db: &TestDb,
    tenant: TenantId,
    before: &[Vec<Uuid>; 4],
    recomputed_of: &[Uuid],
) {
    let after = ids(db, tenant).await;
    let gone: Vec<Vec<Uuid>> = before
        .iter()
        .zip(&after)
        .map(|(b, a)| b.iter().filter(|id| !a.contains(id)).copied().collect())
        .collect();
    let admin = db.admin().await;
    let recomputed: Vec<Uuid> = admin
        .query_typed(
            "select id from gnomon.version
             where tenant_id = $1 and versioned_id = any($2) and id = any($3)",
            &[
                (&tenant.0, Type::UUID),
                (&recomputed_of, Type::UUID_ARRAY),
                (&before[3], Type::UUID_ARRAY),
            ],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_no_residue(
        &admin,
        &Residue {
            episodes: &gone[0],
            nodes: &gone[1],
            edges: &gone[2],
            versions: &gone[3],
            recomputed: &recomputed,
            markers: &[],
        },
    )
    .await;
}

/// Writes the case, runs its Erasures in turn and compares the reads after
/// each with the oracle. Returns the disagreements, the number of reads made
/// and the number of Erasures of Episodes, Nodes, Edges and Scopes that ran.
async fn check(
    db: &TestDb,
    store: &Store,
    tenant: TenantId,
    case: &Case,
) -> (Vec<String>, usize, [usize; 4]) {
    let writer = user_of_both_scopes(store, tenant);
    let mut episodes: Vec<EpisodeId> = Vec::new();
    for (i, s) in case.episodes.iter().enumerate() {
        episodes.push(
            writer
                .record_episode(&NewEpisode {
                    tenant: Some(tenant),
                    scope: Scope::new(SCOPES[s.scope]),
                    content: format!("Episode {i}: {s:?}"),
                    reference_time: at(f64::from(s.reference_day)),
                    authority: Some(s.authority),
                })
                .await
                .unwrap(),
        );
    }
    let kind = if case.ranked {
        RANKED_KIND
    } else {
        ROOT_NODE_KIND
    };
    // The Nodes start bare, so every value they hold is one a write states.
    let mut client = db.login().await;
    let tx = client.transaction().await.unwrap();
    tx.batch_execute("set local role authenticated")
        .await
        .unwrap();
    tx.query_typed(
        "select set_config('request.jwt.claims', $1, true)",
        &[(
            &json!({
                "role": "authenticated",
                "app_metadata": { "tenant_id": tenant.0.to_string(), "scopes": SCOPES }
            })
            .to_string(),
            Type::TEXT,
        )],
    )
    .await
    .unwrap();
    let mut nodes = Vec::new();
    for scope in NODE_SCOPES {
        let id: Uuid = tx
            .query_typed(
                "select gnomon.create_node($1, $2, $3)",
                &[
                    (&tenant.0, Type::UUID),
                    (&SCOPES[scope], Type::TEXT),
                    (&kind, Type::TEXT),
                ],
            )
            .await
            .unwrap()[0]
            .get(0);
        nodes.push(NodeId(id));
    }
    tx.commit().await.unwrap();
    let mut edges: Vec<EdgeId> = Vec::new();
    for EdgeAt {
        scope,
        source,
        target,
    } in EDGES
    {
        edges.push(
            writer
                .create_edge(
                    &NewEdge {
                        tenant: Some(tenant),
                        scope: Scope::new(SCOPES[scope]),
                        kind: ROOT_EDGE_KIND.into(),
                        source: nodes[source],
                        target: nodes[target],
                    },
                    &Patch::new(
                        episodes[first_episode(case, scope)],
                        ValidTime::UNKNOWN,
                        Map::new(),
                    ),
                )
                .await
                .unwrap()
                .0,
        );
    }

    let mut instants = Vec::new();
    for tx in &case.sequence {
        let batch: Vec<_> = tx
            .iter()
            .map(|w| {
                let valid = ValidTime {
                    from: w
                        .from
                        .map_or(Bound::Unknown, |d| Bound::At(at(f64::from(d)))),
                    to: w.to.map_or(Bound::Unknown, |d| Bound::At(at(f64::from(d)))),
                };
                (
                    nodes[w.node],
                    Patch::new(episodes[w.episode], valid, w.values()),
                )
            })
            .collect();
        let outcomes = writer.write_nodes(tenant, &batch).await.unwrap();
        instants.push(outcomes[0].recorded_at);
    }

    let mut wrong = Vec::new();
    let mut reads = 0;
    let mut ran = [0usize; 4];
    let mut erased = Erased::default();
    for &erasure in &case.erasures {
        // An Erasure of something already gone is skipped, as a caller that
        // found it gone would skip it.
        let before = ids(db, tenant).await;
        let mut recomputed_of = Vec::new();
        match erasure {
            Erasure::Episode(e) => {
                if erased.is_episode_erased(case, e) {
                    continue;
                }
                let scope = Scope::new(SCOPES[case.episodes[e].scope]);
                let done = store
                    .as_worker(tenant, scope, None)
                    .erase_episode(tenant, episodes[e])
                    .await
                    .unwrap();
                recomputed_of.extend(done.recomputed_nodes.iter().map(|n| n.0));
                recomputed_of.extend(done.recomputed_edges.iter().map(|k| k.0));
                erased.episodes.push(e);
                ran[0] += 1;
            }
            Erasure::Node(n) => {
                if !exists(db, "node", nodes[n].0).await {
                    continue;
                }
                store
                    .as_worker(tenant, Scope::new(SCOPES[NODE_SCOPES[n]]), None)
                    .erase_node(tenant, nodes[n])
                    .await
                    .unwrap();
                erased.nodes.push(n);
                ran[1] += 1;
            }
            Erasure::Edge(k) => {
                if !exists(db, "edge", edges[k].0).await {
                    continue;
                }
                store
                    .as_worker(tenant, Scope::new(SCOPES[EDGES[k].scope]), None)
                    .erase_edge(tenant, edges[k])
                    .await
                    .unwrap();
                erased.edges.push(k);
                ran[2] += 1;
            }
            Erasure::Scope(s) => {
                if erased.scopes.contains(&s) {
                    continue;
                }
                let scope = Scope::new(SCOPES[s]);
                store
                    .as_worker(tenant, scope.clone(), None)
                    .erase_scope(tenant, &scope)
                    .await
                    .unwrap();
                erased.scopes.push(s);
                ran[3] += 1;
            }
        }
        assert_erasure_left_no_residue(db, tenant, &before, &recomputed_of).await;
        assert_search_counts_match_the_token_rows(&db.admin().await).await;
        for (k, edge) in edges.iter().enumerate() {
            let expected = !erased.is_edge_erased(case, k);
            if exists(db, "edge", edge.0).await != expected {
                wrong.push(format!("{erased:?}: Edge {k} exists is not {expected}"));
            }
        }
        // Every known-at instant, and the current belief.
        let known: Vec<(usize, AsOf)> = instants
            .iter()
            .enumerate()
            .map(|(t, i)| (t + 1, AsOf::at(at(0.0), *i)))
            .chain([(case.sequence.len(), AsOf::current(at(0.0)))])
            .collect();
        for (txs, as_of) in known {
            let mut previous: Vec<Option<gnomon::NodeVersion>> = vec![None; nodes.len()];
            for day in 0..=32 {
                let d = f64::from(day);
                let as_of = AsOf {
                    valid_at: at(d),
                    ..as_of
                };
                let read = writer.read_nodes(tenant, &nodes, as_of).await.unwrap();
                reads += 1;
                for (n, node) in nodes.iter().enumerate() {
                    let version = read.iter().find(|v| v.node == *node).cloned();
                    let got = version.as_ref().map(|v| v.values.clone());
                    let expected = oracle(case, n, txs, &erased, d);
                    if got != expected {
                        wrong.push(format!(
                            "{erased:?}, Node {n}, after transaction {txs}, at day {d}: \
                             read {got:?}, oracle {expected:?}"
                        ));
                    }
                    if let (Some(p), Some(v)) = (&previous[n], &version)
                        && p.values == v.values
                        && p.id != v.id
                    {
                        wrong.push(format!(
                            "{erased:?}, Node {n}, after transaction {txs}: Versions {} \
                                 and {} meet at day {d} with equal values",
                            p.id, v.id
                        ));
                    }
                    previous[n] = version;
                }
            }
        }
    }
    (wrong, reads, ran)
}
