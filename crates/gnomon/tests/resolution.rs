//! Seam 1, Resolution (ADR-0010, ADR-0012, migration 0019): a mention
//! matched against the current names and Aliases of the Nodes the graph
//! already has, by full text and by vector, through the Rust core's Store
//! API against real Postgres. The adversarial cases live in sql_boundary.rs.

mod support;

use gnomon::{AsOf, KindOf, Mention, NewKind, ValidTime};
use serde_json::json;
use support::erasure::{at, World};
use support::vectors::{cosine_distance, Rng};
use tokio_postgres::types::Type;

#[tokio::test]
async fn a_mention_returns_ranked_candidate_nodes_with_the_name_or_alias_that_matched() {
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    let ada = w
        .node(
            "company",
            e,
            ValidTime::UNKNOWN,
            json!({ "name": "Ada", "aliases": ["Analytical Engines"] }),
        )
        .await;
    let institute = w
        .node(
            "company",
            e,
            ValidTime::UNKNOWN,
            json!({ "name": "Ada Lovelace Institute", "aliases": ["The Countess"] }),
        )
        .await;
    w.node(
        "company",
        e,
        ValidTime::UNKNOWN,
        json!({ "name": "Grace Hopper" }),
    )
    .await;

    // Both Nodes named Ada, the shorter name first: BM25 scores one word of
    // one word above one word of three.
    let found = w
        .caller()
        .resolve(w.tenant, &Mention::text("ada"), 10)
        .await
        .unwrap();
    assert_eq!(
        found.iter().map(|c| c.node).collect::<Vec<_>>(),
        vec![ada, institute]
    );
    assert_eq!(found[0].name, "Ada");
    assert!(!found[0].alias);
    assert_eq!(found[1].name, "Ada Lovelace Institute");
    assert_eq!(found.iter().map(|c| c.rank).collect::<Vec<_>>(), vec![1, 2]);
    let text = found[0].full_text.as_ref().unwrap();
    assert_eq!((text.name.as_str(), text.rank), ("Ada", 1));
    assert!(text.score > 0.0);
    assert!(found[0].vector.is_none(), "no embedding is stored");
    assert!(found[0].score > found[1].score);

    // An Alias matches as an Alias.
    let found = w
        .caller()
        .resolve(w.tenant, &Mention::text("Countess"), 10)
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].node, institute);
    assert_eq!(found[0].name, "The Countess");
    assert!(found[0].alias);

    // A word no name or Alias holds finds nothing.
    assert!(w
        .caller()
        .resolve(w.tenant, &Mention::text("Babbage"), 10)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn a_mention_with_an_embedding_is_matched_against_the_stored_embeddings_of_names_and_aliases()
{
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    let ada = w
        .node(
            "company",
            e,
            ValidTime::UNKNOWN,
            json!({ "name": "Ada", "aliases": ["The Countess"] }),
        )
        .await;
    let grace = w
        .node(
            "company",
            e,
            ValidTime::UNKNOWN,
            json!({ "name": "Grace Hopper" }),
        )
        .await;
    let mut rng = Rng::new(19);
    let (v_ada, v_countess, v_grace) = (rng.unit(), rng.unit(), rng.unit());
    for (node, name, v) in [
        (ada, "Ada", &v_ada),
        (ada, "The Countess", &v_countess),
        (grace, "Grace Hopper", &v_grace),
    ] {
        w.caller()
            .store_name_embedding(w.tenant, node, name, v)
            .await
            .unwrap();
    }

    // By vector alone: the Node whose Alias is nearest, matched by that
    // Alias at full precision, then the other.
    let q = rng.near(&v_countess, 0.3);
    let found = w
        .caller()
        .resolve(w.tenant, &Mention::embedding(&q), 10)
        .await
        .unwrap();
    assert_eq!(
        found.iter().map(|c| c.node).collect::<Vec<_>>(),
        vec![ada, grace]
    );
    assert_eq!(found[0].name, "The Countess");
    assert!(found[0].alias);
    assert!(found[0].full_text.is_none());
    let vector = found[0].vector.as_ref().unwrap();
    assert_eq!((vector.name.as_str(), vector.rank), ("The Countess", 1));
    let exact = cosine_distance(&q, &v_countess);
    assert!(
        (vector.distance - exact).abs() < 1e-5,
        "the distance is at full precision: {} against {exact}",
        vector.distance
    );

    // Text and vector together, fused: Grace first in both halves, Ada
    // found by vector alone. Reciprocal rank fusion with k = 60 gives
    // 1/61 + 1/61 and 1/62.
    let q = rng.near(&v_grace, 0.3);
    let found = w
        .caller()
        .resolve(
            w.tenant,
            &Mention::text("grace hopper").with_embedding(&q),
            10,
        )
        .await
        .unwrap();
    assert_eq!(
        found.iter().map(|c| c.node).collect::<Vec<_>>(),
        vec![grace, ada]
    );
    assert_eq!(found[0].full_text.as_ref().unwrap().rank, 1);
    assert_eq!(found[0].vector.as_ref().unwrap().rank, 1);
    assert!((found[0].score - 2.0 / 61.0).abs() < 1e-12);
    assert!(found[1].full_text.is_none());
    assert_eq!(found[1].vector.as_ref().unwrap().rank, 2);
    assert!((found[1].score - 1.0 / 62.0).abs() < 1e-12);
    assert_eq!(found[1].rank, 2);
}

#[tokio::test]
async fn only_current_names_and_aliases_are_searched() {
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    let ada = w
        .node(
            "company",
            e,
            ValidTime::UNKNOWN,
            json!({ "name": "Ada", "aliases": ["The Countess"] }),
        )
        .await;
    let mut rng = Rng::new(20);
    let (v_ada, v_countess) = (rng.unit(), rng.unit());
    for (name, v) in [("Ada", &v_ada), ("The Countess", &v_countess)] {
        w.caller()
            .store_name_embedding(w.tenant, ada, name, v)
            .await
            .unwrap();
    }
    let before = w
        .caller()
        .resolve(w.tenant, &Mention::text("ada"), 10)
        .await
        .unwrap();
    assert_eq!(before.len(), 1);
    let found_by = |text: &'static str| {
        let w = &w;
        async move {
            w.caller()
                .resolve(w.tenant, &Mention::text(text), 10)
                .await
                .unwrap()
        }
    };

    // A later Episode replaces the name and drops the Alias over the same
    // Valid time, so the Version that held them is no longer the current
    // belief. It stays readable As-of before the write.
    let later = w.episode(at(2024, 2, 1), None).await;
    let renamed_at = w
        .write(
            ada,
            later,
            ValidTime::UNKNOWN,
            json!({ "name": "Grace", "aliases": [] }),
        )
        .await;
    let past = w
        .caller()
        .read_node(
            w.tenant,
            ada,
            AsOf::at(
                at(2024, 6, 1),
                renamed_at - chrono::Duration::microseconds(1),
            ),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(past.values["name"], "Ada");
    assert!(
        found_by("ada").await.is_empty(),
        "a replaced name is not searched"
    );
    assert!(
        found_by("countess").await.is_empty(),
        "a dropped Alias is not searched"
    );
    for v in [&v_ada, &v_countess] {
        assert!(
            w.caller()
                .resolve(w.tenant, &Mention::embedding(v), 10)
                .await
                .unwrap()
                .is_empty(),
            "a replaced name's embedding matches nothing"
        );
    }
    assert_eq!(found_by("grace").await[0].node, ada);

    // A name the Node had over an earlier Valid time is still current: the
    // Store still believes the Node was called that then, and a mention in
    // an Episode may use it. A valid-at instant narrows the match.
    let twitter = w
        .node(
            "company",
            e,
            ValidTime::since(at(2010, 1, 1)),
            json!({ "name": "Twitter" }),
        )
        .await;
    w.write(
        twitter,
        later,
        ValidTime::since(at(2023, 7, 24)),
        json!({ "name": "X Corp" }),
    )
    .await;
    assert_eq!(found_by("twitter").await[0].node, twitter);
    assert_eq!(found_by("corp").await[0].node, twitter);
    let at_valid = |text: &'static str, when| {
        let w = &w;
        async move {
            w.caller()
                .resolve(w.tenant, &Mention::text(text).valid_at(when), 10)
                .await
                .unwrap()
        }
    };
    assert_eq!(at_valid("twitter", at(2020, 1, 1)).await[0].node, twitter);
    assert!(at_valid("twitter", at(2024, 1, 1)).await.is_empty());
    assert!(at_valid("corp", at(2020, 1, 1)).await.is_empty());

    // Only names and Aliases are searched: a property a Kind marks as
    // searchable is found by full-text Search and never by Resolution.
    w.caller()
        .register_kind(&NewKind {
            tenant: Some(w.tenant),
            name: "product".into(),
            of: KindOf::Node,
            parent: None,
            properties: json!({ "type": "object",
                                "properties": { "tagline": { "type": "string" } } }),
            search: vec!["tagline".into()],
        })
        .await
        .unwrap();
    let engine = w
        .node(
            "product",
            e,
            ValidTime::UNKNOWN,
            json!({ "name": "Engine", "tagline": "computes Bernoulli numbers" }),
        )
        .await;
    let searched = w
        .caller()
        .search_full_text(w.tenant, "bernoulli", AsOf::current(at(2024, 6, 1)), 10)
        .await
        .unwrap();
    assert_eq!(searched.len(), 1);
    assert_eq!(searched[0].of, gnomon::Versioned::Node(engine));
    assert!(found_by("bernoulli").await.is_empty());
    assert_eq!(found_by("engine").await[0].node, engine);
}

#[tokio::test]
async fn a_node_renamed_in_the_same_session_is_found_under_its_new_name_through_full_text_before_its_embedding_exists(
) {
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    let node = w
        .node("company", e, ValidTime::UNKNOWN, json!({ "name": "Ada" }))
        .await;
    let mut rng = Rng::new(21);
    let (v_ada, v_grace) = (rng.unit(), rng.unit());
    w.caller()
        .store_name_embedding(w.tenant, node, "Ada", &v_ada)
        .await
        .unwrap();

    // The rename, then at once the Pipeline resolves a mention of the new
    // name with its embedding. No embedding of the new name exists yet.
    let later = w.episode(at(2024, 2, 1), None).await;
    w.write(
        node,
        later,
        ValidTime::UNKNOWN,
        json!({ "name": "Grace Hopper" }),
    )
    .await;
    let found = w
        .caller()
        .resolve(
            w.tenant,
            &Mention::text("Grace Hopper").with_embedding(&v_grace),
            10,
        )
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].node, node);
    assert_eq!(found[0].name, "Grace Hopper");
    assert_eq!(found[0].full_text.as_ref().unwrap().name, "Grace Hopper");
    assert!(
        found[0].vector.is_none(),
        "the new name has no embedding and the old name is not current: {:?}",
        found[0].vector
    );

    // Once the Pipeline stores it, the vector half finds it too.
    w.caller()
        .store_name_embedding(w.tenant, node, "Grace Hopper", &v_grace)
        .await
        .unwrap();
    let found = w
        .caller()
        .resolve(w.tenant, &Mention::embedding(&v_grace), 10)
        .await
        .unwrap();
    assert_eq!(found[0].node, node);
    assert_eq!(found[0].vector.as_ref().unwrap().name, "Grace Hopper");

    // Within one transaction too: the write's name rows and token rows are
    // there before it commits, and the word counts are its own changes.
    let mut client = w.db.login().await;
    let tx = w.raw(&mut client).await;
    tx.query_typed(
        "select * from gnomon.write_node($1, $2, $3, null, 'unknown', null, 'unknown', $4, null)",
        &[
            (&w.tenant.0, Type::UUID),
            (&node.0, Type::UUID),
            (&later.0, Type::UUID),
            (&json!({ "name": "Countess Lovelace" }), Type::JSONB),
        ],
    )
    .await
    .unwrap();
    let names: Vec<(uuid::Uuid, String)> = tx
        .query_typed(
            "select node_id, name from gnomon.resolve($1, 'lovelace', null, null, 10)",
            &[(&w.tenant.0, Type::UUID)],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    assert_eq!(names, vec![(node.0, "Countess Lovelace".to_owned())]);
    tx.commit().await.unwrap();
    assert_eq!(
        w.caller()
            .resolve(w.tenant, &Mention::text("countess"), 10)
            .await
            .unwrap()[0]
            .node,
        node
    );
}

/// How many name rows and full vectors of `name` are stored, past RLS.
async fn stored(w: &World, name: &str) -> (i64, i64) {
    let r =
        w.db.admin()
            .await
            .query_typed(
                "select (select count(*) from gnomon.node_name where name = $1),
                    (select count(*) from gnomon.embedding
                     where text_hash = gnomon.sentence_text_hash($1))",
                &[(&name, Type::TEXT)],
            )
            .await
            .unwrap();
    (r[0].get(0), r[0].get(1))
}

#[tokio::test]
async fn erasure_deletes_the_name_and_alias_rows_of_erased_text_and_keeps_a_shared_full_vector() {
    let w = World::new().await;
    let first = w.episode(at(2024, 1, 1), None).await;
    let erased = w.episode(at(2024, 2, 1), None).await;
    // The erased Episode alone gives Ada the Alias "Analytical Engine". Its
    // text "The Countess" is also the name of another Node, from the first
    // Episode.
    let ada = w
        .node(
            "company",
            first,
            ValidTime::UNKNOWN,
            json!({ "name": "Ada" }),
        )
        .await;
    w.write(
        ada,
        erased,
        ValidTime::UNKNOWN,
        json!({ "aliases": ["Analytical Engine", "The Countess"] }),
    )
    .await;
    let countess = w
        .node(
            "company",
            first,
            ValidTime::UNKNOWN,
            json!({ "name": "The Countess" }),
        )
        .await;
    let grace = w
        .node(
            "company",
            first,
            ValidTime::UNKNOWN,
            json!({ "name": "Grace Hopper" }),
        )
        .await;
    let mut rng = Rng::new(22);
    let (v_ada, v_engine, v_countess, v_grace) = (rng.unit(), rng.unit(), rng.unit(), rng.unit());
    for (node, name, v) in [
        (ada, "Ada", &v_ada),
        (ada, "Analytical Engine", &v_engine),
        (ada, "The Countess", &v_countess),
        (grace, "Grace Hopper", &v_grace),
    ] {
        w.caller()
            .store_name_embedding(w.tenant, node, name, v)
            .await
            .unwrap();
    }
    assert_eq!(stored(&w, "Analytical Engine").await, (1, 1));
    assert_eq!(stored(&w, "The Countess").await, (2, 1));

    w.erase(erased).await;

    // The erased Alias is gone: its name rows, its full vector, and every
    // match by its text or its embedding.
    assert_eq!(stored(&w, "Analytical Engine").await, (0, 0));
    assert!(w
        .caller()
        .resolve(w.tenant, &Mention::text("analytical"), 10)
        .await
        .unwrap()
        .is_empty());
    assert!(w
        .caller()
        .resolve(w.tenant, &Mention::embedding(&v_engine), 10)
        .await
        .unwrap()
        .iter()
        .all(|c| c.node != ada));
    // The other Node's name with the same text keeps the full vector, and
    // only Ada lost the Alias.
    assert_eq!(stored(&w, "The Countess").await, (1, 1));
    let found = w
        .caller()
        .resolve(
            w.tenant,
            &Mention::text("countess").with_embedding(&v_countess),
            10,
        )
        .await
        .unwrap();
    assert_eq!(
        found.iter().map(|c| c.node).collect::<Vec<_>>()[..1],
        [countess]
    );
    assert!(found.iter().all(|c| c.node != ada));
    // Ada's name stands, as the first Episode gave it, found by full text.
    // The Version that held it was recomputed, so its embedding goes with
    // it, and the Pipeline embeds the name again.
    assert_eq!(
        w.caller()
            .resolve(w.tenant, &Mention::text("ada"), 10)
            .await
            .unwrap()[0]
            .node,
        ada
    );
    assert_eq!(stored(&w, "Ada").await, (1, 0));
    w.caller()
        .store_name_embedding(w.tenant, ada, "Ada", &v_ada)
        .await
        .unwrap();
    assert_eq!(
        w.caller()
            .resolve(w.tenant, &Mention::embedding(&v_ada), 1)
            .await
            .unwrap()[0]
            .node,
        ada
    );

    // Erasing a Node erases its names and their full vectors.
    w.erase_node(grace).await;
    assert_eq!(stored(&w, "Grace Hopper").await, (0, 0));
}

#[tokio::test]
async fn a_name_uses_the_full_vector_its_text_already_has_and_needs_no_new_embedding() {
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    let mut rng = Rng::new(23);

    // A Sentence "Ada Lovelace owns Acme" is embedded as Search embeds it.
    let lovelace = w
        .node(
            "account",
            e,
            ValidTime::UNKNOWN,
            json!({ "name": "Ada Lovelace" }),
        )
        .await;
    let acme = w
        .node("company", e, ValidTime::UNKNOWN, json!({ "name": "Acme" }))
        .await;
    let owns = w
        .edge(lovelace, acme, e, ValidTime::UNKNOWN, json!({}))
        .await;
    let edge_version = w.current_edge_version(owns, at(2024, 6, 1)).await;
    let sentence = "Ada Lovelace owns Acme";
    w.store_sentence(edge_version, sentence).await;
    let v_sentence = rng.unit();
    w.caller()
        .store_embedding(
            w.tenant,
            edge_version,
            &support::vectors::sentence_hash(&w, sentence).await,
            &v_sentence,
        )
        .await
        .unwrap();

    // A Node whose Alias has that text is matched by its full vector at
    // once, with no embedding stored for the name.
    let node = w
        .node(
            "company",
            e,
            ValidTime::UNKNOWN,
            json!({ "name": "Holding", "aliases": [sentence] }),
        )
        .await;
    let found = w
        .caller()
        .resolve(w.tenant, &Mention::embedding(&v_sentence), 10)
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].node, node);
    assert_eq!(found[0].name, sentence);
    assert!(found[0].vector.as_ref().unwrap().distance.abs() < 1e-5);

    // The first embedding stored for a text is the one every name with
    // that text uses: a second Node with the name, and a later Version of
    // the first that keeps it, need no call, and a later call's vector is
    // not used.
    let v_acme = rng.unit();
    w.caller()
        .store_name_embedding(w.tenant, acme, "Acme", &v_acme)
        .await
        .unwrap();
    let twin = w
        .node("company", e, ValidTime::UNKNOWN, json!({ "name": "Acme" }))
        .await;
    w.write(
        acme,
        w.episode(at(2024, 2, 1), None).await,
        ValidTime::UNKNOWN,
        json!({ "aliases": ["Acme Corporation"] }),
    )
    .await;
    w.caller()
        .store_name_embedding(w.tenant, twin, "Acme", &rng.unit())
        .await
        .unwrap();
    let found = w
        .caller()
        .resolve(w.tenant, &Mention::embedding(&v_acme), 10)
        .await
        .unwrap();
    let mut both = [acme, twin];
    both.sort_by_key(|n| n.0);
    assert_eq!(
        found.iter().map(|c| c.node).collect::<Vec<_>>()[..2],
        both[..]
    );
    for c in &found[..2] {
        assert_eq!(c.name, "Acme");
        assert!(c.vector.as_ref().unwrap().distance.abs() < 1e-5, "{c:?}");
    }
    assert_eq!(stored(&w, "Acme").await.1, 1, "one full vector per text");
}

#[tokio::test]
async fn storing_a_name_embedding_refuses_a_name_the_node_does_not_have_now_or_a_bad_vector() {
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    let ada = w
        .node("company", e, ValidTime::UNKNOWN, json!({ "name": "Ada" }))
        .await;
    w.write(
        ada,
        w.episode(at(2024, 2, 1), None).await,
        ValidTime::UNKNOWN,
        json!({ "name": "Grace" }),
    )
    .await;
    let v = Rng::new(24).unit();
    for name in ["Ada", "Babbage", ""] {
        let refused = w
            .caller()
            .store_name_embedding(w.tenant, ada, name, &v)
            .await;
        assert!(
            matches!(refused, Err(gnomon::Error::InvalidWrite(_))),
            "{name:?}: {refused:?}"
        );
    }
    for bad in [vec![1.0f32; 3], vec![0.0f32; v.len()], {
        let mut x = v.clone();
        x[0] = f32::NAN;
        x
    }] {
        let refused = w
            .caller()
            .store_name_embedding(w.tenant, ada, "Grace", &bad)
            .await;
        assert_eq!(
            refused.unwrap_err().sqlstate(),
            Some(&tokio_postgres::error::SqlState::INVALID_PARAMETER_VALUE)
        );
        let refused = w
            .caller()
            .resolve(w.tenant, &Mention::embedding(&bad), 10)
            .await;
        assert_eq!(
            refused.unwrap_err().sqlstate(),
            Some(&tokio_postgres::error::SqlState::INVALID_PARAMETER_VALUE)
        );
    }
    let refused = w
        .caller()
        .resolve(w.tenant, &Mention::text("grace"), 0)
        .await;
    assert_eq!(
        refused.unwrap_err().sqlstate(),
        Some(&tokio_postgres::error::SqlState::INVALID_PARAMETER_VALUE)
    );
    // A Node the caller cannot see.
    let other = w
        .node_in(
            "billing",
            "company",
            w.episode_in("billing", at(2024, 1, 1), "billing").await,
            json!({ "name": "Grace" }),
        )
        .await;
    let refused = w
        .caller()
        .store_name_embedding(w.tenant, other, "Grace", &v)
        .await;
    assert!(
        matches!(refused, Err(gnomon::Error::InvalidWrite(_))),
        "{refused:?}"
    );
    assert_eq!(stored(&w, "Grace").await, (2, 0));
}

/// Mean recall@10 of Resolution's vector half against an exact
/// full-precision search over the current names, by Node, for `queries`, by
/// a caller who sees `scopes`.
async fn recall_at_10(
    w: &World,
    corpus: &support::vectors::NameCorpus,
    scopes: &[&str],
    queries: &[Vec<f32>],
    valid_at: Option<chrono::DateTime<chrono::Utc>>,
) -> f64 {
    let caller = w.seeing(scopes);
    let mut total = 0.0;
    for q in queries {
        let truth = corpus.exact(q, scopes, valid_at, 10);
        assert_eq!(truth.len(), 10, "the visible set holds at least 10 Nodes");
        let mention = Mention {
            valid_at,
            ..Mention::embedding(q)
        };
        let got = caller.resolve(w.tenant, &mention, 10).await.unwrap();
        for c in &got {
            assert!(
                scopes.contains(&c.scope.as_str()),
                "a Node of Scope {} was found by a caller of {scopes:?}",
                c.scope
            );
        }
        let hit = got.iter().filter(|c| truth.contains(&c.node.0)).count();
        total += hit as f64 / 10.0;
    }
    total / queries.len() as f64
}

#[tokio::test]
async fn recall_at_10_matches_prototype_5_for_current_names_in_a_small_scope_inside_a_large_tenant()
{
    // Prototype 5's layout, scaled down: a Scope of 1,000 Nodes inside a
    // Tenant whose other Scope holds 4,000, on clustered synthetic
    // 1,536-dimension vectors, each Node with one to three current names
    // and a third with a replaced name as well. The small Scope draws on
    // four subtopics and the large one on all 80, the small Scope's four
    // among them, so the large Scope's names crowd every query of the small
    // one, and replaced names sit among the current ones. Prototype 5 spread
    // its names over 2,000 subtopics, so a query's neighbourhood here is
    // denser than any it measured: drawn on 22 subtopics, as the vector
    // Search suite's large Scope is, the large Scope put about 650 current
    // names in each subtopic a query falls in, and recall over both Scopes
    // was 0.90 with the re-rank at 400 and 1.00 with it at 4,000.
    let w = World::new().await;
    let mut rng = Rng::new(25);
    let centres = support::vectors::subtopics(&mut rng, 8, 10);
    let small: Vec<Vec<f32>> = centres[..4].to_vec();
    let large: Vec<Vec<f32>> = centres.clone();
    let mut corpus = support::vectors::NameCorpus::default();
    support::vectors::load_names(
        &w.db,
        &mut corpus,
        w.tenant,
        "small",
        1_000,
        &small,
        &mut rng,
    )
    .await;
    support::vectors::load_names(
        &w.db,
        &mut corpus,
        w.tenant,
        "large",
        4_000,
        &large,
        &mut rng,
    )
    .await;
    let queries: Vec<Vec<f32>> = (0..12)
        .map(|_| {
            let c = &small[rng.below(small.len())];
            rng.near(c, 0.6)
        })
        .collect();

    // Prototype 5 measured recall@10 of 0.97 for current names through the
    // partial index at 1,000 visible rows, 0.98 at 10,000, 0.97 at 50,000
    // and 0.95 at 200,000, re-ranking 200 (FINDINGS.md, measurement 4). Its
    // name vectors and ours are drawn the same way, and ours re-ranks 400.
    // The tolerance is 0.05: synthetic clusters hold many near-equidistant
    // neighbours, and pgvector sums in single precision where the oracle
    // sums in double.
    const PROTOTYPE_5: f64 = 0.97;
    const TOLERANCE: f64 = 0.05;
    let mut measured = Vec::new();
    for (label, scopes, valid_at) in [
        ("small Scope", &["small"][..], None),
        ("both Scopes", &["small", "large"][..], None),
        (
            "both Scopes, valid at 2022-01-01",
            &["small", "large"][..],
            Some(at(2022, 1, 1)),
        ),
    ] {
        let recall = recall_at_10(&w, &corpus, scopes, &queries, valid_at).await;
        println!(
            "recall@10 {label}: {recall:.3} over {} current names",
            corpus.current(scopes)
        );
        measured.push((label, recall));
    }
    for (label, recall) in measured {
        assert!(
            recall >= PROTOTYPE_5 - TOLERANCE,
            "recall@10 {label} is {recall:.3}, below prototype 5's {PROTOTYPE_5} less {TOLERANCE}"
        );
    }
}

/// The sequential scans, index scans and rows fetched by index scans of the
/// full-vector table, in this backend's pending statistics.
async fn full_vector_reads(tx: &tokio_postgres::Transaction<'_>) -> (i64, i64, i64) {
    let r = tx
        .query_typed(
            "select coalesce(seq_scan, 0), coalesce(idx_scan, 0), coalesce(idx_tup_fetch, 0)
             from pg_stat_xact_user_tables where relid = 'gnomon.embedding'::regclass",
            &[],
        )
        .await
        .unwrap();
    (r[0].get(0), r[0].get(1), r[0].get(2))
}

#[tokio::test]
async fn the_re_rank_reads_each_candidate_names_full_vector_by_one_primary_key_probe() {
    // As for vector Search (ADR-0010): under RLS the planner estimates one
    // row for every scan, so an unpinned re-rank can scan every visible full
    // vector. The plan must probe the full vector of each of the 400
    // candidates by its key and read no other, even when the planner is
    // pushed off nested loops. No timing is asserted.
    let w = World::new().await;
    let mut rng = Rng::new(26);
    let centres = support::vectors::subtopics(&mut rng, 1, 4);
    let mut corpus = support::vectors::NameCorpus::default();
    support::vectors::load_names(
        &w.db,
        &mut corpus,
        w.tenant,
        support::erasure::SCOPE,
        600,
        &centres,
        &mut rng,
    )
    .await;
    let current = corpus.current(&[support::erasure::SCOPE]);
    assert!(
        current > 400,
        "more names are current than the re-rank keeps: {current}"
    );
    let query = rng.near(&centres[0], 0.6);

    let mut client = w.db.login().await;
    for setting in ["", "set local enable_nestloop = off"] {
        let tx = w.raw(&mut client).await;
        if !setting.is_empty() {
            tx.batch_execute(setting).await.unwrap();
        }
        let before = full_vector_reads(&tx).await;
        let found = tx
            .query_typed(
                "select count(*) from gnomon.resolve($1, '', $2, null, 10)",
                &[(&w.tenant.0, Type::UUID), (&query, Type::FLOAT4_ARRAY)],
            )
            .await
            .unwrap()[0]
            .get::<_, i64>(0);
        assert_eq!(found, 10);
        let after = full_vector_reads(&tx).await;
        let (seq, probes, fetched) = (after.0 - before.0, after.1 - before.1, after.2 - before.2);
        assert_eq!(
            (seq, probes, fetched),
            (0, 400, 400),
            "with {setting:?}, the re-rank read the full vectors by {seq} sequential scans and \
             {probes} index scans fetching {fetched} rows, where 400 probes of one row each \
             are the plan; {} full vectors are stored",
            corpus.texts.len()
        );
        tx.rollback().await.unwrap();
    }
}

#[tokio::test]
async fn fusion_ranks_a_node_both_halves_find_second_above_one_a_single_half_finds_first() {
    // Reciprocal rank fusion with k = 60: second in both halves is 2/62,
    // first in one half alone is 1/61. Each half must hand fusion more than
    // the call's limit, or a call for one candidate never sees the Node
    // both halves found.
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    let mut rng = Rng::new(27);
    let target = rng.unit();
    // First by text alone: the mention's one word, in a name of one word.
    let by_text = w
        .node(
            "company",
            e,
            ValidTime::UNKNOWN,
            json!({ "name": "Lovelace" }),
        )
        .await;
    // First by vector alone: a name with no word of the mention, nearest.
    let by_vector = w
        .node(
            "company",
            e,
            ValidTime::UNKNOWN,
            json!({ "name": "Countess" }),
        )
        .await;
    // Second in both: the word in a longer name, embedded a little further.
    let both = w
        .node(
            "company",
            e,
            ValidTime::UNKNOWN,
            json!({ "name": "Ada Lovelace Institute" }),
        )
        .await;
    w.caller()
        .store_name_embedding(w.tenant, by_vector, "Countess", &rng.near(&target, 0.1))
        .await
        .unwrap();
    w.caller()
        .store_name_embedding(
            w.tenant,
            both,
            "Ada Lovelace Institute",
            &rng.near(&target, 0.6),
        )
        .await
        .unwrap();
    let mention = Mention::text("lovelace").with_embedding(&target);
    let all = w.caller().resolve(w.tenant, &mention, 10).await.unwrap();
    let ranks = |node| {
        let c = all.iter().find(|c| c.node == node).unwrap();
        (
            c.full_text.as_ref().map(|m| m.rank),
            c.vector.as_ref().map(|m| m.rank),
        )
    };
    assert_eq!(ranks(by_text), (Some(1), None));
    assert_eq!(ranks(by_vector), (None, Some(1)));
    assert_eq!(ranks(both), (Some(2), Some(2)));
    assert_eq!(all[0].node, both);

    let one = w.caller().resolve(w.tenant, &mention, 1).await.unwrap();
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].node, both, "{one:?}");
    assert!((one[0].score - 2.0 / 62.0).abs() < 1e-12);
}

#[tokio::test]
async fn full_text_scores_count_only_the_callers_allowed_scopes() {
    // BM25 reads the word counts of the caller's allowed Scopes alone
    // (ADR-0010), so another Scope's names never change a score or an
    // order the caller sees.
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    w.node("company", e, ValidTime::UNKNOWN, json!({ "name": "Ada" }))
        .await;
    w.node("company", e, ValidTime::UNKNOWN, json!({ "name": "Grace" }))
        .await;
    let score = || async {
        w.seeing(&[support::erasure::SCOPE])
            .resolve(w.tenant, &Mention::text("ada"), 10)
            .await
            .unwrap()[0]
            .full_text
            .as_ref()
            .unwrap()
            .score
    };
    let before = score().await;
    let billing = w.episode_in("billing", at(2024, 1, 1), "billing").await;
    for i in 0..20 {
        w.node_in(
            "billing",
            "company",
            billing,
            json!({ "name": format!("Ada {i}") }),
        )
        .await;
    }
    assert_eq!(
        score().await,
        before,
        "billing's names changed support's score"
    );
    // A caller who sees both Scopes counts both, and the word is commoner.
    let both = w
        .seeing(&[support::erasure::SCOPE, "billing"])
        .resolve(w.tenant, &Mention::text("ada"), 30)
        .await
        .unwrap();
    assert_eq!(both.len(), 21);
    let support_ada = both
        .iter()
        .find(|c| c.scope.as_str() == support::erasure::SCOPE)
        .unwrap();
    assert!(support_ada.full_text.as_ref().unwrap().score < before);
}
