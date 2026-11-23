//! Seam 1, the Erasure of a Node, an Edge or a Scope (ADR-0003, ADR-0005,
//! ADR-0009, ADR-0013): each deletes what it names, every Edge that joins an
//! erased Node whatever the Edge's Scope, and each endpoint those Edges leave
//! with nothing in a Scope the caller sees, through the Rust core's Store API
//! against real Postgres. The oracle test is erase_oracle.rs, and the
//! adversarial cases live in sql_boundary.rs.

mod support;

use std::time::Duration;

use gnomon::{Bound, EndOfValidity, NewEdge, Patch, Scope, ValidTime};
use serde_json::json;
use support::erasure::{
    assert_no_residue, assert_only_removed, at, every_row, tables_with_a_scope, values, Residue,
    World, SCOPE,
};
use tokio_postgres::types::Type;
use uuid::Uuid;

/// Text only the erased rows hold, so a scan of every table can find any
/// copy of it.
const MARKER: &str = "erased-51c9";

#[tokio::test]
async fn erasing_an_edge_removes_its_rows_and_its_bare_endpoint_and_nothing_else() {
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    let f = w.episode(at(2024, 1, 2), None).await;
    let acme = w
        .node("company", f, ValidTime::UNKNOWN, json!({ "name": "Acme" }))
        .await;
    let a = w
        .node("account", f, ValidTime::UNKNOWN, json!({ "plan": "basic" }))
        .await;
    // b is a bare identity: no Episode describes it.
    let mut client = w.db.login().await;
    let tx = w.raw(&mut client).await;
    let b: Uuid = tx
        .query_typed(
            "select gnomon.create_node($1, $2, 'account')",
            &[(&w.tenant.0, Type::UUID), (&SCOPE, Type::TEXT)],
        )
        .await
        .unwrap()[0]
        .get(0);
    tx.commit().await.unwrap();
    let b = gnomon::NodeId(b);
    let erased = w
        .edge(b, acme, e, ValidTime::UNKNOWN, json!({ "share": 7 }))
        .await;
    w.write_edge(
        erased,
        f,
        ValidTime::between(at(2024, 3, 1), at(2024, 6, 1)),
        json!({ "share": 8 }),
    )
    .await;
    let version = w.current_edge_version(erased, at(2024, 4, 1)).await;
    w.store_sentence(version, &format!("{MARKER}: b owns 8% of Acme"))
        .await;
    let kept = w
        .edge(a, acme, f, ValidTime::UNKNOWN, json!({ "share": 40 }))
        .await;
    let kept_version = w.current_edge_version(kept, at(2024, 4, 1)).await;
    w.store_sentence(kept_version, "a owns 40% of Acme").await;
    let versions = w.versions_of(&[erased.0]).await;
    assert!(versions.len() >= 3, "{versions:?}");
    let admin = w.db.admin().await;
    let before = every_row(&admin).await;

    let erasure = w.erase_edge(erased).await;

    assert_eq!(erasure.erased_edges, vec![erased]);
    assert_eq!(erasure.erased_nodes, vec![b], "the bare endpoint goes");
    assert!(erasure.erased_episodes.is_empty());
    assert_no_residue(
        &admin,
        &Residue {
            nodes: &[b.0],
            edges: &[erased.0],
            versions: &versions,
            markers: &[MARKER],
            ..Residue::default()
        },
    )
    .await;
    // Only rows naming the Edge, its Versions or b went. The Episodes, Acme,
    // which still has Versions, and the other Edge and its Sentence stay.
    let mut gone = vec![erased.0, b.0];
    gone.extend(&versions);
    assert_only_removed(&admin, &before, &gone).await;
    assert!(w
        .caller()
        .read_episode(w.tenant, e)
        .await
        .unwrap()
        .is_some());
    assert_eq!(
        w.sentence_of(kept_version).await.as_deref(),
        Some("a owns 40% of Acme")
    );
    assert_eq!(
        w.current_values(acme, at(2024, 4, 1)).await,
        Some(json!({ "name": "Acme" }))
    );
}

#[tokio::test]
async fn erasing_a_node_erases_every_edge_that_joins_it_whatever_its_scope_and_the_bare_endpoints_the_caller_sees(
) {
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    let g = w.episode(at(2024, 1, 2), None).await;
    let billing = w
        .episode_in("billing", at(2024, 1, 3), "billing's Episode")
        .await;
    let hr = w.episode_in("hr", at(2024, 1, 4), "hr's Episode").await;

    // The Node to erase, with a value only it holds, a second write and
    // another Episode's statement about it.
    let x = w
        .node(
            "account",
            e,
            ValidTime::UNKNOWN,
            json!({ "plan": format!("{MARKER}-plan") }),
        )
        .await;
    w.write(
        x,
        g,
        ValidTime::between(at(2024, 3, 1), at(2024, 6, 1)),
        json!({ "plan": "gold" }),
    )
    .await;
    w.caller()
        .end_node_validity(
            w.tenant,
            x,
            &EndOfValidity {
                episode: g,
                at: Bound::At(at(2030, 1, 1)),
            },
        )
        .await
        .unwrap();
    // Billing's company, which keeps its Versions, joined to x by an Edge of
    // billing, which the support worker cannot see.
    let acme = w
        .node_in("billing", "company", billing, json!({ "name": "Acme" }))
        .await;
    let hidden = w
        .edge_in("billing", x, acme, billing, json!({ "share": 3 }))
        .await;
    let hidden_version = w.versions_of(&[hidden.0]).await[0];
    w.store_sentence_in(
        "billing",
        hidden_version,
        &format!("{MARKER}: x owns 3% of Acme"),
    )
    .await;
    // A bare company of hr, which the worker cannot see, joined to x by an
    // Edge of hr.
    let hr_bare = w.bare_node_in("hr", "company").await;
    let to_hr = w.edge_in("hr", x, hr_bare, hr, json!({ "share": 4 })).await;
    // A bare company of support, joined to x by an Edge of billing.
    let support_bare = w.bare_node_in(SCOPE, "company").await;
    let to_support = w
        .edge_in("billing", x, support_bare, billing, json!({ "share": 5 }))
        .await;
    // An Edge of billing that does not join x, and a support account.
    let other = w
        .node_in("billing", "account", billing, json!({ "plan": "pro" }))
        .await;
    let untouched = w
        .edge_in("billing", other, acme, billing, json!({ "share": 6 }))
        .await;
    // An Edge of support, which the worker sees, joining x to a support
    // company that keeps its Versions.
    let initech = w
        .node(
            "company",
            e,
            ValidTime::UNKNOWN,
            json!({ "name": "Initech" }),
        )
        .await;
    let seen = w
        .edge(x, initech, g, ValidTime::UNKNOWN, json!({ "share": 7 }))
        .await;

    let mut edges = vec![hidden, to_hr, to_support, seen];
    edges.sort();
    let mut ids: Vec<Uuid> = edges.iter().map(|e| e.0).collect();
    ids.push(x.0);
    let versions = w.versions_of(&ids).await;
    // The stage records of the Episode that wrote x, which stay with the
    // Episode (ADR-0011).
    let e_stages = w.stage_records(SCOPE, e, "claude-sonnet-5-5").await;
    let admin = w.db.admin().await;
    let before = every_row(&admin).await;

    let erasure = w.erase_node(x).await;

    // Every Edge that joined x is gone, but the result names only the one the
    // worker sees, so it learns no ID of billing or hr.
    assert_eq!(erasure.erased_edges, vec![seen]);
    for edge in &edges {
        assert!(!w.exists("edge", edge.0).await, "{edge} still joins x");
    }
    let mut nodes = vec![x, support_bare];
    nodes.sort();
    assert_eq!(erasure.erased_nodes, nodes);
    assert!(erasure.erased_episodes.is_empty());
    assert_no_residue(
        &admin,
        &Residue {
            nodes: &[x.0, support_bare.0],
            edges: &ids[..4],
            versions: &versions,
            markers: &[MARKER],
            ..Residue::default()
        },
    )
    .await;
    let mut gone = ids.clone();
    gone.push(support_bare.0);
    gone.extend(&versions);
    assert_only_removed(&admin, &before, &gone).await;
    // The bare company of hr stays: the worker cannot see hr.
    assert!(w.exists("node", hr_bare.0).await);
    assert!(w.exists("node", acme.0).await);
    assert!(w.exists("node", initech.0).await);
    assert!(w.exists("edge", untouched.0).await);
    for episode in [e, g] {
        assert!(w
            .caller()
            .read_episode(w.tenant, episode)
            .await
            .unwrap()
            .is_some());
    }
    assert_eq!(w.stage_records_of(SCOPE, e).await, e_stages);
}

#[tokio::test]
async fn erasing_a_scope_removes_every_row_in_it_and_every_edge_that_joins_its_nodes_and_nothing_else(
) {
    let w = World::new().await;
    // A user of billing and support erases billing.
    w.db.grant_erasure("authenticated").await;
    let b1 = w
        .episode_in(
            "billing",
            at(2024, 1, 1),
            &format!("{MARKER} billing's source"),
        )
        .await;
    let b2 = w
        .episode_in("billing", at(2024, 1, 2), "billing's second")
        .await;
    let s1 = w.episode(at(2024, 1, 3), None).await;
    let h1 = w.episode_in("hr", at(2024, 1, 4), "hr's Episode").await;

    let acme = w
        .node_in(
            "billing",
            "company",
            b1,
            json!({ "name": format!("{MARKER} Acme") }),
        )
        .await;
    let acct = w
        .node_in("billing", "account", b1, json!({ "plan": "basic" }))
        .await;
    w.worker_in("billing")
        .end_node_validity(
            w.tenant,
            acct,
            &EndOfValidity {
                episode: b2,
                at: Bound::At(at(2030, 1, 1)),
            },
        )
        .await
        .unwrap();
    let inside = w
        .edge_in("billing", acct, acme, b1, json!({ "share": 10 }))
        .await;
    let inside_version = w.versions_of(&[inside.0]).await[0];
    w.store_sentence_in("billing", inside_version, &format!("{MARKER}: 10% of Acme"))
        .await;
    w.worker_in("billing")
        .retract_episode(w.tenant, b2)
        .await
        .unwrap();

    // Edges into billing from support and hr, and out of billing to a bare
    // company of support and a bare company of hr, which the user cannot see.
    let sa = w
        .node("account", s1, ValidTime::UNKNOWN, json!({ "plan": "pro" }))
        .await;
    let from_support = w.edge_in(SCOPE, sa, acme, s1, json!({ "share": 20 })).await;
    let ha = w
        .node_in("hr", "account", h1, json!({ "plan": "staff" }))
        .await;
    let from_hr = w.edge_in("hr", ha, acme, h1, json!({ "share": 30 })).await;
    let support_bare = w.bare_node_in(SCOPE, "company").await;
    let to_support = w
        .edge_in("billing", acct, support_bare, b1, json!({ "share": 40 }))
        .await;
    let hr_bare = w.bare_node_in("hr", "company").await;
    let to_hr = w
        .edge_in("billing", acct, hr_bare, b1, json!({ "share": 50 }))
        .await;
    // Support's own graph, which the Erasure leaves alone.
    let sc = w
        .node(
            "company",
            s1,
            ValidTime::UNKNOWN,
            json!({ "name": "Initech" }),
        )
        .await;
    let kept = w
        .edge(sa, sc, s1, ValidTime::UNKNOWN, json!({ "share": 60 }))
        .await;

    let mut episodes = vec![b1, b2];
    episodes.sort_by_key(|e| e.0);
    let mut nodes = vec![acme, acct, support_bare];
    nodes.sort();
    let mut edges = [inside, from_support, from_hr, to_support, to_hr];
    edges.sort();
    let edge_ids: Vec<Uuid> = edges.iter().map(|e| e.0).collect();
    let versions = w
        .versions_of(&[edge_ids.clone(), vec![acme.0, acct.0]].concat())
        .await;
    // Stage records of a billing Episode, which go with the Scope, and of a
    // support Episode, which stay (ADR-0011).
    w.stage_records("billing", b1, &format!("{MARKER}-model"))
        .await;
    let s1_stages = w.stage_records(SCOPE, s1, "claude-sonnet-5-5").await;
    let admin = w.db.admin().await;
    let before = every_row(&admin).await;

    let erasure = w
        .seeing(&["billing", SCOPE])
        .erase_scope(w.tenant, &Scope::new("billing"))
        .await
        .unwrap();

    assert_eq!(erasure.erased_episodes, episodes);
    assert_eq!(erasure.erased_nodes, nodes);
    // The result names the erased Edges of billing and support, which the
    // user sees, and not hr's.
    let mut seen: Vec<_> = edges.iter().copied().filter(|e| *e != from_hr).collect();
    seen.sort();
    assert_eq!(erasure.erased_edges, seen);
    assert!(!w.exists("edge", from_hr.0).await);
    // No table holds a row of billing.
    let tables = tables_with_a_scope(&admin).await;
    for expected in [
        "episode",
        "node",
        "edge",
        "version",
        "episode_retraction",
        "sentence",
        "stage_record",
        "stage_usage",
    ] {
        assert!(tables.iter().any(|t| t == expected), "{tables:?}");
    }
    for table in &tables {
        let n: i64 = admin
            .query_typed(
                &format!("select count(*) from gnomon.{table} where scope = 'billing'"),
                &[],
            )
            .await
            .unwrap()[0]
            .get(0);
        assert_eq!(n, 0, "gnomon.{table} still holds rows of the erased Scope");
    }
    let episode_ids: Vec<Uuid> = episodes.iter().map(|e| e.0).collect();
    let node_ids: Vec<Uuid> = nodes.iter().map(|n| n.0).collect();
    assert_no_residue(
        &admin,
        &Residue {
            episodes: &episode_ids,
            nodes: &node_ids,
            edges: &edge_ids,
            versions: &versions,
            markers: &[MARKER],
            ..Residue::default()
        },
    )
    .await;
    // Nothing else outside billing changed: the rows that went are those
    // that name what the Erasure erased.
    let gone = [episode_ids, node_ids, edge_ids, versions].concat();
    assert_only_removed(&admin, &before, &gone).await;
    assert!(w.exists("node", hr_bare.0).await, "hr's bare company stays");
    assert!(w.exists("node", ha.0).await);
    assert!(w.exists("edge", kept.0).await);
    assert_eq!(
        w.current_values(sa, at(2024, 4, 1)).await,
        Some(json!({ "plan": "pro" }))
    );
    assert!(w.stage_records_of("billing", b1).await.is_empty());
    assert_eq!(w.stage_records_of(SCOPE, s1).await, s1_stages);
}

#[tokio::test]
async fn writes_to_what_an_erasure_erases_wait_for_it_and_writes_elsewhere_do_not() {
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    let billing = w
        .episode_in("billing", at(2024, 1, 2), "billing's Episode")
        .await;
    let x = w
        .node("account", e, ValidTime::UNKNOWN, json!({ "plan": "basic" }))
        .await;
    let acme = w
        .node_in("billing", "company", billing, json!({ "name": "Acme" }))
        .await;
    // An Edge of billing joins x, so the Erasure locks Acme too, as an
    // endpoint of an Edge it erases.
    w.edge_in("billing", x, acme, billing, json!({ "share": 1 }))
        .await;
    let free = w
        .node("account", e, ValidTime::UNKNOWN, json!({ "plan": "pro" }))
        .await;

    let mut client = w.db.login().await;
    let tx = w.raw(&mut client).await;
    tx.query_typed(
        "select from gnomon.erase_node($1, $2)",
        &[(&w.tenant.0, Type::UUID), (&x.0, Type::UUID)],
    )
    .await
    .unwrap();

    let tenant = w.tenant;
    // A billing write to Acme, and a new Edge that would join x.
    let write_acme = tokio::spawn({
        let other = w.db.store().await;
        async move {
            other
                .as_worker(tenant, Scope::new("billing"))
                .write_node(
                    tenant,
                    acme,
                    &Patch::new(
                        billing,
                        ValidTime::UNKNOWN,
                        values(json!({ "name": "Acme Ltd" })),
                    ),
                )
                .await
        }
    });
    let join_x = tokio::spawn({
        let other = w.db.store().await;
        async move {
            other
                .as_user(values(json!({
                    "role": "authenticated",
                    "app_metadata": { "tenant_id": tenant.0.to_string(), "scopes": [SCOPE, "billing"] }
                })))
                .create_edge(
                    &NewEdge {
                        tenant: Some(tenant),
                        scope: Scope::new("billing"),
                        kind: "owns".into(),
                        source: x,
                        target: acme,
                    },
                    &Patch::new(billing, ValidTime::UNKNOWN, values(json!({ "share": 2 }))),
                )
                .await
        }
    });
    // A write to a Node the Erasure does not erase goes on.
    tokio::time::timeout(
        Duration::from_secs(10),
        w.caller().write_node(
            w.tenant,
            free,
            &Patch::new(e, ValidTime::UNKNOWN, values(json!({ "plan": "free" }))),
        ),
    )
    .await
    .expect("a write elsewhere does not wait")
    .unwrap();
    let admin = w.db.admin().await;
    let mut waiting = 0;
    for _ in 0..200 {
        waiting = admin
            .query_typed(
                "select count(*) from pg_locks where locktype = 'advisory' and not granted
                 and database = (select oid from pg_database where datname = current_database())",
                &[],
            )
            .await
            .unwrap()[0]
            .get::<_, i64>(0);
        if waiting >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        waiting, 2,
        "the write to Acme and the new Edge wait on the Erasure"
    );
    assert!(!write_acme.is_finished() && !join_x.is_finished());

    tx.commit().await.unwrap();
    // Acme survives the Erasure, so its write goes on; x is gone, so no Edge
    // can join it.
    tokio::time::timeout(Duration::from_secs(30), write_acme)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let joined = tokio::time::timeout(Duration::from_secs(30), join_x)
        .await
        .unwrap()
        .unwrap();
    assert!(joined.is_err(), "an Edge joined an erased Node: {joined:?}");
    assert!(!w.exists("node", x.0).await);
}

#[tokio::test]
async fn an_episode_recorded_in_a_scope_while_its_erasure_waits_is_erased_on_the_retry() {
    let w = World::new().await;
    let b1 = w
        .episode_in("billing", at(2024, 1, 1), "billing's Episode")
        .await;
    let acme = w
        .node_in("billing", "company", b1, json!({ "name": "Acme" }))
        .await;

    // A write to Acme holds Acme's lock, so the Erasure waits there after it
    // has locked billing's Episodes.
    let mut client = w.db.login().await;
    let holder = w.raw(&mut client).await;
    holder
        .query_typed(
            "select pg_advisory_xact_lock(gnomon.node_lock_key($1, $2))",
            &[(&w.tenant.0, Type::UUID), (&acme.0, Type::UUID)],
        )
        .await
        .unwrap();
    let tenant = w.tenant;
    let erasure = tokio::spawn({
        let other = w.db.store().await;
        async move {
            other
                .as_worker(tenant, Scope::new("billing"))
                .erase_scope(tenant, &Scope::new("billing"))
                .await
        }
    });
    let admin = w.db.admin().await;
    let mut waiting = 0;
    for _ in 0..200 {
        waiting = admin
            .query_typed(
                "select count(*) from pg_locks where locktype = 'advisory' and not granted
                 and database = (select oid from pg_database where datname = current_database())",
                &[],
            )
            .await
            .unwrap()[0]
            .get::<_, i64>(0);
        if waiting >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(waiting, 1, "the Erasure waits on Acme's lock");
    // Meanwhile billing records another Episode, whose lock would come after
    // the Nodes' locks.
    let b2 = w
        .episode_in("billing", at(2024, 1, 2), "recorded meanwhile")
        .await;
    holder.commit().await.unwrap();

    let erasure = tokio::time::timeout(Duration::from_secs(30), erasure)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let mut episodes = vec![b1, b2];
    episodes.sort_by_key(|e| e.0);
    assert_eq!(erasure.erased_episodes, episodes, "the retry erased both");
    assert_eq!(erasure.erased_nodes, vec![acme]);
    for table in tables_with_a_scope(&admin).await {
        let n: i64 = admin
            .query_typed(
                &format!("select count(*) from gnomon.{table} where scope = 'billing'"),
                &[],
            )
            .await
            .unwrap()[0]
            .get(0);
        assert_eq!(n, 0, "gnomon.{table} still holds rows of billing");
    }
}

#[tokio::test]
async fn without_the_grant_no_erasure_runs_and_what_is_erased_cannot_be_erased_again() {
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    let acme = w
        .node("company", e, ValidTime::UNKNOWN, json!({ "name": "Acme" }))
        .await;
    let a = w
        .node("account", e, ValidTime::UNKNOWN, json!({ "plan": "basic" }))
        .await;
    let edge = w
        .edge(a, acme, e, ValidTime::UNKNOWN, json!({ "share": 1 }))
        .await;
    // A user, whom the deployment granted no Erasure.
    let user = w.seeing(&[SCOPE]);
    for err in [
        user.erase_node(w.tenant, a).await.unwrap_err(),
        user.erase_edge(w.tenant, edge).await.unwrap_err(),
        user.erase_scope(w.tenant, &Scope::new(SCOPE))
            .await
            .unwrap_err(),
    ] {
        assert_eq!(
            err.sqlstate(),
            Some(&tokio_postgres::error::SqlState::INSUFFICIENT_PRIVILEGE),
            "{err:?}"
        );
    }
    assert!(w.exists("edge", edge.0).await);

    w.erase_edge(edge).await;
    w.erase_node(a).await;
    for err in [
        w.caller().erase_edge(w.tenant, edge).await.unwrap_err(),
        w.caller().erase_node(w.tenant, a).await.unwrap_err(),
    ] {
        assert!(matches!(err, gnomon::Error::InvalidWrite(_)), "{err:?}");
    }
    // A Scope with nothing left in it erases nothing, and that is no error.
    w.erase_scope(SCOPE).await;
    let empty = w.erase_scope(SCOPE).await;
    assert!(empty.erased_episodes.is_empty() && empty.erased_nodes.is_empty());
}

#[tokio::test]
async fn erasing_an_edge_leaves_a_bare_endpoint_in_a_scope_the_caller_cannot_see() {
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    // Two bare endpoints: an account of support, and a company of hr, which
    // the support worker cannot see.
    let account = w.bare_node_in(SCOPE, "account").await;
    let hr_bare = w.bare_node_in("hr", "company").await;
    let edge = w
        .edge_in(SCOPE, account, hr_bare, e, json!({ "share": 9 }))
        .await;

    let erasure = w.erase_edge(edge).await;

    assert_eq!(erasure.erased_edges, vec![edge]);
    assert_eq!(
        erasure.erased_nodes,
        vec![account],
        "only the visible one goes"
    );
    assert!(!w.exists("edge", edge.0).await);
    assert!(!w.exists("node", account.0).await);
    assert!(
        w.exists("node", hr_bare.0).await,
        "a bare Node of a Scope the caller cannot see stays"
    );
}
