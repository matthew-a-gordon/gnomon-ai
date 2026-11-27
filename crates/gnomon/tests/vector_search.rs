//! Seam 1, the vector branch of Search (ADR-0010): an embedding stored after
//! the write, and a query embedding at a valid-at and a known-at instant
//! returning the nearest Versions at full precision, exactly over the rows
//! the caller can see, through the Rust core's Store API against real
//! Postgres. The adversarial cases live in sql_boundary.rs.

mod support;

use gnomon::{
    AsOf, Bound, BoundSide, BoundStatement, Precision, ValidTime, VectorCandidate, Versioned,
};
use jiff::SignedDuration;
use serde_json::json;
use support::erasure::{SCOPE, World, at};
use support::vectors::{Rng, cosine_distance, embed_every_version, sentence_hash};

fn now() -> AsOf {
    AsOf::current(at(2024, 6, 1))
}

#[tokio::test]
async fn a_version_is_found_by_its_vector_once_its_embedding_is_stored() {
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    let ada = w
        .node("account", e, ValidTime::UNKNOWN, json!({ "name": "Ada" }))
        .await;
    let version = w.read(ada, at(2024, 6, 1)).await.unwrap();
    let mut rng = Rng::new(1);
    let embedding = rng.unit();

    assert!(
        w.caller()
            .search_vector(w.tenant, &embedding, now(), 10)
            .await
            .unwrap()
            .is_empty(),
        "nothing is found before an embedding is stored"
    );
    w.caller()
        .store_embedding(w.tenant, version.id, &version.text_hash, &embedding)
        .await
        .unwrap();

    let query = rng.near(&embedding, 0.3);
    let found: Vec<VectorCandidate> = w
        .caller()
        .search_vector(w.tenant, &query, now(), 10)
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].version, version.id);
    assert_eq!(found[0].of, Versioned::Node(ada));
    assert_eq!(found[0].rank, 1);
    let exact = cosine_distance(&query, &embedding);
    assert!(
        (found[0].distance - exact).abs() < 1e-5,
        "the distance is at full precision: {} against {exact}",
        found[0].distance
    );
}

#[tokio::test]
async fn versions_with_the_same_text_share_one_full_vector_and_each_has_its_own_quantized_row() {
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    // A Version, then its Refinement, which states the start it left unknown
    // and so has the same text (ADR-0008); and a second account with the
    // same values in the same Scope.
    let a = w
        .node("account", e, ValidTime::UNKNOWN, json!({ "plan": "basic" }))
        .await;
    let first = w.read(a, at(2024, 6, 1)).await.unwrap();
    let refined_at = w
        .caller()
        .state_bound(
            w.tenant,
            a,
            &BoundStatement {
                episode: e,
                version: first.id,
                side: BoundSide::Start,
                bound: Bound::Period {
                    at: at(2023, 1, 1),
                    precision: Precision::Year,
                },
            },
        )
        .await
        .unwrap()
        .recorded_at;
    let refinement = w.read(a, at(2024, 6, 1)).await.unwrap();
    assert_ne!(refinement.id, first.id);
    assert_eq!(refinement.text_hash, first.text_hash);
    let b = w
        .node("account", e, ValidTime::UNKNOWN, json!({ "plan": "basic" }))
        .await;
    let twin = w.read(b, at(2024, 6, 1)).await.unwrap();
    assert_eq!(twin.text_hash, first.text_hash);

    // The first store keeps its vector. The later ones pass other vectors
    // for the same text, and the full vector stored first is theirs too.
    let mut rng = Rng::new(2);
    let stored = rng.unit();
    for (version, embedding) in [
        (first.id, stored.clone()),
        (refinement.id, rng.unit()),
        (twin.id, rng.unit()),
    ] {
        w.caller()
            .store_embedding(w.tenant, version, &first.text_hash, &embedding)
            .await
            .unwrap();
    }

    let query = rng.near(&stored, 0.5);
    let exact = cosine_distance(&query, &stored);
    let current = w
        .caller()
        .search_vector(w.tenant, &query, now(), 10)
        .await
        .unwrap();
    assert_eq!(
        current.iter().map(|c| c.version).collect::<Vec<_>>(),
        {
            let mut v = vec![refinement.id, twin.id];
            v.sort_by_key(|x| x.0);
            v
        },
        "each current Version is found once, ties going to the lower Version ID"
    );
    for c in &current {
        assert!((c.distance - exact).abs() < 1e-5, "{c:?} against {exact}");
    }
    // The first Version, closed by the Refinement, is found As-of before it,
    // when the second account did not exist yet.
    let before = w
        .caller()
        .search_vector(
            w.tenant,
            &query,
            AsOf::at(at(2024, 6, 1), refined_at - SignedDuration::from_micros(1)),
            10,
        )
        .await
        .unwrap();
    assert_eq!(
        before.iter().map(|c| c.version).collect::<Vec<_>>(),
        vec![first.id]
    );

    let admin = w.db.admin().await;
    let count = |sql: &'static str| {
        let admin = &admin;
        async move { admin.query_typed(sql, &[]).await.unwrap()[0].get::<_, i64>(0) }
    };
    assert_eq!(count("select count(*) from gnomon.embedding").await, 1);
    assert_eq!(count("select count(*) from gnomon.embedding_bit").await, 3);
}

#[tokio::test]
async fn the_same_text_in_another_scope_has_a_full_vector_of_its_own() {
    let w = World::new().await;
    let support = w.episode(at(2024, 1, 1), None).await;
    let billing = w.episode_in("billing", at(2024, 1, 1), "an Episode").await;
    let a = w
        .node(
            "account",
            support,
            ValidTime::UNKNOWN,
            json!({ "plan": "basic" }),
        )
        .await;
    let b = w
        .node_in("billing", "account", billing, json!({ "plan": "basic" }))
        .await;
    let a_version = w.read(a, at(2024, 6, 1)).await.unwrap();
    let both = w.seeing(&[SCOPE, "billing"]);
    let b_version = both.read_node(w.tenant, b, now()).await.unwrap().unwrap();
    assert_eq!(a_version.text_hash, b_version.text_hash);

    let mut rng = Rng::new(3);
    let (va, vb) = (rng.unit(), rng.unit());
    w.caller()
        .store_embedding(w.tenant, a_version.id, &a_version.text_hash, &va)
        .await
        .unwrap();
    w.worker_in("billing")
        .store_embedding(w.tenant, b_version.id, &b_version.text_hash, &vb)
        .await
        .unwrap();

    // Each Scope keeps the vector it stored.
    let hits = both.search_vector(w.tenant, &vb, now(), 10).await.unwrap();
    assert_eq!(hits[0].version, b_version.id);
    assert!(hits[0].distance.abs() < 1e-5);
    assert_eq!(hits[1].version, a_version.id);
    assert!((hits[1].distance - cosine_distance(&vb, &va)).abs() < 1e-5);
    // A caller of one Scope finds only its own.
    let support_only = w
        .caller()
        .search_vector(w.tenant, &vb, now(), 10)
        .await
        .unwrap();
    assert_eq!(
        support_only.iter().map(|c| c.version).collect::<Vec<_>>(),
        vec![a_version.id]
    );
}

#[tokio::test]
async fn vector_search_returns_the_top_k_at_full_precision_nearest_first() {
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    let mut rng = Rng::new(4);
    let centre = rng.unit();
    let mut stored = Vec::new();
    for i in 0..30 {
        let node = w
            .node(
                "company",
                e,
                ValidTime::UNKNOWN,
                json!({ "name": format!("company {i}") }),
            )
            .await;
        let version = w.read(node, at(2024, 6, 1)).await.unwrap();
        let embedding = rng.near(&centre, 0.4 + 0.05 * i as f64);
        w.caller()
            .store_embedding(w.tenant, version.id, &version.text_hash, &embedding)
            .await
            .unwrap();
        stored.push((version.id, embedding));
    }

    let query = rng.near(&centre, 0.2);
    let mut truth: Vec<(f64, gnomon::VersionId)> = stored
        .iter()
        .map(|(id, v)| (cosine_distance(&query, v), *id))
        .collect();
    truth.sort_by(|a, b| a.partial_cmp(b).unwrap());

    let found = w
        .caller()
        .search_vector(w.tenant, &query, now(), 7)
        .await
        .unwrap();
    assert_eq!(found.len(), 7, "at most the limit");
    for (i, (c, (d, id))) in found.iter().zip(&truth).enumerate() {
        assert_eq!(c.version, *id, "rank {}", i + 1);
        assert_eq!(c.rank, i as i64 + 1);
        assert!((c.distance - d).abs() < 1e-5, "{c:?} against {d}");
    }
}

#[tokio::test]
async fn vector_search_honours_the_valid_at_and_known_at_instants() {
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    let mut rng = Rng::new(5);
    // An account on the basic plan through 2024, whose embedding is stored,
    // then a write that moves it to pro from 2025, which closes that Version
    // after its embedding was stored.
    let a = w
        .node(
            "account",
            e,
            ValidTime::between(at(2024, 1, 1), at(2026, 1, 1)),
            json!({ "plan": "basic" }),
        )
        .await;
    let basic = w.read(a, at(2024, 6, 1)).await.unwrap();
    let v_basic = rng.unit();
    w.caller()
        .store_embedding(w.tenant, basic.id, &basic.text_hash, &v_basic)
        .await
        .unwrap();
    let changed_at = w
        .write(
            a,
            e,
            ValidTime::between(at(2025, 1, 1), at(2026, 1, 1)),
            json!({ "plan": "pro" }),
        )
        .await;
    let early = w.read(a, at(2024, 6, 1)).await.unwrap();
    let pro = w.read(a, at(2025, 6, 1)).await.unwrap();
    assert_ne!(
        early.id, basic.id,
        "the write replaced the Version over 2025"
    );
    for version in [&early, &pro] {
        w.caller()
            .store_embedding(w.tenant, version.id, &version.text_hash, &rng.unit())
            .await
            .unwrap();
    }

    let found = |as_of: AsOf| {
        let w = &w;
        let q = v_basic.clone();
        async move {
            w.caller()
                .search_vector(w.tenant, &q, as_of, 10)
                .await
                .unwrap()
                .into_iter()
                .map(|c| c.version)
                .collect::<Vec<_>>()
        }
    };
    let before = changed_at - SignedDuration::from_micros(1);
    // Before the write, the basic Version held at both valid-at instants.
    assert_eq!(
        found(AsOf::at(at(2024, 6, 1), before)).await,
        vec![basic.id]
    );
    assert_eq!(
        found(AsOf::at(at(2025, 6, 1), before)).await,
        vec![basic.id]
    );
    // Now each instant has its own Version, and the closed one is not found.
    assert_eq!(found(AsOf::current(at(2024, 6, 1))).await, vec![early.id]);
    assert_eq!(found(AsOf::current(at(2025, 6, 1))).await, vec![pro.id]);
    assert_eq!(
        found(AsOf::at(at(2025, 6, 1), changed_at)).await,
        vec![pro.id]
    );
    // Outside the Valid time, and before anything was recorded, nothing.
    assert!(found(AsOf::current(at(2027, 1, 1))).await.is_empty());
    assert!(
        found(AsOf::at(
            at(2024, 6, 1),
            basic.recorded_from - SignedDuration::from_secs(1)
        ))
        .await
        .is_empty()
    );
}

#[tokio::test]
async fn an_edge_version_is_embedded_under_its_sentences_hash_or_its_own_and_no_other() {
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    let a = w
        .node("account", e, ValidTime::UNKNOWN, json!({ "plan": "basic" }))
        .await;
    let c = w
        .node("company", e, ValidTime::UNKNOWN, json!({ "name": "Acme" }))
        .await;
    let owns = w
        .edge(a, c, e, ValidTime::UNKNOWN, json!({ "share": 40 }))
        .await;
    let version = w.current_edge_version(owns, at(2024, 6, 1)).await;
    let edge = w
        .caller()
        .read_edge(w.tenant, owns, now())
        .await
        .unwrap()
        .unwrap();
    let mut rng = Rng::new(6);
    let (by_values, by_sentence) = (rng.unit(), rng.unit());

    // Before its Sentence exists, an Edge Version is embedded by its values.
    w.caller()
        .store_embedding(w.tenant, version, &edge.text_hash, &by_values)
        .await
        .unwrap();
    let sentence = "a owns forty percent of Acme";
    let unstored = sentence_hash(&w, sentence).await;
    let refused = w
        .caller()
        .store_embedding(w.tenant, version, &unstored, &by_sentence)
        .await
        .unwrap_err();
    assert!(
        matches!(refused, gnomon::Error::InvalidWrite(_)),
        "a Sentence not stored yet is not the Version's text: {refused:?}"
    );

    // Once it is stored, the Version's embedding moves to the Sentence.
    w.store_sentence(version, sentence).await;
    w.caller()
        .store_embedding(w.tenant, version, &unstored, &by_sentence)
        .await
        .unwrap();
    let hits = w
        .caller()
        .search_vector(w.tenant, &by_sentence, now(), 10)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1, "one quantized row per Version: {hits:?}");
    assert_eq!(hits[0].of, Versioned::Edge(owns));
    assert!(hits[0].distance.abs() < 1e-5);

    // A hash of some other text is refused.
    let other = sentence_hash(&w, "d owns one percent of Acme").await;
    let refused = w
        .caller()
        .store_embedding(w.tenant, version, &other, &rng.unit())
        .await
        .unwrap_err();
    assert!(
        matches!(refused, gnomon::Error::InvalidWrite(_)),
        "{refused:?}"
    );
}

#[tokio::test]
async fn an_embedding_or_a_query_of_the_wrong_length_zero_length_or_not_finite_is_refused() {
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    let a = w
        .node("account", e, ValidTime::UNKNOWN, json!({ "plan": "basic" }))
        .await;
    let version = w.read(a, at(2024, 6, 1)).await.unwrap();
    let mut rng = Rng::new(7);
    let good = rng.unit();
    let mut short = good.clone();
    short.pop();
    let zero = vec![0.0f32; good.len()];
    let mut nan = good.clone();
    nan[3] = f32::NAN;
    let mut infinite = good.clone();
    infinite[5] = f32::INFINITY;
    for bad in [&short, &zero, &nan, &infinite, &vec![]] {
        let stored = w
            .caller()
            .store_embedding(w.tenant, version.id, &version.text_hash, bad)
            .await
            .unwrap_err();
        assert_eq!(
            stored.sqlstate().map(|s| s.code()),
            Some("22023"),
            "{stored:?}"
        );
        let searched = w
            .caller()
            .search_vector(w.tenant, bad, now(), 10)
            .await
            .unwrap_err();
        assert_eq!(
            searched.sqlstate().map(|s| s.code()),
            Some("22023"),
            "{searched:?}"
        );
    }
    for limit in [0, -1] {
        let refused = w
            .caller()
            .search_vector(w.tenant, &good, now(), limit)
            .await
            .unwrap_err();
        assert_eq!(refused.sqlstate().map(|s| s.code()), Some("22023"));
    }
    // A Version the caller cannot see.
    let refused = w
        .caller()
        .store_embedding(
            w.tenant,
            gnomon::VersionId(uuid::Uuid::new_v4()),
            &version.text_hash,
            &good,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(refused, gnomon::Error::InvalidWrite(_)),
        "{refused:?}"
    );
    // None of them stored anything.
    assert!(
        w.caller()
            .search_vector(w.tenant, &good, now(), 10)
            .await
            .unwrap()
            .is_empty()
    );
}

/// The As-of instants of prototype 5: "now", and its past read.
fn prototype_now() -> AsOf {
    AsOf::current(at(2026, 11, 21))
}
fn prototype_past() -> AsOf {
    AsOf::at(at(2025, 1, 1), at(2025, 6, 1))
}

/// Mean recall@10 of the Store's vector Search against exact full-precision
/// search, for `queries`, by a caller who sees `scopes`.
async fn recall_at_10(
    w: &World,
    corpus: &support::vectors::Corpus,
    scopes: &[&str],
    queries: &[Vec<f32>],
    as_of: AsOf,
) -> f64 {
    let caller = w.seeing(scopes);
    let mut total = 0.0;
    for q in queries {
        let truth = corpus.exact(q, scopes, as_of.valid_at, as_of.known_at, 10);
        assert_eq!(
            truth.len(),
            10,
            "the visible set holds at least 10 Versions"
        );
        let got = caller.search_vector(w.tenant, q, as_of, 10).await.unwrap();
        for c in &got {
            assert!(
                scopes.contains(&c.scope.as_str()),
                "a Version of Scope {} was found by a caller of {scopes:?}",
                c.scope
            );
        }
        let hit = got.iter().filter(|c| truth.contains(&c.version.0)).count();
        total += hit as f64 / 10.0;
    }
    total / queries.len() as f64
}

#[tokio::test]
async fn recall_at_10_matches_prototype_5_for_a_small_scope_inside_a_large_tenant_and_a_past_known_at()
 {
    // Prototype 5's layout, scaled down: a Scope of 1,000 texts inside a
    // Tenant whose other Scope holds 9,000, on clustered synthetic
    // 1,536-dimension vectors. The small Scope draws on four subtopics and
    // the large one on 22, the small Scope's four among them, so the large
    // Scope's rows crowd every query of the small one.
    let w = World::new().await;
    let versions =
        w.db.admin()
            .await
            .query_typed(
                "select current_setting('server_version'),
                    (select extversion from pg_extension where extname = 'vector')",
                &[],
            )
            .await
            .unwrap();
    println!(
        "PostgreSQL {}, pgvector {}",
        versions[0].get::<_, String>(0),
        versions[0].get::<_, String>(1)
    );
    let mut rng = Rng::new(18);
    let centres = support::vectors::subtopics(&mut rng, 8, 10);
    let small: Vec<Vec<f32>> = centres[..4].to_vec();
    let large: Vec<Vec<f32>> = centres[..22].to_vec();
    let mut corpus = support::vectors::Corpus::default();
    support::vectors::load(
        &w.db,
        &mut corpus,
        w.tenant,
        "small",
        1_000,
        &small,
        &mut rng,
    )
    .await;
    support::vectors::load(
        &w.db,
        &mut corpus,
        w.tenant,
        "large",
        9_000,
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

    // Prototype 5 measured recall@10 of 1.00 for a 1,000-row Scope inside a
    // 200,000-row Tenant, at "now" and at its past known-at, with per-Version
    // quantized rows (FINDINGS.md, measurement 4), and 1.00 for 10,000
    // visible rows. Its vectors and ours are drawn the same way. Ours are
    // fewer: the policy is an index condition, so the rest of the Tenant
    // changes which rows are scanned only through the large Scope's rows near
    // the same subtopics, which this Tenant has. Our re-rank keeps 400 where
    // that measurement kept 200. The tolerance is
    // 0.05: synthetic clusters hold many near-equidistant neighbours, and
    // pgvector sums in single precision where the oracle sums in double.
    const PROTOTYPE_5: f64 = 1.00;
    const TOLERANCE: f64 = 0.05;
    let mut measured = Vec::new();
    for (label, scopes, as_of) in [
        ("small Scope, now", &["small"][..], prototype_now()),
        (
            "small Scope, past known-at",
            &["small"][..],
            prototype_past(),
        ),
        ("both Scopes, now", &["small", "large"][..], prototype_now()),
        (
            "both Scopes, past known-at",
            &["small", "large"][..],
            prototype_past(),
        ),
    ] {
        let visible = corpus.visible(scopes, as_of.valid_at, as_of.known_at);
        let recall = recall_at_10(&w, &corpus, scopes, &queries, as_of).await;
        println!("recall@10 {label}: {recall:.3} over {visible} visible Versions");
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
/// full-vector table, in this backend's pending statistics. The view also
/// counts the backend's earlier transactions until their statistics are
/// flushed, so a test reads it before and after and takes the difference.
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
async fn the_re_rank_reads_each_candidates_full_vector_by_one_primary_key_probe() {
    // Under RLS the planner estimates one row for every scan, so an unpinned
    // re-rank can turn into a join that scans every visible full vector
    // (prototype 5: 660 ms against 96 ms). The plan must probe the full
    // vector of each of the 400 candidates by its key, and read no other,
    // even when the planner is pushed off nested loops, as a misestimate
    // pushes it. No timing is asserted.
    let w = World::new().await;
    let mut rng = Rng::new(19);
    let centres = support::vectors::subtopics(&mut rng, 1, 4);
    let mut corpus = support::vectors::Corpus::default();
    support::vectors::load(
        &w.db,
        &mut corpus,
        w.tenant,
        SCOPE,
        1_000,
        &centres,
        &mut rng,
    )
    .await;
    let as_of = prototype_now();
    let visible = corpus.visible(&[SCOPE], as_of.valid_at, as_of.known_at);
    assert!(
        visible > 400,
        "more Versions are visible than the re-rank keeps: {visible}"
    );
    let query = rng.near(&centres[0], 0.6);

    let mut client = w.db.login().await;
    for setting in ["", "set local enable_nestloop = off"] {
        let tx = w.raw(&mut client).await;
        if !setting.is_empty() {
            tx.batch_execute(setting).await.unwrap();
        }
        // The view also counts this backend's earlier transactions until
        // their statistics are flushed, so the search's reads are the
        // difference.
        let before = full_vector_reads(&tx).await;
        let found = tx
            .query_typed(
                "select count(*) from gnomon.search_vector($1, $2, $3, $4, 10)",
                &[
                    (&w.tenant.0, tokio_postgres::types::Type::UUID),
                    (&query, tokio_postgres::types::Type::FLOAT4_ARRAY),
                    (&as_of.valid_at, tokio_postgres::types::Type::TIMESTAMPTZ),
                    (&as_of.known_at, tokio_postgres::types::Type::TIMESTAMPTZ),
                ],
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
             are the plan; {} full vectors are visible",
            corpus.texts.len()
        );
        tx.rollback().await.unwrap();
    }
}

/// How many full vectors of the text whose hash is `hash` are stored, past
/// RLS.
async fn full_vectors_of(w: &World, hash: &str) -> i64 {
    w.db.admin()
        .await
        .query_typed(
            "select count(*) from gnomon.embedding where text_hash = $1",
            &[(&hash, tokio_postgres::types::Type::TEXT)],
        )
        .await
        .unwrap()[0]
        .get(0)
}

#[tokio::test]
async fn erasure_deletes_the_embeddings_of_erased_and_recomputed_text_and_keeps_a_shared_full_vector()
 {
    let w = World::new().await;
    let f = w.episode(at(2024, 1, 1), None).await;
    let e = w.episode(at(2024, 2, 1), None).await;
    // x, which only E describes, has the same text as y, which only F
    // describes, so they share a full vector. z, which only E describes,
    // has a text of its own. E also gives v, which F describes, another plan
    // for a while, so the Erasure recomputes v.
    let x = w
        .node(
            "account",
            e,
            ValidTime::UNKNOWN,
            json!({ "plan": "shared" }),
        )
        .await;
    let y = w
        .node(
            "account",
            f,
            ValidTime::UNKNOWN,
            json!({ "plan": "shared" }),
        )
        .await;
    let z = w
        .node(
            "account",
            e,
            ValidTime::UNKNOWN,
            json!({ "plan": "erased-only" }),
        )
        .await;
    let v = w
        .node("account", f, ValidTime::UNKNOWN, json!({ "plan": "basic" }))
        .await;
    w.write(
        v,
        e,
        ValidTime::between(at(2024, 3, 1), at(2024, 6, 1)),
        json!({ "plan": "erased-plan" }),
    )
    .await;
    let mut rng = Rng::new(8);
    let vectors = embed_every_version(&w, SCOPE, &mut rng).await;
    let hash_of = |node| {
        let w = &w;
        async move { w.read(node, at(2024, 4, 1)).await.unwrap().text_hash }
    };
    let (shared, erased_only, erased_plan) = (hash_of(x).await, hash_of(z).await, hash_of(v).await);
    let basic = w.read(v, at(2024, 1, 1)).await.unwrap().text_hash;
    assert_eq!(hash_of(y).await, shared);
    assert_eq!(
        full_vectors_of(&w, &shared).await,
        1,
        "x and y share one full vector"
    );

    w.erase(e).await;

    // y's quantized row and the full vector it shares with x stay; x's row
    // went with x.
    let found = w
        .caller()
        .search_vector(
            w.tenant,
            &vectors[&shared],
            AsOf::current(at(2024, 4, 1)),
            10,
        )
        .await
        .unwrap();
    assert_eq!(found[0].of, Versioned::Node(y));
    assert!(found[0].distance.abs() < 1e-5);
    assert!(found.iter().all(|c| c.of != Versioned::Node(x)));
    assert_eq!(full_vectors_of(&w, &shared).await, 1);
    // The full vectors only E's text and v's recomputed Versions used are gone.
    for hash in [&erased_only, &erased_plan, &basic] {
        assert_eq!(
            full_vectors_of(&w, hash).await,
            0,
            "the full vector of {hash} is left"
        );
    }
    // v's surviving Versions have no embedding until the Pipeline embeds
    // their text again; the full-text branch finds them meanwhile.
    let survivors = w.versions_of(&[v.0]).await;
    assert!(!survivors.is_empty());
    let admin = w.db.admin().await;
    let left: i64 = admin
        .query_typed(
            "select count(*) from gnomon.embedding_bit where version_id = any($1)",
            &[(&survivors, tokio_postgres::types::Type::UUID_ARRAY)],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(left, 0, "a recomputed Version keeps its embedding");
    support::erasure::assert_no_residue(
        &admin,
        &support::erasure::Residue {
            episodes: &[e.0],
            recomputed: &survivors,
            ..Default::default()
        },
    )
    .await;
}

#[tokio::test]
async fn erasing_a_node_deletes_the_embeddings_of_its_edge_in_a_scope_the_caller_cannot_see() {
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    let billing = w
        .episode_in("billing", at(2024, 1, 2), "billing's Episode")
        .await;
    // x, of the support Scope, is joined by an Edge of billing, which the
    // support worker cannot see. Another billing Edge, which the Erasure
    // leaves, has the same text as the first, and a third its own.
    let x = w
        .node("account", e, ValidTime::UNKNOWN, json!({ "plan": "basic" }))
        .await;
    let other = w
        .node_in("billing", "account", billing, json!({ "plan": "pro" }))
        .await;
    let acme = w
        .node_in("billing", "company", billing, json!({ "name": "Acme" }))
        .await;
    let erased = w
        .edge_in("billing", x, acme, billing, json!({ "share": 40 }))
        .await;
    let kept = w
        .edge_in("billing", other, acme, billing, json!({ "share": 40 }))
        .await;
    let alone = w
        .edge_in("billing", x, acme, billing, json!({ "share": 7 }))
        .await;
    let mut rng = Rng::new(9);
    embed_every_version(&w, SCOPE, &mut rng).await;
    let vectors = embed_every_version(&w, "billing", &mut rng).await;
    let billing_caller = w.worker_in("billing");
    let everyone = w.seeing(support::erasure::ALL_SCOPES);
    let edge_hash = |edge| {
        let c = &everyone;
        let w = &w;
        async move {
            c.read_edge(w.tenant, edge, AsOf::current(at(2024, 6, 1)))
                .await
                .unwrap()
                .unwrap()
                .text_hash
        }
    };
    let (shared, own) = (edge_hash(erased).await, edge_hash(alone).await);
    assert_eq!(edge_hash(kept).await, shared);
    let mut erased_ids: Vec<uuid::Uuid> = vec![x.0, erased.0, alone.0];
    erased_ids.extend(w.versions_of(&erased_ids).await);
    let before = support::erasure::every_row(&w.db.admin().await).await;

    let erasure = w.erase_node(x).await;
    assert!(erasure.erased_nodes.contains(&x));

    let found = billing_caller
        .search_vector(
            w.tenant,
            &vectors[&shared],
            AsOf::current(at(2024, 6, 1)),
            10,
        )
        .await
        .unwrap();
    let edges: Vec<Versioned> = found.iter().map(|c| c.of).collect();
    assert!(edges.contains(&Versioned::Edge(kept)), "{edges:?}");
    assert!(!edges.contains(&Versioned::Edge(erased)), "{edges:?}");
    assert!(!edges.contains(&Versioned::Edge(alone)), "{edges:?}");
    assert_eq!(
        full_vectors_of(&w, &shared).await,
        1,
        "the full vector the kept Edge uses stays"
    );
    assert_eq!(
        full_vectors_of(&w, &own).await,
        0,
        "the full vector only the erased Edge used is gone"
    );
    support::erasure::assert_only_removed(&w.db.admin().await, &before, &erased_ids).await;
}
