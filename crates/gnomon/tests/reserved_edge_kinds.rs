//! Seam 1, the reserved Edge Kinds `same_as` and `different_from`
//! (ADR-0012): Resolution records its findings as Edges, not as merged
//! Nodes. Every Ontology has both Kinds without registering them, each joins
//! any Node to any other Node, both are symmetric, and no consumer can
//! register, redefine or specialize them. They are ordinary Edges, so they
//! carry Citations, end by a Retraction and are erased like any Edge, and
//! reads do not collapse the Nodes they join. The adversarial cases live in
//! sql_boundary.rs.

mod support;

use gnomon::{
    AsOf, DIFFERENT_FROM_KIND, Direction, EdgeId, EpisodeId, Error, Kind, KindOf, MergeRule,
    NewEdge, NewEpisode, NewKind, NewNode, NodeId, Patch, ROOT_EDGE_KIND, ROOT_NODE_KIND,
    Retraction, SAME_AS_KIND, Scope, Store, TenantId, Traversal, ValidTime, WriteStatus,
};
use jiff::{Timestamp, civil::date, tz::TimeZone};
use serde_json::{Map, Value, json};
use std::sync::Arc;
use support::TestDb;

const SCOPE: &str = "support";
const RESERVED: [&str; 2] = [SAME_AS_KIND, DIFFERENT_FROM_KIND];

fn day(d: i8) -> Timestamp {
    date(2024, 1, d)
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

/// One database and a Store on it, with a worker in SCOPE for any Tenant.
struct World {
    db: Arc<TestDb>,
    store: Store,
    tenant: TenantId,
}

impl World {
    /// A fresh deployment, which has granted no one Kind registration.
    async fn new() -> World {
        let db = TestDb::migrated().await;
        let store = Store::connect(&db.login_url(), &support::connect_options())
            .await
            .unwrap();
        World {
            db: Arc::new(db),
            store,
            tenant: support::tenant(),
        }
    }

    /// Another Tenant on the same database, which first appears now.
    async fn another_tenant(&self) -> World {
        World {
            db: self.db.clone(),
            store: Store::connect(&self.db.login_url(), &support::connect_options())
                .await
                .unwrap(),
            tenant: support::tenant(),
        }
    }

    fn caller(&self) -> gnomon::Caller<'_> {
        self.store.as_worker(self.tenant, Scope::new(SCOPE), None)
    }

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

    fn new_edge(&self, kind: &str, source: NodeId, target: NodeId) -> NewEdge {
        NewEdge {
            tenant: Some(self.tenant),
            scope: Scope::new(SCOPE),
            kind: kind.to_owned(),
            source,
            target,
        }
    }

    /// Creates or addresses the Edge of `kind` from `source` to `target` and
    /// writes that it holds over `vt`, citing `ep`.
    async fn edge(
        &self,
        kind: &str,
        source: NodeId,
        target: NodeId,
        ep: EpisodeId,
        vt: ValidTime,
    ) -> (EdgeId, gnomon::WriteOutcome) {
        self.caller()
            .create_edge(
                &self.new_edge(kind, source, target),
                &Patch::new(ep, vt, Map::new()),
            )
            .await
            .unwrap()
    }

    /// The neighbours a traversal from `node` reaches, as (Edge, direction,
    /// neighbour).
    async fn steps(&self, node: NodeId, direction: Direction) -> Vec<(EdgeId, Direction, NodeId)> {
        self.caller()
            .traverse(
                self.tenant,
                node,
                AsOf::current(day(15)),
                &Traversal {
                    direction,
                    kinds: vec![],
                },
            )
            .await
            .unwrap()
            .into_iter()
            .map(|s| (s.edge.edge, s.direction, s.neighbour))
            .collect()
    }

    /// Registers Kinds as a deployment that granted Kind registration to the
    /// worker role.
    async fn register(&self, kind: NewKind) -> Result<Kind, Error> {
        self.caller().register_kind(&kind).await
    }
}

fn edge_kind(tenant: TenantId, name: &str, parent: Option<&str>) -> NewKind {
    NewKind {
        tenant: Some(tenant),
        name: name.into(),
        of: KindOf::Edge {
            source: vec![],
            target: vec![],
        },
        parent: parent.map(str::to_owned),
        properties: json!({ "type": "object", "properties": {} }),
        search: vec![],
    }
}

fn node_kind(tenant: TenantId, name: &str) -> NewKind {
    NewKind {
        tenant: Some(tenant),
        name: name.into(),
        of: KindOf::Node,
        parent: None,
        properties: json!({ "type": "object", "properties": {} }),
        search: vec![],
    }
}

/// Asserts that `world`'s Tenant reads both reserved Kinds, and that its root
/// Kinds and the registered Kinds in `registered` are not symmetric.
async fn assert_reserved_kinds(world: &World, registered: &[&str]) {
    for name in RESERVED {
        let kind = world
            .caller()
            .read_kind(world.tenant, name)
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("{name} is missing from a Tenant's Ontology"));
        assert_eq!(
            kind,
            Kind {
                tenant: world.tenant,
                name: name.into(),
                of: KindOf::Edge {
                    source: vec![ROOT_NODE_KIND.into()],
                    target: vec![ROOT_NODE_KIND.into()],
                },
                parent: Some(ROOT_EDGE_KIND.into()),
                ancestors: vec![ROOT_EDGE_KIND.into()],
                properties: json!({ "type": "object", "properties": {}, "required": [] }),
                rules: Default::default(),
                search: vec!["sentence".into()],
                symmetric: true,
            },
        );
    }
    for name in [ROOT_EDGE_KIND, ROOT_NODE_KIND].iter().chain(registered) {
        let kind = world.caller().read_kind(world.tenant, name).await.unwrap();
        assert!(!kind.unwrap().symmetric, "{name} is symmetric");
    }
}

#[tokio::test]
async fn both_kinds_exist_in_every_ontology_without_registration_and_join_any_node_kinds() {
    // A fresh deployment, which has granted no one Kind registration, reads
    // both Kinds.
    let w = World::new().await;
    assert_reserved_kinds(&w, &[]).await;
    // The Tenant then registers Node Kinds of its own, which leave the
    // reserved Kinds as they were.
    w.db.grant_kind_registration("gnomon_worker").await;
    w.register(node_kind(w.tenant, "person")).await.unwrap();
    w.register(node_kind(w.tenant, "company")).await.unwrap();
    assert_reserved_kinds(&w, &["person", "company"]).await;
    // Another Tenant first appears now, long after the migrations ran, and
    // registers nothing.
    let later = w.another_tenant().await;
    assert_reserved_kinds(&later, &[]).await;

    // Any Node Kind to any Node Kind, in either Tenant.
    let (ada, acme, root) = (
        w.node("person", "Ada").await,
        w.node("company", "Acme").await,
        w.node(ROOT_NODE_KIND, "Analytical Engine").await,
    );
    for name in RESERVED {
        for (source, target) in [(ada, acme), (acme, root), (root, ada)] {
            let ep = w.episode(day(2)).await;
            let (_, outcome) = w.edge(name, source, target, ep, ValidTime::UNKNOWN).await;
            assert_eq!(outcome.status, WriteStatus::Written, "{name}");
        }
    }
    let (x, y) = (
        later.node(ROOT_NODE_KIND, "x").await,
        later.node(ROOT_NODE_KIND, "y").await,
    );
    for name in RESERVED {
        let ep = later.episode(day(2)).await;
        let (_, outcome) = later.edge(name, x, y, ep, ValidTime::UNKNOWN).await;
        assert_eq!(outcome.status, WriteStatus::Written, "{name}");
    }
}

#[tokio::test]
async fn writing_the_edge_from_b_to_a_addresses_the_edge_from_a_to_b_and_traversal_finds_it_from_either()
 {
    let w = World::new().await;
    for name in RESERVED {
        let (a, b) = (
            w.node(ROOT_NODE_KIND, "Ada").await,
            w.node(ROOT_NODE_KIND, "A. Lovelace").await,
        );
        let (first, second) = (w.episode(day(2)).await, w.episode(day(3)).await);
        let vt = ValidTime::between(day(10), day(20));
        let (ab, written) = w.edge(name, a, b, first, vt).await;
        let (ba, again) = w.edge(name, b, a, second, vt).await;
        assert_eq!(ba, ab, "{name} from B to A is another Edge");
        assert_eq!(written.status, WriteStatus::Written);
        // The second write states what the Edge already holds, so it
        // corroborates the Version with its Citation.
        assert_eq!(again.status, WriteStatus::Unchanged, "{name}");
        let cited: Vec<EpisodeId> = w
            .caller()
            .list_citations(w.tenant, written.opened[0])
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.episode)
            .collect();
        assert_eq!(cited, vec![first, second], "{name}");

        // Either Node reaches the other through the one Edge, whichever
        // direction the traversal follows.
        for (from, to) in [(a, b), (b, a)] {
            for direction in [Direction::Outgoing, Direction::Incoming] {
                assert_eq!(
                    w.steps(from, direction).await,
                    vec![(ab, direction, to)],
                    "{name} from {from} following {direction:?}"
                );
            }
            assert_eq!(
                w.steps(from, Direction::Both).await,
                vec![(ab, Direction::Outgoing, to)],
                "{name} from {from} following both"
            );
        }
    }

    // An Edge of a Kind that is not symmetric keeps its direction: from B to
    // A is another Edge, and following Edges out of A finds only its own.
    let (a, b) = (
        w.node(ROOT_NODE_KIND, "a").await,
        w.node(ROOT_NODE_KIND, "b").await,
    );
    let ep = w.episode(day(2)).await;
    let (ab, _) = w.edge(ROOT_EDGE_KIND, a, b, ep, ValidTime::UNKNOWN).await;
    let (ba, _) = w.edge(ROOT_EDGE_KIND, b, a, ep, ValidTime::UNKNOWN).await;
    assert_ne!(ab, ba);
    assert_eq!(
        w.steps(a, Direction::Outgoing).await,
        vec![(ab, Direction::Outgoing, b)]
    );
}

#[tokio::test]
async fn a_reserved_edge_joins_two_different_nodes() {
    let w = World::new().await;
    let a = w.node(ROOT_NODE_KIND, "Ada").await;
    for name in RESERVED {
        let ep = w.episode(day(2)).await;
        let err = w
            .caller()
            .create_edge(
                &w.new_edge(name, a, a),
                &Patch::new(ep, ValidTime::UNKNOWN, Map::new()),
            )
            .await
            .unwrap_err();
        match err {
            Error::InvalidWrite(m) => assert_eq!(
                m,
                format!("an Edge of the symmetric Kind \"{name}\" joins two different Nodes")
            ),
            other => panic!("{name} from a Node to itself: {other:?}"),
        }
    }
}

#[tokio::test]
async fn kind_registration_refuses_to_redefine_reuse_or_specialize_either_name() {
    let w = World::new().await;
    w.db.grant_kind_registration("gnomon_worker").await;
    w.register(edge_kind(w.tenant, "knows", None))
        .await
        .unwrap();
    let before: Vec<Kind> = {
        let mut kinds = Vec::new();
        for name in RESERVED {
            kinds.push(w.caller().read_kind(w.tenant, name).await.unwrap().unwrap());
        }
        kinds
    };

    for name in RESERVED {
        // Registering the name again, as an Edge Kind with a definition of
        // its own, as a Kind below another Edge Kind, or as a Node Kind.
        for kind in [
            edge_kind(w.tenant, name, None),
            NewKind {
                properties: json!({
                    "type": "object",
                    "properties": { "confidence": { "type": "number" } }
                }),
                ..edge_kind(w.tenant, name, None)
            },
            edge_kind(w.tenant, name, Some("knows")),
            node_kind(w.tenant, name),
        ] {
            match w.register(kind.clone()).await {
                Err(Error::InvalidKind(m)) => assert_eq!(
                    m,
                    format!(
                        "\"{name}\" is a reserved Edge Kind, which every Ontology already has; \
                         a Kind cannot take its name"
                    ),
                    "{kind:?}"
                ),
                other => panic!("registering {kind:?}: {other:?}"),
            }
        }
        // A Kind below a reserved Kind.
        match w
            .register(edge_kind(w.tenant, "duplicate_of", Some(name)))
            .await
        {
            Err(Error::InvalidKind(_)) => {}
            other => panic!("specializing {name}: {other:?}"),
        }
        // Its merge rule, which is the recency rule for good.
        for rule in [MergeRule::Ranked, MergeRule::Recency] {
            match w.caller().set_merge_rule(w.tenant, name, rule).await {
                Err(Error::InvalidKind(_)) => {}
                other => panic!("setting the merge rule of {name} to {rule:?}: {other:?}"),
            }
        }
        assert_eq!(
            w.caller().merge_rule(w.tenant, name).await.unwrap(),
            MergeRule::Recency
        );
    }

    for (name, kind) in RESERVED.iter().zip(&before) {
        assert_eq!(
            w.caller().read_kind(w.tenant, name).await.unwrap().as_ref(),
            Some(kind),
            "{name} changed"
        );
    }
    assert_eq!(
        w.caller()
            .read_kind(w.tenant, "duplicate_of")
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn a_reserved_edge_carries_citations_ends_by_a_retraction_and_is_erased_like_any_edge() {
    let w = World::new().await;
    w.db.grant_erasure("gnomon_worker").await;
    let vt = ValidTime::between(day(10), day(20));
    let current = AsOf::current(day(15));
    for name in RESERVED {
        let (a, b) = (
            w.node(ROOT_NODE_KIND, "Ada").await,
            w.node(ROOT_NODE_KIND, "A. Lovelace").await,
        );
        let found = w.episode(day(2)).await;
        let (edge, written) = w.edge(name, a, b, found, vt).await;
        let version = written.opened[0];
        let citations = w.caller().list_citations(w.tenant, version).await.unwrap();
        assert_eq!(
            citations
                .iter()
                .map(|c| (c.episode, c.property.as_str()))
                .collect::<Vec<_>>(),
            vec![(found, "holds")],
            "{name}"
        );

        // A Retraction ends it, and it stays readable As-of before.
        let wrong = w.episode(day(3)).await;
        let retracted = w
            .caller()
            .retract_edge_version(
                w.tenant,
                edge,
                &Retraction {
                    episode: wrong,
                    version,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            w.caller().read_edge(w.tenant, edge, current).await.unwrap(),
            None
        );
        assert!(w.steps(a, Direction::Both).await.is_empty(), "{name}");
        let earlier = w
            .caller()
            .read_edge(w.tenant, edge, AsOf::at(day(15), written.recorded_at))
            .await
            .unwrap()
            .expect("the retracted Version is readable As-of before the Retraction");
        assert_eq!(earlier.id, version);
        assert!(retracted.recorded_at > written.recorded_at);

        // Stating it again, from B to A, addresses the same Edge, which
        // holds again.
        let again = w.episode(day(4)).await;
        let (same, rewritten) = w.edge(name, b, a, again, vt).await;
        assert_eq!(same, edge, "{name} after a Retraction is another Edge");
        assert_eq!(rewritten.status, WriteStatus::Written);
        assert_eq!(
            w.steps(b, Direction::Outgoing).await,
            vec![(edge, Direction::Outgoing, a)]
        );

        // Erasing the Edge removes it, and the pair then has none: stating it
        // again creates a new Edge, which either order addresses.
        let erased = w.caller().erase_edge(w.tenant, edge).await.unwrap();
        assert_eq!(erased.erased_edges, vec![edge]);
        assert_eq!(
            w.caller().read_edge(w.tenant, edge, current).await.unwrap(),
            None
        );
        assert!(w.steps(a, Direction::Both).await.is_empty(), "{name}");
        let ep = w.episode(day(5)).await;
        let (renewed, _) = w.edge(name, b, a, ep, vt).await;
        assert_ne!(renewed, edge);
        let (same, _) = w.edge(name, a, b, ep, vt).await;
        assert_eq!(same, renewed);

        // Erasing the one Episode it cites erases it with it.
        let erasure = w.caller().erase_episode(w.tenant, ep).await.unwrap();
        assert_eq!(erasure.erased_edges, vec![renewed], "{name}");
        assert!(w.steps(b, Direction::Both).await.is_empty(), "{name}");
    }
}

#[tokio::test]
async fn reads_and_traversal_do_not_collapse_two_nodes_joined_by_same_as() {
    let w = World::new().await;
    let (a, b) = (
        w.node(ROOT_NODE_KIND, "Ada").await,
        w.node(ROOT_NODE_KIND, "A. Lovelace").await,
    );
    let ep = w.episode(day(2)).await;
    w.edge(SAME_AS_KIND, a, b, ep, ValidTime::UNKNOWN).await;
    // A later write to one of them stays its own.
    let later = w.episode(day(3)).await;
    w.caller()
        .write_node(
            w.tenant,
            a,
            &Patch::new(
                later,
                ValidTime::UNKNOWN,
                values(json!({ "aliases": ["Countess"] })),
            ),
        )
        .await
        .unwrap();

    let now = AsOf::current(day(15));
    let held = |v: &gnomon::NodeVersion| Value::Object(v.values.clone());
    let (va, vb) = (
        w.caller()
            .read_node(w.tenant, a, now)
            .await
            .unwrap()
            .unwrap(),
        w.caller()
            .read_node(w.tenant, b, now)
            .await
            .unwrap()
            .unwrap(),
    );
    assert_eq!(held(&va), json!({ "name": "Ada", "aliases": ["Countess"] }));
    assert_eq!(held(&vb), json!({ "name": "A. Lovelace" }));

    let batch = w.caller().read_nodes(w.tenant, &[a, b], now).await.unwrap();
    assert_eq!(
        batch.iter().map(|v| (v.node, held(v))).collect::<Vec<_>>(),
        vec![(a, held(&va)), (b, held(&vb))]
    );

    let steps = w
        .caller()
        .traverse(w.tenant, a, now, &Traversal::all())
        .await
        .unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].neighbour, b);
    assert_eq!(
        steps[0].neighbour_version.as_ref().map(held),
        Some(held(&vb))
    );
}
