//! Seam 1, Nodes: writing a patch of property values for a Node, citing an
//! Episode, reading the Node As-of a valid-at and a known-at instant, and
//! listing the Citations behind a Version, through the Rust core's Store API
//! against real Postgres (ADR-0004, ADR-0007, ADR-0008). The adversarial
//! cases live in sql_boundary.rs.

mod support;

use gnomon::{
    AsOf, EpisodeId, NewEpisode, NewNode, Patch, ROOT_NODE_KIND, Scope, Store, TenantId, ValidTime,
    WriteStatus,
};
use jiff::{SignedDuration, Timestamp};
use serde_json::{Map, Value, json};
use support::TestDb;

const SCOPE: &str = "support";

fn day(d: i8) -> Timestamp {
    support::utc(2024, 1, d)
}

fn values(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        other => panic!("not an object: {other}"),
    }
}

/// One Tenant's Store, as a worker in SCOPE.
struct World {
    _db: TestDb,
    store: Store,
    tenant: TenantId,
}

impl World {
    async fn new() -> World {
        let db = TestDb::migrated().await;
        let store = Store::connect(&db.login_url(), &support::connect_options())
            .await
            .unwrap();
        World {
            _db: db,
            store,
            tenant: support::tenant(),
        }
    }

    fn caller(&self) -> gnomon::Caller<'_> {
        self.store.as_worker(self.tenant, Scope::new(SCOPE), None)
    }

    /// Records an Episode with a Reference time.
    async fn episode(&self, reference_time: Timestamp) -> EpisodeId {
        self.caller()
            .record_episode(&NewEpisode {
                tenant: Some(self.tenant),
                scope: Scope::new(SCOPE),
                content: format!("an Episode from {reference_time}"),
                reference_time,
                authority: None,
            })
            .await
            .unwrap()
    }

    fn new_node(&self, kind: &str) -> NewNode {
        NewNode {
            tenant: Some(self.tenant),
            scope: Scope::new(SCOPE),
            kind: kind.to_owned(),
        }
    }
}

#[tokio::test]
async fn a_node_written_citing_an_episode_reads_back_as_of_now_and_its_version_cites_the_episode() {
    let w = World::new().await;
    let ep = w.episode(day(1)).await;
    let mut patch = Patch::new(
        ep,
        ValidTime::since(day(2)),
        values(json!({ "name": "Ada Lovelace" })),
    );
    patch.places.insert("name".into(), "line 3".into());

    let (node, outcome) = w
        .caller()
        .create_node(&w.new_node(ROOT_NODE_KIND), &patch)
        .await
        .unwrap();
    assert_eq!(outcome.status, WriteStatus::Written);
    assert_eq!(outcome.opened.len(), 1);

    let read = w
        .caller()
        .read_node(w.tenant, node, AsOf::current(day(5)))
        .await
        .unwrap()
        .expect("the Node holds a Version at day 5");
    assert_eq!(read.node, node);
    assert_eq!(read.kind, ROOT_NODE_KIND);
    assert_eq!(read.values, values(json!({ "name": "Ada Lovelace" })));
    assert_eq!(read.valid_time, ValidTime::since(day(2)));
    assert_eq!(read.recorded_from, outcome.recorded_at);
    assert_eq!(read.recorded_to, None);
    assert_eq!(read.id, outcome.opened[0]);

    // Before its Valid time starts, nothing held.
    assert_eq!(
        w.caller()
            .read_node(w.tenant, node, AsOf::current(day(1)))
            .await
            .unwrap(),
        None
    );
    // Before the write was recorded, gnomon knew nothing.
    let before = outcome.recorded_at - SignedDuration::from_micros(1);
    assert_eq!(
        w.caller()
            .read_node(w.tenant, node, AsOf::at(day(5), before))
            .await
            .unwrap(),
        None
    );

    let citations = w.caller().list_citations(w.tenant, read.id).await.unwrap();
    assert_eq!(citations.len(), 1, "{citations:?}");
    assert_eq!(citations[0].episode, ep);
    assert_eq!(citations[0].property, "name");
    assert_eq!(citations[0].place.as_deref(), Some("line 3"));
    assert_eq!(citations[0].recorded_at, outcome.recorded_at);
}

impl World {
    /// A World whose deployment lets workers register Kinds.
    async fn registering() -> World {
        let w = World::new().await;
        w._db.grant_kind_registration("gnomon_worker").await;
        w
    }

    async fn register_node_kind(&self, name: &str, properties: Value) {
        self.caller()
            .register_kind(&gnomon::NewKind {
                tenant: Some(self.tenant),
                name: name.to_owned(),
                of: gnomon::KindOf::Node,
                parent: None,
                properties,
                search: vec![],
            })
            .await
            .unwrap();
    }

    async fn create(&self, patch: &Patch) -> (gnomon::NodeId, gnomon::WriteOutcome) {
        self.caller()
            .create_node(&self.new_node(ROOT_NODE_KIND), patch)
            .await
            .unwrap()
    }

    async fn write(&self, node: gnomon::NodeId, patch: &Patch) -> gnomon::WriteOutcome {
        self.caller()
            .write_node(self.tenant, node, patch)
            .await
            .unwrap()
    }

    async fn read(&self, node: gnomon::NodeId, as_of: AsOf) -> Option<gnomon::NodeVersion> {
        self.caller()
            .read_node(self.tenant, node, as_of)
            .await
            .unwrap()
    }

    /// The values that hold at `valid_at` as gnomon believes now.
    async fn now(&self, node: gnomon::NodeId, valid_at: Timestamp) -> Option<Map<String, Value>> {
        self.read(node, AsOf::current(valid_at))
            .await
            .map(|v| v.values)
    }
}

fn name(n: &str) -> Map<String, Value> {
    values(json!({ "name": n }))
}

#[tokio::test]
async fn the_later_reference_time_wins_whatever_order_the_episodes_were_recorded_in() {
    let w = World::new().await;
    let late = w.episode(day(10)).await;
    let early = w.episode(day(5)).await;
    let (node, _) = w
        .create(&Patch::new(late, ValidTime::UNKNOWN, name("Later")))
        .await;
    let second = w
        .write(
            node,
            &Patch::new(early, ValidTime::UNKNOWN, name("Earlier")),
        )
        .await;

    assert_eq!(w.now(node, day(1)).await, Some(name("Later")));
    assert_eq!(
        second.status,
        WriteStatus::Unchanged,
        "the losing value changes no Version"
    );
}

#[tokio::test]
async fn with_equal_reference_times_the_later_recorded_contribution_wins() {
    let w = World::new().await;
    let first = w.episode(day(5)).await;
    let second = w.episode(day(5)).await;
    let (node, _) = w
        .create(&Patch::new(first, ValidTime::UNKNOWN, name("First")))
        .await;
    let outcome = w
        .write(
            node,
            &Patch::new(second, ValidTime::UNKNOWN, name("Second")),
        )
        .await;

    assert_eq!(outcome.status, WriteStatus::Written);
    assert_eq!(w.now(node, day(1)).await, Some(name("Second")));
}

#[tokio::test]
async fn a_bounded_patch_splits_the_version_it_overlaps_and_each_period_keeps_its_own_winner() {
    let w = World::new().await;
    let base = w.episode(day(1)).await;
    let patch = w.episode(day(2)).await;
    let (node, _) = w
        .create(&Patch::new(
            base,
            ValidTime::since(day(1)),
            values(json!({ "name": "Ada", "aliases": ["A"] })),
        ))
        .await;
    w.write(
        node,
        &Patch::new(patch, ValidTime::between(day(10), day(20)), name("Ada L")),
    )
    .await;

    let at = |d| w.read(node, AsOf::current(day(d)));
    let before = at(5).await.unwrap();
    let during = at(15).await.unwrap();
    let after = at(25).await.unwrap();
    assert_eq!(
        before.values,
        values(json!({ "name": "Ada", "aliases": ["A"] }))
    );
    assert_eq!(before.valid_time, ValidTime::between(day(1), day(10)));
    assert_eq!(
        during.values,
        values(json!({ "name": "Ada L", "aliases": ["A"] }))
    );
    assert_eq!(during.valid_time, ValidTime::between(day(10), day(20)));
    assert_eq!(after.values, before.values);
    assert_eq!(after.valid_time, ValidTime::since(day(20)));
}

#[tokio::test]
async fn a_null_value_ends_a_property_over_its_valid_time() {
    let w = World::new().await;
    let base = w.episode(day(1)).await;
    let end = w.episode(day(2)).await;
    let (node, _) = w
        .create(&Patch::new(
            base,
            ValidTime::since(day(1)),
            values(json!({ "name": "Ada", "aliases": ["A"] })),
        ))
        .await;
    w.write(
        node,
        &Patch::new(
            end,
            ValidTime::since(day(10)),
            values(json!({ "aliases": null })),
        ),
    )
    .await;
    assert_eq!(w.now(node, day(20)).await, Some(name("Ada")));

    // Ending the last property leaves no Version at all.
    w.write(
        node,
        &Patch::new(
            end,
            ValidTime::since(day(10)),
            values(json!({ "name": null })),
        ),
    )
    .await;
    assert_eq!(w.now(node, day(20)).await, None);
    assert_eq!(
        w.now(node, day(5)).await,
        Some(values(json!({ "name": "Ada", "aliases": ["A"] })))
    );
}

#[tokio::test]
async fn as_of_reads_at_a_past_known_at_return_what_gnomon_believed_then() {
    let w = World::new().await;
    let a = w.episode(day(1)).await;
    let b = w.episode(day(2)).await;
    let (node, first) = w
        .create(&Patch::new(a, ValidTime::UNKNOWN, name("A")))
        .await;
    let second = w
        .write(node, &Patch::new(b, ValidTime::UNKNOWN, name("B")))
        .await;
    assert!(second.recorded_at > first.recorded_at);

    let then = w
        .read(node, AsOf::at(day(1), first.recorded_at))
        .await
        .unwrap();
    assert_eq!(then.values, name("A"));
    assert_eq!(then.id, first.opened[0]);
    assert_eq!(
        then.recorded_to,
        Some(second.recorded_at),
        "closed by the second write"
    );

    let just_before = second.recorded_at - SignedDuration::from_micros(1);
    assert_eq!(
        w.read(node, AsOf::at(day(1), just_before))
            .await
            .unwrap()
            .values,
        name("A")
    );
    let at_second = w
        .read(node, AsOf::at(day(1), second.recorded_at))
        .await
        .unwrap();
    assert_eq!(at_second.values, name("B"));
    assert_eq!(at_second.id, second.opened[0]);
    assert_eq!(w.now(node, day(1)).await, Some(name("B")));
}

#[tokio::test]
async fn a_write_that_changes_nothing_is_unchanged_and_cites_its_episode_from_the_version() {
    let w = World::new().await;
    let a = w.episode(day(1)).await;
    let again = w.episode(day(3)).await;
    let (node, first) = w
        .create(&Patch::new(a, ValidTime::since(day(1)), name("Ada")))
        .await;
    // Restating the value over part of the Version changes nothing.
    let mut repeat = Patch::new(again, ValidTime::between(day(5), day(6)), name("Ada"));
    repeat.places.insert("name".into(), "paragraph 2".into());
    let outcome = w.write(node, &repeat).await;

    assert_eq!(outcome.status, WriteStatus::Unchanged);
    assert!(outcome.opened.is_empty());
    let version = w.read(node, AsOf::current(day(5))).await.unwrap();
    assert_eq!(version.id, first.opened[0], "the same Version still holds");
    assert_eq!(version.valid_time, ValidTime::since(day(1)));

    let citations = w
        .caller()
        .list_citations(w.tenant, version.id)
        .await
        .unwrap();
    let cited: Vec<_> = citations
        .iter()
        .map(|c| (c.episode, c.property.as_str(), c.place.as_deref()))
        .collect();
    assert_eq!(
        cited,
        vec![(a, "name", None), (again, "name", Some("paragraph 2"))],
        "the repeat corroborates the Version"
    );
}

#[tokio::test]
async fn a_new_version_cites_every_episode_behind_its_values_and_not_the_losers() {
    let w = World::new().await;
    let a = w.episode(day(1)).await;
    let b = w.episode(day(2)).await;
    let loser = w.episode(day(1) - SignedDuration::from_hours(24)).await;
    let (node, _) = w
        .create(&Patch::new(a, ValidTime::UNKNOWN, name("Ada")))
        .await;
    w.write(
        node,
        &Patch::new(loser, ValidTime::UNKNOWN, name("Not Ada")),
    )
    .await;
    let out = w
        .write(
            node,
            &Patch::new(b, ValidTime::UNKNOWN, values(json!({ "aliases": ["A"] }))),
        )
        .await;

    let citations = w
        .caller()
        .list_citations(w.tenant, out.opened[0])
        .await
        .unwrap();
    let cited: Vec<_> = citations
        .iter()
        .map(|c| (c.property.as_str(), c.episode))
        .collect();
    assert_eq!(cited, vec![("aliases", b), ("name", a)]);
}

#[tokio::test]
async fn each_value_is_stored_as_a_contribution_with_its_episode_and_valid_time() {
    let w = World::new().await;
    let a = w.episode(day(1)).await;
    let b = w.episode(day(1) - SignedDuration::from_hours(24)).await;
    let (node, _) = w
        .create(&Patch::new(
            a,
            ValidTime::between(day(1), day(9)),
            values(json!({ "name": "Ada", "aliases": ["A"] })),
        ))
        .await;
    // A value that loses its merge is held all the same.
    w.write(
        node,
        &Patch::new(b, ValidTime::since(day(3)), name("Losing")),
    )
    .await;

    /// Episode, property, value and Valid time, with `None` for an unknown
    /// bound.
    type Held = (
        uuid::Uuid,
        String,
        Value,
        Option<Timestamp>,
        Option<Timestamp>,
    );
    let got: Vec<Held> = w
        .rows_for(
            "select episode_id, property, value, nullif(valid_from, '-infinity'),
                    nullif(valid_to, 'infinity')
             from gnomon.contribution where node_id = $1 order by id",
            node,
        )
        .await
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4)))
        .collect();
    assert_eq!(
        got,
        vec![
            (
                a.0,
                "aliases".into(),
                json!(["A"]),
                Some(day(1)),
                Some(day(9))
            ),
            (a.0, "name".into(), json!("Ada"), Some(day(1)), Some(day(9))),
            (b.0, "name".into(), json!("Losing"), Some(day(3)), None),
        ]
    );
}

fn invalid(result: Result<impl std::fmt::Debug, gnomon::Error>) -> String {
    match result {
        Err(gnomon::Error::InvalidWrite(m)) => m,
        other => panic!("expected an invalid write, got {other:?}"),
    }
}

#[tokio::test]
async fn writes_are_checked_against_the_kinds_compiled_rules() {
    let w = World::registering().await;
    w.register_node_kind(
        "person",
        json!({
            "properties": {
                "age": { "type": "integer", "minimum": 0 },
                "email": { "type": "string", "format": "uuid" }
            },
            "required": ["age"]
        }),
    )
    .await;
    let ep = w.episode(day(1)).await;
    let person = w.new_node("person");
    let create = |v: Value| {
        let patch = Patch::new(ep, ValidTime::UNKNOWN, values(v));
        let caller = w.caller();
        let person = person.clone();
        async move { caller.create_node(&person, &patch).await }
    };

    // The same messages on every image: they come from the migrations.
    assert_eq!(
        invalid(create(json!({ "age": -1 })).await),
        "the value -1 of property \"age\" does not satisfy Kind \"person\""
    );
    assert_eq!(
        invalid(create(json!({ "age": "forty" })).await),
        "the value \"forty\" of property \"age\" does not satisfy Kind \"person\""
    );
    assert_eq!(
        invalid(create(json!({ "age": 40, "height": 180 })).await),
        "Kind \"person\" has no property \"height\""
    );
    assert_eq!(
        invalid(create(json!({ "name": 7, "age": 40 })).await),
        "the value 7 of property \"name\" does not satisfy Kind \"person\""
    );
    assert_eq!(
        invalid(create(json!({ "name": "Ada" })).await),
        "the write leaves required property \"age\" of Kind \"person\" without a value over \
         [-infinity, infinity)"
    );

    let (node, _) = create(json!({ "age": 40, "name": "Ada", "aliases": ["A"] }))
        .await
        .unwrap();
    // A null is not checked: it ends the property. Ending a required one is
    // still refused while another property holds.
    assert_eq!(
        invalid(
            w.caller()
                .write_node(
                    w.tenant,
                    node,
                    &Patch::new(ep, ValidTime::UNKNOWN, values(json!({ "age": null })))
                )
                .await
        ),
        "the write leaves required property \"age\" of Kind \"person\" without a value over \
         [-infinity, infinity)"
    );
    let ended = w
        .write(
            node,
            &Patch::new(ep, ValidTime::UNKNOWN, values(json!({ "email": null }))),
        )
        .await;
    assert_eq!(ended.status, WriteStatus::Unchanged);
    assert_eq!(
        w.now(node, day(1)).await,
        Some(values(
            json!({ "age": 40, "name": "Ada", "aliases": ["A"] })
        ))
    );
}

#[tokio::test]
async fn a_root_node_has_only_a_name_and_aliases() {
    let w = World::new().await;
    let ep = w.episode(day(1)).await;
    let r = w
        .caller()
        .create_node(
            &w.new_node(ROOT_NODE_KIND),
            &Patch::new(ep, ValidTime::UNKNOWN, values(json!({ "age": 1 }))),
        )
        .await;
    assert_eq!(invalid(r), "Kind \"node\" has no property \"age\"");
    let r = w
        .caller()
        .create_node(
            &w.new_node(ROOT_NODE_KIND),
            &Patch::new(
                ep,
                ValidTime::UNKNOWN,
                values(json!({ "aliases": ["A", 1] })),
            ),
        )
        .await;
    assert_eq!(
        invalid(r),
        "the value [\"A\", 1] of property \"aliases\" does not satisfy Kind \"node\""
    );
    let r = w
        .caller()
        .create_node(
            &w.new_node("person"),
            &Patch::new(ep, ValidTime::UNKNOWN, name("Ada")),
        )
        .await;
    assert_eq!(invalid(r), "\"person\" is not a Node Kind of the Tenant");
}

#[tokio::test]
async fn empty_and_reversed_valid_times_are_refused() {
    let w = World::new().await;
    let ep = w.episode(day(1)).await;
    let (node, _) = w
        .create(&Patch::new(ep, ValidTime::UNKNOWN, name("Ada")))
        .await;
    for valid in [
        ValidTime::between(day(5), day(5)),
        ValidTime::between(day(6), day(5)),
    ] {
        let r = w
            .caller()
            .write_node(w.tenant, node, &Patch::new(ep, valid, name("B")))
            .await;
        let m = invalid(r);
        assert!(
            m.starts_with("the Valid time [") && m.ends_with(") is empty"),
            "{m}"
        );
    }
    assert_eq!(w.now(node, day(5)).await, Some(name("Ada")));
}

#[tokio::test]
async fn a_write_must_cite_an_episode_the_caller_can_see_in_the_nodes_scope() {
    let w = World::new().await;
    let ep = w.episode(day(1)).await;
    let (node, _) = w
        .create(&Patch::new(ep, ValidTime::UNKNOWN, name("Ada")))
        .await;
    let other_scope = w
        .store
        .as_worker(w.tenant, Scope::new("billing"), None)
        .record_episode(&NewEpisode {
            tenant: Some(w.tenant),
            scope: Scope::new("billing"),
            content: "billing".into(),
            reference_time: day(2),
            authority: None,
        })
        .await
        .unwrap();
    for missing in [other_scope, EpisodeId(uuid::Uuid::new_v4())] {
        let r = w
            .caller()
            .write_node(
                w.tenant,
                node,
                &Patch::new(missing, ValidTime::UNKNOWN, name("B")),
            )
            .await;
        assert_eq!(
            invalid(r),
            format!("Episode {missing} is not visible in the Node's Scope")
        );
    }
    let r = w
        .caller()
        .write_node(
            w.tenant,
            node,
            &Patch::new(ep, ValidTime::UNKNOWN, Map::new()),
        )
        .await;
    assert_eq!(
        invalid(r),
        "a write states a value for at least one property"
    );
}

#[tokio::test]
async fn the_recorded_time_of_every_write_in_a_transaction_is_its_transaction_time() {
    let w = World::new().await;
    let ep = w.episode(day(1)).await;
    let (a, _) = w
        .create(&Patch::new(ep, ValidTime::UNKNOWN, name("A")))
        .await;
    let (b, _) = w
        .create(&Patch::new(ep, ValidTime::UNKNOWN, name("B")))
        .await;
    let outcomes = w
        .caller()
        .write_nodes(
            w.tenant,
            &[
                (a, Patch::new(ep, ValidTime::since(day(2)), name("A2"))),
                (b, Patch::new(ep, ValidTime::since(day(2)), name("B2"))),
            ],
        )
        .await
        .unwrap();
    assert_eq!(outcomes[0].recorded_at, outcomes[1].recorded_at);
    for node in [a, b] {
        let v = w.read(node, AsOf::current(day(3))).await.unwrap();
        assert_eq!(v.recorded_from, outcomes[0].recorded_at);
    }
    let ep_recorded = w
        .caller()
        .read_episode(w.tenant, ep)
        .await
        .unwrap()
        .unwrap()
        .recorded_at;
    assert!(outcomes[0].recorded_at > ep_recorded);
}

impl World {
    /// Runs `sql` with the Node as $1, as a worker of this World would from
    /// any SQL client.
    async fn rows_for(&self, sql: &str, node: gnomon::NodeId) -> Vec<tokio_postgres::Row> {
        use tokio_postgres::types::Type;
        let mut client = self._db.login().await;
        let tx = client.transaction().await.unwrap();
        tx.batch_execute("set local role gnomon_worker")
            .await
            .unwrap();
        let claims =
            json!({ "app_metadata": { "tenant_id": self.tenant.to_string(), "scopes": [SCOPE] } })
                .to_string();
        tx.query_typed(
            "select set_config('request.jwt.claims', $1, true)",
            &[(&claims, Type::TEXT)],
        )
        .await
        .unwrap();
        let rows = tx.query_typed(sql, &[(&node.0, Type::UUID)]).await.unwrap();
        tx.commit().await.unwrap();
        rows
    }

    /// Every Version row of the Node, current and closed: its ID, values,
    /// and Recorded time, with `None` for an open end.
    async fn history(
        &self,
        node: gnomon::NodeId,
    ) -> Vec<(uuid::Uuid, Value, Timestamp, Option<Timestamp>)> {
        self.rows_for(
            "select id, props, recorded_from, nullif(recorded_to, 'infinity')
             from gnomon.version where node_id = $1 order by recorded_from, valid_from",
            node,
        )
        .await
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3)))
        .collect()
    }

    /// The Episodes cited from any Version of the Node.
    async fn cited(&self, node: gnomon::NodeId) -> Vec<uuid::Uuid> {
        self.rows_for(
            "select distinct c.episode_id from gnomon.citation c
             join gnomon.version v on v.id = c.version_id where v.node_id = $1 order by 1",
            node,
        )
        .await
        .iter()
        .map(|r| r.get(0))
        .collect()
    }
}

#[tokio::test]
async fn a_value_written_and_ended_in_one_transaction_leaves_no_version_and_no_citation() {
    let w = World::new().await;
    let base = w.episode(day(1)).await;
    let add = w.episode(day(2)).await;
    let end = w.episode(day(3)).await;
    let (node, _) = w
        .create(&Patch::new(
            base,
            ValidTime::between(day(1), day(10)),
            name("Ada"),
        ))
        .await;
    let before = w.history(node).await;

    let outcomes = w
        .caller()
        .write_nodes(
            w.tenant,
            &[
                (
                    node,
                    Patch::new(add, ValidTime::between(day(20), day(30)), name("Ada L")),
                ),
                (
                    node,
                    Patch::new(
                        end,
                        ValidTime::between(day(20), day(30)),
                        values(json!({ "name": null })),
                    ),
                ),
            ],
        )
        .await
        .unwrap();
    assert_eq!(outcomes[0].status, WriteStatus::Written);
    assert_eq!(outcomes[0].opened.len(), 1);
    assert_eq!(
        outcomes[1].status,
        WriteStatus::Written,
        "the second write removed a Version"
    );
    assert!(outcomes[1].opened.is_empty());

    assert_eq!(
        w.history(node).await,
        before,
        "the first write's Version vanished"
    );
    assert_eq!(w.cited(node).await, vec![base.0]);
    assert_eq!(w.read(node, AsOf::current(day(25))).await, None);
    assert_eq!(
        w.read(node, AsOf::at(day(25), outcomes[0].recorded_at))
            .await,
        None
    );
}

#[tokio::test]
async fn a_second_write_in_one_transaction_replaces_the_first_version_in_place() {
    let w = World::new().await;
    let a = w.episode(day(1)).await;
    let b = w.episode(day(2)).await;
    let c = w.episode(day(3)).await;
    let (node, first) = w
        .create(&Patch::new(a, ValidTime::UNKNOWN, name("A")))
        .await;
    let outcomes = w
        .caller()
        .write_nodes(
            w.tenant,
            &[
                (node, Patch::new(b, ValidTime::UNKNOWN, name("B"))),
                (node, Patch::new(c, ValidTime::UNKNOWN, name("C"))),
            ],
        )
        .await
        .unwrap();
    let t = outcomes[1].recorded_at;

    let history = w.history(node).await;
    assert_eq!(
        history,
        vec![
            (
                first.opened[0].0,
                json!({ "name": "A" }),
                first.recorded_at,
                Some(t)
            ),
            (outcomes[1].opened[0].0, json!({ "name": "C" }), t, None),
        ],
        "B was never a Version"
    );
    assert!(!outcomes[0].opened.contains(&outcomes[1].opened[0]));
    assert_eq!(w.cited(node).await, vec![a.0, c.0]);
}

#[tokio::test]
async fn a_second_write_that_restores_the_state_before_the_transaction_reopens_the_version() {
    let w = World::new().await;
    let a = w.episode(day(1)).await;
    let b = w.episode(day(2)).await;
    let again = w.episode(day(3)).await;
    let (node, first) = w
        .create(&Patch::new(a, ValidTime::since(day(1)), name("A")))
        .await;
    let history_before = w.history(node).await;

    let outcomes = w
        .caller()
        .write_nodes(
            w.tenant,
            &[
                (
                    node,
                    Patch::new(b, ValidTime::between(day(5), day(9)), name("B")),
                ),
                (
                    node,
                    Patch::new(again, ValidTime::between(day(5), day(9)), name("A")),
                ),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        outcomes[0].opened.len(),
        3,
        "the first write split the Version in three"
    );
    assert_eq!(
        outcomes[1].opened, first.opened,
        "the second reopened the original"
    );

    assert_eq!(w.history(node).await, history_before);
    let citations = w
        .caller()
        .list_citations(w.tenant, first.opened[0])
        .await
        .unwrap();
    let cited: Vec<_> = citations.iter().map(|c| c.episode).collect();
    assert_eq!(
        cited,
        vec![a, again],
        "the restating Episode corroborates; B is not cited"
    );
}

#[tokio::test]
async fn concurrent_writes_to_one_node_all_land_in_recorded_order() {
    let w = World::new().await;
    let ep = w.episode(day(1)).await;
    let (node, _) = w
        .create(&Patch::new(ep, ValidTime::UNKNOWN, name("Ada")))
        .await;
    let mut stores = Vec::new();
    for _ in 0..6 {
        stores.push(
            Store::connect(&w._db.login_url(), &support::connect_options())
                .await
                .unwrap(),
        );
    }
    let writes = stores.iter().enumerate().map(|(i, store)| {
        let w = &w;
        async move {
            let caller = store.as_worker(w.tenant, Scope::new(SCOPE), None);
            let mut out = Vec::new();
            for j in 0..5 {
                let ep = w.episode(day(2)).await;
                let alias = format!("writer {i} write {j}");
                let patch = Patch::new(
                    ep,
                    ValidTime::UNKNOWN,
                    values(json!({ "aliases": [alias] })),
                );
                out.push(caller.write_node(w.tenant, node, &patch).await.unwrap());
            }
            out
        }
    });
    let outcomes: Vec<_> = futures_join_all(writes)
        .await
        .into_iter()
        .flatten()
        .collect();
    assert_eq!(outcomes.len(), 30);

    // Every write recorded at its own time, each closing the Version before.
    let history = w.history(node).await;
    assert_eq!(history.len(), 31);
    for pair in history.windows(2) {
        assert_eq!(pair[0].3, Some(pair[1].2), "{history:?}");
    }
    let last = outcomes.iter().max_by_key(|o| o.recorded_at).unwrap();
    assert_eq!(history[30].0, last.opened[0].0);
}

/// Runs the futures concurrently on this task and collects their outputs.
async fn futures_join_all<F: std::future::Future>(
    futures: impl IntoIterator<Item = F>,
) -> Vec<F::Output> {
    let handles: Vec<_> = futures.into_iter().map(Box::pin).collect();
    let mut outputs: Vec<Option<F::Output>> = handles.iter().map(|_| None).collect();
    let mut handles: Vec<_> = handles.into_iter().map(Some).collect();
    std::future::poll_fn(|cx| {
        let mut pending = false;
        for (h, out) in handles.iter_mut().zip(outputs.iter_mut()) {
            if let Some(f) = h {
                match f.as_mut().poll(cx) {
                    std::task::Poll::Ready(v) => {
                        *out = Some(v);
                        *h = None;
                    }
                    std::task::Poll::Pending => pending = true,
                }
            }
        }
        if pending {
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(())
        }
    })
    .await;
    outputs.into_iter().map(Option::unwrap).collect()
}
