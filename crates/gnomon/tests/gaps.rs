//! Seam 1 for the gap listing (migration 0021): for one Scope, what
//! `maintain` repairs. That is the Edge Versions without a Sentence, the
//! search texts without an embedding, and the Nodes and Communities without
//! a current Summary, through the Rust core's Store API against real
//! Postgres. The listing is paged by key, in one order on every call. The
//! adversarial cases live in sql_boundary.rs.

mod support;

use std::collections::HashSet;

use gnomon::{
    AsOf, Bound, BoundSide, BoundStatement, Caller, CommunityId, DerivedCitation, EpisodeId, Gap,
    GapPage, NewCommunity, NewSummary, NodeId, Precision, Scope, ValidTime, VersionId,
};
use serde_json::json;
use support::erasure::{at, World, SCOPE};
use support::vectors::{sentence_hash, Rng};
use tokio_postgres::types::Type;
use uuid::Uuid;

/// Every gap of `scope` the caller sees, one page after another.
async fn all_gaps(caller: &Caller<'_>, w: &World, scope: &str) -> Vec<Gap> {
    let mut out = Vec::new();
    let mut after = None;
    loop {
        let page: GapPage = caller
            .list_gaps(w.tenant, &Scope::new(scope), after.as_ref(), 1000)
            .await
            .unwrap();
        out.extend(page.gaps);
        match page.next {
            Some(next) => after = Some(next),
            None => return out,
        }
    }
}

async fn gaps(w: &World) -> Vec<Gap> {
    let gaps = all_gaps(&w.caller(), w, SCOPE).await;
    assert_ordered(&gaps);
    gaps
}

/// The order the listing promises: by kind, in the order of [`kind_rank`],
/// then by ID and hash within a kind.
fn assert_ordered(gaps: &[Gap]) {
    let keys: Vec<(usize, Uuid, String)> = gaps.iter().map(key).collect();
    for pair in keys.windows(2) {
        assert!(pair[0] < pair[1], "out of order: {pair:?} in {gaps:?}");
    }
}

fn kind_rank(g: &Gap) -> usize {
    match g {
        Gap::Sentence { .. } => 0,
        Gap::SentenceEmbedding { .. } => 1,
        Gap::NodeEmbedding { .. } => 2,
        Gap::NameEmbedding { .. } => 3,
        Gap::SummaryEmbedding { .. } => 4,
        Gap::CommunityEmbedding { .. } => 5,
        Gap::Summary { .. } => 6,
        Gap::Community { .. } => 7,
    }
}

fn key(g: &Gap) -> (usize, Uuid, String) {
    let (id, hash) = match g {
        Gap::Sentence { version, .. } => (version.0, String::new()),
        Gap::SentenceEmbedding {
            version, text_hash, ..
        }
        | Gap::NodeEmbedding {
            version, text_hash, ..
        } => (version.0, text_hash.clone()),
        Gap::NameEmbedding {
            node, text_hash, ..
        } => (node.0, text_hash.clone()),
        Gap::SummaryEmbedding { summary, .. } => (summary.0, String::new()),
        Gap::CommunityEmbedding { community } | Gap::Community { community } => {
            (community.0, String::new())
        }
        Gap::Summary { node } => (node.0, String::new()),
    };
    (kind_rank(g), id, hash)
}

fn set(gaps: Vec<Gap>) -> HashSet<Gap> {
    gaps.into_iter().collect()
}

async fn node_version(w: &World, node: NodeId) -> (VersionId, String) {
    let v = w.read(node, at(2024, 6, 1)).await.unwrap();
    (v.id, v.text_hash)
}

async fn name_hash(w: &World, name: &str) -> String {
    sentence_hash(w, name).await
}

async fn summary(w: &World, node: NodeId, episode: EpisodeId) -> gnomon::SummaryId {
    w.caller()
        .write_summary(
            w.tenant,
            &NewSummary {
                scope: Scope::new(SCOPE),
                node,
                content: "A summary.".into(),
                as_of_valid: at(2024, 6, 1),
                citations: vec![DerivedCitation::of(episode)],
            },
        )
        .await
        .unwrap()
}

async fn community(
    w: &World,
    replaces: Vec<CommunityId>,
    members: Vec<NodeId>,
    episode: EpisodeId,
) -> CommunityId {
    w.caller()
        .write_community(
            w.tenant,
            &NewCommunity {
                scope: Scope::new(SCOPE),
                replaces,
                members,
                content: "A group.".into(),
                as_of_valid: at(2024, 6, 1),
                citations: vec![DerivedCitation::of(episode)],
            },
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn each_kind_of_gap_is_listed_until_it_is_repaired() {
    let w = World::new().await;
    let mut rng = Rng::new(126);
    let f = w.episode(at(2024, 1, 1), None).await;
    let ada = w
        .node("account", f, ValidTime::UNKNOWN, json!({ "name": "Ada" }))
        .await;
    let acme = w
        .node(
            "company",
            f,
            ValidTime::UNKNOWN,
            json!({ "name": "Acme", "aliases": ["ACME Corp"] }),
        )
        .await;
    let owns = w
        .edge(ada, acme, f, ValidTime::UNKNOWN, json!({ "share": 7 }))
        .await;
    let edge_version = w.current_edge_version(owns, at(2024, 6, 1)).await;
    let (ada_version, ada_hash) = node_version(&w, ada).await;
    let (acme_version, acme_hash) = node_version(&w, acme).await;
    let name = |node: NodeId, text: &str, hash: String| Gap::NameEmbedding {
        node,
        text_hash: hash,
        name: text.into(),
    };

    assert_eq!(
        set(gaps(&w).await),
        set(vec![
            Gap::Sentence {
                version: edge_version,
                edge: owns
            },
            Gap::NodeEmbedding {
                version: ada_version,
                node: ada,
                text_hash: ada_hash.clone()
            },
            Gap::NodeEmbedding {
                version: acme_version,
                node: acme,
                text_hash: acme_hash.clone()
            },
            name(ada, "Ada", name_hash(&w, "Ada").await),
            name(acme, "Acme", name_hash(&w, "Acme").await),
            name(acme, "ACME Corp", name_hash(&w, "ACME Corp").await),
            Gap::Summary { node: ada },
            Gap::Summary { node: acme },
        ])
    );

    // The Sentence is stored, and its embedding is missing next.
    let sentence = "Ada holds seven percent of Acme";
    w.caller()
        .store_sentence(w.tenant, edge_version, sentence)
        .await
        .unwrap();
    let hash = sentence_hash(&w, sentence).await;
    let listed = gaps(&w).await;
    assert!(!listed.iter().any(|g| matches!(g, Gap::Sentence { .. })));
    assert!(listed.contains(&Gap::SentenceEmbedding {
        version: edge_version,
        edge: owns,
        text_hash: hash.clone(),
        sentence: sentence.into(),
    }));
    // An embedding of the Edge Version's values does not fill it: once the
    // Sentence exists, the Version is embedded by the Sentence.
    let values_hash = w
        .caller()
        .read_edge(w.tenant, owns, AsOf::current(at(2024, 6, 1)))
        .await
        .unwrap()
        .unwrap()
        .text_hash;
    w.caller()
        .store_embedding(w.tenant, edge_version, &values_hash, &rng.unit())
        .await
        .unwrap();
    assert!(gaps(&w)
        .await
        .iter()
        .any(|g| matches!(g, Gap::SentenceEmbedding { .. })));
    w.caller()
        .store_embedding(w.tenant, edge_version, &hash, &rng.unit())
        .await
        .unwrap();
    assert!(!gaps(&w)
        .await
        .iter()
        .any(|g| matches!(g, Gap::SentenceEmbedding { .. })));

    // The Node Versions' texts and the names, one at a time.
    w.caller()
        .store_embedding(w.tenant, ada_version, &ada_hash, &rng.unit())
        .await
        .unwrap();
    w.caller()
        .store_embedding(w.tenant, acme_version, &acme_hash, &rng.unit())
        .await
        .unwrap();
    for (node, text) in [(ada, "Ada"), (acme, "Acme"), (acme, "ACME Corp")] {
        w.caller()
            .store_name_embedding(w.tenant, node, text, &rng.unit())
            .await
            .unwrap();
    }
    assert_eq!(
        set(gaps(&w).await),
        set(vec![
            Gap::Summary { node: ada },
            Gap::Summary { node: acme }
        ])
    );

    // A rename: the new Version and the new name lack embeddings, and the
    // name the Version replaced is no longer current, so it is not a gap.
    let g = w.episode(at(2024, 2, 1), None).await;
    w.write(
        ada,
        g,
        ValidTime::UNKNOWN,
        json!({ "name": "Ada Lovelace" }),
    )
    .await;
    let (renamed, renamed_hash) = node_version(&w, ada).await;
    assert_eq!(
        set(gaps(&w).await),
        set(vec![
            Gap::NodeEmbedding {
                version: renamed,
                node: ada,
                text_hash: renamed_hash.clone()
            },
            name(ada, "Ada Lovelace", name_hash(&w, "Ada Lovelace").await),
            Gap::Summary { node: ada },
            Gap::Summary { node: acme },
        ])
    );
    w.caller()
        .store_embedding(w.tenant, renamed, &renamed_hash, &rng.unit())
        .await
        .unwrap();
    w.caller()
        .store_name_embedding(w.tenant, ada, "Ada Lovelace", &rng.unit())
        .await
        .unwrap();

    // A Summary, then its embedding.
    let s = summary(&w, ada, f).await;
    assert_eq!(
        set(gaps(&w).await),
        set(vec![
            Gap::SummaryEmbedding {
                summary: s,
                node: ada
            },
            Gap::Summary { node: acme },
        ])
    );
    w.caller()
        .store_summary_embedding(w.tenant, s, &rng.unit())
        .await
        .unwrap();
    let t = summary(&w, acme, f).await;
    w.caller()
        .store_summary_embedding(w.tenant, t, &rng.unit())
        .await
        .unwrap();

    // A Community, then its embedding.
    let c = community(&w, vec![], vec![ada, acme], f).await;
    assert_eq!(
        gaps(&w).await,
        vec![Gap::CommunityEmbedding { community: c }]
    );
    w.caller()
        .store_community_embedding(w.tenant, c, &rng.unit())
        .await
        .unwrap();
    assert_eq!(gaps(&w).await, vec![]);

    // A regenerated Summary closes the old one, which keeps its embedding;
    // only the new one is missing.
    let s2 = summary(&w, ada, g).await;
    assert_eq!(
        gaps(&w).await,
        vec![Gap::SummaryEmbedding {
            summary: s2,
            node: ada
        }]
    );
}

#[tokio::test]
async fn two_nodes_with_one_name_are_each_listed_and_one_embedding_fills_both() {
    let w = World::new().await;
    let f = w.episode(at(2024, 1, 1), None).await;
    let first = w
        .node("company", f, ValidTime::UNKNOWN, json!({ "name": "Acme" }))
        .await;
    let second = w
        .node("company", f, ValidTime::UNKNOWN, json!({ "name": "Acme" }))
        .await;
    let names = |gaps: Vec<Gap>| -> Vec<Gap> {
        gaps.into_iter()
            .filter(|g| matches!(g, Gap::NameEmbedding { .. }))
            .collect()
    };
    let hash = name_hash(&w, "Acme").await;
    let mut expected = vec![
        Gap::NameEmbedding {
            node: first,
            text_hash: hash.clone(),
            name: "Acme".into(),
        },
        Gap::NameEmbedding {
            node: second,
            text_hash: hash,
            name: "Acme".into(),
        },
    ];
    expected.sort_by_key(key);
    assert_eq!(names(gaps(&w).await), expected);
    w.caller()
        .store_name_embedding(w.tenant, first, "Acme", &Rng::new(3).unit())
        .await
        .unwrap();
    assert_eq!(names(gaps(&w).await), vec![]);
}

#[tokio::test]
async fn a_node_with_no_current_version_needs_no_summary() {
    let w = World::new().await;
    let f = w.episode(at(2024, 1, 1), None).await;
    let ada = w
        .node("account", f, ValidTime::UNKNOWN, json!({ "name": "Ada" }))
        .await;
    // A bare Node, which no Episode describes, has nothing to summarise.
    w.bare_node_in(SCOPE, "company").await;
    let summaries = |gaps: Vec<Gap>| -> Vec<Gap> {
        gaps.into_iter()
            .filter(|g| matches!(g, Gap::Summary { .. }))
            .collect()
    };
    assert_eq!(summaries(gaps(&w).await), vec![Gap::Summary { node: ada }]);
    // Retracted, the Node holds no current Version, and nothing is
    // summarised.
    let (version, _) = node_version(&w, ada).await;
    w.caller()
        .retract_node_version(
            w.tenant,
            ada,
            &gnomon::Retraction {
                episode: f,
                version,
            },
        )
        .await
        .unwrap();
    assert_eq!(summaries(gaps(&w).await), vec![]);
}

fn communities(gaps: Vec<Gap>) -> Vec<Gap> {
    gaps.into_iter()
        .filter(|g| matches!(g, Gap::Community { .. }))
        .collect()
}

#[tokio::test]
async fn a_community_whose_replacement_an_erasure_removed_is_listed_until_its_group_is_regenerated()
{
    let w = World::new().await;
    let f = w.episode(at(2024, 1, 1), None).await;
    let e = w.episode(at(2024, 2, 1), None).await;
    let ada = w
        .node("account", f, ValidTime::UNKNOWN, json!({ "name": "Ada" }))
        .await;
    let acme = w
        .node("company", f, ValidTime::UNKNOWN, json!({ "name": "Acme" }))
        .await;
    let bob = w
        .node("account", f, ValidTime::UNKNOWN, json!({ "name": "Bob" }))
        .await;
    // Three generations, the last drawn on E. Each closes the one before,
    // and the members drift, so the first shares no Node with the last.
    let c1 = community(&w, vec![], vec![ada, acme], f).await;
    let c2 = community(&w, vec![c1], vec![acme, bob], f).await;
    let c3 = community(&w, vec![c2], vec![bob], e).await;
    assert_eq!(communities(gaps(&w).await), vec![]);

    w.erase(e).await;

    // The current Community is gone. The one it replaced is closed and has
    // no successor, so its group has no current Summary. The first still has
    // its successor, which is closed but stands.
    assert!(!w.exists("community", c3.0).await);
    assert_eq!(
        communities(gaps(&w).await),
        vec![Gap::Community { community: c2 }]
    );

    // Regenerated, the group has a current Community again.
    community(&w, vec![], vec![acme, bob], f).await;
    assert_eq!(communities(gaps(&w).await), vec![]);
}

#[tokio::test]
async fn erasing_an_episode_puts_the_edge_versions_it_recomputes_back_in_the_listing() {
    let w = World::new().await;
    let mut rng = Rng::new(226);
    let f = w.episode(at(2024, 1, 1), None).await;
    let e = w.episode(at(2024, 2, 1), None).await;
    let g = w.episode(at(2024, 3, 1), None).await;
    let ada = w
        .node("account", f, ValidTime::UNKNOWN, json!({ "name": "Ada" }))
        .await;
    let acme = w
        .node("company", f, ValidTime::UNKNOWN, json!({ "name": "Acme" }))
        .await;
    let owns = w
        .edge(ada, acme, f, ValidTime::UNKNOWN, json!({ "share": 40 }))
        .await;
    // E restates the share for a while, and G writes after it, so the
    // Erasure reopens F's Version and replays G's write.
    w.write_edge(
        owns,
        e,
        ValidTime::between(at(2024, 3, 1), at(2024, 6, 1)),
        json!({ "share": 99 }),
    )
    .await;
    w.write_edge(
        owns,
        g,
        ValidTime::since(at(2024, 8, 1)),
        json!({ "share": 50 }),
    )
    .await;
    let edge_versions = || async {
        w.db.admin()
            .await
            .query_typed(
                "select id from gnomon.version where edge_id = $1 order by id",
                &[(&owns.0, Type::UUID)],
            )
            .await
            .unwrap()
            .iter()
            .map(|r| VersionId(r.get(0)))
            .collect::<Vec<_>>()
    };
    let before = edge_versions().await;
    for (i, v) in before.iter().enumerate() {
        let sentence = format!("Ada holds a share of Acme, take {i}");
        w.caller()
            .store_sentence(w.tenant, *v, &sentence)
            .await
            .unwrap();
        w.caller()
            .store_embedding(
                w.tenant,
                *v,
                &sentence_hash(&w, &sentence).await,
                &rng.unit(),
            )
            .await
            .unwrap();
    }
    let of_edge = |gaps: Vec<Gap>| -> Vec<Gap> {
        gaps.into_iter()
            .filter(|g| {
                matches!(g, Gap::Sentence { edge, .. } | Gap::SentenceEmbedding { edge, .. }
                         if *edge == owns)
            })
            .collect()
    };
    assert_eq!(of_edge(gaps(&w).await), vec![]);

    w.erase(e).await;

    // Every Edge Version the Erasure left is one it recomputed: F's,
    // reopened, and those it replayed from G's write. None has a Sentence,
    // so each is listed, and none is listed for a missing embedding, which
    // follows its Sentence.
    let after = edge_versions().await;
    let reopened: Vec<&VersionId> = after.iter().filter(|v| before.contains(v)).collect();
    let replayed: Vec<&VersionId> = after.iter().filter(|v| !before.contains(v)).collect();
    assert!(!reopened.is_empty(), "the Erasure reopened a Version");
    assert!(!replayed.is_empty(), "the Erasure replayed a Version");
    let mut expected: Vec<Gap> = after
        .iter()
        .map(|v| Gap::Sentence {
            version: *v,
            edge: owns,
        })
        .collect();
    expected.sort_by_key(key);
    assert_eq!(of_edge(gaps(&w).await), expected);

    // Repaired, each needs its embedding, then nothing.
    for v in &after {
        w.caller()
            .store_sentence(w.tenant, *v, "Ada holds forty percent of Acme")
            .await
            .unwrap();
    }
    let hash = sentence_hash(&w, "Ada holds forty percent of Acme").await;
    assert_eq!(of_edge(gaps(&w).await).len(), after.len());
    let vector = rng.unit();
    for v in &after {
        w.caller()
            .store_embedding(w.tenant, *v, &hash, &vector)
            .await
            .unwrap();
    }
    assert_eq!(of_edge(gaps(&w).await), vec![]);
}

#[tokio::test]
async fn a_refinement_version_has_no_sentence_and_is_listed_until_one_is_stored() {
    let w = World::new().await;
    let f = w.episode(at(2024, 1, 1), None).await;
    let ada = w
        .node("account", f, ValidTime::UNKNOWN, json!({ "name": "Ada" }))
        .await;
    let acme = w
        .node("company", f, ValidTime::UNKNOWN, json!({ "name": "Acme" }))
        .await;
    let owns = w
        .edge(ada, acme, f, ValidTime::UNKNOWN, json!({ "share": 7 }))
        .await;
    let first = w.current_edge_version(owns, at(2024, 6, 1)).await;
    let sentence = "Ada holds seven percent of Acme";
    let hash = sentence_hash(&w, sentence).await;
    w.caller()
        .store_sentence(w.tenant, first, sentence)
        .await
        .unwrap();
    w.caller()
        .store_embedding(w.tenant, first, &hash, &Rng::new(5).unit())
        .await
        .unwrap();

    let outcome = w
        .caller()
        .state_edge_bound(
            w.tenant,
            owns,
            &BoundStatement {
                episode: f,
                version: first,
                side: BoundSide::Start,
                bound: Bound::Period {
                    at: at(2023, 5, 1),
                    precision: Precision::Month,
                },
            },
        )
        .await
        .unwrap();
    assert_eq!(outcome.change, gnomon::BoundChange::Refinement);
    let refined = outcome.opened[0];
    let of_edge = |gaps: Vec<Gap>| -> Vec<Gap> {
        gaps.into_iter()
            .filter(|g| {
                matches!(g, Gap::Sentence { edge, .. } | Gap::SentenceEmbedding { edge, .. }
                         if *edge == owns)
            })
            .collect()
    };
    // The Refinement Version carries no Sentence. The closed Version keeps
    // its own, and its embedding.
    assert_eq!(
        of_edge(gaps(&w).await),
        vec![Gap::Sentence {
            version: refined,
            edge: owns
        }]
    );
    assert_eq!(w.sentence_of(first).await.as_deref(), Some(sentence));

    // The Pipeline stores it again, and the same text reuses the full
    // vector, whatever the call passes.
    w.caller()
        .store_sentence(w.tenant, refined, sentence)
        .await
        .unwrap();
    assert_eq!(
        of_edge(gaps(&w).await),
        vec![Gap::SentenceEmbedding {
            version: refined,
            edge: owns,
            text_hash: hash.clone(),
            sentence: sentence.into()
        }]
    );
    w.caller()
        .store_embedding(w.tenant, refined, &hash, &Rng::new(6).unit())
        .await
        .unwrap();
    assert_eq!(of_edge(gaps(&w).await), vec![]);
}

/// Nodes named `n0`, `n1`, ... in the World's Scope, each a NodeEmbedding,
/// a NameEmbedding and a Summary gap.
async fn named_nodes(w: &World, episode: EpisodeId, from: usize, count: usize) -> Vec<NodeId> {
    let mut out = Vec::new();
    for i in from..from + count {
        out.push(
            w.node(
                "company",
                episode,
                ValidTime::UNKNOWN,
                json!({ "name": format!("n{i}") }),
            )
            .await,
        );
    }
    out
}

#[tokio::test]
async fn the_listing_pages_by_key_in_one_order_on_every_call() {
    let w = World::new().await;
    let f = w.episode(at(2024, 1, 1), None).await;
    let hub = w
        .node("account", f, ValidTime::UNKNOWN, json!({ "name": "hub" }))
        .await;
    for node in named_nodes(&w, f, 0, 5).await.into_iter().skip(1) {
        w.edge(hub, node, f, ValidTime::UNKNOWN, json!({})).await;
    }
    let full = gaps(&w).await;
    assert_eq!(full.len(), 4 + 3 * 6, "{full:?}");
    assert_eq!(gaps(&w).await, full, "the same order on every call");

    for limit in [1, 3, 4, 19, 50] {
        let mut paged = Vec::new();
        let mut after = None;
        let mut pages = 0;
        loop {
            let page = w
                .caller()
                .list_gaps(w.tenant, &Scope::new(SCOPE), after.as_ref(), limit)
                .await
                .unwrap();
            assert!(page.gaps.len() <= limit as usize);
            pages += 1;
            paged.extend(page.gaps.clone());
            match page.next {
                Some(next) => {
                    assert_eq!(
                        page.gaps.len(),
                        limit as usize,
                        "a page before the last is full"
                    );
                    assert_eq!(Some(&next), page.gaps.last().map(Gap::cursor).as_ref());
                    after = Some(next);
                }
                None => break,
            }
        }
        assert_eq!(paged, full, "limit {limit}");
        assert!(
            pages <= full.len() / limit as usize + 1,
            "limit {limit}: {pages} pages"
        );
    }
}

#[tokio::test]
async fn paging_through_a_changing_set_neither_skips_nor_repeats_a_gap_that_stayed() {
    let w = World::new().await;
    let mut rng = Rng::new(326);
    let f = w.episode(at(2024, 1, 1), None).await;
    let hub = w
        .node("account", f, ValidTime::UNKNOWN, json!({ "name": "hub" }))
        .await;
    for node in named_nodes(&w, f, 0, 6).await.into_iter().skip(1) {
        w.edge(hub, node, f, ValidTime::UNKNOWN, json!({})).await;
    }
    let initial = gaps(&w).await;

    // Between pages, new gaps appear in every kind the Nodes give, and some
    // gaps ahead of the cursor and behind it are repaired.
    let mut collected: Vec<Gap> = Vec::new();
    let mut after = None;
    let mut added = 6;
    let mut repaired: HashSet<Gap> = HashSet::new();
    loop {
        let page = w
            .caller()
            .list_gaps(w.tenant, &Scope::new(SCOPE), after.as_ref(), 4)
            .await
            .unwrap();
        collected.extend(page.gaps.clone());
        let Some(next) = page.next else { break };
        after = Some(next);
        named_nodes(&w, f, added, 1).await;
        added += 1;
        let remaining = gaps(&w).await;
        // One just behind the cursor and one well ahead of it.
        for g in [collected.last().cloned(), remaining.last().cloned()]
            .into_iter()
            .flatten()
        {
            if remaining.contains(&g) && repaired.insert(g.clone()) {
                repair(&w, &g, &mut rng, f).await;
            }
        }
    }
    let last = gaps(&w).await;
    let seen: HashSet<&Gap> = collected.iter().collect();
    assert_eq!(
        seen.len(),
        collected.len(),
        "a gap was listed twice: {collected:?}"
    );
    for g in initial.iter().filter(|g| last.contains(g)) {
        assert!(seen.contains(g), "{g:?} stayed throughout and was skipped");
    }
    assert!(!repaired.is_empty());
}

/// Repairs one gap as `maintain` would.
async fn repair(w: &World, g: &Gap, rng: &mut Rng, episode: EpisodeId) {
    let c = w.caller();
    match g {
        Gap::Sentence { version, .. } => c
            .store_sentence(w.tenant, *version, "A Sentence.")
            .await
            .unwrap(),
        Gap::SentenceEmbedding {
            version, text_hash, ..
        }
        | Gap::NodeEmbedding {
            version, text_hash, ..
        } => c
            .store_embedding(w.tenant, *version, text_hash, &rng.unit())
            .await
            .unwrap(),
        Gap::NameEmbedding { node, name, .. } => c
            .store_name_embedding(w.tenant, *node, name, &rng.unit())
            .await
            .unwrap(),
        Gap::SummaryEmbedding { summary, .. } => c
            .store_summary_embedding(w.tenant, *summary, &rng.unit())
            .await
            .unwrap(),
        Gap::CommunityEmbedding { community } => c
            .store_community_embedding(w.tenant, *community, &rng.unit())
            .await
            .unwrap(),
        Gap::Summary { node } => {
            summary(w, *node, episode).await;
        }
        Gap::Community { .. } => unreachable!("no Community gap here"),
    }
}

#[tokio::test]
async fn a_scope_lists_only_its_own_gaps_even_for_a_caller_who_sees_others() {
    let w = World::new().await;
    let f = w.episode(at(2024, 1, 1), None).await;
    w.node("account", f, ValidTime::UNKNOWN, json!({ "name": "Ada" }))
        .await;
    let billing = w
        .episode_in("billing", at(2024, 1, 1), "a billing Episode")
        .await;
    let cy = w
        .node_in("billing", "account", billing, json!({ "name": "Cy" }))
        .await;
    let globex = w
        .node_in("billing", "company", billing, json!({ "name": "Globex" }))
        .await;
    // An Edge of billing from its Node to support's, so it has every kind of
    // gap there, a Summary and a Community among them.
    w.edge_in("billing", cy, globex, billing, json!({})).await;
    let s = w
        .worker_in("billing")
        .write_summary(
            w.tenant,
            &NewSummary {
                scope: Scope::new("billing"),
                node: cy,
                content: "Cy.".into(),
                as_of_valid: at(2024, 6, 1),
                citations: vec![DerivedCitation::of(billing)],
            },
        )
        .await
        .unwrap();
    let c = w
        .worker_in("billing")
        .write_community(
            w.tenant,
            &NewCommunity {
                scope: Scope::new("billing"),
                replaces: vec![],
                members: vec![cy, globex],
                content: "Cy and Globex.".into(),
                as_of_valid: at(2024, 6, 1),
                citations: vec![DerivedCitation::of(billing)],
            },
        )
        .await
        .unwrap();

    let both = w.seeing(&[SCOPE, "billing"]);
    let support_gaps = all_gaps(&both, &w, SCOPE).await;
    let billing_gaps = all_gaps(&both, &w, "billing").await;
    assert_eq!(
        support_gaps,
        gaps(&w).await,
        "the support worker sees the same"
    );
    let in_billing: HashSet<Uuid> =
        w.db.admin()
            .await
            .query_typed(
                "select id from gnomon.version where scope = 'billing'
             union all select id from gnomon.node where scope = 'billing'
             union all select id from gnomon.summary where scope = 'billing'
             union all select id from gnomon.community where scope = 'billing'",
                &[],
            )
            .await
            .unwrap()
            .iter()
            .map(|r| r.get(0))
            .collect();
    for g in &support_gaps {
        assert!(
            !in_billing.contains(&key(g).1),
            "support listed billing's {g:?}"
        );
    }
    for expected in [
        Gap::Summary { node: globex },
        Gap::SummaryEmbedding {
            summary: s,
            node: cy,
        },
        Gap::CommunityEmbedding { community: c },
    ] {
        assert!(billing_gaps.contains(&expected), "{expected:?}");
    }
    for g in &billing_gaps {
        assert!(in_billing.contains(&key(g).1), "billing listed {g:?}");
    }
    let kinds: HashSet<usize> = billing_gaps.iter().map(kind_rank).collect();
    assert_eq!(
        kinds.len(),
        6,
        "billing has every kind but two: {billing_gaps:?}"
    );
}

#[tokio::test]
async fn a_scope_the_caller_cannot_see_another_tenant_and_a_limit_below_one_are_refused() {
    let w = World::new().await;
    let refused = w
        .caller()
        .list_gaps(w.tenant, &Scope::new("billing"), None, 10)
        .await
        .unwrap_err();
    assert!(
        matches!(refused, gnomon::Error::InvalidWrite(_)),
        "{refused:?}"
    );
    let refused = w
        .caller()
        .list_gaps(support::tenant(), &Scope::new(SCOPE), None, 10)
        .await
        .unwrap_err();
    assert!(
        matches!(refused, gnomon::Error::TenantMismatch(_)),
        "{refused:?}"
    );
    for limit in [0, -1] {
        let refused = w
            .caller()
            .list_gaps(w.tenant, &Scope::new(SCOPE), None, limit)
            .await
            .unwrap_err();
        assert_eq!(
            refused.sqlstate(),
            Some(&tokio_postgres::error::SqlState::INVALID_PARAMETER_VALUE),
            "{limit}: {refused:?}"
        );
    }
    // An empty Scope lists nothing and has no next page.
    let page = w
        .caller()
        .list_gaps(w.tenant, &Scope::new(SCOPE), None, 10)
        .await
        .unwrap();
    assert_eq!(page.gaps, vec![]);
    assert_eq!(page.next, None);
}
