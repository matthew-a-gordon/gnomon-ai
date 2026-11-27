//! Seam 1 for storing a Sentence (ADR-0013, `0004_search.sql`): the Pipeline
//! stores the Sentence of one Edge Version after the Version commits, through
//! the Rust core's Store API against real Postgres. Its token rows and BM25
//! counts are written in the same transaction, so full-text Search finds the
//! Version at commit. A replaced Sentence's old embedding goes with it. The
//! adversarial cases live in sql_boundary.rs, and the gap listing in gaps.rs.

mod support;

use gnomon::{AsOf, Error, StageWrite, ValidTime, VersionId, Versioned};
use serde_json::json;
use support::erasure::{
    World, assert_no_unused_full_vector, assert_search_counts_match_the_token_rows, at,
};
use support::vectors::{Rng, sentence_hash};
use tokio_postgres::Client;
use tokio_postgres::types::Type;
use uuid::Uuid;

fn now() -> AsOf {
    AsOf::current(at(2024, 6, 1))
}

/// An account, a company and an Edge between them, with no Sentence yet.
async fn owned(w: &World) -> (gnomon::EdgeId, VersionId) {
    let e = w.episode(at(2024, 1, 1), None).await;
    let ada = w
        .node("account", e, ValidTime::UNKNOWN, json!({ "name": "Ada" }))
        .await;
    let acme = w
        .node("company", e, ValidTime::UNKNOWN, json!({ "name": "Acme" }))
        .await;
    let owns = w
        .edge(ada, acme, e, ValidTime::UNKNOWN, json!({ "share": 7 }))
        .await;
    let version = w.current_edge_version(owns, at(2024, 6, 1)).await;
    (owns, version)
}

/// Every Version row, with its Recorded time and values, read past RLS.
async fn versions(admin: &Client) -> Vec<(Uuid, String, String, String)> {
    admin
        .query_typed(
            "select id, recorded_from::text, recorded_to::text, props::text
             from gnomon.version order by id",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3)))
        .collect()
}

async fn found(w: &World, query: &str) -> Vec<Versioned> {
    w.caller()
        .search_full_text(w.tenant, query, now(), 10)
        .await
        .unwrap()
        .into_iter()
        .map(|c| c.of)
        .collect()
}

#[tokio::test]
async fn storing_a_sentence_creates_no_version_changes_no_recorded_time_and_is_found_at_once() {
    let w = World::new().await;
    let (owns, version) = owned(&w).await;
    let admin = w.db.admin().await;
    let before = versions(&admin).await;
    assert_eq!(found(&w, "holds").await, vec![]);

    w.caller()
        .store_sentence(w.tenant, version, "Ada holds seven percent of Acme")
        .await
        .unwrap();

    assert_eq!(
        versions(&admin).await,
        before,
        "no Version is added, removed or changed"
    );
    let read = w
        .caller()
        .read_edge(w.tenant, owns, now())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(read.id, version);
    assert_eq!(
        read.sentence.as_deref(),
        Some("Ada holds seven percent of Acme")
    );
    // The next call finds it by a word only the Sentence holds, before any
    // embedding exists.
    assert_eq!(found(&w, "holds").await, vec![Versioned::Edge(owns)]);
    let embeddings: i64 = admin
        .query_typed("select count(*) from gnomon.embedding_bit", &[])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(embeddings, 0);
    assert_search_counts_match_the_token_rows(&admin).await;
}

#[tokio::test]
async fn a_sentences_words_and_counts_are_written_in_its_own_transaction() {
    let w = World::new().await;
    let (owns, version) = owned(&w).await;
    let search = "select edge_id from gnomon.search_full_text($1, 'holds', $2, null, 10)";
    let store = "select gnomon.store_sentence($1, $2, 'Ada holds seven percent of Acme')";

    // Inside the transaction that stores it, the Sentence's words are found
    // already, with the transaction's own changes to the counts.
    let mut client = w.db.login().await;
    let tx = w.raw(&mut client).await;
    tx.execute_typed(
        store,
        &[(&w.tenant.0, Type::UUID), (&version.0, Type::UUID)],
    )
    .await
    .unwrap();
    let rows = tx
        .query_typed(
            search,
            &[
                (&w.tenant.0, Type::UUID),
                (&at(2024, 6, 1), Type::TIMESTAMPTZ),
            ],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, Uuid>(0), owns.0);
    tx.rollback().await.unwrap();

    // A rolled-back store leaves no Sentence, no word and no count.
    let admin = w.db.admin().await;
    assert_eq!(w.sentence_of(version).await, None);
    assert_eq!(found(&w, "holds").await, vec![]);
    assert_search_counts_match_the_token_rows(&admin).await;

    // Committed, the counts are applied at commit and match the token rows.
    let tx = w.raw(&mut client).await;
    tx.execute_typed(
        store,
        &[(&w.tenant.0, Type::UUID), (&version.0, Type::UUID)],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(found(&w, "holds").await, vec![Versioned::Edge(owns)]);
    assert_search_counts_match_the_token_rows(&admin).await;
}

#[tokio::test]
async fn storing_again_replaces_the_sentence_and_its_words() {
    let w = World::new().await;
    let (owns, version) = owned(&w).await;
    let admin = w.db.admin().await;
    let before = versions(&admin).await;
    w.caller()
        .store_sentence(w.tenant, version, "Ada holds seven percent of Acme")
        .await
        .unwrap();
    w.caller()
        .store_sentence(w.tenant, version, "Ada owns a stake in Acme")
        .await
        .unwrap();

    assert_eq!(
        w.sentence_of(version).await.as_deref(),
        Some("Ada owns a stake in Acme")
    );
    assert_eq!(found(&w, "holds").await, vec![]);
    assert_eq!(found(&w, "stake").await, vec![Versioned::Edge(owns)]);
    assert_search_counts_match_the_token_rows(&admin).await;

    // The same text again changes nothing.
    w.caller()
        .store_sentence(w.tenant, version, "Ada owns a stake in Acme")
        .await
        .unwrap();
    assert_eq!(found(&w, "stake").await, vec![Versioned::Edge(owns)]);
    assert_eq!(versions(&admin).await, before);
    assert_search_counts_match_the_token_rows(&admin).await;
}

/// The quantized rows of a Version and the full vectors of a Scope, read
/// past RLS: `(version, hash)` and the hashes.
async fn embeddings(admin: &Client, scope: &str) -> (Vec<(Uuid, String)>, Vec<String>) {
    let bits = admin
        .query_typed(
            "select version_id, text_hash from gnomon.embedding_bit where scope = $1 order by 1, 2",
            &[(&scope, Type::TEXT)],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    let full = admin
        .query_typed(
            "select text_hash from gnomon.embedding where scope = $1 order by 1",
            &[(&scope, Type::TEXT)],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    (bits, full)
}

#[tokio::test]
async fn replacing_a_sentence_removes_its_old_embedding_from_vector_search_and_the_tables() {
    let w = World::new().await;
    let (owns, version) = owned(&w).await;
    let admin = w.db.admin().await;
    let mut rng = Rng::new(26);
    let (old_vector, new_vector) = (rng.unit(), rng.unit());
    let old = "Ada holds seven percent of Acme";
    let old_hash = sentence_hash(&w, old).await;
    w.caller()
        .store_sentence(w.tenant, version, old)
        .await
        .unwrap();
    w.caller()
        .store_embedding(w.tenant, version, &old_hash, &old_vector)
        .await
        .unwrap();
    let hits = w
        .caller()
        .search_vector(w.tenant, &old_vector, now(), 10)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].of, Versioned::Edge(owns));

    let new = "Ada owns a stake in Acme";
    w.caller()
        .store_sentence(w.tenant, version, new)
        .await
        .unwrap();

    // The old text's quantized row and full vector are gone, so vector
    // Search no longer finds the Version by them, and nothing of the old
    // text is left for an Erasure to miss.
    assert_eq!(
        w.caller()
            .search_vector(w.tenant, &old_vector, now(), 10)
            .await
            .unwrap(),
        vec![]
    );
    assert_eq!(embeddings(&admin, "support").await, (vec![], vec![]));
    assert_no_unused_full_vector(&admin).await;

    // The new Sentence is embedded as any other.
    let new_hash = sentence_hash(&w, new).await;
    w.caller()
        .store_embedding(w.tenant, version, &new_hash, &new_vector)
        .await
        .unwrap();
    let hits = w
        .caller()
        .search_vector(w.tenant, &new_vector, now(), 10)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].version, version);
    assert_eq!(
        embeddings(&admin, "support").await,
        (vec![(version.0, new_hash.clone())], vec![new_hash])
    );
}

#[tokio::test]
async fn replacing_a_sentence_keeps_a_full_vector_another_version_or_a_name_still_uses() {
    let w = World::new().await;
    let (_, version) = owned(&w).await;
    let admin = w.db.admin().await;
    let mut rng = Rng::new(27);
    let shared = "Acme Holdings";
    let shared_hash = sentence_hash(&w, shared).await;
    let vector = rng.unit();

    // Another Edge's Version and a Node's name have the same text, and use
    // the same full vector.
    let e = w.episode(at(2024, 1, 2), None).await;
    let bob = w
        .node("account", e, ValidTime::UNKNOWN, json!({ "name": "Bob" }))
        .await;
    let holdings = w
        .node("company", e, ValidTime::UNKNOWN, json!({ "name": shared }))
        .await;
    let other_edge = w
        .edge(bob, holdings, e, ValidTime::UNKNOWN, json!({ "share": 1 }))
        .await;
    let other = w.current_edge_version(other_edge, at(2024, 6, 1)).await;
    for v in [version, other] {
        w.caller()
            .store_sentence(w.tenant, v, shared)
            .await
            .unwrap();
        w.caller()
            .store_embedding(w.tenant, v, &shared_hash, &vector)
            .await
            .unwrap();
    }
    w.caller()
        .store_name_embedding(w.tenant, holdings, shared, &vector)
        .await
        .unwrap();

    w.caller()
        .store_sentence(w.tenant, version, "Ada owns a stake in Acme")
        .await
        .unwrap();

    // Only the replaced Version's quantized row goes. The full vector stays
    // while the other Version's row uses it.
    let (bits, full) = embeddings(&admin, "support").await;
    assert_eq!(bits, vec![(other.0, shared_hash.clone())]);
    assert_eq!(full, vec![shared_hash.clone()]);

    // With that Version's Sentence replaced too, the name still uses it.
    w.caller()
        .store_sentence(w.tenant, other, "Bob owns one percent of Acme Holdings")
        .await
        .unwrap();
    let (bits, full) = embeddings(&admin, "support").await;
    assert_eq!(bits, vec![]);
    assert_eq!(full, vec![shared_hash]);
    let resolved = w
        .caller()
        .resolve(w.tenant, &gnomon::Mention::embedding(&vector), 5)
        .await;
    assert!(
        resolved.is_ok(),
        "the name is still matched by its vector: {resolved:?}"
    );
    assert_eq!(resolved.unwrap()[0].node, holdings);
}

#[tokio::test]
async fn a_node_version_a_version_the_caller_cannot_see_and_an_empty_sentence_are_refused() {
    let w = World::new().await;
    let (_, version) = owned(&w).await;
    let e = w.episode(at(2024, 1, 1), None).await;
    let node = w
        .node(
            "company",
            e,
            ValidTime::UNKNOWN,
            json!({ "name": "Initech" }),
        )
        .await;
    let node_version = w.read(node, at(2024, 6, 1)).await.unwrap().id;
    let billing = w
        .episode_in("billing", at(2024, 1, 1), "a billing Episode")
        .await;
    let a = w
        .node_in("billing", "account", billing, json!({ "name": "Cy" }))
        .await;
    let b = w
        .node_in("billing", "company", billing, json!({ "name": "Globex" }))
        .await;
    let billing_edge = w
        .edge_in("billing", a, b, billing, json!({ "share": 3 }))
        .await;
    let billing_version = w
        .worker_in("billing")
        .read_edge(w.tenant, billing_edge, now())
        .await
        .unwrap()
        .unwrap()
        .id;

    for (v, what) in [
        (node_version, "a Node Version"),
        (billing_version, "another Scope's Edge Version"),
        (VersionId(Uuid::new_v4()), "no Version at all"),
    ] {
        let refused = w
            .caller()
            .store_sentence(w.tenant, v, "A Sentence.")
            .await
            .unwrap_err();
        assert!(
            matches!(refused, Error::InvalidWrite(_)),
            "{what}: {refused:?}"
        );
    }
    for empty in ["", "   "] {
        let refused = w
            .caller()
            .store_sentence(w.tenant, version, empty)
            .await
            .unwrap_err();
        assert_eq!(
            refused.sqlstate(),
            Some(&tokio_postgres::error::SqlState::INVALID_PARAMETER_VALUE),
            "{empty:?}: {refused:?}"
        );
    }
    let refused = w
        .caller()
        .store_sentence(support::tenant(), version, "A Sentence.")
        .await
        .unwrap_err();
    assert!(
        matches!(refused, Error::TenantMismatch(_)),
        "another Tenant: {refused:?}"
    );
    assert_eq!(w.sentence_of(version).await, None);
    assert_eq!(w.sentence_of(billing_version).await, None);
}

#[tokio::test]
async fn a_sentence_of_a_closed_version_is_stored_and_found_as_of_then() {
    let w = World::new().await;
    let (owns, first) = owned(&w).await;
    let written = w
        .caller()
        .read_edge(w.tenant, owns, now())
        .await
        .unwrap()
        .unwrap()
        .recorded_from;
    let later = w.episode(at(2024, 2, 1), None).await;
    w.write_edge(owns, later, ValidTime::UNKNOWN, json!({ "share": 9 }))
        .await;
    assert_ne!(w.current_edge_version(owns, at(2024, 6, 1)).await, first);

    // The Pipeline may store a Sentence after a later write closed its
    // Version. It is found at a known-at instant the Version held.
    w.caller()
        .store_sentence(w.tenant, first, "Ada holds seven percent of Acme")
        .await
        .unwrap();
    let past = w
        .caller()
        .search_full_text(w.tenant, "holds", AsOf::at(at(2024, 6, 1), written), 10)
        .await
        .unwrap();
    assert_eq!(past.len(), 1);
    assert_eq!(past[0].version, first);
    assert_eq!(found(&w, "holds").await, vec![]);
}

#[tokio::test]
async fn a_sentence_commits_with_the_stage_records_of_its_transaction() {
    let w = World::new().await;
    let (_, version) = owned(&w).await;
    let e = w.episode(at(2024, 3, 1), None).await;
    let caller = w.caller();
    let mut tx = caller.transaction(w.tenant).await.unwrap();
    tx.store_sentence(version, "Ada holds seven percent of Acme")
        .await
        .unwrap();
    tx.write_stages(&[StageWrite::pending(e, "embeddings")])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let records = w.caller().stage_records(w.tenant, e).await.unwrap();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].stage, "embeddings");

    // A refused store fails its transaction, which then commits nothing.
    let f = w.episode(at(2024, 3, 2), None).await;
    let mut tx = caller.transaction(w.tenant).await.unwrap();
    let refused = tx
        .store_sentence(VersionId(Uuid::new_v4()), "A Sentence.")
        .await;
    assert!(refused.is_err());
    let _ = tx
        .write_stages(&[StageWrite::pending(f, "embeddings")])
        .await;
    assert!(matches!(tx.commit().await, Err(gnomon::Error::RolledBack)));
    assert_eq!(w.caller().stage_records(w.tenant, f).await.unwrap(), vec![]);
}
