//! Seam 1, ranked merge and Authority: a Kind turns ranked merge on, and a
//! Version's values are then chosen by Authority, then Reference time, then
//! Recorded time, through the Rust core's Store API against real Postgres
//! (ADR-0007). The adversarial cases live in sql_boundary.rs.

mod support;

use gnomon::{
    AsOf, Bound, BoundChange, BoundSide, BoundStatement, EpisodeId, Error, KindOf, MergeRule,
    NewEpisode, NewKind, NewNode, NodeId, NodeVersion, Patch, Precision, Scope, Store, TenantId,
    ValidTime, VersionId,
};
use jiff::Timestamp;
use serde_json::{Map, Value, json};
use support::TestDb;

const SCOPE: &str = "support";

fn at(y: i16, m: i8, d: i8) -> Timestamp {
    support::utc(y, m, d)
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
    /// A Tenant with the Node Kind `account`, which declares `plan`, and the
    /// Kind `team_account` below it. Neither ranks yet.
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
        w.register("account", None, json!({ "plan": { "type": "string" } }))
            .await;
        w.register(
            "team_account",
            Some("account"),
            json!({ "seats": { "type": "integer" } }),
        )
        .await;
        w
    }

    async fn register(&self, name: &str, parent: Option<&str>, properties: Value) {
        self.caller()
            .register_kind(&NewKind {
                tenant: Some(self.tenant),
                name: name.to_owned(),
                of: KindOf::Node,
                parent: parent.map(str::to_owned),
                properties: json!({ "type": "object", "properties": properties }),
                search: vec![],
            })
            .await
            .unwrap();
    }

    fn caller(&self) -> gnomon::Caller<'_> {
        self.store.as_worker(self.tenant, Scope::new(SCOPE), None)
    }

    async fn set_rule(&self, kind: &str, rule: MergeRule) -> Result<(), Error> {
        self.caller().set_merge_rule(self.tenant, kind, rule).await
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

    async fn create(&self, kind: &str, episode: EpisodeId, vt: ValidTime, v: Value) -> NodeId {
        self.caller()
            .create_node(
                &NewNode {
                    tenant: Some(self.tenant),
                    scope: Scope::new(SCOPE),
                    kind: kind.to_owned(),
                },
                &Patch::new(episode, vt, values(v)),
            )
            .await
            .unwrap()
            .0
    }

    async fn write(&self, node: NodeId, episode: EpisodeId, vt: ValidTime, v: Value) {
        self.caller()
            .write_node(self.tenant, node, &Patch::new(episode, vt, values(v)))
            .await
            .unwrap();
    }

    async fn read(&self, node: NodeId, valid_at: Timestamp) -> Option<NodeVersion> {
        self.caller()
            .read_node(self.tenant, node, AsOf::current(valid_at))
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

    /// How many Contributions the Node holds, which a losing one stays among.
    async fn contributions(&self, node: NodeId) -> i64 {
        self.db
            .admin()
            .await
            .query_typed(
                "select count(*) from gnomon.contribution where node_id = $1",
                &[(&node.0, tokio_postgres::types::Type::UUID)],
            )
            .await
            .unwrap()[0]
            .get(0)
    }

    /// Deletes an Episode's Contributions and bound statements as the
    /// operator, and leaves the Episode. The Node's next write then
    /// recomputes its current Versions from what remains. These tests
    /// predate Erasure (erase_episode.rs), which also rewrites the Node's
    /// closed history.
    async fn erase_rows_of(&self, episode: EpisodeId) {
        let admin = self.db.admin().await;
        for sql in [
            "delete from gnomon.bound_statement where episode_id = $1",
            "delete from gnomon.contribution where episode_id = $1",
        ] {
            admin
                .execute_typed(sql, &[(&episode.0, tokio_postgres::types::Type::UUID)])
                .await
                .unwrap();
        }
    }
}

#[tokio::test]
async fn a_higher_authority_contribution_wins_over_a_later_lower_authority_one_where_valid_times_overlap()
 {
    let w = World::new().await;
    w.set_rule("account", MergeRule::Ranked).await.unwrap();
    let high = w.episode(at(2024, 1, 1), Some(10)).await;
    let low = w.episode(at(2024, 6, 1), Some(0)).await;
    let from = ValidTime::since(at(2024, 1, 1));

    let node = w
        .create("account", high, from, json!({ "plan": "gold" }))
        .await;
    w.write(node, low, from, json!({ "plan": "basic" })).await;

    let v = w.read(node, at(2024, 7, 1)).await.unwrap();
    assert_eq!(v.values["plan"], "gold");
    assert_eq!(v.merge_rule, MergeRule::Ranked);
}

#[tokio::test]
async fn where_valid_times_do_not_overlap_each_period_keeps_its_own_winner_whatever_its_authority()
{
    let w = World::new().await;
    w.set_rule("account", MergeRule::Ranked).await.unwrap();
    let high = w.episode(at(2024, 1, 1), Some(10)).await;
    let low = w.episode(at(2024, 6, 1), Some(0)).await;

    // The higher Authority holds January and February, the lower one March on.
    let node = w
        .create(
            "account",
            high,
            ValidTime::between(at(2024, 1, 1), at(2024, 3, 1)),
            json!({ "plan": "gold" }),
        )
        .await;
    w.write(
        node,
        low,
        ValidTime::since(at(2024, 3, 1)),
        json!({ "plan": "basic" }),
    )
    .await;
    assert_eq!(w.plan(node, at(2024, 2, 15)).await.as_deref(), Some("gold"));
    assert_eq!(w.plan(node, at(2024, 4, 1)).await.as_deref(), Some("basic"));

    // A lower Authority that overlaps only part of the higher one's period
    // wins the rest.
    let node = w
        .create(
            "account",
            high,
            ValidTime::between(at(2024, 1, 1), at(2024, 3, 1)),
            json!({ "plan": "gold" }),
        )
        .await;
    w.write(
        node,
        low,
        ValidTime::since(at(2024, 2, 1)),
        json!({ "plan": "basic" }),
    )
    .await;
    assert_eq!(w.plan(node, at(2024, 2, 15)).await.as_deref(), Some("gold"));
    assert_eq!(w.plan(node, at(2024, 4, 1)).await.as_deref(), Some("basic"));
    assert_eq!(
        w.contributions(node).await,
        2,
        "a Contribution a period loses stays held"
    );
}

#[tokio::test]
async fn without_ranked_merge_the_later_reference_time_wins_whatever_the_authority() {
    let w = World::new().await;
    let high = w.episode(at(2024, 1, 1), Some(10)).await;
    let low = w.episode(at(2024, 6, 1), Some(0)).await;
    let from = ValidTime::since(at(2024, 1, 1));
    let node = w
        .create("account", high, from, json!({ "plan": "gold" }))
        .await;
    w.write(node, low, from, json!({ "plan": "basic" })).await;

    let v = w.read(node, at(2024, 7, 1)).await.unwrap();
    assert_eq!(v.values["plan"], "basic");
    assert_eq!(v.merge_rule, MergeRule::Recency);
}

#[tokio::test]
async fn equal_authority_falls_back_to_reference_time_then_recorded_time_and_none_ranks_as_0() {
    let w = World::new().await;
    w.set_rule("account", MergeRule::Ranked).await.unwrap();
    let from = ValidTime::since(at(2024, 1, 1));

    // Equal Authority: the later Reference time wins, though it was recorded first.
    let newer = w.episode(at(2024, 6, 1), Some(5)).await;
    let older = w.episode(at(2024, 2, 1), Some(5)).await;
    let node = w
        .create("account", newer, from, json!({ "plan": "from_june" }))
        .await;
    w.write(node, older, from, json!({ "plan": "from_february" }))
        .await;
    assert_eq!(
        w.plan(node, at(2024, 7, 1)).await.as_deref(),
        Some("from_june")
    );

    // The same Reference time: the Contribution recorded later wins.
    let first = w.episode(at(2024, 3, 1), Some(5)).await;
    let second = w.episode(at(2024, 3, 1), Some(5)).await;
    let node = w
        .create("account", first, from, json!({ "plan": "first" }))
        .await;
    w.write(node, second, from, json!({ "plan": "second" }))
        .await;
    assert_eq!(
        w.plan(node, at(2024, 7, 1)).await.as_deref(),
        Some("second")
    );

    // An Episode with no Authority ranks as 0: above -1, below 1, and level
    // with 0, where the later Reference time decides.
    let none_early = w.episode(at(2024, 1, 1), None).await;
    let minus = w.episode(at(2024, 9, 1), Some(-1)).await;
    let node = w
        .create("account", none_early, from, json!({ "plan": "none" }))
        .await;
    w.write(node, minus, from, json!({ "plan": "minus_one" }))
        .await;
    assert_eq!(w.plan(node, at(2024, 10, 1)).await.as_deref(), Some("none"));
    let one = w.episode(at(2024, 1, 2), Some(1)).await;
    w.write(node, one, from, json!({ "plan": "one" })).await;
    assert_eq!(w.plan(node, at(2024, 10, 1)).await.as_deref(), Some("one"));
    let zero_late = w.episode(at(2024, 12, 1), Some(0)).await;
    let node = w
        .create("account", none_early, from, json!({ "plan": "none" }))
        .await;
    w.write(node, zero_late, from, json!({ "plan": "zero" }))
        .await;
    assert_eq!(w.plan(node, at(2025, 1, 1)).await.as_deref(), Some("zero"));
}

#[tokio::test]
async fn a_child_kind_may_turn_ranked_merge_on_and_is_refused_when_it_tries_to_turn_it_off() {
    let w = World::new().await;
    assert_eq!(
        w.caller()
            .merge_rule(w.tenant, "team_account")
            .await
            .unwrap(),
        MergeRule::Recency
    );

    // A child Kind turns it on while its parent does not rank.
    w.set_rule("team_account", MergeRule::Ranked).await.unwrap();
    assert_eq!(
        w.caller()
            .merge_rule(w.tenant, "team_account")
            .await
            .unwrap(),
        MergeRule::Ranked
    );
    assert_eq!(
        w.caller().merge_rule(w.tenant, "account").await.unwrap(),
        MergeRule::Recency
    );
    // It may turn it off again while no ancestor ranks, and on once more.
    w.set_rule("team_account", MergeRule::Recency)
        .await
        .unwrap();
    w.set_rule("team_account", MergeRule::Ranked).await.unwrap();

    // The parent ranks, so the child inherits it and cannot turn it off.
    w.set_rule("account", MergeRule::Ranked).await.unwrap();
    for own in [MergeRule::Recency] {
        match w.set_rule("team_account", own).await {
            Err(Error::InvalidKind(m)) => {
                assert!(m.contains("cannot turn ranked merge off"), "{m}")
            }
            other => panic!("turning ranked merge off was not refused: {other:?}"),
        }
    }
    assert_eq!(
        w.caller()
            .merge_rule(w.tenant, "team_account")
            .await
            .unwrap(),
        MergeRule::Ranked
    );

    // A child registered below a Kind that ranks inherits the rule, and so does a grandchild.
    w.register(
        "solo_account",
        Some("account"),
        json!({ "owner": { "type": "string" } }),
    )
    .await;
    w.register(
        "trial_account",
        Some("solo_account"),
        json!({ "days": { "type": "integer" } }),
    )
    .await;
    for kind in ["solo_account", "trial_account"] {
        assert_eq!(
            w.caller().merge_rule(w.tenant, kind).await.unwrap(),
            MergeRule::Ranked,
            "{kind}"
        );
    }
    match w.set_rule("trial_account", MergeRule::Recency).await {
        Err(Error::InvalidKind(m)) => assert!(m.contains(r#"its ancestor "account" ranks"#), "{m}"),
        other => panic!("turning ranked merge off was not refused: {other:?}"),
    }
    // The inheriting child ranks a write.
    let high = w.episode(at(2024, 1, 1), Some(10)).await;
    let low = w.episode(at(2024, 6, 1), Some(0)).await;
    let from = ValidTime::since(at(2024, 1, 1));
    let node = w
        .create("trial_account", high, from, json!({ "plan": "gold" }))
        .await;
    w.write(node, low, from, json!({ "plan": "basic" })).await;
    assert_eq!(w.plan(node, at(2024, 7, 1)).await.as_deref(), Some("gold"));

    // A root Kind has no rule of its own, and an unregistered Kind has none to set.
    for kind in ["node", "ghost"] {
        assert!(
            matches!(
                w.set_rule(kind, MergeRule::Ranked).await,
                Err(Error::InvalidKind(_))
            ),
            "{kind}"
        );
    }
}

#[tokio::test]
async fn each_version_records_the_rule_that_chose_it_and_a_rule_change_leaves_existing_versions_unchanged()
 {
    let w = World::new().await;
    let a = w.episode(at(2024, 1, 1), Some(0)).await;
    let b = w.episode(at(2024, 2, 1), Some(0)).await;
    let jan = ValidTime::between(at(2024, 1, 1), at(2024, 2, 1));
    let node = w.create("account", a, jan, json!({ "plan": "gold" })).await;
    let before = w.read(node, at(2024, 1, 15)).await.unwrap();
    assert_eq!(before.merge_rule, MergeRule::Recency);

    // The change itself touches no Version: it is the same row, same
    // Recorded time, same open belief.
    w.set_rule("account", MergeRule::Ranked).await.unwrap();
    assert_eq!(w.read(node, at(2024, 1, 15)).await.unwrap(), before);

    // A later write to another period records the new rule, and the earlier
    // Version keeps the old one.
    w.write(
        node,
        b,
        ValidTime::between(at(2024, 3, 1), at(2024, 4, 1)),
        json!({ "plan": "basic" }),
    )
    .await;
    assert_eq!(w.read(node, at(2024, 1, 15)).await.unwrap(), before);
    assert_eq!(
        w.read(node, at(2024, 3, 15)).await.unwrap().merge_rule,
        MergeRule::Ranked
    );

    // Turning the rule off again leaves both as they were, and the next
    // write records recency.
    w.set_rule("account", MergeRule::Recency).await.unwrap();
    assert_eq!(w.read(node, at(2024, 1, 15)).await.unwrap(), before);
    assert_eq!(
        w.read(node, at(2024, 3, 15)).await.unwrap().merge_rule,
        MergeRule::Ranked
    );
    w.write(
        node,
        b,
        ValidTime::between(at(2024, 5, 1), at(2024, 6, 1)),
        json!({ "plan": "free" }),
    )
    .await;
    assert_eq!(
        w.read(node, at(2024, 5, 15)).await.unwrap().merge_rule,
        MergeRule::Recency
    );
    assert_eq!(
        w.read(node, at(2024, 3, 15)).await.unwrap().merge_rule,
        MergeRule::Ranked
    );
}

#[tokio::test]
async fn a_lower_authority_end_of_validity_loses_to_the_values_a_higher_authority_supplied_and_stays_held()
 {
    let w = World::new().await;
    w.set_rule("account", MergeRule::Ranked).await.unwrap();
    let high = w.episode(at(2024, 1, 1), Some(10)).await;
    let low = w.episode(at(2024, 6, 1), Some(0)).await;
    let node = w
        .create(
            "account",
            high,
            ValidTime::since(at(2024, 1, 1)),
            json!({ "plan": "gold" }),
        )
        .await;

    // The lower Authority says the plan ended in March. Where their Valid
    // times overlap, the higher Authority's value stands.
    w.write(
        node,
        low,
        ValidTime::since(at(2024, 3, 1)),
        json!({ "plan": null }),
    )
    .await;
    assert_eq!(w.plan(node, at(2024, 5, 1)).await.as_deref(), Some("gold"));
    assert_eq!(w.plan(node, at(2024, 2, 1)).await.as_deref(), Some("gold"));
    assert_eq!(
        w.contributions(node).await,
        2,
        "the losing statement is held"
    );

    // The statement takes effect once the higher-Authority Episode is gone.
    w.erase_rows_of(high).await;
    let other = w.episode(at(2024, 7, 1), Some(0)).await;
    w.write(
        node,
        other,
        ValidTime::since(at(2024, 1, 1)),
        json!({ "name": "Acme" }),
    )
    .await;
    assert_eq!(
        w.plan(node, at(2024, 5, 1)).await,
        None,
        "the held End of validity now holds"
    );
}

#[tokio::test]
async fn a_higher_authority_end_of_validity_wins_over_a_later_lower_authority_value() {
    let w = World::new().await;
    w.set_rule("account", MergeRule::Ranked).await.unwrap();
    let low = w.episode(at(2024, 1, 1), Some(0)).await;
    let high = w.episode(at(2024, 2, 1), Some(10)).await;
    let later_low = w.episode(at(2024, 9, 1), Some(0)).await;
    let node = w
        .create(
            "account",
            low,
            ValidTime::since(at(2024, 1, 1)),
            json!({ "plan": "gold" }),
        )
        .await;
    w.write(
        node,
        high,
        ValidTime::since(at(2024, 3, 1)),
        json!({ "plan": null }),
    )
    .await;
    assert_eq!(w.plan(node, at(2024, 2, 1)).await.as_deref(), Some("gold"));
    assert_eq!(w.plan(node, at(2024, 5, 1)).await, None);

    // A later, lower-Authority value does not undo the End of validity.
    w.write(
        node,
        later_low,
        ValidTime::since(at(2024, 1, 1)),
        json!({ "plan": "basic" }),
    )
    .await;
    assert_eq!(w.plan(node, at(2024, 5, 1)).await, None);
    assert_eq!(w.plan(node, at(2024, 2, 1)).await.as_deref(), Some("basic"));
}

/// An account whose plan the higher Authority stated over [Jan, Apr), so its
/// one Version ends on 1 April.
async fn account_to_april(w: &World, authority: i32) -> (NodeId, VersionId, EpisodeId) {
    let first = w.episode(at(2024, 1, 1), Some(authority)).await;
    let node = w
        .create(
            "account",
            first,
            ValidTime::between(at(2024, 1, 1), at(2024, 4, 1)),
            json!({ "plan": "gold" }),
        )
        .await;
    let v = w.read(node, at(2024, 2, 1)).await.unwrap();
    (node, v.id, first)
}

async fn state_end(
    w: &World,
    node: NodeId,
    version: VersionId,
    episode: EpisodeId,
    to: Timestamp,
) -> gnomon::BoundOutcome {
    w.caller()
        .state_bound(
            w.tenant,
            node,
            &BoundStatement {
                episode,
                version,
                side: BoundSide::End,
                bound: Bound::At(to),
            },
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn a_bound_statement_from_a_lower_authority_episode_loses_and_stays_held() {
    let w = World::new().await;
    w.set_rule("account", MergeRule::Ranked).await.unwrap();
    let (node, version, _) = account_to_april(&w, 5).await;

    // The statement is a Correction against the Version's stored bound, and
    // is recorded as one, though the merge ignores it: the call reports what
    // the earlier bound decides, not whether the statement won.
    let weak = w.episode(at(2024, 6, 1), Some(0)).await;
    let outcome = state_end(&w, node, version, weak, at(2024, 2, 15)).await;
    assert_eq!(outcome.change, BoundChange::Correction);
    assert!(
        outcome.opened.is_empty(),
        "the held statement opened no Version"
    );
    let v = w.read(node, at(2024, 3, 1)).await.unwrap();
    assert_eq!(v.id, version, "the Version still runs to April");
    assert_eq!(v.valid_time.to, Bound::At(at(2024, 4, 1)));
    assert!(v.recorded_to.is_none());

    // An equal Authority corrects it, as a later statement did before ranked merge.
    let peer = w.episode(at(2024, 6, 2), Some(5)).await;
    let outcome = state_end(&w, node, version, peer, at(2024, 3, 1)).await;
    assert_eq!(outcome.change, BoundChange::Correction);
    assert_eq!(outcome.opened.len(), 1);
    assert_eq!(
        w.read(node, at(2024, 3, 15)).await,
        None,
        "the Version now ends on 1 March"
    );
}

#[tokio::test]
async fn a_held_bound_statement_takes_effect_when_the_higher_authority_statement_is_erased() {
    let w = World::new().await;
    w.set_rule("account", MergeRule::Ranked).await.unwrap();
    let (node, version, _) = account_to_april(&w, 0).await;

    // A higher Authority moves the end to May, then a lower one, later, says
    // February. The higher statement holds, and the lower one is held.
    let strong = w.episode(at(2024, 5, 1), Some(9)).await;
    assert_eq!(
        state_end(&w, node, version, strong, at(2024, 5, 1))
            .await
            .change,
        BoundChange::Correction
    );
    let current = w.read(node, at(2024, 4, 15)).await.unwrap();
    let weak = w.episode(at(2024, 9, 1), Some(0)).await;
    state_end(&w, node, current.id, weak, at(2024, 2, 1)).await;
    assert!(
        w.read(node, at(2024, 4, 15)).await.is_some(),
        "the higher statement still holds"
    );
    assert_eq!(
        w.read(node, at(2024, 4, 15)).await.unwrap().valid_time.to,
        Bound::At(at(2024, 5, 1))
    );

    // Erase the higher statement's Episode: the held one takes effect.
    w.erase_rows_of(strong).await;
    let other = w.episode(at(2024, 10, 1), Some(0)).await;
    w.write(
        node,
        other,
        ValidTime::since(at(2024, 1, 1)),
        json!({ "name": "Acme" }),
    )
    .await;
    let after = w.read(node, at(2024, 4, 15)).await.unwrap();
    assert!(
        !after.values.contains_key("plan"),
        "the plan ended on 1 February"
    );
    let before = w.read(node, at(2024, 1, 15)).await.unwrap();
    assert_eq!(before.values["plan"], "gold");
    assert_eq!(before.valid_time.to, Bound::At(at(2024, 2, 1)));
}

#[tokio::test]
async fn a_version_starting_where_an_imprecise_end_falls_takes_its_start_precision_from_contributions_that_start_there()
 {
    // Ticket 8 left this open for ranked merge. A Contribution's imprecise
    // end gives the Version it closes an imprecise end, and gives the next
    // Version nothing: the next Version's start precision comes only from the
    // Contributions that start at it, whatever their Authority.
    let w = World::new().await;
    w.set_rule("account", MergeRule::Ranked).await.unwrap();
    let high = w.episode(at(2024, 1, 1), Some(10)).await;
    let low = w.episode(at(2024, 6, 1), Some(0)).await;
    let march = Bound::Period {
        at: at(2024, 3, 17),
        precision: Precision::Month,
    };
    let node = w
        .create(
            "account",
            high,
            ValidTime {
                from: Bound::At(at(2024, 1, 1)),
                to: march,
            },
            json!({ "plan": "gold" }),
        )
        .await;
    w.write(
        node,
        low,
        ValidTime::since_bound(Bound::Period {
            at: at(2024, 3, 1),
            precision: Precision::Day,
        }),
        json!({ "plan": "basic" }),
    )
    .await;
    let first = w.read(node, at(2024, 2, 1)).await.unwrap();
    let second = w.read(node, at(2024, 3, 1)).await.unwrap();
    assert_eq!(
        first.valid_time.to,
        Bound::Period {
            at: at(2024, 3, 1),
            precision: Precision::Month
        }
    );
    assert_eq!(
        second.valid_time.from,
        Bound::Period {
            at: at(2024, 3, 1),
            precision: Precision::Day
        }
    );
    assert_eq!(first.values["plan"], "gold");
    assert_eq!(second.values["plan"], "basic");
}
