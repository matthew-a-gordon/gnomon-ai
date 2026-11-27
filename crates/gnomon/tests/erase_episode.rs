//! Seam 1, Erase Episode (ADR-0003, ADR-0007, ADR-0009, ADR-0013): the
//! Episode, its Contributions, statements and Citations, and every Version
//! derived from it alone are deleted, and every surviving Version it touched,
//! current and closed, is recomputed under the rule it recorded, through the
//! Rust core's Store API against real Postgres. The oracle test is
//! erase_oracle.rs, and the adversarial cases live in sql_boundary.rs.

mod support;

use std::time::Duration;

use gnomon::{
    AsOf, Bound, BoundSide, BoundStatement, DerivedCitation, EndOfValidity, EpisodeId, MergeRule,
    NewCommunity, NewEdge, NewEpisode, NewNode, NewSummary, Patch, Precision, Retraction, Scope,
    ValidTime,
};
use serde_json::{Value, json};
use support::erasure::{
    Residue, SCOPE, World, assert_no_residue, at, gnomon_tables, snapshot,
    tables_derived_from_version_text, tables_with_an_episode_reference, values,
};
use tokio_postgres::types::Type;
use uuid::Uuid;

/// Text that only the erased Episode states, so a scan of every table can
/// find any copy of it.
const MARKER: &str = "erased-7f3a";

#[tokio::test]
async fn after_an_erasure_no_row_of_any_table_names_the_episode_or_holds_a_value_only_it_supplied()
{
    let w = World::new().await;
    let f = w.episode(at(2024, 1, 1), None).await;
    let g = w.episode(at(2024, 1, 15), None).await;
    let e = w
        .episode_saying(at(2024, 2, 1), None, &format!("{MARKER} the erased source"))
        .await;

    // An account F describes, which E gives another plan for a while and an
    // Alias for ever.
    let a = w
        .node("account", f, ValidTime::UNKNOWN, json!({ "plan": "basic" }))
        .await;
    w.write(
        a,
        e,
        ValidTime::between(at(2024, 3, 1), at(2024, 6, 1)),
        json!({ "plan": format!("{MARKER}-plan") }),
    )
    .await;
    w.write(
        a,
        e,
        ValidTime::UNKNOWN,
        json!({ "aliases": [format!("{MARKER}-alias")] }),
    )
    .await;
    // An account only E describes, whose end G states.
    let b = w
        .node(
            "account",
            e,
            ValidTime::UNKNOWN,
            json!({ "plan": format!("{MARKER}-only") }),
        )
        .await;
    let b_version = w.read(b, at(2024, 1, 1)).await.unwrap().id;
    w.caller()
        .state_bound(
            w.tenant,
            b,
            &BoundStatement {
                episode: g,
                version: b_version,
                side: BoundSide::End,
                bound: Bound::Period {
                    at: at(2030, 1, 1),
                    precision: Precision::Year,
                },
            },
        )
        .await
        .unwrap();
    // A company F describes, whose start E states.
    let c = w
        .node("company", f, ValidTime::UNKNOWN, json!({ "name": "Acme" }))
        .await;
    let c_version = w.read(c, at(2024, 1, 1)).await.unwrap().id;
    w.caller()
        .state_bound(
            w.tenant,
            c,
            &BoundStatement {
                episode: e,
                version: c_version,
                side: BoundSide::Start,
                bound: Bound::Period {
                    at: at(2020, 1, 1),
                    precision: Precision::Year,
                },
            },
        )
        .await
        .unwrap();
    // An Edge only E states, an Edge F states that E gives another share and
    // ends, and an Edge E never touches.
    let e_only = w
        .edge(b, c, e, ValidTime::UNKNOWN, json!({ "share": 7 }))
        .await;
    let shared = w
        .edge(a, c, f, ValidTime::UNKNOWN, json!({ "share": 40 }))
        .await;
    // The Sentence of the Version F alone gave the Edge, which E's write
    // closes and the Erasure reopens. Its text owes nothing to E, but the
    // Erasure deletes its Sentence (ADR-0013), and the words of that Sentence
    // must go with it, though the Version stays.
    let shared_first = w.current_edge_version(shared, at(2024, 4, 1)).await;
    w.store_sentence(shared_first, "a holds forty percent of Acme")
        .await;
    w.write_edge(
        shared,
        e,
        ValidTime::between(at(2024, 3, 1), at(2024, 6, 1)),
        json!({ "share": 41 }),
    )
    .await;
    let d = w
        .node("account", f, ValidTime::UNKNOWN, json!({ "plan": "pro" }))
        .await;
    let untouched = w
        .edge(d, c, f, ValidTime::UNKNOWN, json!({ "share": 1 }))
        .await;
    w.caller()
        .end_edge_validity(
            w.tenant,
            shared,
            &EndOfValidity {
                episode: e,
                at: Bound::At(at(2025, 1, 1)),
            },
        )
        .await
        .unwrap();
    // Sentences the Pipeline would have written: one of a Version derived from
    // E alone, one of a Version E's Erasure recomputes, one it leaves alone.
    let e_only_version = w.current_edge_version(e_only, at(2024, 1, 1)).await;
    let shared_version = w.current_edge_version(shared, at(2024, 1, 1)).await;
    let untouched_version = w.current_edge_version(untouched, at(2024, 1, 1)).await;
    w.store_sentence(e_only_version, &format!("{MARKER}: b owns 7% of Acme"))
        .await;
    w.store_sentence(shared_version, "a owns 40% of Acme").await;
    w.store_sentence(untouched_version, "d owns 1% of Acme")
        .await;
    // Stage records and usage counts the Pipeline would have written, for E
    // and for F (ADR-0011). Only E's go.
    w.write_stage_records(SCOPE, e, &format!("{MARKER}-model"))
        .await;
    let f_stages = w.write_stage_records(SCOPE, f, "claude-sonnet-5-5").await;
    // Embeddings the Pipeline would have stored (ADR-0010): one of every
    // Version's values, current and closed, and one of the Sentence of the
    // Version derived from E alone and of the one F alone gave the shared
    // Edge, each moving that Version's embedding to its Sentence.
    let mut rng = support::vectors::Rng::new(3);
    support::vectors::embed_every_version(&w, SCOPE, &mut rng).await;
    // And one of every current name and Alias, for Resolution (migration
    // 0019), the Alias E alone gave among them. Its name rows and its full
    // vector go.
    let names = support::vectors::embed_every_name(&w, SCOPE, &mut rng).await;
    assert!(names.contains_key(&format!("{MARKER}-alias")));
    for (version, sentence) in [
        (e_only_version, format!("{MARKER}: b owns 7% of Acme")),
        (shared_first, "a holds forty percent of Acme".to_owned()),
    ] {
        let hash = support::vectors::sentence_hash(&w, &sentence).await;
        w.caller()
            .store_embedding(w.tenant, version, &hash, &rng.unit())
            .await
            .unwrap();
    }
    // A Sentence the Pipeline embedded and then replaced, on the Edge E never
    // touches, and the replacement's embedding. The replaced text's
    // embedding goes when it is replaced (migration 0021), so no full vector
    // is left that no row uses, which no Erasure could find.
    for sentence in [
        format!("{MARKER}: d held 1% of Acme"),
        "d owns 1% of Acme".into(),
    ] {
        w.store_sentence(untouched_version, &sentence).await;
        let hash = support::vectors::sentence_hash(&w, &sentence).await;
        w.caller()
            .store_embedding(w.tenant, untouched_version, &hash, &rng.unit())
            .await
            .unwrap();
    }
    // Summaries and Communities the Pipeline would have written (ADR-0014,
    // ADR-0015). a's first Summary cites E and its regeneration F alone, so a
    // closed row cites E. A Community of a and c cites E and F, and one of c
    // cites F alone. Every row that cites E goes, closed or open.
    let summarize = |content: String, cites: Vec<EpisodeId>| NewSummary {
        scope: Scope::new(SCOPE),
        node: a,
        content,
        as_of_valid: at(2024, 4, 1),
        citations: cites.into_iter().map(DerivedCitation::of).collect(),
    };
    let e_summary = w
        .caller()
        .write_summary(
            w.tenant,
            &summarize(format!("{MARKER}: a's plan"), vec![e, f]),
        )
        .await
        .unwrap();
    let f_summary = w
        .caller()
        .write_summary(w.tenant, &summarize("a is on basic".into(), vec![f]))
        .await
        .unwrap();
    let community =
        |members: Vec<gnomon::NodeId>, content: String, cites: Vec<EpisodeId>| NewCommunity {
            scope: Scope::new(SCOPE),
            replaces: vec![],
            members,
            content,
            as_of_valid: at(2024, 4, 1),
            citations: cites.into_iter().map(DerivedCitation::of).collect(),
        };
    let e_community = w
        .caller()
        .write_community(
            w.tenant,
            &community(vec![a, c], format!("{MARKER}: a and Acme"), vec![f, e]),
        )
        .await
        .unwrap();
    let f_community = w
        .caller()
        .write_community(w.tenant, &community(vec![c], "Acme".into(), vec![f]))
        .await
        .unwrap();
    // Each is embedded for Search (migration 0020), and each has token rows
    // of its text, E's marker among them for the two that cite E.
    for (i, id) in [e_summary, f_summary].into_iter().enumerate() {
        let v = support::vectors::Rng::new(40 + i as u64).unit();
        w.caller()
            .store_summary_embedding(w.tenant, id, &v)
            .await
            .unwrap();
    }
    for (i, id) in [e_community, f_community].into_iter().enumerate() {
        let v = support::vectors::Rng::new(50 + i as u64).unit();
        w.caller()
            .store_community_embedding(w.tenant, id, &v)
            .await
            .unwrap();
    }
    // Last, E is retracted, which marks it and retracts what only it cites.
    w.caller().retract_episode(w.tenant, e).await.unwrap();

    // Every Version the Erasure deletes or recomputes: those of a Node or
    // Edge E touched, held at or after the first Recorded time E touched it.
    let admin = w.db.admin().await;
    let recomputed: Vec<Uuid> = admin
        .query_typed(
            "select v.id from gnomon.version v
             where v.recorded_to >= (
               select min(x.at) from (
                 select c.recorded_at as at from gnomon.contribution c
                   where c.episode_id = $1 and c.versioned_id = v.versioned_id
                 union all
                 select b.recorded_at from gnomon.bound_statement b
                   where b.episode_id = $1 and b.versioned_id = v.versioned_id
               ) x)",
            &[(&e.0, Type::UUID)],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert!(recomputed.contains(&shared_version.0));
    assert!(recomputed.contains(&shared_first.0));
    assert!(recomputed.contains(&e_only_version.0));
    assert!(!recomputed.contains(&untouched_version.0));

    // E's words are in the token table, its Alias of a, which Node Kinds
    // search, and its Sentence, so the check below finds any it leaves.
    let marked: i64 = admin
        .query_typed(
            "select count(*) from gnomon.search_token where strpos(token, $1) > 0",
            &[(&MARKER, Type::TEXT)],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert!(marked >= 2, "E's words are indexed: {marked}");
    let marked: i64 = admin
        .query_typed(
            "select count(*) from gnomon.derived_search_token where strpos(token, $1) > 0",
            &[(&MARKER, Type::TEXT)],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert!(
        marked >= 2,
        "the words of E's Summary and Community are indexed: {marked}"
    );
    // E's Alias of a is in name rows with bits, which Resolution finds.
    let alias = format!("{MARKER}-alias");
    let resolved = |text: String| {
        let w = &w;
        async move {
            w.caller()
                .resolve(w.tenant, &gnomon::Mention::text(&text), 10)
                .await
                .unwrap()
        }
    };
    assert_eq!(resolved(alias.clone()).await[0].node, a);
    let embedded: i64 = admin
        .query_typed(
            "select count(*) from gnomon.node_name where name = $1 and q is not null",
            &[(&alias, Type::TEXT)],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert!(
        embedded >= 1,
        "E's Alias is embedded in each current Version of a"
    );

    let erasure = w.erase(e).await;
    assert_eq!(erasure.erased_edges, vec![e_only]);
    assert!(erasure.erased_nodes.contains(&b));

    // No row of any table names E or holds a value only E stated, and
    // nothing derived from the text of a Version the Erasure deleted or
    // recomputed is left. The check reads every table in the schema, so a
    // table a later migration adds fails here until Erasure deletes from it.
    assert_no_residue(
        &admin,
        &Residue {
            episodes: &[e.0],
            recomputed: &recomputed,
            markers: &[MARKER],
            ..Residue::default()
        },
    )
    .await;
    assert!(
        resolved(alias.clone()).await.is_empty(),
        "Resolution finds a Node by E's Alias"
    );
    assert_eq!(resolved("Acme".into()).await[0].node, c);
    let referencing = tables_with_an_episode_reference(&admin).await;
    for expected in [
        "bound_statement",
        "citation",
        "community_citation",
        "contribution",
        "episode_retraction",
        "stage_record",
        "stage_usage",
        "statement",
        "summary_citation",
    ] {
        assert!(
            referencing.iter().any(|t| t == expected),
            "{expected} is missing from {referencing:?}"
        );
    }
    assert!(
        w.caller()
            .read_episode(w.tenant, e)
            .await
            .unwrap()
            .is_none()
    );
    // The Summary and Community that cite E are gone with their members, and
    // those that cite F alone stand.
    for (table, id) in [("summary", e_summary.0), ("community", e_community.0)] {
        assert!(
            !w.exists(table, id).await,
            "{table} {id} cites E and stands"
        );
    }
    for (table, id) in [("summary", f_summary.0), ("community", f_community.0)] {
        assert!(w.exists(table, id).await, "{table} {id} cites F alone");
    }
    let members: i64 = admin
        .query_typed(
            "select count(*) from gnomon.community_member where community_id = $1",
            &[(&e_community.0, Type::UUID)],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(members, 0, "the erased Community's members stand");
    // Their Search rows go with them, and those of the rows that cite F
    // alone stand.
    for (column, gone, kept) in [
        ("summary_id", e_summary.0, f_summary.0),
        ("community_id", e_community.0, f_community.0),
    ] {
        for table in ["derived_search_token", "derived_embedding"] {
            let count = |id: Uuid| {
                let admin = &admin;
                async move {
                    admin
                        .query_typed(
                            &format!("select count(*) from gnomon.{table} where {column} = $1"),
                            &[(&id, Type::UUID)],
                        )
                        .await
                        .unwrap()[0]
                        .get::<_, i64>(0)
                }
            };
            assert_eq!(count(gone).await, 0, "{table} keeps rows of {gone}");
            assert!(count(kept).await > 0, "{table} lost the rows of {kept}");
        }
    }
    support::erasure::assert_search_counts_match_the_token_rows(&admin).await;
    assert!(w.stage_records_of(SCOPE, e).await.is_empty());
    assert_eq!(w.stage_records_of(SCOPE, f).await, f_stages);

    // What the Erasure left alone keeps its Sentence.
    // F's first Version of the Edge holds again, with no Sentence and no
    // word of the one it had.
    assert!(w.exists("version", shared_first.0).await);
    assert_eq!(w.sentence_of(shared_first).await, None);
    let forty: i64 = admin
        .query_typed(
            "select count(*) from gnomon.search_token where version_id = $1 and token = 'forty'",
            &[(&shared_first.0, Type::UUID)],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(
        forty, 0,
        "the reopened Version keeps a word of its erased Sentence"
    );
    let derived = tables_derived_from_version_text(&admin).await;
    assert!(derived.iter().any(|t| t == "sentence"), "{derived:?}");
    assert!(derived.iter().any(|t| t == "search_token"), "{derived:?}");
    assert!(derived.iter().any(|t| t == "embedding_bit"), "{derived:?}");
    let full_vectors: i64 = admin
        .query_typed("select count(*) from gnomon.embedding", &[])
        .await
        .unwrap()[0]
        .get(0);
    assert!(full_vectors > 0, "the Versions E never touched keep theirs");
    support::erasure::assert_search_counts_match_the_token_rows(&admin).await;
    assert_eq!(
        w.sentence_of(untouched_version).await.as_deref(),
        Some("d owns 1% of Acme")
    );

    // What the others said survives.
    assert_eq!(
        w.current_values(a, at(2024, 4, 1)).await,
        Some(json!({ "plan": "basic" }))
    );
    assert_eq!(
        w.current_values(c, at(2024, 4, 1)).await,
        Some(json!({ "name": "Acme" }))
    );
    let c_now = w.read(c, at(2024, 4, 1)).await.unwrap();
    assert_eq!(
        c_now.valid_time,
        ValidTime::UNKNOWN,
        "E's stated start is gone"
    );
    let shared_now = w
        .caller()
        .read_edge(w.tenant, shared, AsOf::current(at(2026, 1, 1)))
        .await
        .unwrap()
        .expect("F's Edge holds after E's End of validity is erased");
    assert_eq!(Value::Object(shared_now.values), json!({ "share": 40 }));
}

#[tokio::test]
async fn surviving_versions_current_and_closed_are_recomputed_from_the_remaining_contributions() {
    let w = World::new().await;
    let f = w.episode(at(2024, 1, 1), None).await;
    let e = w.episode(at(2024, 2, 1), None).await;
    let g = w.episode(at(2024, 3, 1), None).await;
    let ada = w
        .node("node", f, ValidTime::UNKNOWN, json!({ "name": "Ada" }))
        .await;
    let t1 = w.read(ada, at(2024, 1, 1)).await.unwrap().recorded_from;
    let t2 = w
        .write(
            ada,
            e,
            ValidTime::between(at(2024, 3, 1), at(2024, 6, 1)),
            json!({ "name": "Erased" }),
        )
        .await;
    let t3 = w
        .write(
            ada,
            g,
            ValidTime {
                from: Bound::At(at(2024, 4, 1)),
                to: Bound::Unknown,
            },
            json!({ "aliases": ["Countess"] }),
        )
        .await;
    assert_eq!(
        w.values_as_of(ada, at(2024, 5, 1), t3).await,
        Some(json!({ "name": "Erased", "aliases": ["Countess"] }))
    );

    let erasure = w.erase(e).await;
    assert_eq!(erasure.recomputed_nodes, vec![ada]);
    assert!(erasure.erased_nodes.is_empty());

    // As if E never existed, at every known-at instant.
    for known in [t1, t2, t3] {
        for month in 1..=8 {
            let read = w.values_as_of(ada, at(2024, month, 15), known).await;
            let mut expected = json!({ "name": "Ada" });
            if known >= t3 && month >= 4 {
                expected["aliases"] = json!(["Countess"]);
            }
            assert_eq!(
                read,
                Some(expected),
                "valid-at month {month}, known-at {known}"
            );
        }
    }
    // The Version that was current at E's instant is closed where G's write
    // closed it, and its Recorded time no longer stops at E's write.
    let closed = w.read_as_of(ada, at(2024, 5, 1), t2).await.unwrap();
    assert_eq!(closed.recorded_from, t1);
    assert_eq!(closed.recorded_to, Some(t3));
    // The current Versions cite only F and G.
    let current = w.read(ada, at(2024, 5, 1)).await.unwrap();
    let cited: Vec<EpisodeId> = w
        .caller()
        .list_citations(w.tenant, current.id)
        .await
        .unwrap()
        .into_iter()
        .map(|c| c.episode)
        .collect();
    assert!(
        cited.contains(&f) && cited.contains(&g) && !cited.contains(&e),
        "{cited:?}"
    );
}

#[tokio::test]
async fn each_recomputed_version_keeps_the_merge_rule_it_recorded_when_the_kinds_rule_changed_since()
 {
    let w = World::new().await;
    let f = w.episode(at(2024, 1, 1), Some(5)).await;
    let e = w.episode(at(2024, 2, 1), Some(0)).await;
    let h = w.episode(at(2024, 3, 1), Some(0)).await;
    let j = w.episode(at(2024, 4, 1), Some(0)).await;
    let acct = w
        .node("account", f, ValidTime::UNKNOWN, json!({ "plan": "gold" }))
        .await;
    w.write(acct, e, ValidTime::UNKNOWN, json!({ "aliases": ["e"] }))
        .await;
    // Under recency the later Reference time wins.
    let t3 = w
        .write(acct, h, ValidTime::UNKNOWN, json!({ "plan": "basic" }))
        .await;
    w.caller()
        .set_merge_rule(w.tenant, "account", MergeRule::Ranked)
        .await
        .unwrap();
    // Under ranked merge F's Authority wins.
    let t5 = w
        .write(acct, j, ValidTime::UNKNOWN, json!({ "name": "J" }))
        .await;
    let before = w.read_as_of(acct, at(2024, 5, 1), t3).await.unwrap();
    assert_eq!(before.merge_rule, MergeRule::Recency);
    assert_eq!(before.values["plan"], json!("basic"));

    w.erase(e).await;

    // The step H wrote recorded recency and keeps it; the Kind's rule now is
    // ranked, which would choose F's plan.
    let replayed = w.read_as_of(acct, at(2024, 5, 1), t3).await.unwrap();
    assert_eq!(replayed.merge_rule, MergeRule::Recency);
    assert_eq!(Value::Object(replayed.values), json!({ "plan": "basic" }));
    // The step J wrote recorded ranked and keeps it.
    let now = w.read_as_of(acct, at(2024, 5, 1), t5).await.unwrap();
    assert_eq!(now.merge_rule, MergeRule::Ranked);
    assert_eq!(
        Value::Object(now.values),
        json!({ "plan": "gold", "name": "J" })
    );
    assert_eq!(
        w.current_values(acct, at(2024, 5, 1)).await,
        Some(json!({ "plan": "gold", "name": "J" }))
    );
}

#[tokio::test]
async fn a_statement_that_lost_to_the_erased_higher_authority_episode_takes_effect() {
    let w = World::new().await;
    w.caller()
        .set_merge_rule(w.tenant, "account", MergeRule::Ranked)
        .await
        .unwrap();
    let g = w.episode(at(2024, 1, 1), Some(0)).await;
    let e = w.episode(at(2024, 1, 1), Some(5)).await;
    let f = w.episode(at(2024, 2, 1), Some(0)).await;
    let acct = w
        .node("account", g, ValidTime::UNKNOWN, json!({ "plan": "basic" }))
        .await;
    // E's higher-Authority Correction, then F's lower-Authority End of
    // validity, which loses to it and is held.
    w.caller()
        .correct_node(
            w.tenant,
            acct,
            &Patch::new(e, ValidTime::UNKNOWN, values(json!({ "plan": "gold" }))),
        )
        .await
        .unwrap();
    let ended = w
        .caller()
        .end_node_validity(
            w.tenant,
            acct,
            &EndOfValidity {
                episode: f,
                at: Bound::At(at(2024, 6, 1)),
            },
        )
        .await
        .unwrap()
        .recorded_at;
    assert_eq!(
        w.current_values(acct, at(2024, 7, 1)).await,
        Some(json!({ "plan": "gold" })),
        "F's End of validity is held"
    );

    w.erase(e).await;

    assert_eq!(
        w.current_values(acct, at(2024, 3, 1)).await,
        Some(json!({ "plan": "basic" }))
    );
    assert_eq!(w.current_values(acct, at(2024, 7, 1)).await, None);
    // From the instant F stated it, as if E never existed.
    assert_eq!(w.values_as_of(acct, at(2024, 7, 1), ended).await, None);
}

#[tokio::test]
async fn erasing_the_episode_behind_a_retraction_makes_the_retracted_version_current_again() {
    let w = World::new().await;
    let f = w.episode(at(2024, 1, 1), None).await;
    let e = w.episode(at(2024, 2, 1), None).await;
    let acct = w
        .node("account", f, ValidTime::UNKNOWN, json!({ "plan": "basic" }))
        .await;
    let version = w.read(acct, at(2024, 1, 1)).await.unwrap().id;
    let retracted = w
        .caller()
        .retract_node_version(
            w.tenant,
            acct,
            &Retraction {
                episode: e,
                version,
            },
        )
        .await
        .unwrap()
        .recorded_at;
    assert_eq!(w.read(acct, at(2024, 1, 1)).await, None);

    let erasure = w.erase(e).await;
    assert_eq!(erasure.recomputed_nodes, vec![acct]);

    let now = w.read(acct, at(2024, 1, 1)).await.expect("current again");
    assert_eq!(Value::Object(now.values), json!({ "plan": "basic" }));
    assert_eq!(now.recorded_to, None);
    assert_eq!(
        w.values_as_of(acct, at(2024, 1, 1), retracted).await,
        Some(json!({ "plan": "basic" }))
    );
    assert!(
        w.caller()
            .list_node_statements(w.tenant, acct)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn a_node_left_with_no_versions_is_erased_with_its_last_edge_and_a_bare_node_stays() {
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    let f = w.episode(at(2024, 1, 2), None).await;
    let acme = w
        .node("company", f, ValidTime::UNKNOWN, json!({ "name": "Acme" }))
        .await;
    // a: only E describes it, and only E's Edge joins it.
    let a = w
        .node("account", e, ValidTime::UNKNOWN, json!({ "plan": "basic" }))
        .await;
    let e_edge = w
        .edge(a, acme, e, ValidTime::UNKNOWN, json!({ "share": 10 }))
        .await;
    // b: only E describes it, but F's Edge joins it.
    let b = w
        .node("account", e, ValidTime::UNKNOWN, json!({ "plan": "pro" }))
        .await;
    let f_edge = w
        .edge(b, acme, f, ValidTime::UNKNOWN, json!({ "share": 20 }))
        .await;
    // A bare Node no Edge joins, which no Episode describes.
    let mut client = w.db.login().await;
    let tx = w.raw(&mut client).await;
    let bare: Uuid = tx
        .query_typed(
            "select gnomon.create_node($1, $2, 'company')",
            &[(&w.tenant.0, Type::UUID), (&SCOPE, Type::TEXT)],
        )
        .await
        .unwrap()[0]
        .get(0);
    tx.commit().await.unwrap();

    let erasure = w.erase(e).await;
    assert_eq!(erasure.erased_edges, vec![e_edge]);
    assert_eq!(erasure.erased_nodes, vec![a]);
    assert_eq!(erasure.recomputed_nodes, vec![b]);
    assert!(!w.exists("edge", e_edge.0).await);
    assert!(!w.exists("node", a.0).await);
    // b has no Version now, and stays as a bare identity while F's Edge joins
    // it.
    assert!(w.exists("node", b.0).await);
    assert_eq!(w.read(b, at(2024, 1, 1)).await, None);
    assert!(w.exists("node", acme.0).await);
    assert!(w.exists("node", bare).await);

    // Erasing F erases its Edge, then b with its last Edge, then Acme, which
    // F alone described and no Edge joins any more.
    let erasure = w.erase(f).await;
    assert_eq!(erasure.erased_edges, vec![f_edge]);
    let mut expected = vec![acme, b];
    expected.sort();
    assert_eq!(erasure.erased_nodes, expected);
    assert!(!w.exists("node", b.0).await);
    assert!(
        w.exists("node", bare).await,
        "a bare Node no Edge joins stays"
    );
}

#[tokio::test]
async fn the_same_erasure_on_the_same_starting_data_always_removes_the_same_rows() {
    let w = World::new().await;
    let f = w.episode(at(2024, 1, 1), Some(1)).await;
    let e = w.episode(at(2024, 2, 1), Some(2)).await;
    let g = w.episode(at(2024, 3, 1), Some(0)).await;
    let acme = w
        .node("company", f, ValidTime::UNKNOWN, json!({ "name": "Acme" }))
        .await;
    let mut accounts = Vec::new();
    for i in 0..5 {
        let a = w
            .node(
                "account",
                f,
                ValidTime::UNKNOWN,
                json!({ "plan": format!("p{i}") }),
            )
            .await;
        w.write(
            a,
            e,
            ValidTime::between(at(2024, 1 + i, 1), at(2024, 3 + i, 1)),
            json!({ "plan": "erased" }),
        )
        .await;
        w.write(
            a,
            g,
            ValidTime::between(at(2024, 2 + i, 1), at(2024, 9, 1)),
            json!({ "aliases": [format!("g{i}")] }),
        )
        .await;
        w.edge(a, acme, e, ValidTime::UNKNOWN, json!({ "share": i }))
            .await;
        accounts.push(a);
    }
    let tables: Vec<String> = gnomon_tables(&w.db.admin().await)
        .await
        .into_iter()
        // No caller reads the list an Erasure of a Node, an Edge or a Scope
        // keeps while it runs, which is empty once it returns.
        .filter(|t| t != "schema_version" && t != "tenancy" && t != "erasure_reach")
        // The BM25 counts change when the Erasure commits, not inside it.
        // `assert_only_removed` checks them against the token rows.
        .filter(|t| !support::erasure::BM25_COUNT_TABLES.contains(&t.as_str()))
        .collect();

    let mut runs = Vec::new();
    for _ in 0..2 {
        let mut client = w.db.login().await;
        let tx = w.raw(&mut client).await;
        let before = snapshot(&tx, &tables).await;
        tx.query_typed(
            "select from gnomon.erase_episode($1, $2)",
            &[(&w.tenant.0, Type::UUID), (&e.0, Type::UUID)],
        )
        .await
        .unwrap();
        let after = snapshot(&tx, &tables).await;
        tx.rollback().await.unwrap();
        let removed: Vec<String> = before
            .iter()
            .filter(|r| after.binary_search(r).is_err())
            .cloned()
            .collect();
        assert!(!removed.is_empty());
        runs.push(removed);
    }
    assert_eq!(
        runs[0], runs[1],
        "two runs of one Erasure removed different rows"
    );

    // And the Erasure the Store commits removes exactly those rows.
    let mut client = w.db.login().await;
    let tx = w.raw(&mut client).await;
    let before = snapshot(&tx, &tables).await;
    tx.commit().await.unwrap();
    w.erase(e).await;
    let mut client = w.db.login().await;
    let tx = w.raw(&mut client).await;
    let after = snapshot(&tx, &tables).await;
    let removed: Vec<String> = before
        .iter()
        .filter(|r| after.binary_search(r).is_err())
        .cloned()
        .collect();
    assert_eq!(removed, runs[0]);
}

#[tokio::test]
async fn a_write_to_an_affected_node_waits_for_the_erasure_to_commit_and_one_to_an_unaffected_node_does_not()
 {
    let w = World::new().await;
    let f = w.episode(at(2024, 1, 1), None).await;
    let e = w.episode(at(2024, 2, 1), None).await;
    let affected = w
        .node("account", f, ValidTime::UNKNOWN, json!({ "plan": "basic" }))
        .await;
    w.write(affected, e, ValidTime::UNKNOWN, json!({ "plan": "erased" }))
        .await;
    let unaffected = w
        .node("account", f, ValidTime::UNKNOWN, json!({ "plan": "pro" }))
        .await;
    let acme = w
        .node("company", f, ValidTime::UNKNOWN, json!({ "name": "Acme" }))
        .await;
    let later = w.episode(at(2024, 3, 1), None).await;

    // The Erasure runs and holds its locks until it commits.
    let mut client = w.db.login().await;
    let tx = w.raw(&mut client).await;
    tx.query_typed(
        "select from gnomon.erase_episode($1, $2)",
        &[(&w.tenant.0, Type::UUID), (&e.0, Type::UUID)],
    )
    .await
    .unwrap();

    let other = w.db.store().await;
    let tenant = w.tenant;
    let write_affected = tokio::spawn({
        let other = w.db.store().await;
        async move {
            other
                .as_worker(tenant, Scope::new(SCOPE))
                .write_node(
                    tenant,
                    affected,
                    &Patch::new(later, ValidTime::UNKNOWN, values(json!({ "name": "late" }))),
                )
                .await
                .unwrap()
        }
    });
    let join_affected = tokio::spawn({
        let other = w.db.store().await;
        async move {
            other
                .as_worker(tenant, Scope::new(SCOPE))
                .create_edge(
                    &NewEdge {
                        tenant: Some(tenant),
                        scope: Scope::new(SCOPE),
                        kind: "owns".into(),
                        source: affected,
                        target: acme,
                    },
                    &Patch::new(later, ValidTime::UNKNOWN, values(json!({ "share": 5 }))),
                )
                .await
                .unwrap()
        }
    });
    // The unaffected write completes while the Erasure is still open.
    tokio::time::timeout(
        Duration::from_secs(10),
        other.as_worker(tenant, Scope::new(SCOPE)).write_node(
            tenant,
            unaffected,
            &Patch::new(later, ValidTime::UNKNOWN, values(json!({ "name": "free" }))),
        ),
    )
    .await
    .expect("a write to an unaffected Node does not wait")
    .unwrap();
    // Both wait on an advisory lock the Erasure holds.
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
        "the affected write and the Edge do not wait on the Erasure"
    );
    assert!(
        !write_affected.is_finished(),
        "the affected write did not wait"
    );
    assert!(
        !join_affected.is_finished(),
        "an Edge joined an affected Node without waiting"
    );

    tx.commit().await.unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(30), write_affected)
        .await
        .expect("the affected write goes on once the Erasure commits")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), join_affected)
        .await
        .unwrap()
        .unwrap();
    // It wrote after the Erasure, from the Contributions that remained.
    let now = w.read(affected, at(2024, 1, 1)).await.unwrap();
    assert_eq!(now.recorded_from, outcome.recorded_at);
    assert_eq!(
        Value::Object(now.values),
        json!({ "plan": "basic", "name": "late" })
    );
}

#[tokio::test]
async fn an_erasure_deletes_the_sentence_of_every_edge_version_it_deletes_or_recomputes() {
    let w = World::new().await;
    let f = w.episode(at(2024, 1, 1), None).await;
    let e = w.episode(at(2024, 2, 1), None).await;
    let acme = w
        .node("company", f, ValidTime::UNKNOWN, json!({ "name": "Acme" }))
        .await;
    let a = w
        .node("account", f, ValidTime::UNKNOWN, json!({ "plan": "basic" }))
        .await;
    let edge = w
        .edge(a, acme, f, ValidTime::UNKNOWN, json!({ "share": 40 }))
        .await;
    let surviving = w.current_edge_version(edge, at(2024, 1, 1)).await;
    w.store_sentence(surviving, "a owns 40% of Acme").await;
    // E restates the share for a while. The Version F's write opened is
    // closed, and E's Versions get Sentences of their own.
    w.write_edge(
        edge,
        e,
        ValidTime::between(at(2024, 3, 1), at(2024, 6, 1)),
        json!({ "share": 99 }),
    )
    .await;
    let derived = w.current_edge_version(edge, at(2024, 4, 1)).await;
    let after_e = w.current_edge_version(edge, at(2024, 7, 1)).await;
    w.store_sentence(derived, "a owns 99% of Acme").await;
    w.store_sentence(after_e, "a owns 40% of Acme again").await;

    w.erase(e).await;

    // E's Versions are deleted, and their Sentences with them by the foreign
    // key. F's Version survives, reopened, and its Sentence is deleted too,
    // because the Erasure recomputed it. The Pipeline writes a new one.
    assert_eq!(w.sentence_of(derived).await, None);
    assert_eq!(w.sentence_of(after_e).await, None);
    assert_eq!(
        w.current_edge_version(edge, at(2024, 4, 1)).await,
        surviving
    );
    assert_eq!(w.sentence_of(surviving).await, None);
    let read = w
        .caller()
        .read_edge(w.tenant, edge, AsOf::current(at(2024, 4, 1)))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(read.sentence, None);
    assert_eq!(Value::Object(read.values), json!({ "share": 40 }));
}

#[tokio::test]
async fn a_citation_of_a_retracted_episode_does_not_keep_a_version_current_once_its_corroborator_is_erased()
 {
    let w = World::new().await;
    let r = w.episode(at(2024, 1, 1), None).await;
    let e = w.episode(at(2024, 2, 1), None).await;
    let acct = w
        .node("account", r, ValidTime::UNKNOWN, json!({ "plan": "basic" }))
        .await;
    let written = w.read(acct, at(2024, 1, 1)).await.unwrap().recorded_from;
    // E states the same, so the Version cites both, and Retract Episode
    // leaves it current.
    w.write(acct, e, ValidTime::UNKNOWN, json!({ "plan": "basic" }))
        .await;
    let retraction = w.caller().retract_episode(w.tenant, r).await.unwrap();
    assert!(retraction.retracted.is_empty());
    assert!(w.read(acct, at(2024, 1, 1)).await.is_some());

    w.erase(e).await;

    // As if E never existed, Retract Episode found the Version cited by R
    // alone and retracted it. R's Citation counts as another Episode's for
    // what the Erasure deletes, so R's value is still there before the
    // Retraction, but R is retracted, so it is not current after it.
    assert_eq!(
        w.values_as_of(acct, at(2024, 1, 1), written).await,
        Some(json!({ "plan": "basic" }))
    );
    assert_eq!(
        w.values_as_of(acct, at(2024, 1, 1), retraction.recorded_at)
            .await,
        None
    );
    assert_eq!(w.read(acct, at(2024, 1, 1)).await, None);
    let statements = w
        .caller()
        .list_node_statements(w.tenant, acct)
        .await
        .unwrap();
    assert_eq!(statements.len(), 1, "{statements:?}");
    assert_eq!(statements[0].episode, r);
    assert_eq!(statements[0].recorded_at, retraction.recorded_at);
}

#[tokio::test]
async fn an_erasure_of_many_nodes_holds_one_advisory_lock_for_each_and_runs_on_the_default_lock_table()
 {
    const NODES: usize = 500;
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    let mut nodes = Vec::new();
    for i in 0..NODES {
        nodes.push(
            w.node(
                "account",
                e,
                ValidTime::UNKNOWN,
                json!({ "plan": format!("p{i}") }),
            )
            .await,
        );
    }
    let mut client = w.db.login().await;
    let tx = w.raw(&mut client).await;
    let row = tx
        .query_typed(
            "select current_setting('max_locks_per_transaction')::int,
                    current_setting('max_connections')::int
                      + current_setting('max_prepared_transactions')::int",
            &[],
        )
        .await
        .unwrap();
    let (per_transaction, slots): (i32, i32) = (row[0].get(0), row[0].get(1));
    tx.query_typed(
        "select from gnomon.erase_episode($1, $2)",
        &[(&w.tenant.0, Type::UUID), (&e.0, Type::UUID)],
    )
    .await
    .unwrap();
    let held: i64 = tx
        .query_typed(
            "select count(*) from pg_locks where pid = pg_backend_pid() and locktype = 'advisory'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    tx.commit().await.unwrap();
    // One lock for the Episode and one for each Node, as the migration's
    // header documents: more than max_locks_per_transaction, within the
    // shared table of max_locks_per_transaction * (max_connections +
    // max_prepared_transactions).
    assert_eq!(held, 1 + NODES as i64);
    assert!(
        held > i64::from(per_transaction),
        "the server allows {per_transaction}"
    );
    assert!(held < i64::from(per_transaction) * i64::from(slots));
    println!(
        "{held} advisory locks with max_locks_per_transaction = {per_transaction} \
         and {slots} connection slots"
    );
    for node in [nodes[0], nodes[NODES - 1]] {
        assert!(!w.exists("node", node.0).await);
    }
}

#[tokio::test]
async fn without_the_deployments_grant_erasure_fails_and_an_erased_episode_cannot_be_erased_again()
{
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    let acct = w
        .node("account", e, ValidTime::UNKNOWN, json!({ "plan": "basic" }))
        .await;
    let as_user = w.store.as_user(values(json!({
        "role": "authenticated",
        "app_metadata": { "tenant_id": w.tenant.0.to_string(), "scopes": [SCOPE] }
    })));
    let err = as_user.erase_episode(w.tenant, e).await.unwrap_err();
    assert_eq!(
        err.sqlstate(),
        Some(&tokio_postgres::error::SqlState::INSUFFICIENT_PRIVILEGE),
        "{err:?}"
    );
    assert!(w.read(acct, at(2024, 1, 1)).await.is_some());

    w.erase(e).await;
    let again = w.caller().erase_episode(w.tenant, e).await.unwrap_err();
    assert!(matches!(again, gnomon::Error::InvalidWrite(_)), "{again:?}");
}

#[tokio::test]
async fn a_node_an_edge_in_a_scope_the_caller_cannot_see_still_joins_stays_bare() {
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    let a = w
        .node("account", e, ValidTime::UNKNOWN, json!({ "plan": "basic" }))
        .await;
    // A worker of another Scope joins a with an Edge of its own.
    let billing = w.store.as_worker(w.tenant, Scope::new("billing"));
    let f = billing
        .record_episode(&NewEpisode {
            tenant: Some(w.tenant),
            scope: Scope::new("billing"),
            content: "billing's Episode".into(),
            reference_time: at(2024, 1, 2),
            authority: None,
        })
        .await
        .unwrap();
    let acme = billing
        .create_node(
            &NewNode {
                tenant: Some(w.tenant),
                scope: Scope::new("billing"),
                kind: "company".into(),
            },
            &Patch::new(f, ValidTime::UNKNOWN, values(json!({ "name": "Acme" }))),
        )
        .await
        .unwrap()
        .0;
    let both = w.store.as_user(values(json!({
        "role": "authenticated",
        "app_metadata": { "tenant_id": w.tenant.0.to_string(), "scopes": [SCOPE, "billing"] }
    })));
    let hidden = both
        .create_edge(
            &NewEdge {
                tenant: Some(w.tenant),
                scope: Scope::new("billing"),
                kind: "owns".into(),
                source: a,
                target: acme,
            },
            &Patch::new(f, ValidTime::UNKNOWN, values(json!({ "share": 3 }))),
        )
        .await
        .unwrap()
        .0;

    // The support worker cannot see the Edge, yet the Erasure goes on, and a
    // stays as a bare identity because the Edge still joins it.
    let erasure = w.erase(e).await;
    assert!(erasure.erased_nodes.is_empty(), "{erasure:?}");
    assert_eq!(erasure.recomputed_nodes, vec![a]);
    assert!(w.exists("node", a.0).await);
    assert!(w.exists("edge", hidden.0).await);
    assert_eq!(w.read(a, at(2024, 1, 1)).await, None);
}

#[tokio::test]
async fn another_episodes_retraction_of_a_version_only_the_erased_episode_supplied_goes_with_it() {
    let w = World::new().await;
    let f = w.episode(at(2024, 1, 1), None).await;
    let e = w.episode(at(2024, 2, 1), None).await;
    let g = w.episode(at(2024, 3, 1), None).await;
    let acct = w
        .node("account", f, ValidTime::UNKNOWN, json!({ "plan": "basic" }))
        .await;
    w.write(
        acct,
        e,
        ValidTime::between(at(2024, 3, 1), at(2024, 6, 1)),
        json!({ "plan": "gold" }),
    )
    .await;
    let only_e = w.read(acct, at(2024, 4, 1)).await.unwrap().id;
    // G retracts the Version only E supplied, so nothing holds over E's
    // Valid time.
    w.caller()
        .retract_node_version(
            w.tenant,
            acct,
            &Retraction {
                episode: g,
                version: only_e,
            },
        )
        .await
        .unwrap();
    assert_eq!(w.read(acct, at(2024, 4, 1)).await, None);

    w.erase(e).await;

    // As if E never existed, G retracted nothing, and F's plan holds over
    // what was E's Valid time.
    for month in [1, 4, 7] {
        assert_eq!(
            w.current_values(acct, at(2024, month, 15)).await,
            Some(json!({ "plan": "basic" })),
            "month {month}"
        );
    }
    assert!(
        w.caller()
            .list_node_statements(w.tenant, acct)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn full_text_search_and_resolution_find_a_node_by_its_name_after_erasing_an_unrelated_episode()
 {
    // Acme's current Version comes from F. G, of the same Scope, writes
    // nothing of Acme. H states a value of Acme only before its Version's
    // Valid time starts, so its write leaves that Version open and
    // unchanged. Erasing G, then H, leaves Acme found by its name both ways.
    let w = World::new().await;
    let f = w.episode(at(2024, 1, 1), None).await;
    let g = w.episode(at(2024, 1, 2), None).await;
    let h = w.episode(at(2024, 1, 3), None).await;
    let acme = w
        .node(
            "company",
            f,
            ValidTime::between(at(2024, 3, 1), at(2030, 1, 1)),
            json!({ "name": "Acme" }),
        )
        .await;
    w.node(
        "company",
        g,
        ValidTime::UNKNOWN,
        json!({ "name": "Globex" }),
    )
    .await;
    w.write(
        acme,
        h,
        ValidTime::between(at(2023, 1, 1), at(2023, 6, 1)),
        json!({ "aliases": ["Acme Ltd"] }),
    )
    .await;
    let version = w.read(acme, at(2024, 6, 1)).await.unwrap().id;
    let as_of = AsOf::current(at(2024, 6, 1));
    let found = |when: &'static str| {
        let w = &w;
        async move {
            let text = w
                .caller()
                .search_full_text(w.tenant, "acme", as_of, 10)
                .await
                .unwrap();
            assert_eq!(
                text.iter().map(|c| c.version).collect::<Vec<_>>(),
                vec![version],
                "full-text Search {when}"
            );
            let resolved = w
                .caller()
                .resolve(w.tenant, &gnomon::Mention::text("Acme"), 10)
                .await
                .unwrap();
            assert_eq!(
                resolved.iter().map(|c| c.node).collect::<Vec<_>>(),
                vec![acme],
                "Resolution {when}"
            );
        }
    };
    found("before the Erasures").await;
    w.erase(g).await;
    found("after erasing an Episode that wrote nothing of Acme").await;
    let erasure = w.erase(h).await;
    assert_eq!(erasure.recomputed_nodes, vec![acme]);
    assert_eq!(w.read(acme, at(2024, 6, 1)).await.unwrap().id, version);
    found("after erasing an Episode that left Acme's Version open").await;
}
