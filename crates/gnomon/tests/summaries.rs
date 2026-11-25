//! Seam 1, Summaries and Communities (ADR-0003, ADR-0014, ADR-0015,
//! migration 0018), through the Rust core's Store API against real Postgres:
//! writing and reading a Summary of a Node and a Community of Nodes with
//! their Citations, regenerating one by closing the old row, the one-Scope
//! rule, Tenant and Scope isolation, and Erasure of an Episode, a Node and a
//! Scope. The adversarial cases live in sql_boundary.rs, and the no-residue
//! test in erase_episode.rs covers all five tables.

mod support;

use gnomon::{
    AsOf, Community, CommunityId, DerivedCitation, EpisodeId, Error, NewCommunity, NewSummary,
    NodeId, Scope, Summary, SummaryId,
};
use serde_json::json;
use support::erasure::{at, World, ALL_SCOPES, SCOPE};
use tokio_postgres::types::Type;
use uuid::Uuid;

const BILLING: &str = "billing";

fn summary(scope: &str, node: NodeId, content: &str, cites: &[EpisodeId]) -> NewSummary {
    NewSummary {
        scope: Scope::new(scope),
        node,
        content: content.into(),
        as_of_valid: at(2024, 6, 1),
        citations: cites.iter().map(|e| DerivedCitation::of(*e)).collect(),
    }
}

fn community(
    scope: &str,
    members: &[NodeId],
    content: &str,
    cites: &[EpisodeId],
    replaces: &[CommunityId],
) -> NewCommunity {
    NewCommunity {
        scope: Scope::new(scope),
        replaces: replaces.to_vec(),
        members: members.to_vec(),
        content: content.into(),
        as_of_valid: at(2024, 6, 1),
        citations: cites.iter().map(|e| DerivedCitation::of(*e)).collect(),
    }
}

async fn write_summary(w: &World, s: &NewSummary) -> SummaryId {
    w.seeing(ALL_SCOPES)
        .write_summary(w.tenant, s)
        .await
        .unwrap()
}

async fn write_community(w: &World, c: &NewCommunity) -> CommunityId {
    w.seeing(ALL_SCOPES)
        .write_community(w.tenant, c)
        .await
        .unwrap()
}

/// The current Summaries of a Node that a user of every Scope reads at a
/// valid-at instant after every `as_of_valid` the tests write.
async fn current_summaries(w: &World, node: NodeId) -> Vec<Summary> {
    w.seeing(ALL_SCOPES)
        .read_summaries(w.tenant, node, AsOf::current(at(2025, 1, 1)))
        .await
        .unwrap()
}

async fn current_communities(w: &World, node: NodeId) -> Vec<Community> {
    w.seeing(ALL_SCOPES)
        .read_communities(w.tenant, node, AsOf::current(at(2025, 1, 1)))
        .await
        .unwrap()
}

/// How many rows of a table name `id` in `column`, past RLS.
async fn rows_naming(w: &World, table: &str, column: &str, id: Uuid) -> i64 {
    w.db.admin()
        .await
        .query_typed(
            &format!("select count(*) from gnomon.{table} where {column} = $1"),
            &[(&id, Type::UUID)],
        )
        .await
        .unwrap()[0]
        .get(0)
}

fn assert_invalid_write<T: std::fmt::Debug>(r: Result<T, Error>, says: &str, what: &str) {
    match r {
        Err(Error::InvalidWrite(m)) => assert!(m.contains(says), "{what}: {m}"),
        other => panic!("{what}: expected an invalid write saying {says:?}, got {other:?}"),
    }
}

#[tokio::test]
async fn a_summary_of_a_node_is_read_with_its_citations_from_the_valid_at_instant_it_was_written_for(
) {
    let w = World::new().await;
    let f = w.episode(at(2024, 1, 1), None).await;
    let g = w.episode(at(2024, 2, 1), None).await;
    let a = w
        .node(
            "account",
            f,
            gnomon::ValidTime::UNKNOWN,
            json!({ "plan": "pro" }),
        )
        .await;
    let mut new = summary(SCOPE, a, "An account on the pro plan since January.", &[f]);
    new.citations.push(DerivedCitation {
        episode: g,
        property: Some("plan".into()),
        place: Some("line 3".into()),
    });
    let id = write_summary(&w, &new).await;

    let read = current_summaries(&w, a).await;
    assert_eq!(read.len(), 1, "{read:?}");
    let s = &read[0];
    assert_eq!(s.id, id);
    assert_eq!(s.scope, Scope::new(SCOPE));
    assert_eq!(s.node, a);
    assert_eq!(s.content, "An account on the pro plan since January.");
    assert_eq!(s.as_of_valid, at(2024, 6, 1));
    assert_eq!(s.recorded_to, None, "it is current");
    let mut expected = new.citations.clone();
    expected.sort_by_key(|c| c.episode.0);
    assert_eq!(s.citations, expected);

    // At a valid-at instant before the one it was written for, a read finds
    // no Summary, and at that instant it finds this one (ADR-0015).
    for (valid_at, found) in [(at(2024, 5, 31), 0), (at(2024, 6, 1), 1)] {
        let n = w
            .caller()
            .read_summaries(w.tenant, a, AsOf::current(valid_at))
            .await
            .unwrap()
            .len();
        assert_eq!(n, found, "read at {valid_at}");
    }
}

#[tokio::test]
async fn regenerating_a_summary_closes_the_old_row_which_an_earlier_known_at_still_reads_and_deletes_nothing(
) {
    let w = World::new().await;
    let f = w.episode(at(2024, 1, 1), None).await;
    let g = w.episode(at(2024, 2, 1), None).await;
    let a = w
        .node(
            "account",
            f,
            gnomon::ValidTime::UNKNOWN,
            json!({ "plan": "pro" }),
        )
        .await;
    let first = write_summary(&w, &summary(SCOPE, a, "first", &[f])).await;
    let first_row = current_summaries(&w, a).await.remove(0);
    let second = write_summary(&w, &summary(SCOPE, a, "second", &[f, g])).await;

    let now = current_summaries(&w, a).await;
    assert_eq!(now.len(), 1, "one current Summary per Node and Scope");
    assert_eq!((now[0].id, now[0].content.as_str()), (second, "second"));

    // The first row is closed when the second was written, and a read at a
    // known-at instant before that still finds it with its Citation.
    let then = w
        .caller()
        .read_summaries(
            w.tenant,
            a,
            AsOf::at(at(2025, 1, 1), first_row.recorded_from),
        )
        .await
        .unwrap();
    assert_eq!(then.len(), 1, "{then:?}");
    assert_eq!((then[0].id, then[0].content.as_str()), (first, "first"));
    assert_eq!(then[0].recorded_to, Some(now[0].recorded_from));
    assert_eq!(then[0].citations, vec![DerivedCitation::of(f)]);

    // Nothing was deleted: both rows and all three Citations stand.
    assert_eq!(rows_naming(&w, "summary", "node_id", a.0).await, 2);
    assert_eq!(
        rows_naming(&w, "summary_citation", "summary_id", first.0).await,
        1
    );
    assert_eq!(
        rows_naming(&w, "summary_citation", "summary_id", second.0).await,
        2
    );
}

#[tokio::test]
async fn a_node_joined_by_an_edge_of_another_scope_has_one_summary_in_each_scope() {
    let w = World::new().await;
    let f = w.episode(at(2024, 1, 1), None).await;
    let b = w.episode_in(BILLING, at(2024, 1, 2), "billing's").await;
    let a = w
        .node(
            "account",
            f,
            gnomon::ValidTime::UNKNOWN,
            json!({ "plan": "pro" }),
        )
        .await;
    let acme = w
        .node_in(BILLING, "company", b, json!({ "name": "Acme" }))
        .await;
    w.edge_in(BILLING, a, acme, b, json!({ "share": 5 })).await;
    write_summary(&w, &summary(SCOPE, a, "support's account", &[f])).await;
    write_summary(&w, &summary(BILLING, a, "owns 5% of Acme", &[b])).await;

    let both = current_summaries(&w, a).await;
    let seen: Vec<(&str, &str)> = both
        .iter()
        .map(|s| (s.scope.as_str(), s.content.as_str()))
        .collect();
    assert_eq!(
        seen,
        vec![(BILLING, "owns 5% of Acme"), (SCOPE, "support's account")]
    );
    // Each Scope's caller reads only its own.
    for (scope, content) in [(SCOPE, "support's account"), (BILLING, "owns 5% of Acme")] {
        let own = w
            .seeing(&[scope])
            .read_summaries(w.tenant, a, AsOf::current(at(2025, 1, 1)))
            .await
            .unwrap();
        assert_eq!(own.len(), 1, "{scope}: {own:?}");
        assert_eq!(own[0].content, content);
    }
}

#[tokio::test]
async fn a_summary_cites_only_episodes_of_its_own_scope_and_at_least_one() {
    let w = World::new().await;
    let f = w.episode(at(2024, 1, 1), None).await;
    let b = w.episode_in(BILLING, at(2024, 1, 2), "billing's").await;
    let hr = w.episode_in("hr", at(2024, 1, 3), "hr's").await;
    let a = w
        .node(
            "account",
            f,
            gnomon::ValidTime::UNKNOWN,
            json!({ "plan": "pro" }),
        )
        .await;
    let both = w.seeing(&[SCOPE, BILLING]);

    // An Episode of another Scope the caller sees is refused by its Scope
    // (ADR-0014), and one of a Scope it cannot see is not visible.
    assert_invalid_write(
        both.write_summary(w.tenant, &summary(SCOPE, a, "mixed", &[f, b]))
            .await,
        "cites only Episodes of its own Scope",
        "an Episode of billing in a Summary of support",
    );
    assert_invalid_write(
        both.write_summary(w.tenant, &summary(SCOPE, a, "hidden", &[f, hr]))
            .await,
        "is not visible to the caller",
        "an Episode of a Scope the caller cannot see",
    );
    assert_invalid_write(
        both.write_summary(w.tenant, &summary(SCOPE, a, "uncited", &[]))
            .await,
        "at least one Episode",
        "a Summary with no Citation",
    );
    // A Summary in a Scope the caller cannot see, or of a Node it cannot see.
    assert_invalid_write(
        both.write_summary(w.tenant, &summary("hr", a, "elsewhere", &[hr]))
            .await,
        "Scope hr is not visible",
        "a Summary in a Scope the caller cannot see",
    );
    assert_invalid_write(
        w.seeing(&[BILLING])
            .write_summary(w.tenant, &summary(BILLING, a, "of support's Node", &[b]))
            .await,
        "is not visible to the caller",
        "a Summary of a Node the caller cannot see",
    );
    assert_eq!(rows_naming(&w, "summary", "node_id", a.0).await, 0);
}

#[tokio::test]
async fn another_tenant_and_another_scope_read_no_summary_or_community() {
    let w = World::new().await;
    let f = w.episode(at(2024, 1, 1), None).await;
    let a = w
        .node(
            "account",
            f,
            gnomon::ValidTime::UNKNOWN,
            json!({ "plan": "pro" }),
        )
        .await;
    write_summary(&w, &summary(SCOPE, a, "support's", &[f])).await;
    write_community(
        &w,
        &community(SCOPE, &[a], "support's Community", &[f], &[]),
    )
    .await;
    assert_eq!(current_summaries(&w, a).await.len(), 1);
    assert_eq!(current_communities(&w, a).await.len(), 1);

    let as_of = AsOf::current(at(2025, 1, 1));
    let billing = w.seeing(&[BILLING]);
    assert!(billing
        .read_summaries(w.tenant, a, as_of)
        .await
        .unwrap()
        .is_empty());
    assert!(billing
        .read_communities(w.tenant, a, as_of)
        .await
        .unwrap()
        .is_empty());
    let other = support::tenant();
    let stranger = w.store.as_worker(other, Scope::new(SCOPE));
    assert!(stranger
        .read_summaries(other, a, as_of)
        .await
        .unwrap()
        .is_empty());
    assert!(stranger
        .read_communities(other, a, as_of)
        .await
        .unwrap()
        .is_empty());
    // Naming the first Tenant from the second's claims is refused.
    assert!(matches!(
        stranger.read_summaries(w.tenant, a, as_of).await,
        Err(Error::TenantMismatch(_))
    ));
    assert!(matches!(
        stranger
            .write_summary(w.tenant, &summary(SCOPE, a, "forged", &[f]))
            .await,
        Err(Error::TenantMismatch(_))
    ));
}

#[tokio::test]
async fn a_community_is_read_with_its_nodes_and_citations_and_regenerating_it_closes_the_old_one() {
    let w = World::new().await;
    let f = w.episode(at(2024, 1, 1), None).await;
    let g = w.episode(at(2024, 2, 1), None).await;
    let a = w
        .node(
            "account",
            f,
            gnomon::ValidTime::UNKNOWN,
            json!({ "plan": "pro" }),
        )
        .await;
    let acme = w
        .node(
            "company",
            f,
            gnomon::ValidTime::UNKNOWN,
            json!({ "name": "Acme" }),
        )
        .await;
    let globex = w
        .node(
            "company",
            g,
            gnomon::ValidTime::UNKNOWN,
            json!({ "name": "Globex" }),
        )
        .await;
    let first = write_community(&w, &community(SCOPE, &[acme, a], "a and Acme", &[f], &[])).await;
    let first_row = current_communities(&w, a).await.remove(0);
    let mut expected_members = vec![a, acme];
    expected_members.sort_by_key(|n| n.0);
    assert_eq!(first_row.id, first);
    assert_eq!(first_row.members, expected_members);
    assert_eq!(first_row.content, "a and Acme");
    assert_eq!(first_row.citations, vec![DerivedCitation::of(f)]);
    assert_eq!(first_row.recorded_to, None);
    assert!(current_communities(&w, globex).await.is_empty());

    // Globex joins: the regenerated Community replaces the first.
    let second = write_community(
        &w,
        &community(
            SCOPE,
            &[a, acme, globex],
            "a, Acme and Globex",
            &[f, g],
            &[first],
        ),
    )
    .await;
    for node in [a, acme, globex] {
        let now = current_communities(&w, node).await;
        assert_eq!(now.len(), 1, "{now:?}");
        assert_eq!(now[0].id, second);
        assert_eq!(now[0].members.len(), 3);
    }
    let then = w
        .caller()
        .read_communities(
            w.tenant,
            a,
            AsOf::at(at(2025, 1, 1), first_row.recorded_from),
        )
        .await
        .unwrap();
    assert_eq!(then.len(), 1);
    assert_eq!(then[0].id, first);
    assert!(then[0].recorded_to.is_some(), "the first was closed");
    assert_eq!(
        rows_naming(&w, "community_member", "community_id", first.0).await,
        2,
        "nothing was deleted"
    );

    // A Community it already replaced is not current, so it cannot be
    // replaced again. Nor can a Node of no Community start one with none.
    assert_invalid_write(
        w.caller()
            .write_community(w.tenant, &community(SCOPE, &[a], "again", &[f], &[first]))
            .await,
        "is not a current Community",
        "replacing a closed Community",
    );
    assert_invalid_write(
        w.caller()
            .write_community(w.tenant, &community(SCOPE, &[], "empty", &[f], &[]))
            .await,
        "at least one Node",
        "a Community of no Node",
    );
}

#[tokio::test]
async fn a_community_holds_only_nodes_of_its_own_scope_even_one_the_caller_sees() {
    let w = World::new().await;
    let f = w.episode(at(2024, 1, 1), None).await;
    let b = w.episode_in(BILLING, at(2024, 1, 2), "billing's").await;
    let a = w
        .node(
            "account",
            f,
            gnomon::ValidTime::UNKNOWN,
            json!({ "plan": "pro" }),
        )
        .await;
    let acme = w
        .node_in(BILLING, "company", b, json!({ "name": "Acme" }))
        .await;
    // Edges of billing join both, but a Community is detected over one
    // Scope's Nodes (ADR-0014), and `a` is support's.
    w.edge_in(BILLING, a, acme, b, json!({ "share": 5 })).await;
    let both = w.seeing(&[SCOPE, BILLING]);
    assert_invalid_write(
        both.write_community(
            w.tenant,
            &community(BILLING, &[acme, a], "across", &[b], &[]),
        )
        .await,
        "holds only Nodes of its own Scope",
        "a Node of support in a Community of billing",
    );
    assert_invalid_write(
        both.write_community(
            w.tenant,
            &community(SCOPE, &[a], "cites billing", &[b], &[]),
        )
        .await,
        "cites only Episodes of its own Scope",
        "a Community citing another Scope's Episode",
    );
    let admin = w.db.admin().await;
    let n: i64 = admin
        .query_typed("select count(*) from gnomon.community", &[])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(n, 0);
    // In billing alone it is written.
    write_community(&w, &community(BILLING, &[acme], "Acme", &[b], &[])).await;
    assert_eq!(current_communities(&w, acme).await.len(), 1);
}

#[tokio::test]
async fn retract_episode_leaves_every_summary_and_community_as_it_was() {
    let w = World::new().await;
    let f = w.episode(at(2024, 1, 1), None).await;
    let e = w.episode(at(2024, 2, 1), None).await;
    let a = w
        .node(
            "account",
            f,
            gnomon::ValidTime::UNKNOWN,
            json!({ "plan": "pro" }),
        )
        .await;
    write_summary(&w, &summary(SCOPE, a, "cites e", &[e])).await;
    write_community(&w, &community(SCOPE, &[a], "cites e", &[e], &[])).await;
    let (summaries, communities) = (
        current_summaries(&w, a).await,
        current_communities(&w, a).await,
    );
    w.caller().retract_episode(w.tenant, e).await.unwrap();
    assert_eq!(current_summaries(&w, a).await, summaries);
    assert_eq!(current_communities(&w, a).await, communities);
}

#[tokio::test]
async fn erase_episode_removes_every_summary_and_community_closed_or_open_that_cites_it_and_no_other(
) {
    let w = World::new().await;
    let f = w.episode(at(2024, 1, 1), None).await;
    let e = w.episode(at(2024, 2, 1), None).await;
    let a = w
        .node(
            "account",
            f,
            gnomon::ValidTime::UNKNOWN,
            json!({ "plan": "pro" }),
        )
        .await;
    let c = w
        .node(
            "company",
            f,
            gnomon::ValidTime::UNKNOWN,
            json!({ "name": "Acme" }),
        )
        .await;
    // a's Summary cited E, and its regeneration cites F alone. E is
    // retracted, which changes nothing: its Citations still count.
    let closed = write_summary(&w, &summary(SCOPE, a, "cites e", &[f, e])).await;
    let open = write_summary(&w, &summary(SCOPE, a, "cites f", &[f])).await;
    let c_summary = write_summary(&w, &summary(SCOPE, c, "c cites e", &[e])).await;
    let closed_community = write_community(&w, &community(SCOPE, &[a, c], "e's", &[e], &[])).await;
    let open_community = write_community(
        &w,
        &community(SCOPE, &[a, c], "f's", &[f], &[closed_community]),
    )
    .await;
    let other_community =
        write_community(&w, &community(SCOPE, &[c], "e and f", &[f, e], &[])).await;
    w.caller().retract_episode(w.tenant, e).await.unwrap();

    w.erase(e).await;

    for (table, id) in [
        ("summary", closed.0),
        ("summary", c_summary.0),
        ("community", closed_community.0),
        ("community", other_community.0),
    ] {
        assert!(
            !w.exists(table, id).await,
            "{table} {id} cites E and stands"
        );
    }
    for (table, column, id) in [
        ("summary_citation", "summary_id", closed.0),
        ("summary_citation", "summary_id", c_summary.0),
        ("community_citation", "community_id", closed_community.0),
        ("community_member", "community_id", closed_community.0),
        ("community_citation", "community_id", other_community.0),
        ("community_member", "community_id", other_community.0),
        ("summary_citation", "episode_id", e.0),
        ("community_citation", "episode_id", e.0),
    ] {
        assert_eq!(
            rows_naming(&w, table, column, id).await,
            0,
            "{table}.{column}"
        );
    }
    // What cites F alone stands, and is current.
    assert!(w.exists("summary", open.0).await);
    assert!(w.exists("community", open_community.0).await);
    assert_eq!(current_summaries(&w, a).await[0].id, open);
    assert!(
        current_summaries(&w, c).await.is_empty(),
        "c's Summary goes"
    );
    let communities: Vec<CommunityId> = current_communities(&w, c)
        .await
        .into_iter()
        .map(|k| k.id)
        .collect();
    assert_eq!(communities, vec![open_community]);
}

#[tokio::test]
async fn erasing_a_node_removes_its_summaries_in_every_scope_even_one_the_caller_cannot_see_and_its_communities(
) {
    let w = World::new().await;
    let f = w.episode(at(2024, 1, 1), None).await;
    let b = w.episode_in(BILLING, at(2024, 1, 2), "billing's").await;
    let x = w
        .node(
            "account",
            f,
            gnomon::ValidTime::UNKNOWN,
            json!({ "plan": "pro" }),
        )
        .await;
    let y = w
        .node(
            "account",
            f,
            gnomon::ValidTime::UNKNOWN,
            json!({ "plan": "basic" }),
        )
        .await;
    let acme = w
        .node_in(BILLING, "company", b, json!({ "name": "Acme" }))
        .await;
    w.edge_in(BILLING, x, acme, b, json!({ "share": 5 })).await;
    w.edge_in(BILLING, y, acme, b, json!({ "share": 6 })).await;
    // Summaries of x and of y in both Scopes, and Communities of support.
    let x_support = write_summary(&w, &summary(SCOPE, x, "x in support", &[f])).await;
    let x_billing = write_summary(&w, &summary(BILLING, x, "x in billing", &[b])).await;
    let y_billing = write_summary(&w, &summary(BILLING, y, "y in billing", &[b])).await;
    let y_support = write_summary(&w, &summary(SCOPE, y, "y in support", &[f])).await;
    let xy = write_community(&w, &community(SCOPE, &[x, y], "x and y", &[f], &[])).await;
    let just_y = write_community(&w, &community(SCOPE, &[y], "y", &[f], &[])).await;
    let billing_community =
        write_community(&w, &community(BILLING, &[acme], "Acme", &[b], &[])).await;

    // The worker of support erases x. It cannot see billing.
    let erasure = w.erase_node(x).await;
    assert_eq!(erasure.erased_nodes, vec![x]);

    for (table, id) in [
        ("summary", x_support.0),
        ("summary", x_billing.0),
        ("community", xy.0),
    ] {
        assert!(!w.exists(table, id).await, "{table} {id} of x stands");
    }
    assert_eq!(rows_naming(&w, "summary", "node_id", x.0).await, 0);
    assert_eq!(
        rows_naming(&w, "summary_citation", "summary_id", x_billing.0).await,
        0
    );
    assert_eq!(rows_naming(&w, "community_member", "node_id", x.0).await, 0);
    // The reach is only to a Summary of the Node the Erasure listed: y's
    // Summary in billing, which the worker cannot see either, stands, and so
    // does every row that does not draw on x.
    for (table, id) in [
        ("summary", y_billing.0),
        ("summary", y_support.0),
        ("community", just_y.0),
        ("community", billing_community.0),
    ] {
        assert!(w.exists(table, id).await, "{table} {id} went with x");
    }
    assert_eq!(
        rows_naming(&w, "summary_citation", "summary_id", y_billing.0).await,
        1
    );
    // The Erasure's list is empty again.
    let left: i64 =
        w.db.admin()
            .await
            .query_typed("select count(*) from gnomon.erasure_reach", &[])
            .await
            .unwrap()[0]
            .get(0);
    assert_eq!(left, 0);
}

#[tokio::test]
async fn erasing_a_scope_removes_its_summaries_and_communities_and_the_summaries_of_its_nodes_elsewhere(
) {
    let w = World::new().await;
    let h = w.episode_in("hr", at(2024, 1, 1), "hr's").await;
    let b = w.episode_in(BILLING, at(2024, 1, 2), "billing's").await;
    let f = w.episode(at(2024, 1, 3), None).await;
    let person = w
        .node_in("hr", "account", h, json!({ "plan": "staff" }))
        .await;
    let acme = w
        .node_in(BILLING, "company", b, json!({ "name": "Acme" }))
        .await;
    let a = w
        .node(
            "account",
            f,
            gnomon::ValidTime::UNKNOWN,
            json!({ "plan": "pro" }),
        )
        .await;
    w.edge_in(BILLING, person, acme, b, json!({ "share": 1 }))
        .await;
    let dept = w
        .node_in("hr", "company", h, json!({ "name": "People" }))
        .await;
    w.edge_in("hr", a, dept, h, json!({ "share": 2 })).await;
    // hr's own Summaries and Community, among them a Summary in hr of
    // support's Node a; a Summary in billing of hr's Node; and rows that owe
    // nothing to hr.
    let in_hr = write_summary(&w, &summary("hr", person, "in hr", &[h])).await;
    let a_in_hr = write_summary(&w, &summary("hr", a, "a seen from hr", &[h])).await;
    let hr_community = write_community(&w, &community("hr", &[person], "hr", &[h], &[])).await;
    let person_in_billing = write_summary(&w, &summary(BILLING, person, "in billing", &[b])).await;
    let acme_in_billing = write_summary(&w, &summary(BILLING, acme, "Acme", &[b])).await;
    let a_in_support = write_summary(&w, &summary(SCOPE, a, "a", &[f])).await;
    let billing_community =
        write_community(&w, &community(BILLING, &[acme], "Acme", &[b], &[])).await;

    // The worker of hr erases hr. It cannot see billing or support.
    w.erase_scope("hr").await;

    for (table, id) in [
        ("summary", in_hr.0),
        ("summary", a_in_hr.0),
        ("community", hr_community.0),
        ("summary", person_in_billing.0),
    ] {
        assert!(!w.exists(table, id).await, "{table} {id} stands");
    }
    for (table, id) in [
        ("summary", acme_in_billing.0),
        ("summary", a_in_support.0),
        ("community", billing_community.0),
    ] {
        assert!(w.exists(table, id).await, "{table} {id} went with hr");
    }
    let admin = w.db.admin().await;
    for table in [
        "summary",
        "summary_citation",
        "community",
        "community_member",
        "community_citation",
    ] {
        let n: i64 = admin
            .query_typed(
                &format!("select count(*) from gnomon.{table} where scope = 'hr'"),
                &[],
            )
            .await
            .unwrap()[0]
            .get(0);
        assert_eq!(n, 0, "gnomon.{table} keeps rows of hr");
    }
}

#[tokio::test]
async fn a_node_an_erasure_would_leave_bare_stays_while_a_summary_that_cites_another_episode_names_it(
) {
    let w = World::new().await;
    let f = w.episode(at(2024, 1, 1), None).await;
    let e = w.episode(at(2024, 2, 1), None).await;
    // Only E describes b, but b's Summary cites F alone.
    let b = w
        .node(
            "account",
            e,
            gnomon::ValidTime::UNKNOWN,
            json!({ "plan": "pro" }),
        )
        .await;
    let s = write_summary(&w, &summary(SCOPE, b, "cites f", &[f])).await;
    let erasure = w.erase(e).await;
    assert!(!erasure.erased_nodes.contains(&b), "{erasure:?}");
    assert!(w.exists("node", b.0).await, "b stays as a bare identity");
    assert!(w.exists("summary", s.0).await);
    // Erasing b itself takes its Summary.
    w.erase_node(b).await;
    assert!(!w.exists("summary", s.0).await);
}

#[tokio::test]
async fn a_summary_write_commits_the_recorded_stage_records_with_it() {
    let w = World::new().await;
    let f = w.episode(at(2024, 1, 1), None).await;
    let a = w
        .node(
            "account",
            f,
            gnomon::ValidTime::UNKNOWN,
            json!({ "plan": "pro" }),
        )
        .await;
    let recording = w
        .caller()
        .recording(vec![gnomon::StageWrite::pending(f, "summaries")]);
    recording
        .write_summary(w.tenant, &summary(SCOPE, a, "with its stage", &[f]))
        .await
        .unwrap();
    let stages: Vec<String> = w
        .stage_records_of(SCOPE, f)
        .await
        .into_iter()
        .map(|r| r.stage)
        .collect();
    assert_eq!(stages, vec!["summaries".to_owned()]);
}
