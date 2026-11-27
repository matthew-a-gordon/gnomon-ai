//! Seam 1, Edges: writing an Edge between two Nodes through the same
//! Contribution and Version model as a Node, reading it As-of a valid-at and
//! a known-at instant, traversing Edges from a Node, and reading many Nodes
//! As-of in one call, through the Rust core's Store API against real
//! Postgres (ADR-0007, ADR-0008). The adversarial cases live in
//! sql_boundary.rs.

mod support;

use chrono::{DateTime, TimeZone, Utc};
use gnomon::{
    AsOf, Bound, BoundChange, BoundSide, BoundStatement, Direction, EdgeId, EpisodeId, Error,
    KindOf, MergeRule, NewEdge, NewEpisode, NewKind, NewNode, NodeId, Patch, Precision,
    ROOT_EDGE_KIND, ROOT_NODE_KIND, Scope, Store, TenantId, Traversal, ValidTime, WriteStatus,
};
use serde_json::{Map, Value, json};
use support::TestDb;

const SCOPE: &str = "support";

fn day(d: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2024, 1, d, 0, 0, 0).unwrap()
}

fn values(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        other => panic!("not an object: {other}"),
    }
}

/// One Tenant's Store, as a worker in SCOPE.
struct World {
    db: TestDb,
    store: Store,
    tenant: TenantId,
}

impl World {
    /// A Tenant whose worker may register Kinds.
    async fn new() -> World {
        let db = TestDb::migrated().await;
        db.grant_kind_registration("gnomon_worker").await;
        let store = Store::connect(&db.login_url(), &support::connect_options())
            .await
            .unwrap();
        World {
            db,
            store,
            tenant: support::tenant(),
        }
    }

    fn caller(&self) -> gnomon::Caller<'_> {
        self.store.as_worker(self.tenant, Scope::new(SCOPE))
    }

    /// An end user of the Tenant who may read and write `scopes`.
    fn user(&self, scopes: &[&str]) -> gnomon::Caller<'_> {
        self.store.as_user(support::user(self.tenant, scopes))
    }

    /// Registers the Node Kinds `person`, `employee` below it and `company`,
    /// and the Edge Kind `works_at` from a `person` to a `company`, which
    /// declares `role`, with `leads` below it.
    async fn ontology(&self) {
        for (name, parent, of, properties) in [
            ("person", None, KindOf::Node, json!({})),
            ("employee", Some("person"), KindOf::Node, json!({})),
            ("company", None, KindOf::Node, json!({})),
            (
                "works_at",
                None,
                KindOf::Edge {
                    source: vec!["person".into()],
                    target: vec!["company".into()],
                },
                json!({ "role": { "type": "string" } }),
            ),
            (
                "leads",
                Some("works_at"),
                KindOf::Edge {
                    source: vec![],
                    target: vec![],
                },
                json!({}),
            ),
        ] {
            self.caller()
                .register_kind(&NewKind {
                    tenant: Some(self.tenant),
                    name: name.into(),
                    of,
                    parent: parent.map(str::to_owned),
                    properties: json!({ "type": "object", "properties": properties }),
                    search: vec![],
                })
                .await
                .unwrap();
        }
    }

    async fn episode(&self, reference_time: DateTime<Utc>) -> EpisodeId {
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

    /// A Node of `kind` named `name` from the start of time.
    async fn node(&self, kind: &str, name: &str) -> NodeId {
        let ep = self.episode(day(1)).await;
        self.caller()
            .create_node(
                &NewNode {
                    tenant: Some(self.tenant),
                    scope: Scope::new(SCOPE),
                    kind: kind.to_owned(),
                },
                &Patch::new(ep, ValidTime::UNKNOWN, values(json!({ "name": name }))),
            )
            .await
            .unwrap()
            .0
    }

    /// An Edge of `kind` from `source` to `target` that holds over `vt` with
    /// `v`, from a new Episode with Reference time `reference_time`.
    async fn edge(
        &self,
        kind: &str,
        source: NodeId,
        target: NodeId,
        reference_time: DateTime<Utc>,
        vt: ValidTime,
        v: Value,
    ) -> EdgeId {
        let ep = self.episode(reference_time).await;
        self.caller()
            .create_edge(
                &self.new_edge(kind, source, target),
                &Patch::new(ep, vt, values(v)),
            )
            .await
            .unwrap()
            .0
    }

    async fn read_edge(&self, edge: EdgeId, as_of: AsOf) -> Option<gnomon::EdgeVersion> {
        self.caller()
            .read_edge(self.tenant, edge, as_of)
            .await
            .unwrap()
    }

    fn new_edge(&self, kind: &str, source: NodeId, target: NodeId) -> NewEdge {
        NewEdge {
            tenant: Some(self.tenant),
            scope: Scope::new(SCOPE),
            kind: kind.to_owned(),
            source,
            target,
        }
    }
}

#[tokio::test]
async fn an_edge_with_no_property_values_holds_over_its_valid_time_and_cites_its_episode() {
    let w = World::new().await;
    let (ada, acme) = (
        w.node(ROOT_NODE_KIND, "Ada").await,
        w.node(ROOT_NODE_KIND, "Acme").await,
    );
    let ep = w.episode(day(2)).await;

    let (edge, outcome): (EdgeId, _) = w
        .caller()
        .create_edge(
            &w.new_edge(ROOT_EDGE_KIND, ada, acme),
            &Patch::new(ep, ValidTime::between(day(10), day(20)), Map::new()),
        )
        .await
        .unwrap();
    assert_eq!(outcome.status, WriteStatus::Written);
    assert_eq!(outcome.opened.len(), 1);

    let v = w
        .caller()
        .read_edge(w.tenant, edge, AsOf::current(day(15)))
        .await
        .unwrap()
        .expect("the Edge holds on day 15");
    assert_eq!(v.id, outcome.opened[0]);
    assert_eq!((v.edge, v.source, v.target), (edge, ada, acme));
    assert_eq!(v.kind, ROOT_EDGE_KIND);
    assert!(v.values.is_empty(), "{:?}", v.values);
    assert_eq!(v.valid_time, ValidTime::between(day(10), day(20)));
    assert_eq!(v.sentence, None);

    for outside in [day(9), day(20)] {
        assert_eq!(
            w.caller()
                .read_edge(w.tenant, edge, AsOf::current(outside))
                .await
                .unwrap(),
            None,
            "the Edge does not hold at {outside}"
        );
    }

    let citations = w.caller().list_citations(w.tenant, v.id).await.unwrap();
    assert_eq!(citations.len(), 1, "{citations:?}");
    assert_eq!(citations[0].episode, ep);
}

#[tokio::test]
async fn an_edges_property_values_go_through_contributions_and_versions_as_a_nodes_do() {
    let w = World::new().await;
    w.ontology().await;
    let (ada, acme) = (
        w.node("person", "Ada").await,
        w.node("company", "Acme").await,
    );
    let edge = w
        .edge(
            "works_at",
            ada,
            acme,
            day(1),
            ValidTime::since(day(10)),
            json!({ "role": "engineer" }),
        )
        .await;
    let first = w.read_edge(edge, AsOf::current(day(15))).await.unwrap();
    assert_eq!(first.values, values(json!({ "role": "engineer" })));
    assert_eq!(first.merge_rule, MergeRule::Recency);

    // A later Episode states a new role from day 20 on.
    let later = w.episode(day(2)).await;
    let outcome = w
        .caller()
        .write_edge(
            w.tenant,
            edge,
            &Patch::new(
                later,
                ValidTime::since(day(20)),
                values(json!({ "role": "cto" })),
            ),
        )
        .await
        .unwrap();
    assert_eq!(outcome.status, WriteStatus::Written);

    let now = |d| AsOf::current(day(d));
    assert_eq!(
        w.read_edge(edge, now(15)).await.unwrap().values["role"],
        "engineer"
    );
    let cto = w.read_edge(edge, now(25)).await.unwrap();
    assert_eq!(cto.values["role"], "cto");
    assert_eq!(cto.valid_time, ValidTime::since(day(20)));
    // Before the second write, gnomon believed Ada was an engineer on day 25.
    let before = w
        .read_edge(
            edge,
            AsOf::at(
                day(25),
                outcome.recorded_at - chrono::Duration::microseconds(1),
            ),
        )
        .await
        .unwrap();
    assert_eq!(before.id, first.id);
    assert_eq!(before.values["role"], "engineer");
    assert_eq!(before.recorded_to, Some(outcome.recorded_at));

    // The new Version cites the later Episode for the role and for holding.
    let cited: Vec<_> = w
        .caller()
        .list_citations(w.tenant, cto.id)
        .await
        .unwrap()
        .into_iter()
        .map(|c| (c.property, c.episode))
        .collect();
    assert!(cited.contains(&("role".to_owned(), later)), "{cited:?}");
    assert!(cited.contains(&("holds".to_owned(), later)), "{cited:?}");

    // Stating the same role again changes nothing.
    let again = w.episode(day(3)).await;
    let repeat = w
        .caller()
        .write_edge(
            w.tenant,
            edge,
            &Patch::new(
                again,
                ValidTime::since(day(20)),
                values(json!({ "role": "cto" })),
            ),
        )
        .await
        .unwrap();
    assert_eq!(repeat.status, WriteStatus::Unchanged);
}

#[tokio::test]
async fn an_edge_whose_endpoint_kinds_the_edge_kind_does_not_allow_is_refused() {
    let w = World::new().await;
    w.ontology().await;
    let ada = w.node("person", "Ada").await;
    let bob = w.node("employee", "Bob").await;
    let acme = w.node("company", "Acme").await;
    let thing = w.node(ROOT_NODE_KIND, "a thing").await;
    let ep = w.episode(day(1)).await;
    let w = &w;
    let create = |kind: &str, source, target| {
        let edge = w.new_edge(kind, source, target);
        async move {
            w.caller()
                .create_edge(&edge, &Patch::new(ep, ValidTime::UNKNOWN, Map::new()))
                .await
        }
    };

    // A person, or a Node of a Kind below person, may work at a company, and
    // a child Edge Kind allows what its parent allows.
    create("works_at", ada, acme).await.unwrap();
    create("works_at", bob, acme).await.unwrap();
    create("leads", bob, acme).await.unwrap();
    // The root Edge Kind joins any Node to any Node.
    create(ROOT_EDGE_KIND, acme, thing).await.unwrap();

    for (kind, source, target, message) in [
        (
            "works_at",
            acme,
            ada,
            "Edge Kind \"works_at\" may not start at a Node of Kind \"company\"; it allows person",
        ),
        (
            "works_at",
            ada,
            bob,
            "Edge Kind \"works_at\" may not end at a Node of Kind \"employee\"; it allows company",
        ),
        (
            "leads",
            ada,
            thing,
            "Edge Kind \"leads\" may not end at a Node of Kind \"node\"; it allows company",
        ),
        (
            "person",
            ada,
            acme,
            "\"person\" is not an Edge Kind of the Tenant",
        ),
        (
            ROOT_NODE_KIND,
            ada,
            acme,
            "\"node\" is not an Edge Kind of the Tenant",
        ),
        (
            "knows",
            ada,
            acme,
            "\"knows\" is not an Edge Kind of the Tenant",
        ),
    ] {
        match create(kind, source, target).await {
            Err(Error::InvalidWrite(m)) => assert_eq!(m, message),
            other => panic!("{kind} from {source} to {target}: {other:?}"),
        }
    }
}

#[tokio::test]
async fn an_edge_write_states_only_the_edge_kinds_properties_and_never_the_built_in_holds() {
    let w = World::new().await;
    w.ontology().await;
    let (ada, acme) = (
        w.node("person", "Ada").await,
        w.node("company", "Acme").await,
    );
    let edge = w
        .edge("works_at", ada, acme, day(1), ValidTime::UNKNOWN, json!({}))
        .await;
    let ep = w.episode(day(2)).await;
    for (v, message) in [
        (
            json!({ "role": 7 }),
            "the value 7 of property \"role\" does not satisfy Kind \"works_at\"",
        ),
        (
            json!({ "name": "Ada at Acme" }),
            "Kind \"works_at\" has no property \"name\"",
        ),
        (
            json!({ "holds": true }),
            "\"holds\" is built in: every write to an Edge states that the Edge holds over its Valid time",
        ),
        (
            json!({ "holds": null }),
            "\"holds\" is built in: every write to an Edge states that the Edge holds over its Valid time",
        ),
    ] {
        match w
            .caller()
            .write_edge(
                w.tenant,
                edge,
                &Patch::new(ep, ValidTime::UNKNOWN, values(v.clone())),
            )
            .await
        {
            Err(Error::InvalidWrite(m)) => assert_eq!(m, message, "{v}"),
            other => panic!("{v}: {other:?}"),
        }
    }
    // A Node ID is not an Edge.
    match w
        .caller()
        .write_edge(
            w.tenant,
            EdgeId(ada.0),
            &Patch::new(ep, ValidTime::UNKNOWN, Map::new()),
        )
        .await
    {
        Err(Error::InvalidWrite(m)) => {
            assert_eq!(m, format!("Edge {ada} is not visible to the caller"))
        }
        other => panic!("{other:?}"),
    }

    // No Edge Kind may declare `holds`.
    match w
        .caller()
        .register_kind(&NewKind {
            tenant: Some(w.tenant),
            name: "owns".into(),
            of: KindOf::Edge {
                source: vec![],
                target: vec![],
            },
            parent: None,
            properties: json!({ "properties": { "holds": { "type": "boolean" } } }),
            search: vec![],
        })
        .await
    {
        Err(Error::InvalidKind(m)) => assert_eq!(
            m,
            "property name \"holds\" is reserved: every Edge records through it where the Edge holds"
        ),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn an_edges_endpoints_may_be_in_any_scope_of_the_tenant_the_caller_can_see() {
    let w = World::new().await;
    let ep = w.episode(day(1)).await;
    let node_in = |scope: &'static str| {
        let w = &w;
        async move {
            let caller = w.store.as_worker(w.tenant, Scope::new(scope));
            let ep = caller
                .record_episode(&NewEpisode {
                    tenant: Some(w.tenant),
                    scope: Scope::new(scope),
                    content: format!("a Node in {scope}"),
                    reference_time: day(1),
                    authority: None,
                })
                .await
                .unwrap();
            caller
                .create_node(
                    &NewNode {
                        tenant: Some(w.tenant),
                        scope: Scope::new(scope),
                        kind: ROOT_NODE_KIND.into(),
                    },
                    &Patch::new(ep, ValidTime::UNKNOWN, values(json!({ "name": scope }))),
                )
                .await
                .unwrap()
                .0
        }
    };
    let mine = node_in(SCOPE).await;
    let shared = node_in("shared").await;
    let hidden = node_in("hidden").await;

    // A user who may see `support` and `shared` joins a Node in each with an
    // Edge in `support`.
    let user = w.user(&[SCOPE, "shared"]);
    let (edge, _) = user
        .create_edge(
            &w.new_edge(ROOT_EDGE_KIND, mine, shared),
            &Patch::new(ep, ValidTime::UNKNOWN, Map::new()),
        )
        .await
        .unwrap();
    let v = user
        .read_edge(w.tenant, edge, AsOf::current(day(5)))
        .await
        .unwrap()
        .unwrap();
    assert_eq!((v.scope.as_str(), v.target), (SCOPE, shared));
    let steps = user
        .traverse(w.tenant, shared, AsOf::current(day(5)), &Traversal::all())
        .await
        .unwrap();
    assert_eq!(steps.len(), 1, "{steps:?}");
    assert_eq!(steps[0].neighbour, mine);

    // A Node in a Scope the user cannot see is refused as if it did not
    // exist, at either end.
    for (source, target, side, missing) in [
        (mine, hidden, "target", hidden),
        (hidden, mine, "source", hidden),
        (
            mine,
            NodeId(uuid::Uuid::new_v4()),
            "target",
            NodeId(uuid::Uuid::nil()),
        ),
    ] {
        let missing = if missing.0.is_nil() { target } else { missing };
        match user
            .create_edge(
                &w.new_edge(ROOT_EDGE_KIND, source, target),
                &Patch::new(ep, ValidTime::UNKNOWN, Map::new()),
            )
            .await
        {
            Err(Error::InvalidWrite(m)) => assert_eq!(
                m,
                format!("the {side} Node {missing} is not visible to the caller")
            ),
            other => panic!("{source} to {target}: {other:?}"),
        }
    }

    // A worker of `support` alone cannot see `shared`, so traversal from its
    // own Node leaves the Edge out, and reading the Edge finds nothing, so no
    // read names the Node it cannot see.
    let steps = w
        .caller()
        .traverse(w.tenant, mine, AsOf::current(day(5)), &Traversal::all())
        .await
        .unwrap();
    assert!(steps.is_empty(), "{steps:?}");
    assert_eq!(w.read_edge(edge, AsOf::current(day(5))).await, None);
}

#[tokio::test]
async fn an_edge_to_a_node_in_another_tenant_is_refused() {
    let w = World::new().await;
    let other = TenantId(uuid::Uuid::new_v4());
    let theirs = {
        let caller = w.store.as_worker(other, Scope::new(SCOPE));
        let ep = caller
            .record_episode(&NewEpisode {
                tenant: Some(other),
                scope: Scope::new(SCOPE),
                content: "another Tenant's Episode".into(),
                reference_time: day(1),
                authority: None,
            })
            .await
            .unwrap();
        caller
            .create_node(
                &NewNode {
                    tenant: Some(other),
                    scope: Scope::new(SCOPE),
                    kind: ROOT_NODE_KIND.into(),
                },
                &Patch::new(ep, ValidTime::UNKNOWN, values(json!({ "name": "theirs" }))),
            )
            .await
            .unwrap()
            .0
    };
    let mine = w.node(ROOT_NODE_KIND, "mine").await;
    let ep = w.episode(day(1)).await;
    for (source, target, side) in [(mine, theirs, "target"), (theirs, mine, "source")] {
        match w
            .caller()
            .create_edge(
                &w.new_edge(ROOT_EDGE_KIND, source, target),
                &Patch::new(ep, ValidTime::UNKNOWN, Map::new()),
            )
            .await
        {
            Err(Error::InvalidWrite(m)) => {
                assert_eq!(
                    m,
                    format!("the {side} Node {theirs} is not visible to the caller")
                )
            }
            other => panic!("{other:?}"),
        }
    }
    // Naming the other Tenant fails before anything is written.
    let mut edge = w.new_edge(ROOT_EDGE_KIND, mine, theirs);
    edge.tenant = Some(other);
    assert!(matches!(
        w.caller()
            .create_edge(&edge, &Patch::new(ep, ValidTime::UNKNOWN, Map::new()))
            .await,
        Err(Error::TenantMismatch(_))
    ));
}

#[tokio::test]
async fn reads_and_traversal_return_each_edge_versions_sentence_or_none_until_one_is_stored() {
    let w = World::new().await;
    let (ada, acme) = (
        w.node(ROOT_NODE_KIND, "Ada").await,
        w.node(ROOT_NODE_KIND, "Acme").await,
    );
    let edge = w
        .edge(
            ROOT_EDGE_KIND,
            ada,
            acme,
            day(1),
            ValidTime::since(day(10)),
            json!({}),
        )
        .await;
    let now = AsOf::current(day(15));
    let v = w.read_edge(edge, now).await.unwrap();
    assert_eq!(v.sentence, None);
    let steps = w
        .caller()
        .traverse(w.tenant, ada, now, &Traversal::all())
        .await
        .unwrap();
    assert_eq!(steps[0].edge.sentence, None);

    // The Pipeline stores it after the Version committed.
    w.caller()
        .store_sentence(w.tenant, v.id, "Ada works at Acme.")
        .await
        .unwrap();

    let read = w.read_edge(edge, now).await.unwrap();
    assert_eq!(read.sentence.as_deref(), Some("Ada works at Acme."));
    for node in [ada, acme] {
        let steps = w
            .caller()
            .traverse(w.tenant, node, now, &Traversal::all())
            .await
            .unwrap();
        assert_eq!(steps.len(), 1, "{steps:?}");
        assert_eq!(steps[0].edge, read);
    }

    // A later Version has no Sentence until the Pipeline writes its own, and
    // the earlier one keeps its Sentence As-of before the change.
    let later = w.episode(day(2)).await;
    let outcome = w
        .caller()
        .write_edge(
            w.tenant,
            edge,
            &Patch::new(later, ValidTime::since(day(5)), Map::new()),
        )
        .await
        .unwrap();
    assert_eq!(w.read_edge(edge, now).await.unwrap().sentence, None);
    let before = w
        .read_edge(
            edge,
            AsOf::at(
                day(15),
                outcome.recorded_at - chrono::Duration::microseconds(1),
            ),
        )
        .await
        .unwrap();
    assert_eq!(before.sentence.as_deref(), Some("Ada works at Acme."));
}

#[tokio::test]
async fn traversal_as_of_an_instant_returns_the_edges_and_neighbouring_nodes_that_held_then() {
    let w = World::new().await;
    w.ontology().await;
    let ada = w.node("person", "Ada").await;
    let acme = w.node("company", "Acme").await;
    let initech = w.node("company", "Initech").await;
    let bob = w.node("employee", "Bob").await;

    // Ada worked at Acme from day 10 to day 20, then at Initech, where she
    // leads from day 20. Bob works at Acme with no stated Valid time.
    let at_acme = w
        .edge(
            "works_at",
            ada,
            acme,
            day(1),
            ValidTime::between(day(10), day(20)),
            json!({ "role": "engineer" }),
        )
        .await;
    let at_initech = w
        .edge(
            "leads",
            ada,
            initech,
            day(1),
            ValidTime::since(day(20)),
            json!({}),
        )
        .await;
    let bob_at_acme = w
        .edge("works_at", bob, acme, day(1), ValidTime::UNKNOWN, json!({}))
        .await;

    // Acme was renamed on day 15.
    let renamed = w.episode(day(2)).await;
    w.caller()
        .write_node(
            w.tenant,
            acme,
            &Patch::new(
                renamed,
                ValidTime::since(day(15)),
                values(json!({ "name": "Acme Corp" })),
            ),
        )
        .await
        .unwrap();

    let follow = |node, valid_at, direction, kinds: &[&str]| {
        let w = &w;
        let traversal = Traversal {
            direction,
            kinds: kinds.iter().map(|k| k.to_string()).collect(),
        };
        async move {
            w.caller()
                .traverse(w.tenant, node, AsOf::current(day(valid_at)), &traversal)
                .await
                .unwrap()
                .into_iter()
                .map(|s| {
                    (
                        s.edge.edge,
                        s.direction,
                        s.neighbour,
                        s.neighbour_version
                            .map(|v| v.values["name"].as_str().unwrap().to_owned()),
                    )
                })
                .collect::<Vec<_>>()
        }
    };
    let name = |n: &str| Some(n.to_owned());

    assert_eq!(
        follow(ada, 12, Direction::Outgoing, &[]).await,
        vec![(at_acme, Direction::Outgoing, acme, name("Acme"))]
    );
    // On day 17 the neighbour reads as it stood then.
    assert_eq!(
        follow(ada, 17, Direction::Both, &[]).await,
        vec![(at_acme, Direction::Outgoing, acme, name("Acme Corp"))]
    );
    assert_eq!(
        follow(ada, 25, Direction::Both, &[]).await,
        vec![(at_initech, Direction::Outgoing, initech, name("Initech"))]
    );
    assert_eq!(follow(ada, 25, Direction::Incoming, &[]).await, vec![]);
    assert_eq!(follow(ada, 5, Direction::Both, &[]).await, vec![]);

    let mut into_acme = vec![
        (at_acme, Direction::Incoming, ada, name("Ada")),
        (bob_at_acme, Direction::Incoming, bob, name("Bob")),
    ];
    into_acme.sort_by_key(|s| s.0);
    assert_eq!(follow(acme, 12, Direction::Incoming, &[]).await, into_acme);
    assert_eq!(
        follow(acme, 25, Direction::Incoming, &[]).await,
        vec![(bob_at_acme, Direction::Incoming, bob, name("Bob"))]
    );
    assert_eq!(follow(acme, 12, Direction::Outgoing, &[]).await, vec![]);

    // A Kind filter takes the Kinds below the named ones too.
    assert_eq!(
        follow(ada, 25, Direction::Both, &["works_at"]).await,
        vec![(at_initech, Direction::Outgoing, initech, name("Initech"))]
    );
    assert_eq!(follow(ada, 25, Direction::Both, &["edge"]).await.len(), 1);
    assert_eq!(follow(ada, 12, Direction::Both, &["leads"]).await, vec![]);

    // As-of a known-at before Acme's rename, day 17 read Acme by its old name.
    let before_rename = w
        .read_edge(bob_at_acme, AsOf::current(day(1)))
        .await
        .unwrap()
        .recorded_from;
    let steps = w
        .caller()
        .traverse(
            w.tenant,
            bob,
            AsOf::at(day(17), before_rename),
            &Traversal::all(),
        )
        .await
        .unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(
        steps[0].neighbour_version.as_ref().unwrap().values["name"],
        "Acme"
    );
}

#[tokio::test]
async fn a_node_with_no_versions_stays_as_a_bare_identity_while_an_edge_joins_it() {
    let w = World::new().await;
    let ada = w.node(ROOT_NODE_KIND, "Ada").await;
    // A Node whose only write states nothing holds no Version.
    let ep = w.episode(day(1)).await;
    let (bare, written) = w
        .caller()
        .create_node(
            &NewNode {
                tenant: Some(w.tenant),
                scope: Scope::new(SCOPE),
                kind: ROOT_NODE_KIND.into(),
            },
            &Patch::new(ep, ValidTime::UNKNOWN, values(json!({ "name": null }))),
        )
        .await
        .unwrap();
    assert!(written.opened.is_empty());
    let edge = w
        .edge(
            ROOT_EDGE_KIND,
            ada,
            bare,
            day(1),
            ValidTime::UNKNOWN,
            json!({}),
        )
        .await;

    let now = AsOf::current(day(5));
    assert_eq!(
        w.caller().read_node(w.tenant, bare, now).await.unwrap(),
        None
    );
    let steps = w
        .caller()
        .traverse(w.tenant, ada, now, &Traversal::all())
        .await
        .unwrap();
    assert_eq!(steps.len(), 1, "{steps:?}");
    assert_eq!((steps[0].edge.edge, steps[0].neighbour), (edge, bare));
    assert_eq!(steps[0].neighbour_version, None);
    let steps = w
        .caller()
        .traverse(w.tenant, bare, now, &Traversal::all())
        .await
        .unwrap();
    assert_eq!(steps.len(), 1, "{steps:?}");
    assert_eq!(steps[0].neighbour, ada);

    // The Edge keeps the bare Node: not even the operator can remove it while
    // the Edge joins it.
    let err =
        w.db.operator()
            .await
            .execute_typed(
                "delete from gnomon.node where id = $1",
                &[(&bare.0, tokio_postgres::types::Type::UUID)],
            )
            .await
            .unwrap_err();
    assert_eq!(
        err.code(),
        Some(&tokio_postgres::error::SqlState::FOREIGN_KEY_VIOLATION),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_batch_as_of_read_returns_each_nodes_version_in_the_order_given() {
    let w = World::new().await;
    let ada = w.node(ROOT_NODE_KIND, "Ada").await;
    let acme = w.node(ROOT_NODE_KIND, "Acme").await;
    let initech = w.node(ROOT_NODE_KIND, "Initech").await;
    let renamed = w.episode(day(2)).await;
    let outcome = w
        .caller()
        .write_node(
            w.tenant,
            acme,
            &Patch::new(
                renamed,
                ValidTime::since(day(15)),
                values(json!({ "name": "Acme Corp" })),
            ),
        )
        .await
        .unwrap();
    let unknown = NodeId(uuid::Uuid::new_v4());

    let names = |as_of| {
        let w = &w;
        async move {
            w.caller()
                .read_nodes(w.tenant, &[initech, acme, unknown, ada, acme], as_of)
                .await
                .unwrap()
                .into_iter()
                .map(|v| (v.node, v.values["name"].as_str().unwrap().to_owned()))
                .collect::<Vec<_>>()
        }
    };
    let expect = |acme_name: &str| {
        vec![
            (initech, "Initech".to_owned()),
            (acme, acme_name.to_owned()),
            (ada, "Ada".to_owned()),
        ]
    };
    assert_eq!(names(AsOf::current(day(10))).await, expect("Acme"));
    assert_eq!(names(AsOf::current(day(20))).await, expect("Acme Corp"));
    assert_eq!(
        names(AsOf::at(
            day(20),
            outcome.recorded_at - chrono::Duration::microseconds(1)
        ))
        .await,
        expect("Acme")
    );
    // Each Version is the one a single read returns.
    let batch = w
        .caller()
        .read_nodes(w.tenant, &[acme], AsOf::current(day(20)))
        .await
        .unwrap();
    let single = w
        .caller()
        .read_node(w.tenant, acme, AsOf::current(day(20)))
        .await
        .unwrap();
    assert_eq!(batch, single.into_iter().collect::<Vec<_>>());
    assert!(
        w.caller()
            .read_nodes(w.tenant, &[], AsOf::current(day(20)))
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn a_second_write_to_an_edge_in_one_transaction_replaces_the_first_in_place() {
    let w = World::new().await;
    w.ontology().await;
    let (ada, acme) = (
        w.node("person", "Ada").await,
        w.node("company", "Acme").await,
    );
    let edge = w
        .edge(
            "works_at",
            ada,
            acme,
            day(1),
            ValidTime::since(day(10)),
            json!({}),
        )
        .await;
    let ep = w.episode(day(2)).await;
    let outcomes = w
        .caller()
        .write_edges(
            w.tenant,
            &[
                (
                    edge,
                    Patch::new(
                        ep,
                        ValidTime::since(day(10)),
                        values(json!({ "role": "intern" })),
                    ),
                ),
                (
                    edge,
                    Patch::new(
                        ep,
                        ValidTime::since(day(10)),
                        values(json!({ "role": "engineer" })),
                    ),
                ),
            ],
        )
        .await
        .unwrap();
    let v = w.read_edge(edge, AsOf::current(day(12))).await.unwrap();
    assert_eq!(v.values["role"], "engineer");
    assert_eq!(outcomes[1].opened, vec![v.id]);
    // The intern Version was replaced in place, so no read at any known-at
    // instant can see it.
    let rows: i64 =
        w.db.admin()
            .await
            .query_typed(
                "select count(*) from gnomon.version where edge_id = $1",
                &[(&edge.0, tokio_postgres::types::Type::UUID)],
            )
            .await
            .unwrap()[0]
            .get(0);
    assert_eq!(rows, 2, "the first Version and the engineer Version");
}

#[tokio::test]
async fn ranked_merge_and_bound_statements_apply_to_edges_as_to_nodes() {
    let w = World::new().await;
    w.ontology().await;
    w.caller()
        .set_merge_rule(w.tenant, "works_at", MergeRule::Ranked)
        .await
        .unwrap();
    let (ada, acme) = (
        w.node("person", "Ada").await,
        w.node("company", "Acme").await,
    );
    let authority = |reference_time, authority| {
        let w = &w;
        async move {
            w.caller()
                .record_episode(&NewEpisode {
                    tenant: Some(w.tenant),
                    scope: Scope::new(SCOPE),
                    content: "an Episode with an Authority".into(),
                    reference_time,
                    authority: Some(authority),
                })
                .await
                .unwrap()
        }
    };
    let high = authority(day(1), 10).await;
    let low = authority(day(2), 0).await;
    let (edge, _) = w
        .caller()
        .create_edge(
            &w.new_edge("leads", ada, acme),
            &Patch::new(high, ValidTime::UNKNOWN, values(json!({ "role": "cto" }))),
        )
        .await
        .unwrap();
    w.caller()
        .write_edge(
            w.tenant,
            edge,
            &Patch::new(low, ValidTime::UNKNOWN, values(json!({ "role": "intern" }))),
        )
        .await
        .unwrap();
    let v = w.read_edge(edge, AsOf::current(day(12))).await.unwrap();
    assert_eq!(v.values["role"], "cto");
    assert_eq!(v.merge_rule, MergeRule::Ranked);

    // A Refinement states the start the Edge left unknown, and the Edge holds
    // from then.
    let outcome = w
        .caller()
        .state_edge_bound(
            w.tenant,
            edge,
            &BoundStatement {
                episode: high,
                version: v.id,
                side: BoundSide::Start,
                bound: Bound::Period {
                    at: day(5),
                    precision: Precision::Month,
                },
            },
        )
        .await
        .unwrap();
    assert_eq!(outcome.change, BoundChange::Refinement);
    let refined = w.read_edge(edge, AsOf::current(day(12))).await.unwrap();
    assert_eq!(refined.id, outcome.opened[0]);
    assert_eq!(
        refined.valid_time.from,
        Bound::Period {
            at: day(1),
            precision: Precision::Month
        }
    );
    assert!(refined.uncertain);
    assert_eq!(refined.values["role"], "cto");
    assert_eq!(refined.text_hash, v.text_hash);
    let before = Utc.with_ymd_and_hms(2023, 12, 31, 0, 0, 0).unwrap();
    assert_eq!(w.read_edge(edge, AsOf::current(before)).await, None);
}
