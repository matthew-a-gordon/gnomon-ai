//! Seam 1, Retract Episode: a Retraction for every Version cited only by the
//! Episode, none for a Version another Episode also cites, and history that
//! stays readable (ADR-0003, ADR-0007, ADR-0009), through the Rust core's
//! Store API against real Postgres. The adversarial cases live in
//! sql_boundary.rs.

mod support;

use gnomon::{
    AsOf, Bound, EdgeId, EndOfValidity, EpisodeId, Error, KindOf, MergeRule, NewEdge, NewEpisode,
    NewKind, NewNode, NodeId, NodeVersion, Patch, Scope, StatementKind, Store, TenantId, ValidTime,
    WriteOutcome, WriteStatus,
};
use jiff::{Timestamp, civil::date, tz::TimeZone};
use serde_json::{Map, Value, json};
use support::TestDb;
use tokio_postgres::types::Type;

const SCOPE: &str = "support";

fn at(y: i16, m: i8, d: i8) -> Timestamp {
    date(y, m, d)
        .at(0, 0, 0, 0)
        .to_zoned(TimeZone::UTC)
        .unwrap()
        .timestamp()
}

fn values(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        other => panic!("not an object: {other}"),
    }
}

struct World {
    db: TestDb,
    store: Store,
    tenant: TenantId,
}

impl World {
    /// A Tenant with the Node Kinds `account`, which declares `plan`, and
    /// `company`, and the Edge Kind `owns`, which declares `share`.
    async fn new() -> World {
        let db = TestDb::migrated().await;
        db.grant_kind_registration("gnomon_worker").await;
        let store = Store::connect(&db.login_url(), &support::connect_options())
            .await
            .unwrap();
        let w = World {
            db,
            store,
            tenant: support::tenant(),
        };
        for (name, of, properties) in [
            (
                "account",
                KindOf::Node,
                json!({ "plan": { "type": "string" } }),
            ),
            ("company", KindOf::Node, json!({})),
            (
                "owns",
                KindOf::Edge {
                    source: vec!["account".into()],
                    target: vec!["company".into()],
                },
                json!({ "share": { "type": "integer" } }),
            ),
        ] {
            w.caller()
                .register_kind(&NewKind {
                    tenant: Some(w.tenant),
                    name: name.into(),
                    of,
                    parent: None,
                    properties: json!({ "type": "object", "properties": properties }),
                    search: vec![],
                })
                .await
                .unwrap();
        }
        w
    }

    fn caller(&self) -> gnomon::Caller<'_> {
        self.store.as_worker(self.tenant, Scope::new(SCOPE), None)
    }

    async fn set_rule(&self, kind: &str, rule: MergeRule) {
        self.caller()
            .set_merge_rule(self.tenant, kind, rule)
            .await
            .unwrap();
    }

    async fn episode(&self, reference_time: Timestamp, authority: Option<i32>) -> EpisodeId {
        self.caller()
            .record_episode(&NewEpisode {
                tenant: Some(self.tenant),
                scope: Scope::new(SCOPE),
                content: format!("an Episode from {reference_time}"),
                reference_time,
                authority,
            })
            .await
            .unwrap()
    }

    /// An account whose plan `episode` states over `vt`.
    async fn account(&self, episode: EpisodeId, vt: ValidTime, plan: &str) -> NodeId {
        self.caller()
            .create_node(
                &NewNode {
                    tenant: Some(self.tenant),
                    scope: Scope::new(SCOPE),
                    kind: "account".into(),
                },
                &Patch::new(episode, vt, values(json!({ "plan": plan }))),
            )
            .await
            .unwrap()
            .0
    }

    async fn company(&self) -> NodeId {
        let ep = self.episode(at(2024, 1, 1), None).await;
        self.caller()
            .create_node(
                &NewNode {
                    tenant: Some(self.tenant),
                    scope: Scope::new(SCOPE),
                    kind: "company".into(),
                },
                &Patch::new(ep, ValidTime::UNKNOWN, values(json!({ "name": "Acme" }))),
            )
            .await
            .unwrap()
            .0
    }

    /// An `owns` Edge from a new account to a new company, which `episode`
    /// states holds over `vt` with a share of 40.
    async fn edge(&self, episode: EpisodeId, vt: ValidTime) -> EdgeId {
        let account = self.account(episode, ValidTime::UNKNOWN, "basic").await;
        let company = self.company().await;
        self.caller()
            .create_edge(
                &NewEdge {
                    tenant: Some(self.tenant),
                    scope: Scope::new(SCOPE),
                    kind: "owns".into(),
                    source: account,
                    target: company,
                },
                &Patch::new(episode, vt, values(json!({ "share": 40 }))),
            )
            .await
            .unwrap()
            .0
    }

    async fn read(&self, node: NodeId, valid_at: Timestamp) -> Option<NodeVersion> {
        self.caller()
            .read_node(self.tenant, node, AsOf::current(valid_at))
            .await
            .unwrap()
    }

    async fn read_as_of(
        &self,
        node: NodeId,
        valid_at: Timestamp,
        known_at: Timestamp,
    ) -> Option<NodeVersion> {
        self.caller()
            .read_node(self.tenant, node, AsOf::at(valid_at, known_at))
            .await
            .unwrap()
    }

    async fn plan(&self, node: NodeId, valid_at: Timestamp) -> Option<String> {
        self.read(node, valid_at).await.and_then(|v| {
            v.values
                .get("plan")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
    }

    async fn edge_holds(&self, edge: EdgeId, valid_at: Timestamp) -> bool {
        self.caller()
            .read_edge(self.tenant, edge, AsOf::current(valid_at))
            .await
            .unwrap()
            .is_some()
    }

    async fn write(&self, node: NodeId, episode: EpisodeId, vt: ValidTime, v: Value) {
        self.caller()
            .write_node(self.tenant, node, &Patch::new(episode, vt, values(v)))
            .await
            .unwrap();
    }

    async fn correct(
        &self,
        node: NodeId,
        episode: EpisodeId,
        vt: ValidTime,
        plan: &str,
    ) -> WriteOutcome {
        self.caller()
            .correct_node(
                self.tenant,
                node,
                &Patch::new(episode, vt, values(json!({ "plan": plan }))),
            )
            .await
            .unwrap()
    }

    async fn end(&self, node: NodeId, episode: EpisodeId, when: Timestamp) -> WriteOutcome {
        self.caller()
            .end_node_validity(
                self.tenant,
                node,
                &EndOfValidity {
                    episode,
                    at: Bound::At(when),
                },
            )
            .await
            .unwrap()
    }

    /// Deletes an Episode's Contributions, statements and bound statements
    /// as the operator, and leaves the Episode. A later write to the Node
    /// then recomputes its current Versions from what remains. These tests
    /// predate Erasure (erase_episode.rs), which also rewrites the Node's
    /// closed history.
    async fn erase_rows_of(&self, episode: EpisodeId) {
        let admin = self.db.admin().await;
        for sql in [
            "delete from gnomon.contribution where episode_id = $1",
            "delete from gnomon.statement where episode_id = $1",
            "delete from gnomon.bound_statement where episode_id = $1",
        ] {
            admin
                .execute_typed(sql, &[(&episode.0, Type::UUID)])
                .await
                .unwrap();
        }
    }

    /// Makes the Node recompute, by a write that states another property
    /// over the whole of time.
    async fn recompute(&self, node: NodeId) {
        let ep = self.episode(at(2030, 1, 1), Some(0)).await;
        self.write(
            node,
            ep,
            ValidTime::UNKNOWN,
            json!({ "name": "recomputed" }),
        )
        .await;
    }
}

impl World {
    async fn retract_episode(&self, episode: EpisodeId) -> gnomon::EpisodeRetraction {
        self.caller()
            .retract_episode(self.tenant, episode)
            .await
            .unwrap()
    }

    async fn citing(&self, version: gnomon::VersionId) -> Vec<(EpisodeId, String)> {
        self.caller()
            .list_citations(self.tenant, version)
            .await
            .unwrap()
            .into_iter()
            .map(|c| (c.episode, c.property))
            .collect()
    }

    /// The numbers of rows Retract Episode must leave alone: the Episode's
    /// Contributions and Citations, and its Episode.
    async fn rows_of(&self, episode: EpisodeId) -> (i64, i64, i64, i64) {
        let admin = self.db.admin().await;
        let mut counts = Vec::new();
        for sql in [
            "select count(*) from gnomon.contribution where episode_id = $1",
            "select count(*) from gnomon.citation where episode_id = $1",
            "select count(*) from gnomon.statement where episode_id = $1",
            "select count(*) from gnomon.episode where id = $1",
        ] {
            counts.push(
                admin
                    .query_typed(sql, &[(&episode.0, Type::UUID)])
                    .await
                    .unwrap()[0]
                    .get::<_, i64>(0),
            );
        }
        (counts[0], counts[1], counts[2], counts[3])
    }
}

#[tokio::test]
async fn every_version_cited_only_by_the_retracted_episode_is_retracted() {
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    let node = w
        .account(
            e,
            ValidTime::between(at(2024, 1, 1), at(2024, 6, 1)),
            "gold",
        )
        .await;
    w.write(
        node,
        e,
        ValidTime::between(at(2024, 6, 1), at(2024, 12, 1)),
        json!({ "plan": "silver" }),
    )
    .await;
    let other = w
        .account(e, ValidTime::since(at(2024, 1, 1)), "basic")
        .await;
    let first = w.read(node, at(2024, 3, 1)).await.unwrap();
    let second = w.read(node, at(2024, 9, 1)).await.unwrap();
    let third = w.read(other, at(2024, 9, 1)).await.unwrap();

    let done = w.retract_episode(e).await;

    let mut retracted = done.retracted.clone();
    retracted.sort_by_key(|v| v.0);
    let mut expected = vec![first.id, second.id, third.id];
    expected.sort_by_key(|v| v.0);
    assert_eq!(retracted, expected);
    assert_eq!(w.plan(node, at(2024, 3, 1)).await, None);
    assert_eq!(w.plan(node, at(2024, 9, 1)).await, None);
    assert_eq!(w.plan(other, at(2024, 9, 1)).await, None);

    // Each Retraction is a statement of the retracted Episode.
    let statements = w
        .caller()
        .list_node_statements(w.tenant, node)
        .await
        .unwrap();
    assert_eq!(statements.len(), 2);
    for s in &statements {
        assert_eq!(s.kind, StatementKind::Retraction);
        assert_eq!(s.episode, e);
        assert_eq!(s.recorded_at, done.recorded_at);
    }
}

#[tokio::test]
async fn a_version_also_cited_by_another_episode_is_not_retracted() {
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    let vt = ValidTime::between(at(2024, 1, 1), at(2024, 12, 1));
    let node = w.account(e, vt, "gold").await;
    // A later Episode states the same value, so it corroborates the Version.
    let f = w.episode(at(2024, 2, 1), None).await;
    w.write(node, f, vt, json!({ "plan": "gold" })).await;
    let version = w.read(node, at(2024, 3, 1)).await.unwrap();
    assert_eq!(w.citing(version.id).await.len(), 2);

    let done = w.retract_episode(e).await;

    assert!(done.retracted.is_empty());
    let now = w.read(node, at(2024, 3, 1)).await.unwrap();
    assert_eq!(now.id, version.id, "the same Version, untouched");
    assert!(now.recorded_to.is_none());
    assert!(
        w.caller()
            .list_node_statements(w.tenant, node)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn an_episode_whose_citations_overlap_another_retracts_only_the_version_it_alone_cites() {
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    let node = w
        .account(
            e,
            ValidTime::between(at(2024, 1, 1), at(2024, 6, 1)),
            "gold",
        )
        .await;
    // Another Episode states another property over the second half, so the
    // Valid time splits: the first half only E cites, the second both.
    let f = w.episode(at(2024, 2, 1), None).await;
    w.write(
        node,
        f,
        ValidTime::between(at(2024, 3, 1), at(2024, 6, 1)),
        json!({ "name": "Acme" }),
    )
    .await;
    let early = w.read(node, at(2024, 2, 1)).await.unwrap();
    let late = w.read(node, at(2024, 4, 1)).await.unwrap();
    assert_ne!(early.id, late.id);

    let done = w.retract_episode(e).await;

    assert_eq!(done.retracted, vec![early.id]);
    assert_eq!(w.plan(node, at(2024, 2, 1)).await, None);
    let still = w.read(node, at(2024, 4, 1)).await.unwrap();
    assert_eq!(still.id, late.id);
    assert_eq!(still.values["plan"], "gold");
}

#[tokio::test]
async fn a_version_the_retracted_episode_cites_twice_is_retracted_once() {
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    let node = w
        .caller()
        .create_node(
            &NewNode {
                tenant: Some(w.tenant),
                scope: Scope::new(SCOPE),
                kind: "account".into(),
            },
            &Patch::new(
                e,
                ValidTime::since(at(2024, 1, 1)),
                values(json!({ "plan": "gold", "name": "Acme" })),
            ),
        )
        .await
        .unwrap()
        .0;
    let version = w.read(node, at(2024, 3, 1)).await.unwrap();
    let citing = w.citing(version.id).await;
    assert_eq!(citing.len(), 2, "one Citation for each property");
    assert!(citing.iter().all(|(episode, _)| *episode == e));

    let done = w.retract_episode(e).await;

    assert_eq!(done.retracted, vec![version.id]);
    assert!(w.read(node, at(2024, 3, 1)).await.is_none());
    assert_eq!(
        w.caller()
            .list_node_statements(w.tenant, node)
            .await
            .unwrap()
            .len(),
        1,
        "one Retraction, not one for each Citation"
    );
}

#[tokio::test]
async fn as_of_reads_before_the_retraction_still_return_the_retracted_versions_and_nothing_is_deleted()
 {
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    let node = w.account(e, ValidTime::since(at(2024, 1, 1)), "gold").await;
    let version = w.read(node, at(2024, 3, 1)).await.unwrap();
    let before = version.recorded_from;
    let rows = w.rows_of(e).await;
    assert_eq!(rows.3, 1);

    let done = w.retract_episode(e).await;
    assert_eq!(done.retracted, vec![version.id]);

    let then = w
        .read_as_of(node, at(2024, 3, 1), before)
        .await
        .expect("the retracted Version is readable before the Retraction");
    assert_eq!(then.id, version.id);
    assert_eq!(then.values["plan"], "gold");
    assert_eq!(then.recorded_to, Some(done.recorded_at));
    assert!(w.read(node, at(2024, 3, 1)).await.is_none());

    // Retract Episode is not Erasure. Its Contributions, its Citations and
    // the Episode remain, and the Retraction is added to them.
    let after = w.rows_of(e).await;
    assert_eq!(after.0, rows.0 + 1, "one more Contribution: the null");
    assert_eq!(after.1, rows.1, "no Citation deleted");
    assert_eq!(after.2, rows.2 + 1, "the Retraction is a statement");
    assert_eq!(after.3, 1, "the Episode stays");
    assert_eq!(w.citing(version.id).await.len(), 1);
}

#[tokio::test]
async fn an_edge_version_cited_only_by_the_retracted_episode_is_retracted() {
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    let edge = w.edge(e, ValidTime::since(at(2024, 1, 1))).await;
    assert!(w.edge_holds(edge, at(2024, 6, 1)).await);
    let version = w
        .caller()
        .read_edge(w.tenant, edge, AsOf::current(at(2024, 6, 1)))
        .await
        .unwrap()
        .unwrap();
    let before = version.recorded_from;

    let done = w.retract_episode(e).await;

    assert!(
        done.retracted.contains(&version.id),
        "the Edge Version, retracted by a null for holds"
    );
    assert!(!w.edge_holds(edge, at(2024, 6, 1)).await);
    let then = w
        .caller()
        .read_edge(w.tenant, edge, AsOf::at(at(2024, 6, 1), before))
        .await
        .unwrap()
        .expect("readable before the Retraction");
    assert_eq!(then.id, version.id);
    let statements = w
        .caller()
        .list_edge_statements(w.tenant, edge)
        .await
        .unwrap();
    assert_eq!(statements.len(), 1);
    assert_eq!(statements[0].kind, StatementKind::Retraction);
    assert_eq!(statements[0].episode, e);
}

#[tokio::test]
async fn an_edge_version_another_episode_also_cites_stays_current() {
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    let edge = w.edge(e, ValidTime::since(at(2024, 1, 1))).await;
    let f = w.episode(at(2024, 2, 1), None).await;
    w.caller()
        .write_edge(
            w.tenant,
            edge,
            &Patch::new(f, ValidTime::since(at(2024, 1, 1)), Map::new()),
        )
        .await
        .unwrap();

    w.retract_episode(e).await;

    assert!(
        w.edge_holds(edge, at(2024, 6, 1)).await,
        "the second Episode also says it holds"
    );
}

#[tokio::test]
async fn retracting_an_episode_again_retracts_nothing_and_an_unknown_episode_is_refused() {
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    let node = w.account(e, ValidTime::since(at(2024, 1, 1)), "gold").await;
    assert_eq!(w.retract_episode(e).await.retracted.len(), 1);
    assert!(w.retract_episode(e).await.retracted.is_empty());
    assert_eq!(
        w.caller()
            .list_node_statements(w.tenant, node)
            .await
            .unwrap()
            .len(),
        1
    );

    let missing = w
        .caller()
        .retract_episode(w.tenant, EpisodeId(uuid::Uuid::nil()))
        .await;
    assert!(
        matches!(missing, Err(Error::InvalidWrite(_))),
        "{missing:?}"
    );
}

// The criteria #11 gives for Retract Episode, under ranked merge.

/// A ranked account whose plan a higher Authority states as gold since
/// January, a peer states as silver since January, and a later Episode of the
/// same lower Authority ends in April. The higher Authority hides both. This
/// returns the Node, the ending Episode and the higher Authority's.
async fn held_end(w: &World) -> (NodeId, EpisodeId, EpisodeId) {
    let high = w.episode(at(2024, 1, 1), Some(10)).await;
    let node = w
        .account(high, ValidTime::since(at(2024, 1, 1)), "gold")
        .await;
    let peer = w.episode(at(2024, 7, 1), Some(0)).await;
    w.write(
        node,
        peer,
        ValidTime::since(at(2024, 1, 1)),
        json!({ "plan": "silver" }),
    )
    .await;
    let low = w.episode(at(2024, 8, 1), Some(0)).await;
    let ended = w.end(node, low, at(2024, 4, 1)).await;
    assert_eq!(
        ended.status,
        WriteStatus::Unchanged,
        "the higher Authority hides it"
    );
    (node, low, high)
}

#[tokio::test]
async fn retract_episode_of_the_lower_authority_episode_removes_the_held_statement() {
    let w = World::new().await;
    w.set_rule("account", MergeRule::Ranked).await;
    let (node, low, high) = held_end(&w).await;
    let held_rows = w.rows_of(low).await;

    // Retract Episode of the Episode that holds the End of validity. It cites
    // no Version, so it retracts none.
    let done = w.retract_episode(low).await;
    assert!(done.retracted.is_empty());
    assert_eq!(
        w.plan(node, at(2024, 6, 1)).await.as_deref(),
        Some("gold"),
        "the higher Authority still decides"
    );

    // History stays: the statement and its Contributions are still rows, and
    // Retract Episode deleted none of them.
    let after = w.rows_of(low).await;
    assert_eq!(after, held_rows);
    assert_eq!(
        w.caller()
            .list_node_statements(w.tenant, node)
            .await
            .unwrap()
            .len(),
        1
    );

    // But the held statement no longer counts. Erasing the higher Authority's
    // rows, as the operator until Erasure exists, leaves no End of validity
    // to take effect: the peer's plan runs on.
    w.erase_rows_of(high).await;
    w.recompute(node).await;
    assert_eq!(
        w.plan(node, at(2024, 2, 1)).await.as_deref(),
        Some("silver")
    );
    assert_eq!(
        w.plan(node, at(2024, 6, 1)).await.as_deref(),
        Some("silver"),
        "the retracted Episode's End of validity did not take effect"
    );
}

#[tokio::test]
async fn without_retract_episode_the_same_held_statement_takes_effect() {
    // The control of the test above: the same sequence with no Retract
    // Episode ends the plan in April, so the test above shows the effect of
    // Retract Episode and not of the sequence.
    let w = World::new().await;
    w.set_rule("account", MergeRule::Ranked).await;
    let (node, _, high) = held_end(&w).await;
    w.erase_rows_of(high).await;
    w.recompute(node).await;
    assert_eq!(w.plan(node, at(2024, 6, 1)).await, None);
}

#[tokio::test]
async fn a_held_correction_of_the_retracted_episode_stops_counting() {
    let w = World::new().await;
    w.set_rule("account", MergeRule::Ranked).await;
    let high = w.episode(at(2024, 1, 1), Some(10)).await;
    let vt = ValidTime::between(at(2024, 1, 1), at(2024, 6, 1));
    let node = w.account(high, vt, "gold").await;
    let peer = w.episode(at(2024, 2, 1), Some(0)).await;
    w.write(node, peer, vt, json!({ "plan": "silver" })).await;
    let low = w.episode(at(2024, 8, 1), Some(0)).await;
    let corrected = w.correct(node, low, vt, "basic").await;
    assert_eq!(corrected.status, WriteStatus::Unchanged, "held");

    w.retract_episode(low).await;
    w.erase_rows_of(high).await;
    w.recompute(node).await;
    assert_eq!(
        w.plan(node, at(2024, 3, 1)).await.as_deref(),
        Some("silver"),
        "the held Correction did not take effect"
    );
}

// The criterion #9 gives: ranked merge applies to a Retraction made by
// Retract Episode.

#[tokio::test]
async fn with_ranked_merge_on_retracting_a_lower_authority_episode_leaves_the_version_a_higher_authority_supplied()
 {
    let w = World::new().await;
    w.set_rule("account", MergeRule::Ranked).await;
    let low = w.episode(at(2024, 9, 1), Some(0)).await;
    let node = w
        .account(
            low,
            ValidTime::between(at(2024, 1, 1), at(2024, 12, 1)),
            "silver",
        )
        .await;
    // A higher Authority, earlier in Reference time, states another plan
    // over the middle, which wins there although it is older.
    let high = w.episode(at(2024, 2, 1), Some(10)).await;
    w.write(
        node,
        high,
        ValidTime::between(at(2024, 4, 1), at(2024, 8, 1)),
        json!({ "plan": "gold" }),
    )
    .await;
    let early = w.read(node, at(2024, 2, 1)).await.unwrap();
    let middle = w.read(node, at(2024, 6, 1)).await.unwrap();
    let late = w.read(node, at(2024, 10, 1)).await.unwrap();
    assert_eq!(middle.values["plan"], "gold");
    assert_eq!(w.citing(middle.id).await, vec![(high, "plan".to_string())]);

    let done = w.retract_episode(low).await;

    let mut retracted = done.retracted.clone();
    retracted.sort_by_key(|v| v.0);
    let mut expected = vec![early.id, late.id];
    expected.sort_by_key(|v| v.0);
    assert_eq!(
        retracted, expected,
        "only the Versions the lower Authority alone cites"
    );
    let still = w.read(node, at(2024, 6, 1)).await.unwrap();
    assert_eq!(
        still.id, middle.id,
        "the higher Authority's Version is untouched"
    );
    assert_eq!(still.values["plan"], "gold");
    assert!(still.recorded_to.is_none());
    assert_eq!(w.plan(node, at(2024, 2, 1)).await, None);
    assert_eq!(w.plan(node, at(2024, 10, 1)).await, None);
}
