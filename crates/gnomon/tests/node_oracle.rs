//! Seam 1, the oracle test for Node writes (ADR-0020). An independent oracle
//! picks each property's winner straight from the Contributions: the latest
//! Reference time, then the latest Recorded time, then the later write. It
//! is compared with As-of reads at every known-at instant the sequence
//! produced and at points across the Valid-time axis, after random
//! sequences of writes, some batched in one transaction. The expected
//! number of disagreements is zero, and proptest shrinks a failing sequence
//! to a short one.

mod support;

use chrono::{DateTime, Duration, TimeZone, Utc};
use gnomon::{
    AsOf, Bound, EpisodeId, NewEpisode, NewNode, NodeId, Patch, Scope, Store, TenantId, ValidTime,
    ROOT_NODE_KIND,
};
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};
use serde_json::{json, Map, Value};
use support::TestDb;

const SCOPE: &str = "support";

/// Day `d` of the Valid-time axis, which may be fractional.
fn at(d: f64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap() + Duration::minutes((d * 1440.0) as i64)
}

/// One write: an Episode's Reference time, a Valid time in whole days
/// (`None` for an unknown bound), and a value or a null for each property it
/// states.
#[derive(Debug, Clone)]
struct Write {
    reference_day: u32,
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

fn write() -> impl Strategy<Value = Write> {
    let value = |xs: &'static [&'static str]| {
        prop_oneof![
            2 => Just(None),
            1 => Just(Some(None)),
            4 => proptest::sample::select(xs).prop_map(|s| Some(Some(s))),
        ]
    };
    (
        1..4u32,
        proptest::option::weighted(0.8, 1..25u32),
        proptest::option::weighted(0.8, 1..8u32),
        value(&["a", "b", "c"]),
        value(&["x", "y"]),
    )
        .prop_filter_map(
            "a write states something",
            |(r, from, len, name, aliases)| {
                if name.is_none() && aliases.is_none() {
                    return None;
                }
                let to = len.map(|l| from.unwrap_or(1) + l);
                Some(Write {
                    reference_day: r,
                    from,
                    to,
                    name,
                    aliases,
                })
            },
        )
}

/// Transactions of one to three writes each.
fn sequence() -> impl Strategy<Value = Vec<Vec<Write>>> {
    proptest::collection::vec(proptest::collection::vec(write(), 1..4), 1..7)
}

/// The values the oracle expects at valid-at `d` after the first `txs`
/// transactions, from the Contributions alone.
fn oracle(sequence: &[Vec<Write>], txs: usize, d: f64) -> Option<Map<String, Value>> {
    let mut out = Map::new();
    for property in ["name", "aliases"] {
        // (Reference time, Recorded time as the transaction's index, the
        // write's index within it): the greatest wins.
        let winner = sequence[..txs]
            .iter()
            .enumerate()
            .flat_map(|(t, tx)| tx.iter().enumerate().map(move |(i, w)| (t, i, w)))
            .filter(|(_, _, w)| w.covers(d))
            .filter_map(|(t, i, w)| {
                w.values()
                    .get(property)
                    .cloned()
                    .map(|v| ((w.reference_day, t, i), v))
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
fn as_of_reads_agree_with_an_oracle_over_the_contributions() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (db, store, tenant) = rt.block_on(async {
        let db = TestDb::migrated().await;
        let store = Store::connect_no_tls(&db.login_url()).await.unwrap();
        (db, store, support::tenant())
    });
    let mut runner = TestRunner::new(Config {
        cases: 32,
        failure_persistence: None,
        ..Config::default()
    });
    let reads = std::cell::Cell::new(0);
    let result = runner.run(&sequence(), |sequence| {
        let disagreements = rt.block_on(check(&store, tenant, &sequence));
        reads.set(reads.get() + disagreements.1);
        prop_assert!(disagreements.0.is_empty(), "{:#?}", disagreements.0);
        Ok(())
    });
    drop(db);
    result.unwrap();
    println!("{} As-of reads, 0 disagreements", reads.get());
}

/// Writes the sequence to a new Node and compares the reads with the oracle.
/// Returns the disagreements and the number of reads made.
async fn check(store: &Store, tenant: TenantId, sequence: &[Vec<Write>]) -> (Vec<String>, usize) {
    let caller = store.as_worker(tenant, Scope::new(SCOPE));
    let mut node: Option<NodeId> = None;
    let mut known_at = Vec::new();
    for tx in sequence {
        let mut patches = Vec::new();
        for w in tx {
            let ep: EpisodeId = caller
                .record_episode(&NewEpisode {
                    tenant: Some(tenant),
                    scope: Scope::new(SCOPE),
                    content: format!("{w:?}"),
                    reference_time: at(f64::from(w.reference_day)),
                    authority: None,
                })
                .await
                .unwrap();
            let valid = ValidTime {
                from: w
                    .from
                    .map_or(Bound::Unknown, |d| Bound::At(at(f64::from(d)))),
                to: w.to.map_or(Bound::Unknown, |d| Bound::At(at(f64::from(d)))),
            };
            patches.push(Patch::new(ep, valid, w.values()));
        }
        let outcomes = match node {
            None => {
                let new = NewNode {
                    tenant: Some(tenant),
                    scope: Scope::new(SCOPE),
                    kind: ROOT_NODE_KIND.into(),
                };
                let (id, first) = caller.create_node(&new, &patches[0]).await.unwrap();
                node = Some(id);
                let rest: Vec<_> = patches[1..].iter().map(|p| (id, p.clone())).collect();
                let mut all = vec![first];
                if !rest.is_empty() {
                    // The rest of the first transaction runs as a second
                    // one, so that the Node exists to batch against.
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
    for (tx, times) in sequence.iter().zip(&known_at) {
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
            let expected = oracle(&store_txs, k + 1, d);
            if read != expected {
                wrong.push(format!(
                    "after transaction {k}, at day {d}: read {read:?}, oracle {expected:?}"
                ));
            }
            // Bounds fall on whole days and the points on half days, so two
            // neighbouring points with equal values lie in one Version
            // unless equal neighbours failed to merge.
            if let (Some(p), Some(v)) = (&previous, &version) {
                if p.values == v.values && p.id != v.id {
                    wrong.push(format!(
                        "after transaction {k}, Versions {} and {} meet at day {d} with equal values",
                        p.id, v.id
                    ));
                }
            }
            previous = version;
        }
    }
    (wrong, reads)
}
