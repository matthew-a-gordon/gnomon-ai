//! Seam 1, Correction, End of validity and Retraction: each is a statement of
//! the Episode that makes it, held like a Contribution, and the earlier
//! Version stays readable As-of an earlier known-at instant, through the Rust
//! core's Store API against real Postgres (ADR-0007, ADR-0008). The
//! adversarial cases live in sql_boundary.rs.

mod support;

use chrono::{DateTime, TimeZone, Utc};
use gnomon::{
    AsOf, Bound, EdgeId, EndOfValidity, EpisodeId, Error, KindOf, MergeRule, NewEdge, NewEpisode,
    NewKind, NewNode, NodeId, NodeVersion, Patch, Retraction, Scope, StatementKind, Store,
    TenantId, ValidTime, WriteOutcome, WriteStatus,
};
use serde_json::{Map, Value, json};
use support::TestDb;
use tokio_postgres::types::Type;

const SCOPE: &str = "support";

fn at(y: i32, m: u32, d: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, m, d, 0, 0, 0).unwrap()
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

    async fn episode(&self, reference_time: DateTime<Utc>, authority: Option<i32>) -> EpisodeId {
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

    async fn read(&self, node: NodeId, valid_at: DateTime<Utc>) -> Option<NodeVersion> {
        self.caller()
            .read_node(self.tenant, node, AsOf::current(valid_at))
            .await
            .unwrap()
    }

    async fn read_as_of(
        &self,
        node: NodeId,
        valid_at: DateTime<Utc>,
        known_at: DateTime<Utc>,
    ) -> Option<NodeVersion> {
        self.caller()
            .read_node(self.tenant, node, AsOf::at(valid_at, known_at))
            .await
            .unwrap()
    }

    async fn plan(&self, node: NodeId, valid_at: DateTime<Utc>) -> Option<String> {
        self.read(node, valid_at).await.and_then(|v| {
            v.values
                .get("plan")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
    }

    async fn edge_holds(&self, edge: EdgeId, valid_at: DateTime<Utc>) -> bool {
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

    async fn end(&self, node: NodeId, episode: EpisodeId, when: DateTime<Utc>) -> WriteOutcome {
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

    async fn retract(
        &self,
        node: NodeId,
        episode: EpisodeId,
        version: gnomon::VersionId,
    ) -> WriteOutcome {
        self.caller()
            .retract_node_version(self.tenant, node, &Retraction { episode, version })
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

    async fn count(&self, sql: &str, id: uuid::Uuid) -> i64 {
        self.db
            .admin()
            .await
            .query_typed(sql, &[(&id, Type::UUID)])
            .await
            .unwrap()[0]
            .get(0)
    }
}

#[tokio::test]
async fn a_correction_replaces_the_earlier_version_for_the_same_valid_time_and_it_stays_readable_before()
 {
    let w = World::new().await;
    let first = w.episode(at(2024, 1, 1), None).await;
    let vt = ValidTime::between(at(2024, 1, 1), at(2024, 6, 1));
    let node = w.account(first, vt, "gold").await;
    let earlier = w.read(node, at(2024, 3, 1)).await.unwrap();
    let before = earlier.recorded_from;

    let second = w.episode(at(2024, 2, 1), None).await;
    let outcome = w.correct(node, second, vt, "silver").await;
    assert_eq!(outcome.status, WriteStatus::Written);
    assert_eq!(outcome.opened.len(), 1);

    let now = w.read(node, at(2024, 3, 1)).await.unwrap();
    assert_eq!(now.values["plan"], "silver");
    assert_eq!(now.valid_time, earlier.valid_time, "the same Valid time");
    assert_ne!(now.id, earlier.id);

    // Read As-of before the Correction: the earlier Version, now closed.
    let then = w
        .read_as_of(node, at(2024, 3, 1), before)
        .await
        .expect("the earlier Version is still readable");
    assert_eq!(then.id, earlier.id);
    assert_eq!(then.values["plan"], "gold");
    assert_eq!(then.recorded_to, Some(outcome.recorded_at));
    assert_eq!(
        then.recorded_from, before,
        "its Recorded time is not rewritten"
    );
}

#[tokio::test]
async fn an_end_of_validity_closes_what_the_version_stated_at_the_stated_valid_time_and_it_stays_current_before()
 {
    let w = World::new().await;
    let first = w.episode(at(2024, 1, 1), None).await;
    let node = w
        .account(first, ValidTime::since(at(2024, 1, 1)), "gold")
        .await;
    let earlier = w.read(node, at(2024, 3, 1)).await.unwrap();

    let ended = w.episode(at(2024, 5, 1), None).await;
    let outcome = w.end(node, ended, at(2024, 4, 1)).await;
    assert_eq!(outcome.status, WriteStatus::Written);

    // Current As-of an earlier valid-at instant, not from the stated one.
    let now = w.read(node, at(2024, 3, 1)).await.unwrap();
    assert_eq!(now.values["plan"], "gold");
    assert_eq!(now.valid_time.to, Bound::At(at(2024, 4, 1)));
    assert_eq!(w.plan(node, at(2024, 4, 1)).await, None);
    assert_eq!(w.plan(node, at(2024, 9, 1)).await, None);

    // As-of before the End of validity the Version had no end.
    let then = w
        .read_as_of(node, at(2024, 9, 1), earlier.recorded_from)
        .await
        .expect("before the statement the Version still held in September");
    assert_eq!(then.id, earlier.id);
    assert_eq!(then.values["plan"], "gold");
}

#[tokio::test]
async fn a_retraction_stops_the_version_being_current_and_it_stays_readable_before() {
    let w = World::new().await;
    let first = w.episode(at(2024, 1, 1), None).await;
    let node = w
        .account(
            first,
            ValidTime::between(at(2024, 1, 1), at(2024, 6, 1)),
            "gold",
        )
        .await;
    let version = w.read(node, at(2024, 3, 1)).await.unwrap();

    let later = w.episode(at(2024, 7, 1), None).await;
    let outcome = w.retract(node, later, version.id).await;
    assert_eq!(outcome.status, WriteStatus::Written);
    assert!(
        outcome.opened.is_empty(),
        "a Retraction opens no Version: nothing is left to hold"
    );

    for day in [at(2024, 1, 1), at(2024, 3, 1), at(2024, 5, 31)] {
        assert_eq!(w.read(node, day).await, None, "never true, at any valid-at");
    }
    let then = w
        .read_as_of(node, at(2024, 3, 1), version.recorded_from)
        .await
        .expect("the retracted Version stays readable before the Retraction");
    assert_eq!(then.id, version.id);
    assert_eq!(then.values["plan"], "gold");
    assert_eq!(then.recorded_to, Some(outcome.recorded_at));
}

#[tokio::test]
async fn each_of_the_three_records_the_episode_that_made_it() {
    let w = World::new().await;
    let first = w.episode(at(2024, 1, 1), None).await;
    let vt = ValidTime::since(at(2024, 1, 1));
    let node = w.account(first, vt, "gold").await;

    let correcting = w.episode(at(2024, 2, 1), None).await;
    let corrected = w.correct(node, correcting, vt, "silver").await;
    let ending = w.episode(at(2024, 3, 1), None).await;
    w.end(node, ending, at(2024, 6, 1)).await;
    let version = w.read(node, at(2024, 4, 1)).await.unwrap();
    let retracting = w.episode(at(2024, 4, 1), None).await;
    w.retract(node, retracting, version.id).await;

    let held = w
        .caller()
        .list_node_statements(w.tenant, node)
        .await
        .unwrap();
    let summary: Vec<_> = held.iter().map(|s| (s.kind, s.episode)).collect();
    assert_eq!(
        summary,
        vec![
            (StatementKind::Correction, correcting),
            (StatementKind::EndOfValidity, ending),
            (StatementKind::Retraction, retracting),
        ]
    );
    assert_eq!(held[0].version, None, "a Correction names no ended Version");
    assert!(held[1].version.is_some());
    assert_eq!(held[2].version, Some(version.id));
    assert_eq!(held[1].valid_time.from, Bound::At(at(2024, 6, 1)));
    assert_eq!(held[2].valid_time.to, Bound::At(at(2024, 6, 1)));

    // The Correction's Version also cites its Episode, as any write's does.
    let cited = w
        .caller()
        .list_citations(w.tenant, corrected.opened[0])
        .await
        .unwrap();
    assert!(
        cited
            .iter()
            .any(|c| c.episode == correcting && c.property == "plan")
    );
}

#[tokio::test]
async fn an_end_of_validity_of_an_edge_leaves_it_holding_no_version_whatever_values_remain() {
    let w = World::new().await;
    let first = w.episode(at(2024, 1, 1), None).await;
    let edge = w.edge(first, ValidTime::since(at(2024, 1, 1))).await;
    let earlier = w
        .caller()
        .read_edge(w.tenant, edge, AsOf::current(at(2024, 2, 1)))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(earlier.values["share"], 40);

    let ending = w.episode(at(2024, 5, 1), None).await;
    let outcome = w
        .caller()
        .end_edge_validity(
            w.tenant,
            edge,
            &EndOfValidity {
                episode: ending,
                at: Bound::At(at(2024, 4, 1)),
            },
        )
        .await
        .unwrap();
    assert_eq!(outcome.status, WriteStatus::Written);

    assert!(w.edge_holds(edge, at(2024, 3, 1)).await);
    assert!(!w.edge_holds(edge, at(2024, 4, 1)).await);
    assert!(!w.edge_holds(edge, at(2025, 1, 1)).await);
    // The Valid time of the earlier Version is closed, not rewritten.
    let still = w
        .caller()
        .read_edge(
            w.tenant,
            edge,
            AsOf::at(at(2024, 9, 1), earlier.recorded_from),
        )
        .await
        .unwrap()
        .expect("as-of before the statement the Edge held in September");
    assert_eq!(still.id, earlier.id);

    let held = w
        .caller()
        .list_edge_statements(w.tenant, edge)
        .await
        .unwrap();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].kind, StatementKind::EndOfValidity);
    assert_eq!(held[0].episode, ending);
}

#[tokio::test]
async fn with_ranked_merge_on_a_lower_authority_statement_does_not_end_a_higher_authority_version()
{
    let w = World::new().await;
    w.set_rule("account", MergeRule::Ranked).await;
    let high = w.episode(at(2024, 1, 1), Some(10)).await;
    let vt = ValidTime::between(at(2024, 1, 1), at(2024, 6, 1));
    let node = w.account(high, vt, "gold").await;
    let version = w.read(node, at(2024, 3, 1)).await.unwrap();

    // A later, lower-Authority Episode corrects, ends and retracts.
    let low = w.episode(at(2024, 8, 1), Some(0)).await;
    let corrected = w.correct(node, low, vt, "basic").await;
    assert_eq!(corrected.status, WriteStatus::Unchanged);
    let ended = w.end(node, low, at(2024, 3, 1)).await;
    assert_eq!(ended.status, WriteStatus::Unchanged);
    let retracted = w.retract(node, low, version.id).await;
    assert_eq!(retracted.status, WriteStatus::Unchanged);

    let now = w.read(node, at(2024, 4, 1)).await.unwrap();
    assert_eq!(
        now.id, version.id,
        "the Version is the one the higher Authority gave"
    );
    assert_eq!(now.values["plan"], "gold");
    assert_eq!(now.valid_time.to, Bound::At(at(2024, 6, 1)));
    assert!(now.recorded_to.is_none());

    // All three stay held, and an equal Authority would have won.
    let held = w
        .caller()
        .list_node_statements(w.tenant, node)
        .await
        .unwrap();
    assert_eq!(held.len(), 3);
    assert!(held.iter().all(|s| s.episode == low));
}

#[tokio::test]
async fn with_ranked_merge_on_a_lower_authority_end_of_validity_of_an_edge_loses_where_a_higher_authority_says_it_holds()
 {
    let w = World::new().await;
    w.set_rule("owns", MergeRule::Ranked).await;
    let high = w.episode(at(2024, 1, 1), Some(10)).await;
    let edge = w.edge(high, ValidTime::since(at(2024, 1, 1))).await;

    let low = w.episode(at(2024, 8, 1), Some(0)).await;
    let outcome = w
        .caller()
        .end_edge_validity(
            w.tenant,
            edge,
            &EndOfValidity {
                episode: low,
                at: Bound::At(at(2024, 4, 1)),
            },
        )
        .await
        .unwrap();
    assert_eq!(outcome.status, WriteStatus::Unchanged);
    assert!(
        w.edge_holds(edge, at(2024, 6, 1)).await,
        "the higher Authority says it holds"
    );

    // Erasing the higher-Authority Episode's rows lets the held statement
    // take effect. A peer of the lower Authority, dated before it, had stated
    // that the Edge holds from January on, so only the End of validity ends
    // it: the Edge then holds until April.
    let peer = w.episode(at(2024, 7, 1), Some(0)).await;
    w.caller()
        .write_edge(
            w.tenant,
            edge,
            &Patch::new(peer, ValidTime::since(at(2024, 1, 1)), Map::new()),
        )
        .await
        .unwrap();
    assert!(
        w.edge_holds(edge, at(2024, 6, 1)).await,
        "the higher Authority still wins"
    );
    w.erase_rows_of(high).await;
    let again = w.episode(at(2024, 9, 1), Some(0)).await;
    w.caller()
        .write_edge(
            w.tenant,
            edge,
            &Patch::new(
                again,
                ValidTime::between(at(2024, 1, 1), at(2024, 1, 2)),
                Map::new(),
            ),
        )
        .await
        .unwrap();
    assert!(w.edge_holds(edge, at(2024, 1, 15)).await);
    assert!(
        !w.edge_holds(edge, at(2024, 6, 1)).await,
        "the held End of validity took effect"
    );
}

#[tokio::test]
async fn a_held_statement_is_found_by_the_episodes_rows_and_takes_effect_when_the_higher_authority_episode_is_erased()
 {
    let w = World::new().await;
    w.set_rule("account", MergeRule::Ranked).await;
    let (node, low, _) = held_end(&w).await;

    // What Retract Episode of `low` must delete: a statement row and the
    // Contributions that carry its id, all keyed by the Episode.
    assert_eq!(
        w.count(
            "select count(*) from gnomon.statement where episode_id = $1",
            low.0
        )
        .await,
        1
    );
    assert_eq!(
        w.count(
            "select count(*) from gnomon.contribution
             where episode_id = $1 and statement_id is not null",
            low.0
        )
        .await,
        1,
        "the End of validity of the Node's one property"
    );
    assert_eq!(w.plan(node, at(2024, 6, 1)).await.as_deref(), Some("gold"));
}

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
async fn deleting_the_higher_authority_episodes_rows_lets_a_held_statement_take_effect() {
    let w = World::new().await;
    w.set_rule("account", MergeRule::Ranked).await;
    let (node, _, high) = held_end(&w).await;

    w.erase_rows_of(high).await;
    w.recompute(node).await;
    assert_eq!(
        w.plan(node, at(2024, 2, 1)).await.as_deref(),
        Some("silver")
    );
    assert_eq!(
        w.plan(node, at(2024, 6, 1)).await,
        None,
        "the held End of validity now ends the peer's plan in April"
    );
}

#[tokio::test]
async fn deleting_the_lower_authority_episodes_rows_removes_the_held_statement() {
    let w = World::new().await;
    w.set_rule("account", MergeRule::Ranked).await;
    let (node, low, high) = held_end(&w).await;

    // Delete what the Episode owns, as Retract Episode will.
    w.erase_rows_of(low).await;
    assert!(
        w.caller()
            .list_node_statements(w.tenant, node)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        w.count(
            "select count(*) from gnomon.contribution where episode_id = $1",
            low.0
        )
        .await,
        0
    );

    // Erasing the higher Episode now leaves no End of validity to take
    // effect: the peer's plan runs on.
    w.erase_rows_of(high).await;
    w.recompute(node).await;
    assert_eq!(
        w.plan(node, at(2024, 6, 1)).await.as_deref(),
        Some("silver")
    );
}

#[tokio::test]
async fn a_statement_with_nothing_to_act_on_is_refused() {
    let w = World::new().await;
    let first = w.episode(at(2024, 1, 1), None).await;
    let node = w
        .account(
            first,
            ValidTime::between(at(2024, 1, 1), at(2024, 6, 1)),
            "gold",
        )
        .await;
    let version = w.read(node, at(2024, 3, 1)).await.unwrap();
    let ep = w.episode(at(2024, 7, 1), None).await;

    // A Correction of a Valid time no current Version covers.
    let e = w
        .caller()
        .correct_node(
            w.tenant,
            node,
            &Patch::new(
                ep,
                ValidTime::between(at(2025, 1, 1), at(2025, 2, 1)),
                values(json!({ "plan": "x" })),
            ),
        )
        .await
        .unwrap_err();
    assert!(matches!(e, Error::InvalidWrite(m) if m.contains("to correct")));

    // An End of validity at an instant no current Version spans, or at its start.
    for when in [at(2024, 7, 1), at(2024, 1, 1)] {
        let e = w
            .caller()
            .end_node_validity(
                w.tenant,
                node,
                &EndOfValidity {
                    episode: ep,
                    at: Bound::At(when),
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(e, Error::InvalidWrite(_)), "{when}: {e}");
    }

    // A Retraction of a Version that is no longer current.
    w.retract(node, ep, version.id).await;
    let e = w
        .caller()
        .retract_node_version(
            w.tenant,
            node,
            &Retraction {
                episode: ep,
                version: version.id,
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(e, Error::InvalidWrite(m) if m.contains("not a current Version")));
}
