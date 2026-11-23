//! Seam 1, the oracle test for Erasure (ADR-0003, ADR-0007, ADR-0020). An
//! independent oracle picks each property's winner straight from the
//! Contributions that remain: under ranked merge the highest Authority, then
//! the latest Reference time, then the latest Recorded time, then the later
//! write, and without it the same order with no Authority. After random
//! sequences of writes from a few Episodes, some batched in one transaction,
//! every Episode is erased in turn, and after each Erasure the oracle is
//! compared with As-of reads at every known-at instant the sequence produced
//! and at points across the Valid-time axis. The expected number of
//! disagreements is zero, and proptest shrinks a failing sequence to a short
//! one.

mod support;

use chrono::{DateTime, Duration, TimeZone, Utc};
use gnomon::{
    AsOf, Bound, EpisodeId, KindOf, MergeRule, NewEpisode, NewKind, NewNode, NodeId, Patch, Scope,
    Store, TenantId, ValidTime, ROOT_NODE_KIND,
};
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};
use serde_json::{json, Map, Value};
use support::TestDb;

const SCOPE: &str = "support";
const RANKED_KIND: &str = "ranked_thing";

/// Day `d` of the Valid-time axis, which may be fractional.
fn at(d: f64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap() + Duration::minutes((d * 1440.0) as i64)
}

/// One Episode: its Reference time in whole days and its Authority.
#[derive(Debug, Clone, Copy)]
struct Source {
    reference_day: u32,
    authority: i32,
}

/// One write: the Episode it cites, a Valid time in whole days (`None` for an
/// unknown bound), and a value or a null for each property it states.
#[derive(Debug, Clone)]
struct Write {
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

#[derive(Debug, Clone)]
struct Case {
    ranked: bool,
    sources: Vec<Source>,
    /// Transactions of one to three writes each.
    sequence: Vec<Vec<Write>>,
    /// The order the Episodes are erased in.
    erasures: Vec<usize>,
}

fn case() -> impl Strategy<Value = Case> {
    (
        any::<bool>(),
        proptest::collection::vec((1..4u32, 0..3i32), 2..5),
    )
        .prop_flat_map(|(ranked, sources)| {
            let n = sources.len();
            let value = |xs: &'static [&'static str]| {
                prop_oneof![
                    2 => Just(None),
                    1 => Just(Some(None)),
                    4 => proptest::sample::select(xs).prop_map(|s| Some(Some(s))),
                ]
            };
            let write = (
                0..n,
                proptest::option::weighted(0.8, 1..25u32),
                proptest::option::weighted(0.8, 1..8u32),
                value(&["a", "b", "c"]),
                value(&["x", "y"]),
            )
                .prop_filter_map(
                    "a write states something",
                    |(e, from, len, name, aliases)| {
                        if name.is_none() && aliases.is_none() {
                            return None;
                        }
                        Some(Write {
                            episode: e,
                            from,
                            to: len.map(|l| from.unwrap_or(1) + l),
                            name,
                            aliases,
                        })
                    },
                );
            (
                Just(ranked),
                Just(
                    sources
                        .into_iter()
                        .map(|(reference_day, authority)| Source {
                            reference_day,
                            authority,
                        })
                        .collect::<Vec<_>>(),
                ),
                proptest::collection::vec(proptest::collection::vec(write, 1..4), 1..6),
                Just((0..n).collect::<Vec<_>>()).prop_shuffle(),
            )
        })
        .prop_map(|(ranked, sources, sequence, erasures)| Case {
            ranked,
            sources,
            sequence,
            erasures,
        })
}

/// The values the oracle expects at valid-at `d` after the first `txs`
/// transactions, from the Contributions of the Episodes not yet erased.
fn oracle(
    case: &Case,
    sequence: &[Vec<Write>],
    txs: usize,
    erased: &[usize],
    d: f64,
) -> Option<Map<String, Value>> {
    let mut out = Map::new();
    for property in ["name", "aliases"] {
        // (Authority under ranked merge, Reference time, Recorded time as the
        // transaction's index, the write's index within it): the greatest
        // wins.
        let winner = sequence[..txs]
            .iter()
            .enumerate()
            .flat_map(|(t, tx)| tx.iter().enumerate().map(move |(i, w)| (t, i, w)))
            .filter(|(_, _, w)| !erased.contains(&w.episode) && w.covers(d))
            .filter_map(|(t, i, w)| {
                let s = case.sources[w.episode];
                let authority = if case.ranked { s.authority } else { 0 };
                w.values()
                    .get(property)
                    .cloned()
                    .map(|v| ((authority, s.reference_day, t, i), v))
            })
            .max_by_key(|(k, _)| *k);
        if let Some((_, v)) = winner {
            if !v.is_null() {
                out.insert(property.into(), v);
            }
        }
    }
    (!out.is_empty()).then_some(out)
}

#[test]
fn as_of_reads_after_every_erasure_agree_with_an_oracle_over_the_remaining_contributions() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (db, store, tenant) = rt.block_on(async {
        let db = TestDb::migrated().await;
        db.grant_kind_registration("gnomon_worker").await;
        db.grant_erasure("gnomon_worker").await;
        let store = Store::connect(&db.login_url(), &support::connect_options())
            .await
            .unwrap();
        let tenant = support::tenant();
        let caller = store.as_worker(tenant, Scope::new(SCOPE));
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
    });
    let mut runner = TestRunner::new(Config {
        cases: 32,
        failure_persistence: None,
        ..Config::default()
    });
    let reads = std::cell::Cell::new(0);
    let erasures = std::cell::Cell::new(0);
    let result = runner.run(&case(), |case| {
        let (wrong, n, e) = rt.block_on(check(&store, tenant, &case));
        reads.set(reads.get() + n);
        erasures.set(erasures.get() + e);
        prop_assert!(wrong.is_empty(), "{:#?}", wrong);
        Ok(())
    });
    drop(db);
    result.unwrap();
    println!(
        "{} Erasures, {} As-of reads after them, 0 disagreements",
        erasures.get(),
        reads.get()
    );
}

/// Writes the case to a new Node, erases its Episodes in turn and compares
/// the reads after each Erasure with the oracle. Returns the disagreements,
/// the number of reads made and the number of Erasures.
async fn check(store: &Store, tenant: TenantId, case: &Case) -> (Vec<String>, usize, usize) {
    let caller = store.as_worker(tenant, Scope::new(SCOPE));
    let mut episodes: Vec<EpisodeId> = Vec::new();
    for (i, s) in case.sources.iter().enumerate() {
        episodes.push(
            caller
                .record_episode(&NewEpisode {
                    tenant: Some(tenant),
                    scope: Scope::new(SCOPE),
                    content: format!("source {i}: {s:?}"),
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
    let mut node: Option<NodeId> = None;
    let mut known_at = Vec::new();
    for tx in &case.sequence {
        let patches: Vec<Patch> = tx
            .iter()
            .map(|w| {
                let valid = ValidTime {
                    from: w
                        .from
                        .map_or(Bound::Unknown, |d| Bound::At(at(f64::from(d)))),
                    to: w.to.map_or(Bound::Unknown, |d| Bound::At(at(f64::from(d)))),
                };
                Patch::new(episodes[w.episode], valid, w.values())
            })
            .collect();
        let outcomes = match node {
            None => {
                let new = NewNode {
                    tenant: Some(tenant),
                    scope: Scope::new(SCOPE),
                    kind: kind.into(),
                };
                let (id, first) = caller.create_node(&new, &patches[0]).await.unwrap();
                node = Some(id);
                let rest: Vec<_> = patches[1..].iter().map(|p| (id, p.clone())).collect();
                let mut all = vec![first];
                if !rest.is_empty() {
                    all.extend(caller.write_nodes(tenant, &rest).await.unwrap());
                }
                all
            }
            Some(id) => {
                let batch: Vec<_> = patches.into_iter().map(|p| (id, p)).collect();
                caller.write_nodes(tenant, &batch).await.unwrap()
            }
        };
        known_at.push(outcomes.iter().map(|o| o.recorded_at).collect::<Vec<_>>());
    }
    let node = node.unwrap();

    // The first transaction may have run as two, so flatten the known-at
    // instants into transactions as the Store saw them.
    let mut store_txs: Vec<Vec<Write>> = Vec::new();
    let mut instants = Vec::new();
    for (tx, times) in case.sequence.iter().zip(&known_at) {
        let mut start = 0;
        for i in 1..=tx.len() {
            if i == tx.len() || times[i] != times[start] {
                store_txs.push(tx[start..i].to_vec());
                instants.push(times[start]);
                start = i;
            }
        }
    }

    let mut wrong = Vec::new();
    let mut reads = 0;
    let mut erased = Vec::new();
    for &e in &case.erasures {
        caller.erase_episode(tenant, episodes[e]).await.unwrap();
        erased.push(e);
        for (k, known) in instants.iter().enumerate() {
            let mut previous: Option<gnomon::NodeVersion> = None;
            for half_days in 0..=64 {
                let d = f64::from(half_days) / 2.0;
                let version = caller
                    .read_node(tenant, node, AsOf::at(at(d), *known))
                    .await
                    .unwrap();
                reads += 1;
                let read = version.as_ref().map(|v| v.values.clone());
                let expected = oracle(case, &store_txs, k + 1, &erased, d);
                if read != expected {
                    wrong.push(format!(
                        "after erasing {erased:?}, after transaction {k}, at day {d}: \
                         read {read:?}, oracle {expected:?}"
                    ));
                }
                if let (Some(p), Some(v)) = (&previous, &version) {
                    if p.values == v.values && p.id != v.id {
                        wrong.push(format!(
                            "after erasing {erased:?}, after transaction {k}, Versions {} and {} \
                             meet at day {d} with equal values",
                            p.id, v.id
                        ));
                    }
                }
                previous = version;
            }
        }
        // The current belief is the last transaction's.
        for half_days in 0..=64 {
            let d = f64::from(half_days) / 2.0;
            let read = caller
                .read_node(tenant, node, AsOf::current(at(d)))
                .await
                .unwrap()
                .map(|v| v.values);
            reads += 1;
            let expected = oracle(case, &store_txs, store_txs.len(), &erased, d);
            if read != expected {
                wrong.push(format!(
                    "after erasing {erased:?}, now, at day {d}: read {read:?}, oracle {expected:?}"
                ));
            }
        }
    }
    (wrong, reads, case.erasures.len())
}
