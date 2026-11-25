//! The performance suite (ticket #21), on demand and apart from the
//! correctness suite. It asserts the Search budgets of ADR-0010 and the write
//! cost prototype 4 measured (`origin/prototype/recompute-cost`,
//! FINDINGS.md), on the data shape of prototype 5
//! (`origin/prototype/exact-search`). ADR-0020 gives it no `criterion`: it
//! asserts budgets and does not compare runs.
//!
//! Cargo builds it only with `--features performance`, so a plain
//! `cargo test` or `cargo nextest run` never runs it. See `docs/testing.md`
//! for the command, `scripts/perf.sh` for the runs on both images and in a
//! container capped at 4 GiB.
//!
//! Each case writes its environment, the machine load around it and its
//! numbers to `target/tmp/performance/` and prints a summary. A run on a busy
//! machine says so. The cases take a lock, so they never overlap even when
//! the test runner runs tests in parallel.

mod support;

use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::{DateTime, TimeZone, Utc};
use gnomon::{
    AsOf, NewEpisode, NewNode, Patch, Scope, SearchRequest, Store, TenantId, ValidTime,
    ROOT_NODE_KIND,
};
use serde_json::{json, Map, Value};
use support::perf::{self, Dataset, Record, Stats};
use support::vectors::Rng;
use support::TestDb;

/// The budgets of ADR-0010: the whole fused Search at p95.
const SEARCH_BUDGET_50K: f64 = 100.0;
const SEARCH_BUDGET_200K: f64 = 250.0;
/// The most a bounded patch write costs at p50 in prototype 4: about 1 ms on a
/// Node with 10 Contributions, 3 ms with 100, and 10 to 11 ms with 500.
const PATCH_P50_CEILING_MS: f64 = 11.0;

/// Prototype 4's bounded-patch p50 (ms) on a Node of this many Contributions,
/// recorded next to the measured number.
fn prototype_4_p50(contributions: usize) -> f64 {
    match contributions {
        10 => 1.3,
        100 => 3.4,
        _ => 10.4,
    }
}

/// Searches discarded before the timed ones, and timed.
const SEARCH_WARM_UP: usize = 10;
const SEARCH_SAMPLES: usize = 100;

static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn one_at_a_time() -> std::sync::MutexGuard<'static, ()> {
    ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner())
}

async fn fused_search_case(case: &str, visible_target: usize, budget_ms: f64) {
    let _one = one_at_a_time();
    let data = Dataset::get(perf::texts_for_visible(visible_target)).await;
    println!(
        "{case}: {} texts, {} Versions, {} visible now",
        data.texts, data.versions, data.visible
    );
    // The case must be about the size it names, or it proves less than it says.
    assert!(
        (data.visible as f64) >= visible_target as f64 * 0.95
            && (data.visible as f64) <= visible_target as f64 * 1.05,
        "{} visible rows is not about {visible_target}",
        data.visible
    );
    let store = data.store().await;
    let caller = store.as_worker(data.tenant(), Scope::new(perf::SCOPE));
    let centres = data.centres().await;
    let mut rng = Rng::new(21);
    let record = Record::start(case, &data.db).await;

    let current = AsOf::current(perf::valid_at());
    let mut times = vec![];
    let mut vector_times = vec![];
    let mut text_times = vec![];
    for i in 0..SEARCH_WARM_UP + SEARCH_SAMPLES {
        let (text, q) = data.query_for(&mut rng, &centres);
        let request = SearchRequest::text(&text, current.clone()).with_embedding(&q);
        let started = Instant::now();
        let results = caller.search(data.tenant(), &request, 10).await.unwrap();
        let took = started.elapsed();
        assert!(!results.is_empty(), "a search found nothing for {text:?}");
        if i >= SEARCH_WARM_UP {
            times.push(took);
            // The two branches alone, for the record.
            if i % 4 == 0 {
                let t = Instant::now();
                caller
                    .search_vector(data.tenant(), &q, current.clone(), 10)
                    .await
                    .unwrap();
                vector_times.push(t.elapsed());
                let t = Instant::now();
                caller
                    .search_full_text(data.tenant(), &text, current.clone(), 10)
                    .await
                    .unwrap();
                text_times.push(t.elapsed());
            }
        }
    }
    let stats = Stats::of(&times);
    let loaded = record.finish(json!({
        "visible_rows": data.visible,
        "versions": data.versions,
        "texts": data.texts,
        "warm_up_discarded": SEARCH_WARM_UP,
        "fused_search": stats.to_json(),
        "vector_branch_alone": Stats::of(&vector_times).to_json(),
        "full_text_branch_alone": Stats::of(&text_times).to_json(),
        "budget_p95_ms": budget_ms,
    }));
    println!(
        "{case}: fused Search over {} visible rows: p50 {:.1} ms, p95 {:.1} ms, p99 {:.1} ms \
         (budget {budget_ms} ms at p95){}",
        data.visible,
        stats.p50,
        stats.p95,
        stats.p99,
        if loaded { ", on a loaded machine" } else { "" }
    );
    assert!(
        stats.p95 < budget_ms,
        "fused Search p95 {:.1} ms is over the {budget_ms} ms budget at {} visible rows{}",
        stats.p95,
        data.visible,
        if loaded {
            " (the machine was loaded: rerun when it is quieter)"
        } else {
            ""
        }
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn fused_search_stays_under_100_ms_at_p95_up_to_50000_visible_rows() {
    fused_search_case("fused_search_50k", 50_000, SEARCH_BUDGET_50K).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn fused_search_stays_under_250_ms_at_p95_up_to_200000_visible_rows() {
    fused_search_case("fused_search_200k", 200_000, SEARCH_BUDGET_200K).await;
}

// ---- bounded patch writes ----

fn day(d: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap() + chrono::Duration::days(d)
}

fn values(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        other => panic!("not an object: {other}"),
    }
}

struct Writes {
    db: TestDb,
    store: Store,
    tenant: TenantId,
}

impl Writes {
    fn caller(&self) -> gnomon::Caller<'_> {
        self.store.as_worker(self.tenant, Scope::new(perf::SCOPE))
    }

    async fn episode(&self, reference_time: DateTime<Utc>) -> gnomon::EpisodeId {
        self.caller()
            .record_episode(&NewEpisode {
                tenant: Some(self.tenant),
                scope: Scope::new(perf::SCOPE),
                content: format!("an Episode from {reference_time}"),
                reference_time,
                authority: None,
            })
            .await
            .unwrap()
    }

    /// A patch of two properties over `[from, from + days)`, as prototype 4's
    /// writes were.
    async fn patch(&self, i: i64, from: i64, days: i64) -> Patch {
        let ep = self.episode(day(i % 7)).await;
        Patch::new(
            ep,
            ValidTime::between(day(from), day(from + days)),
            values(json!({ "name": format!("n{}", i % 5), "aliases": [format!("a{}", i % 3)] })),
        )
    }

    /// A Node holding `contributions` Contributions: one patch of two
    /// properties writes two, with Valid-time intervals of 10 to 60 days
    /// spread over a year.
    async fn node_with(&self, contributions: usize, n: i64) -> gnomon::NodeId {
        let first = self.patch(0, 0, 400).await;
        let (node, _) = self
            .caller()
            .create_node(
                &NewNode {
                    tenant: Some(self.tenant),
                    scope: Scope::new(perf::SCOPE),
                    kind: ROOT_NODE_KIND.to_owned(),
                },
                &first,
            )
            .await
            .unwrap();
        for i in 1..(contributions as i64 / 2) {
            let from = (i * 37 + n * 11) % 330;
            let patch = self.patch(i, from, 10 + (i * 13) % 50).await;
            self.caller()
                .write_node(self.tenant, node, &patch)
                .await
                .unwrap();
        }
        node
    }

    /// One bounded patch write on `node`, timed with its commit.
    async fn timed_patch(&self, node: gnomon::NodeId, round: i64, n: i64) -> Duration {
        let from = (round * 61 + n * 29) % 330;
        let patch = self.patch(round + 1000, from, 14).await;
        let started = Instant::now();
        self.caller()
            .write_node(self.tenant, node, &patch)
            .await
            .unwrap();
        started.elapsed()
    }
}

/// Nodes of each size, patches per Node, and Nodes written to and discarded
/// before the timed ones.
const SIZES: [usize; 3] = [10, 100, 500];
const NODES: i64 = 20;
const PATCHES_PER_NODE: i64 = 3;
const WARM_UP_NODES: i64 = 3;

#[tokio::test(flavor = "multi_thread")]
async fn a_bounded_patch_write_costs_1_to_11_ms_at_p50_and_grows_with_the_nodes_contributions() {
    let _one = one_at_a_time();
    let db = TestDb::migrated().await;
    let store = Store::connect(&db.login_url(), &support::connect_options())
        .await
        .unwrap();
    let w = Writes {
        tenant: support::tenant(),
        store,
        db: db,
    };
    let record = Record::start("bounded_patch_write", &w.db.name).await;
    let mut cells = vec![];
    let mut p50s = vec![];
    for contributions in SIZES {
        let loading = Instant::now();
        let mut nodes = vec![];
        for n in 0..WARM_UP_NODES + NODES {
            nodes.push(w.node_with(contributions, n).await);
        }
        // A running Store has autovacuum analyze what it wrote. The Nodes were
        // just loaded, so the planner's statistics are brought up to date here.
        w.db.admin().await.batch_execute("analyze").await.unwrap();
        let count: i64 =
            w.db.admin()
                .await
                .query_typed(
                    "select count(*) from gnomon.contribution where node_id = $1",
                    &[(&nodes[0].0, tokio_postgres::types::Type::UUID)],
                )
                .await
                .unwrap()[0]
                .get(0);
        // Warm-up Nodes first, their writes discarded.
        for (n, node) in nodes.iter().take(WARM_UP_NODES as usize).enumerate() {
            w.timed_patch(*node, 0, n as i64).await;
        }
        let mut times = vec![];
        for round in 0..PATCHES_PER_NODE {
            for (n, node) in nodes.iter().enumerate().skip(WARM_UP_NODES as usize) {
                times.push(w.timed_patch(*node, round, n as i64).await);
            }
        }
        let stats = Stats::of(&times);
        println!(
            "bounded patch on {contributions} Contributions ({count} on the first Node, loaded in \
             {:.0}s): p50 {:.2} ms, p95 {:.2} ms, n = {}",
            loading.elapsed().as_secs_f64(),
            stats.p50,
            stats.p95,
            stats.n
        );
        p50s.push(stats.p50);
        cells.push(
            json!({ "contributions": contributions, "contributions_on_first_node": count,
                           "bounded_patch": stats.to_json(),
                           "prototype_4_bounded_p50_ms": prototype_4_p50(contributions) }),
        );
    }
    let loaded = record.finish(json!({
        "cells": cells,
        "warm_up_nodes_discarded": WARM_UP_NODES,
        "p50_ceiling_ms": PATCH_P50_CEILING_MS,
    }));
    for (contributions, p50) in SIZES.iter().zip(&p50s) {
        assert!(
            *p50 <= PATCH_P50_CEILING_MS,
            "a bounded patch on {contributions} Contributions took {p50:.2} ms at p50, over \
             {PATCH_P50_CEILING_MS} ms{}",
            if loaded {
                " (the machine was loaded: rerun when it is quieter)"
            } else {
                ""
            }
        );
    }
    assert!(
        p50s[0] < p50s[1] && p50s[1] < p50s[2],
        "the p50 of a patch write does not grow with the Contribution count: {p50s:?}"
    );
}
