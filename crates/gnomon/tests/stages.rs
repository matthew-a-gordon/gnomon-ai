//! Seam 1, stage records (ADR-0011, `0003_reads_and_writes.sql`): a stage record written
//! in the transaction of the Store call that holds the stage's output, the
//! claim of a due record with a lease, two concurrent claimers never getting
//! the same record, an expired lease making a record claimable again, the
//! read of every record of an Episode, Retract Episode keeping them, and the
//! catalog check that no column of the stage tables can hold text. Erasure
//! of stage records is covered in erase_episode.rs and erase_graph.rs, and
//! the cross-Tenant and cross-Scope attempts in sql_boundary.rs.

mod support;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use gnomon::{
    Error, Patch, Scope, StageError, StageOutcome, StageRecord, StageStatus, StageUsage,
    StageWrite, Store, ValidTime,
};
use serde_json::json;
use support::erasure::{World, at, values};
use tokio::sync::Barrier;
use tokio_postgres::Client;
use tokio_postgres::error::SqlState;
use tokio_postgres::types::Type;

const HOUR: Duration = Duration::from_secs(3600);

fn usage(model: &str, calls: i32, input: i64, output: i64, cost: Option<f64>) -> StageUsage {
    StageUsage {
        model: model.into(),
        calls,
        input_tokens: input,
        output_tokens: output,
        cost_usd: cost,
    }
}

fn error(code: &str, status: Option<i32>) -> StageError {
    StageError {
        code: code.into(),
        status,
    }
}

impl World {
    async fn pending(&self, episode: gnomon::EpisodeId, stage: &str) {
        self.caller()
            .write_stages(self.tenant, &[StageWrite::pending(episode, stage)])
            .await
            .unwrap();
    }

    async fn claim(
        &self,
        episode: Option<gnomon::EpisodeId>,
        lease: Duration,
    ) -> Option<StageRecord> {
        self.caller()
            .claim_stage(self.tenant, episode, lease)
            .await
            .unwrap()
    }

    async fn stages(&self, episode: gnomon::EpisodeId) -> Vec<StageRecord> {
        self.caller()
            .stage_records(self.tenant, episode)
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn a_stages_output_and_its_stage_records_commit_together_in_one_transaction() {
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    w.pending(e, "graph").await;
    let claim = w
        .claim(Some(e), HOUR)
        .await
        .expect("the graph stage is due");
    assert_eq!(
        (claim.stage.as_str(), claim.status, claim.attempts),
        ("graph", StageStatus::Running, 1)
    );

    // The graph stage's output, its finished record and the next stage's
    // pending record, in one transaction.
    let caller = w.caller();
    let mut tx = caller.transaction(w.tenant).await.unwrap();
    assert_eq!(tx.tenant(), w.tenant);
    tx.read_episode(e).await.unwrap();
    let (node, _) = tx
        .create_node(
            &gnomon::NewNode {
                tenant: Some(w.tenant),
                scope: Scope::new(support::erasure::SCOPE),
                kind: "account".into(),
            },
            &Patch::new(e, ValidTime::UNKNOWN, values(json!({ "plan": "basic" }))),
        )
        .await
        .unwrap();
    tx.write_stages(&[
        StageWrite::finish(
            &claim,
            StageOutcome::Succeeded,
            vec![
                usage("claude-sonnet-5-5", 2, 1200, 300, Some(0.0135)),
                usage("claude-haiku-5-5", 1, 400, 20, None),
            ],
        ),
        StageWrite::pending(e, "sentences"),
    ])
    .await
    .unwrap();
    // Inside the transaction its own writes show; outside, neither the
    // output nor the records do until it commits.
    assert_eq!(tx.stage_records(e).await.unwrap().len(), 2);
    assert!(w.read(node, at(2024, 1, 1)).await.is_none());
    assert_eq!(
        w.stages(e)
            .await
            .iter()
            .map(|r| r.status)
            .collect::<Vec<_>>(),
        [StageStatus::Running]
    );
    tx.commit().await.unwrap();
    assert!(w.read(node, at(2024, 1, 1)).await.is_some());

    let records = w.stages(e).await;
    let stages: Vec<(&str, StageStatus)> = records
        .iter()
        .map(|r| (r.stage.as_str(), r.status))
        .collect();
    assert_eq!(
        stages,
        vec![
            ("graph", StageStatus::Succeeded),
            ("sentences", StageStatus::Pending)
        ]
    );
    let graph = &records[0];
    assert_eq!(
        (graph.attempts, graph.lease_until, &graph.last_error),
        (1, None, &None)
    );
    assert_eq!(
        graph.usage,
        vec![
            usage("claude-haiku-5-5", 1, 400, 20, None),
            usage("claude-sonnet-5-5", 2, 1200, 300, Some(0.0135)),
        ]
    );
    assert!(records[1].usage.is_empty());
}

#[tokio::test]
async fn a_failed_call_rolls_back_its_transaction_and_a_refused_stage_record_rolls_back_the_output()
{
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    let node = w
        .node("account", e, ValidTime::UNKNOWN, json!({ "plan": "basic" }))
        .await;
    w.pending(e, "graph").await;
    // A lease of zero, which expires at once.
    let claim = w.claim(Some(e), Duration::ZERO).await.unwrap();
    let finish = || {
        StageWrite::finish(
            &claim,
            StageOutcome::Succeeded,
            vec![usage("claude-sonnet-5-5", 1, 10, 10, None)],
        )
    };
    let caller = w.caller();

    // The output is refused, so nothing in its transaction commits: not the
    // record, and not a write that succeeded before it.
    let mut tx = caller.transaction(w.tenant).await.unwrap();
    tx.write_node(
        node,
        &Patch::new(e, ValidTime::UNKNOWN, values(json!({ "plan": "pro" }))),
    )
    .await
    .unwrap();
    let refused = tx
        .write_node(
            node,
            &Patch::new(e, ValidTime::UNKNOWN, values(json!({ "plan": 7 }))),
        )
        .await;
    assert!(
        matches!(refused, Err(Error::InvalidWrite(_))),
        "{refused:?}"
    );
    // Postgres has aborted the transaction, so the next call fails too.
    assert!(tx.write_stages(&[finish()]).await.is_err());
    let commit = tx.commit().await;
    assert!(matches!(commit, Err(Error::RolledBack)), "{commit:?}");
    assert_eq!(
        w.current_values(node, at(2024, 1, 1)).await,
        Some(json!({ "plan": "basic" }))
    );
    let [graph] = &w.stages(e).await[..] else {
        panic!("one record")
    };
    assert_eq!(graph.status, StageStatus::Running);
    assert!(graph.usage.is_empty());

    // The stage record is refused, so the output rolls back with it: its
    // lease expired and another claim took it.
    let retaken = w
        .claim(Some(e), HOUR)
        .await
        .expect("the expired lease is claimable");
    assert_eq!(retaken.attempts, 2);
    let mut tx = caller.transaction(w.tenant).await.unwrap();
    tx.write_node(
        node,
        &Patch::new(e, ValidTime::UNKNOWN, values(json!({ "plan": "pro" }))),
    )
    .await
    .unwrap();
    let lost = tx.write_stages(&[finish()]).await;
    assert!(matches!(lost, Err(Error::StageClaimLost(_))), "{lost:?}");
    assert!(matches!(tx.commit().await, Err(Error::RolledBack)));
    assert_eq!(
        w.current_values(node, at(2024, 1, 1)).await,
        Some(json!({ "plan": "basic" })),
        "the stage's output rolled back with its record"
    );
    let [graph] = &w.stages(e).await[..] else {
        panic!("one record")
    };
    assert_eq!((graph.status, graph.attempts), (StageStatus::Running, 2));
    assert!(graph.usage.is_empty());

    // The current claimant finishes.
    let mut tx = caller.transaction(w.tenant).await.unwrap();
    tx.write_node(
        node,
        &Patch::new(e, ValidTime::UNKNOWN, values(json!({ "plan": "pro" }))),
    )
    .await
    .unwrap();
    tx.write_stages(&[StageWrite::finish(
        &retaken,
        StageOutcome::Succeeded,
        vec![],
    )])
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(w.stages(e).await[0].status, StageStatus::Succeeded);
    assert_eq!(
        w.current_values(node, at(2024, 1, 1)).await,
        Some(json!({ "plan": "pro" }))
    );
}

/// A Store call that stores a stage's output (ADR-0011).
#[derive(Debug, Clone, Copy)]
enum Output {
    /// Recording an Episode, with its first stage's pending record.
    Episode,
    Node,
    Edge,
    Sentence,
    Embedding,
    NameEmbedding,
    Summary,
    SummaryEmbedding,
    Community,
    CommunityEmbedding,
}

const OUTPUTS: [Output; 10] = [
    Output::Episode,
    Output::Node,
    Output::Edge,
    Output::Sentence,
    Output::Embedding,
    Output::NameEmbedding,
    Output::Summary,
    Output::SummaryEmbedding,
    Output::Community,
    Output::CommunityEmbedding,
];

/// Whether a stage's output is saved, as a later caller sees it.
enum Saved {
    Episode(gnomon::EpisodeId),
    Node(gnomon::NodeId),
    Edge(gnomon::EdgeId),
    Sentence(gnomon::VersionId),
    /// A gap the output repairs.
    Repairs(gnomon::Gap),
    Summary(gnomon::NodeId),
    Community(gnomon::NodeId),
}

impl World {
    /// Every gap of the Scope.
    async fn gaps(&self) -> Vec<gnomon::Gap> {
        self.caller()
            .list_gaps(
                self.tenant,
                &Scope::new(support::erasure::SCOPE),
                None,
                1000,
            )
            .await
            .unwrap()
            .gaps
    }

    async fn is_saved(&self, saved: &Saved) -> bool {
        let now = gnomon::AsOf::current(at(2024, 6, 1));
        let c = self.caller();
        match saved {
            Saved::Episode(e) => c.read_episode(self.tenant, *e).await.unwrap().is_some(),
            Saved::Node(n) => c.read_node(self.tenant, *n, now).await.unwrap().is_some(),
            Saved::Edge(e) => c.read_edge(self.tenant, *e, now).await.unwrap().is_some(),
            Saved::Sentence(v) => self.sentence_of(*v).await.is_some(),
            Saved::Repairs(gap) => !self.gaps().await.contains(gap),
            Saved::Summary(n) => !c
                .read_summaries(self.tenant, *n, now)
                .await
                .unwrap()
                .is_empty(),
            Saved::Community(n) => !c
                .read_communities(self.tenant, *n, now)
                .await
                .unwrap()
                .is_empty(),
        }
    }
}

/// A committed Node of the root Kind with a name, for the outputs that
/// embed or summarize it.
async fn named_node(w: &World, e: gnomon::EpisodeId, name: &str) -> gnomon::NodeId {
    w.caller()
        .create_node(
            &gnomon::NewNode {
                tenant: Some(w.tenant),
                scope: Scope::new(support::erasure::SCOPE),
                kind: gnomon::ROOT_NODE_KIND.into(),
            },
            &Patch::new(e, ValidTime::UNKNOWN, values(json!({ "name": name }))),
        )
        .await
        .unwrap()
        .0
}

fn new_summary(node: gnomon::NodeId, e: gnomon::EpisodeId) -> gnomon::NewSummary {
    gnomon::NewSummary {
        scope: Scope::new(support::erasure::SCOPE),
        node,
        content: "A summary.".into(),
        as_of_valid: at(2024, 1, 1),
        citations: vec![gnomon::DerivedCitation::of(e)],
    }
}

fn new_community(node: gnomon::NodeId, e: gnomon::EpisodeId) -> gnomon::NewCommunity {
    gnomon::NewCommunity {
        scope: Scope::new(support::erasure::SCOPE),
        replaces: vec![],
        members: vec![node],
        content: "A community.".into(),
        as_of_valid: at(2024, 1, 1),
        citations: vec![gnomon::DerivedCitation::of(e)],
    }
}

/// Runs `output` in `tx` against what earlier stages committed for `e`, and
/// returns how to tell whether it was saved.
async fn store_output(
    w: &World,
    tx: &mut gnomon::Transaction,
    output: Output,
    e: gnomon::EpisodeId,
    n: usize,
) -> Saved {
    let mut rng = support::vectors::Rng::new(n as u64);
    let name = format!("output {n}");
    match output {
        Output::Episode => unreachable!("recorded with its pending record"),
        Output::Node => {
            let (node, _) = tx
                .create_node(
                    &gnomon::NewNode {
                        tenant: Some(w.tenant),
                        scope: Scope::new(support::erasure::SCOPE),
                        kind: "account".into(),
                    },
                    &Patch::new(e, ValidTime::UNKNOWN, values(json!({ "plan": "basic" }))),
                )
                .await
                .unwrap();
            Saved::Node(node)
        }
        Output::Edge => {
            let a = w
                .node("account", e, ValidTime::UNKNOWN, json!({ "plan": "basic" }))
                .await;
            let b = w
                .node("company", e, ValidTime::UNKNOWN, json!({ "name": "Acme" }))
                .await;
            let (edge, _) = tx
                .create_edge(
                    &gnomon::NewEdge {
                        tenant: Some(w.tenant),
                        scope: Scope::new(support::erasure::SCOPE),
                        kind: "owns".into(),
                        source: a,
                        target: b,
                    },
                    &Patch::new(e, ValidTime::UNKNOWN, values(json!({ "share": 5 }))),
                )
                .await
                .unwrap();
            Saved::Edge(edge)
        }
        Output::Sentence => {
            let a = w
                .node("account", e, ValidTime::UNKNOWN, json!({ "plan": "basic" }))
                .await;
            let b = w
                .node("company", e, ValidTime::UNKNOWN, json!({ "name": "Acme" }))
                .await;
            let edge = w
                .edge(a, b, e, ValidTime::UNKNOWN, json!({ "share": 5 }))
                .await;
            let version = w.current_edge_version(edge, at(2024, 6, 1)).await;
            tx.store_sentence(version, &format!("{name} owns five percent"))
                .await
                .unwrap();
            Saved::Sentence(version)
        }
        Output::Embedding => {
            let node = named_node(w, e, &name).await;
            let version = w.read(node, at(2024, 6, 1)).await.unwrap();
            let gap = gnomon::Gap::NodeEmbedding {
                version: version.id,
                node,
                text_hash: version.text_hash.clone(),
            };
            assert!(w.gaps().await.contains(&gap), "{gap:?} is a gap first");
            tx.store_embedding(version.id, &version.text_hash, &rng.unit())
                .await
                .unwrap();
            Saved::Repairs(gap)
        }
        Output::NameEmbedding => {
            let node = named_node(w, e, &name).await;
            let gap = w
                .gaps()
                .await
                .into_iter()
                .find(|g| matches!(g, gnomon::Gap::NameEmbedding { node: n, .. } if *n == node))
                .expect("the name is a gap first");
            tx.store_name_embedding(node, &name, &rng.unit())
                .await
                .unwrap();
            Saved::Repairs(gap)
        }
        Output::Summary => {
            let node = named_node(w, e, &name).await;
            tx.write_summary(&new_summary(node, e)).await.unwrap();
            Saved::Summary(node)
        }
        Output::SummaryEmbedding => {
            let node = named_node(w, e, &name).await;
            let summary = w
                .caller()
                .write_summary(w.tenant, &new_summary(node, e))
                .await
                .unwrap();
            tx.store_summary_embedding(summary, &rng.unit())
                .await
                .unwrap();
            Saved::Repairs(gnomon::Gap::SummaryEmbedding { summary, node })
        }
        Output::Community => {
            let node = named_node(w, e, &name).await;
            tx.write_community(&new_community(node, e)).await.unwrap();
            Saved::Community(node)
        }
        Output::CommunityEmbedding => {
            let node = named_node(w, e, &name).await;
            let community = w
                .caller()
                .write_community(w.tenant, &new_community(node, e))
                .await
                .unwrap();
            tx.store_community_embedding(community, &rng.unit())
                .await
                .unwrap();
            Saved::Repairs(gnomon::Gap::CommunityEmbedding { community })
        }
    }
}

#[tokio::test]
async fn every_call_that_stores_a_stages_output_commits_it_with_the_stage_record_and_a_rollback_saves_neither()
 {
    let w = World::new().await;
    let caller = w.caller();
    let mut n = 0;
    for output in OUTPUTS {
        for commit in [true, false] {
            n += 1;
            let what = format!(
                "{output:?}, {}",
                if commit { "committed" } else { "rolled back" }
            );
            let (saved, episode, stage) = match output {
                Output::Episode => {
                    // add_episode records the Episode and its first stage's
                    // pending record together.
                    let mut tx = caller.transaction(w.tenant).await.unwrap();
                    let e = tx
                        .record_episode(&gnomon::NewEpisode {
                            tenant: Some(w.tenant),
                            scope: Scope::new(support::erasure::SCOPE),
                            content: what.clone(),
                            reference_time: at(2024, 1, 1),
                            authority: None,
                        })
                        .await
                        .unwrap();
                    tx.write_stages(&[StageWrite::pending(e, "graph")])
                        .await
                        .unwrap();
                    end(tx, commit, n / 2).await;
                    (Saved::Episode(e), e, None)
                }
                _ => {
                    let e = w.episode(at(2024, 1, 1), None).await;
                    w.pending(e, "stage").await;
                    let claim = w.claim(Some(e), HOUR).await.unwrap();
                    let mut tx = caller.transaction(w.tenant).await.unwrap();
                    let saved = store_output(&w, &mut tx, output, e, n).await;
                    tx.write_stages(&[StageWrite::finish(&claim, StageOutcome::Succeeded, vec![])])
                        .await
                        .unwrap();
                    end(tx, commit, n / 2).await;
                    (saved, e, Some(claim))
                }
            };
            assert_eq!(w.is_saved(&saved).await, commit, "{what}: the output");
            let statuses: Vec<StageStatus> =
                w.stages(episode).await.iter().map(|r| r.status).collect();
            let expected = match (stage.is_some(), commit) {
                (false, true) => vec![StageStatus::Pending],
                (false, false) => vec![],
                (true, true) => vec![StageStatus::Succeeded],
                (true, false) => vec![StageStatus::Running],
            };
            assert_eq!(statuses, expected, "{what}: the stage records");
        }
    }
}

/// Commits `tx`, or ends it without a commit: by rolling it back for an
/// even `n` and by dropping it for an odd one, so both are covered.
async fn end(tx: gnomon::Transaction, commit: bool, n: usize) {
    if commit {
        tx.commit().await.unwrap();
    } else if n.is_multiple_of(2) {
        tx.rollback().await.unwrap();
    } else {
        drop(tx);
    }
}

#[tokio::test]
async fn a_claim_takes_a_pending_or_retry_due_record_or_one_whose_lease_expired_and_nothing_else() {
    let w = World::new().await;
    let [pending, due, later, held, lapsed, done, failed] = {
        let mut es = Vec::new();
        for d in 1..=7 {
            es.push(w.episode(at(2024, 1, d), None).await);
        }
        <[_; 7]>::try_from(es).unwrap()
    };
    for e in [pending, due, later, held, lapsed, done, failed] {
        w.pending(e, "graph").await;
    }
    let finish = |e, outcome| {
        let w = &w;
        async move {
            let claim = w.claim(Some(e), HOUR).await.unwrap();
            w.caller()
                .write_stages(w.tenant, &[StageWrite::finish(&claim, outcome, vec![])])
                .await
                .unwrap();
        }
    };
    finish(
        due,
        StageOutcome::Retry {
            error: error("rate_limited", Some(429)),
            after: Duration::ZERO,
        },
    )
    .await;
    finish(
        later,
        StageOutcome::Retry {
            error: error("timeout", None),
            after: HOUR,
        },
    )
    .await;
    finish(done, StageOutcome::Succeeded).await;
    finish(
        failed,
        StageOutcome::Failed {
            error: error("output.invalid_json", None),
        },
    )
    .await;
    w.claim(Some(held), HOUR).await.unwrap();
    // A lease of zero expires at once, by the database's clock.
    w.claim(Some(lapsed), Duration::ZERO).await.unwrap();

    let mut claimed = Vec::new();
    while let Some(c) = w.claim(None, HOUR).await {
        claimed.push((c.episode, c.attempts));
    }
    claimed.sort_by_key(|(e, _)| e.0);
    let mut expected = vec![(pending, 1), (due, 2), (lapsed, 2)];
    expected.sort_by_key(|(e, _)| e.0);
    assert_eq!(claimed, expected);

    // What a retry and a failure recorded, and that a success keeps none.
    let later_record = &w.stages(later).await[0];
    assert_eq!(later_record.status, StageStatus::Retry);
    assert_eq!(later_record.last_error, Some(error("timeout", None)));
    assert!(later_record.retry_at.unwrap() > later_record.updated_at);
    let failed_record = &w.stages(failed).await[0];
    assert_eq!(failed_record.status, StageStatus::Failed);
    assert_eq!(
        failed_record.last_error,
        Some(error("output.invalid_json", None))
    );
    assert_eq!(failed_record.retry_at, None);
    let due_record = &w.stages(due).await[0];
    assert_eq!(
        (
            due_record.status,
            &due_record.last_error,
            due_record.retry_at
        ),
        (
            StageStatus::Running,
            &Some(error("rate_limited", Some(429))),
            None
        )
    );
    assert_eq!(w.stages(done).await[0].last_error, None);
}

#[tokio::test]
async fn concurrent_claimers_never_get_the_same_record() {
    const CLAIMERS: usize = 8;
    const RECORDS: usize = 24;
    let w = World::new().await;
    for d in 0..RECORDS / 3 {
        let e = w.episode(at(2024, 1, 1 + d as u32), None).await;
        for stage in ["graph", "sentences", "embeddings"] {
            w.pending(e, stage).await;
        }
    }
    let url = w.db.login_url();
    let barrier = Arc::new(Barrier::new(CLAIMERS));
    let mut claimers = Vec::new();
    for _ in 0..CLAIMERS {
        let (url, barrier, tenant) = (url.clone(), barrier.clone(), w.tenant);
        claimers.push(tokio::spawn(async move {
            let store = Store::connect(&url, &support::connect_options())
                .await
                .unwrap();
            let caller = store.as_worker(tenant, Scope::new(support::erasure::SCOPE), None);
            barrier.wait().await;
            let mut mine = Vec::new();
            while let Some(c) = caller.claim_stage(tenant, None, HOUR).await.unwrap() {
                mine.push((c.episode, c.stage, c.attempts));
            }
            mine
        }));
    }
    let mut all = Vec::new();
    for c in claimers {
        all.extend(c.await.unwrap());
    }
    let distinct: HashSet<_> = all.iter().map(|(e, s, _)| (*e, s.clone())).collect();
    assert_eq!(
        distinct.len(),
        all.len(),
        "two claimers got the same record: {all:?}"
    );
    assert_eq!(all.len(), RECORDS, "every record was claimed once");
    assert!(all.iter().all(|(_, _, attempts)| *attempts == 1));
}

#[tokio::test]
async fn a_claim_skips_the_record_another_claimer_holds_and_never_waits_for_it() {
    let w = World::new().await;
    let first = w.episode(at(2024, 1, 1), None).await;
    let second = w.episode(at(2024, 1, 2), None).await;
    w.pending(first, "graph").await;
    w.pending(second, "graph").await;

    // A claimer that has claimed, under its lock, and not yet committed.
    let mut held_client = w.db.login().await;
    let mut held = Some(w.raw(&mut held_client).await);
    let rows = held
        .as_ref()
        .unwrap()
        .query_typed(
            "select episode_id from gnomon.claim_stage($1, null, interval '1 hour')",
            &[(&w.tenant.0, Type::UUID)],
        )
        .await
        .unwrap();
    let held_episode: uuid::Uuid = rows[0].get(0);
    assert_eq!(held_episode, first.0, "the record due longest");

    // A second claimer, on its own connection, through the Store.
    let url = w.db.login_url();
    let tenant = w.tenant;
    let mut other = tokio::spawn(async move {
        let store = Store::connect(&url, &support::connect_options())
            .await
            .unwrap();
        store
            .as_worker(tenant, Scope::new(support::erasure::SCOPE), None)
            .claim_stage(tenant, None, HOUR)
            .await
            .unwrap()
    });
    // Without SKIP LOCKED it waits for the held lock, until the holder ends.
    // So the claim either returns, or is seen waiting on a lock, which fails
    // the test once the holder commits and the waiting claim returns too.
    let admin = w.db.admin().await;
    let (claimed, waited) = loop {
        tokio::select! {
            done = &mut other => break (done.unwrap(), false),
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                if waiting_on_a_lock(&admin, &w.db.name).await {
                    held.take().unwrap().commit().await.unwrap();
                    break ((&mut other).await.unwrap(), true);
                }
            }
        }
    };
    assert!(
        !waited,
        "the claim waited for the record another claimer holds, and then returned {claimed:?}"
    );
    let claimed = claimed.expect("the other record is due");
    assert_ne!(
        claimed.episode.0, held_episode,
        "two claimers got the same record"
    );
    assert_eq!(claimed.episode, second);
}

/// Whether a backend of database `db` is waiting on a lock.
async fn waiting_on_a_lock(admin: &Client, db: &str) -> bool {
    !admin
        .query_typed(
            "select from pg_stat_activity where datname = $1 and wait_event_type = 'Lock'",
            &[(&db, Type::TEXT)],
        )
        .await
        .unwrap()
        .is_empty()
}

#[tokio::test]
async fn retract_episode_keeps_its_stage_records_and_usage_counts() {
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    w.node("account", e, ValidTime::UNKNOWN, json!({ "plan": "basic" }))
        .await;
    w.pending(e, "graph").await;
    let claim = w.claim(Some(e), HOUR).await.unwrap();
    w.caller()
        .write_stages(
            w.tenant,
            &[
                StageWrite::finish(
                    &claim,
                    StageOutcome::Succeeded,
                    vec![usage("claude-sonnet-5-5", 3, 900, 120, Some(0.01))],
                ),
                StageWrite::pending(e, "sentences"),
            ],
        )
        .await
        .unwrap();
    let before = w.stages(e).await;
    assert_eq!(before.len(), 2);

    let retraction = w.caller().retract_episode(w.tenant, e).await.unwrap();
    assert_eq!(retraction.retracted.len(), 1);
    assert_eq!(w.stages(e).await, before);
}

#[tokio::test]
async fn a_caller_reads_and_writes_only_the_stage_records_of_episodes_it_sees() {
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    w.pending(e, "graph").await;
    let billing = w.episode_in("billing", at(2024, 1, 1), "billing").await;

    let elsewhere = w.worker_in("billing");
    assert!(
        elsewhere
            .stage_records(w.tenant, e)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        elsewhere
            .claim_stage(w.tenant, None, HOUR)
            .await
            .unwrap()
            .is_none()
    );
    let refused = elsewhere
        .write_stages(w.tenant, &[StageWrite::pending(e, "sentences")])
        .await;
    assert!(
        matches!(refused, Err(Error::InvalidWrite(_))),
        "{refused:?}"
    );
    let refused = w
        .caller()
        .write_stages(w.tenant, &[StageWrite::pending(billing, "graph")])
        .await;
    assert!(
        matches!(refused, Err(Error::InvalidWrite(_))),
        "{refused:?}"
    );
    assert_eq!(w.stages(e).await.len(), 1);
}

#[tokio::test]
async fn a_stage_name_model_name_or_error_code_that_is_not_an_identifier_is_refused() {
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    let refused = w
        .caller()
        .write_stages(w.tenant, &[StageWrite::pending(e, "extract the graph")])
        .await;
    assert_check_violation(refused, "a stage name with spaces");
    w.pending(e, "graph").await;

    let claim = w.claim(Some(e), HOUR).await.unwrap();
    for (what, outcome, usage) in [
        (
            "an error message for a code",
            StageOutcome::Failed {
                error: error(
                    "could not parse the model's reply: {\"plan\": \"pro\"",
                    None,
                ),
            },
            vec![],
        ),
        (
            "a model name holding text",
            StageOutcome::Succeeded,
            vec![usage("the model said hello", 1, 1, 1, None)],
        ),
        (
            "a code longer than 63",
            StageOutcome::Failed {
                error: error(&"a".repeat(64), None),
            },
            vec![],
        ),
    ] {
        let refused = w
            .caller()
            .write_stages(w.tenant, &[StageWrite::finish(&claim, outcome, usage)])
            .await;
        assert_check_violation(refused, what);
    }
    assert_eq!(w.stages(e).await[0].status, StageStatus::Running);
}

fn assert_check_violation(result: Result<(), Error>, what: &str) {
    match result {
        Err(e) => assert_eq!(
            e.sqlstate(),
            Some(&SqlState::CHECK_VIOLATION),
            "{what}: {e:?}"
        ),
        Ok(()) => panic!("{what} was accepted"),
    }
}

/// The column types that cannot hold text, beside an enum of gnomon's own.
const TEXTLESS_TYPES: &[&str] = &[
    "uuid",
    "integer",
    "bigint",
    "smallint",
    "boolean",
    "timestamp with time zone",
];

/// Every column of the stage tables that could hold model output or
/// Episode text, read from the catalog: a column of a type outside
/// [`TEXTLESS_TYPES`], gnomon's enums and a numeric with a fixed precision,
/// or a text column that accepts a value with a space or one of 129
/// characters, one more than the longest identifier the stage tables allow. A text column that is part of a foreign key to the Episode
/// holds the Episode's own key, its Scope, and is not counted. Each probe
/// runs in a savepoint of `tx` and is rolled back.
async fn columns_that_can_hold_text(tx: &mut tokio_postgres::Transaction<'_>) -> Vec<String> {
    let columns = tx
        .query_typed(
            "select c.relname::text, a.attname::text, format_type(a.atttypid, a.atttypmod),
                    t.typtype = 'e' and t.typnamespace = 'gnomon'::regnamespace,
                    t.typname = 'numeric' and a.atttypmod <> -1,
                    t.typcategory = 'S',
                    exists (select from pg_constraint k
                            where k.conrelid = c.oid and k.contype = 'f'
                              and k.confrelid = 'gnomon.episode'::regclass
                              and a.attnum = any(k.conkey))
             from pg_class c
             join pg_attribute a on a.attrelid = c.oid and a.attnum > 0 and not a.attisdropped
             join pg_type t on t.oid = a.atttypid
             where c.relnamespace = 'gnomon'::regnamespace and c.relkind in ('r', 'p')
               and c.relname like 'stage\\_%'
             order by 1, 2",
            &[],
        )
        .await
        .unwrap();
    let mut found = Vec::new();
    for row in &columns {
        let (table, column, ty): (String, String, String) = (row.get(0), row.get(1), row.get(2));
        let (own_enum, fixed_numeric, string, episode_key): (bool, bool, bool, bool) =
            (row.get(3), row.get(4), row.get(5), row.get(6));
        if own_enum || fixed_numeric || TEXTLESS_TYPES.contains(&ty.as_str()) {
            continue;
        }
        if !string {
            found.push(format!("{table}.{column} {ty}"));
            continue;
        }
        if episode_key {
            continue;
        }
        for probe in ["two words".to_owned(), "a".repeat(129)] {
            let sp = tx.savepoint("probe").await.unwrap();
            let refused = sp
                .execute_typed(
                    &format!("update gnomon.{table} set {column} = $1"),
                    &[(&probe, Type::TEXT)],
                )
                .await;
            let held = match refused {
                Ok(n) => {
                    assert!(n > 0, "gnomon.{table} has no row to probe");
                    true
                }
                Err(e) => e.code() != Some(&SqlState::CHECK_VIOLATION),
            };
            sp.rollback().await.unwrap();
            if held {
                found.push(format!("{table}.{column} {ty} holds {:?}", &probe[..9]));
                break;
            }
        }
    }
    found
}

#[tokio::test]
async fn no_column_of_the_stage_tables_can_hold_model_output_or_episode_text() {
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    w.pending(e, "graph").await;
    let claim = w.claim(Some(e), HOUR).await.unwrap();
    w.caller()
        .write_stages(
            w.tenant,
            &[StageWrite::finish(
                &claim,
                StageOutcome::Retry {
                    error: error("rate_limited", Some(429)),
                    after: HOUR,
                },
                vec![usage("claude-sonnet-5-5", 1, 10, 0, Some(0.001))],
            )],
        )
        .await
        .unwrap();

    let mut admin = w.db.admin().await;
    let mut tx = admin.transaction().await.unwrap();
    let tables: Vec<String> = tx
        .query_typed(
            "select relname::text from pg_class
             where relnamespace = 'gnomon'::regnamespace and relname like 'stage\\_%'
               and relkind = 'r' order by 1",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(tables, ["stage_record", "stage_usage"]);
    assert_eq!(
        columns_that_can_hold_text(&mut tx).await,
        Vec::<String>::new()
    );

    // The check fails when a column that can hold text is added, a long
    // identifier among them.
    tx.batch_execute(
        "alter table gnomon.stage_record add column note text;
         alter table gnomon.stage_record add column tag text check (tag ~ '^[a-z]{0,200}$');
         alter table gnomon.stage_usage add column output jsonb;
         alter table gnomon.stage_usage add column reply varchar(200);",
    )
    .await
    .unwrap();
    assert_eq!(
        columns_that_can_hold_text(&mut tx).await,
        vec![
            "stage_record.note text holds \"two words\"".to_owned(),
            "stage_record.tag text holds \"aaaaaaaaa\"".to_owned(),
            "stage_usage.output jsonb".to_owned(),
            "stage_usage.reply character varying(200) holds \"two words\"".to_owned(),
        ]
    );
    tx.rollback().await.unwrap();
}
