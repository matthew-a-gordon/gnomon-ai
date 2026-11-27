//! Seam 1, Search (ADR-0010, ADR-0014, ADR-0015, migration 0020), through
//! the Rust core's Store API against real Postgres: one call fusing the
//! vector, full-text and neighbourhood branches over Versions, Summaries and
//! Communities, each result with its rank in each branch, the two graph
//! signals counted over the rows the caller sees, and the per-query weights.
//! The adversarial cases live in sql_boundary.rs, and the no-residue test in
//! erase_episode.rs covers the new tables.

mod support;

use chrono::{DateTime, Utc};
use gnomon::{
    AsOf, CommunityId, DerivedCitation, EdgeId, EpisodeId, NewCommunity, NewSummary, NodeId, Scope,
    SearchHit, SearchRequest, SearchResult, SearchWeights, SummaryId, ValidTime, VersionId,
    Versioned,
};
use serde_json::json;
use support::erasure::{ALL_SCOPES, SCOPE, World, at};
use support::vectors::Rng;

/// A valid-at instant after every `as_of_valid` the tests write.
fn now() -> DateTime<Utc> {
    at(2025, 1, 1)
}

fn current() -> AsOf {
    AsOf::current(now())
}

async fn search(w: &World, query: &SearchRequest<'_>, limit: i32) -> Vec<SearchResult> {
    w.caller().search(w.tenant, query, limit).await.unwrap()
}

/// A Node of Kind `account` in the worker's Scope with this name, written by
/// a new Episode.
async fn named(w: &World, name: &str) -> (NodeId, EpisodeId) {
    let e = w.episode(at(2024, 1, 1), None).await;
    let n = w
        .node("account", e, ValidTime::UNKNOWN, json!({ "name": name }))
        .await;
    (n, e)
}

async fn company(w: &World, name: &str) -> NodeId {
    let e = w.episode(at(2024, 1, 1), None).await;
    w.node("company", e, ValidTime::UNKNOWN, json!({ "name": name }))
        .await
}

/// The Version a Node holds now.
async fn version_of(w: &World, node: NodeId) -> gnomon::NodeVersion {
    w.read(node, now()).await.expect("the Node holds a Version")
}

async fn summary(w: &World, node: NodeId, content: &str, cites: &[EpisodeId]) -> SummaryId {
    w.caller()
        .write_summary(
            w.tenant,
            &NewSummary {
                scope: Scope::new(SCOPE),
                node,
                content: content.into(),
                as_of_valid: at(2024, 6, 1),
                citations: cites.iter().map(|e| DerivedCitation::of(*e)).collect(),
            },
        )
        .await
        .unwrap()
}

async fn community(
    w: &World,
    members: &[NodeId],
    content: &str,
    cites: &[EpisodeId],
) -> CommunityId {
    w.caller()
        .write_community(
            w.tenant,
            &NewCommunity {
                scope: Scope::new(SCOPE),
                replaces: vec![],
                members: members.to_vec(),
                content: content.into(),
                as_of_valid: at(2024, 6, 1),
                citations: cites.iter().map(|e| DerivedCitation::of(*e)).collect(),
            },
        )
        .await
        .unwrap()
}

fn find<'r>(results: &'r [SearchResult], hit: &SearchHit) -> Option<&'r SearchResult> {
    results.iter().find(|r| &r.hit == hit)
}

fn node_hit(version: VersionId, node: NodeId) -> SearchHit {
    SearchHit::Version {
        version,
        of: Versioned::Node(node),
    }
}

#[tokio::test]
async fn one_search_returns_versions_summaries_and_communities_fused_with_each_branchs_rank() {
    let w = World::new().await;
    let mut rng = Rng::new(20);
    let centre = rng.unit();
    let (acme, e) = named(&w, "Acme widgets").await;
    let acme_version = version_of(&w, acme).await;
    let s = summary(&w, acme, "Acme sells widgets to schools.", &[e]).await;
    let c = community(&w, &[acme], "Makers of widgets around Acme.", &[e]).await;
    // A Node that matches neither the text nor the embedding.
    named(&w, "Bolt").await;

    let near = |rng: &mut Rng| rng.near(&centre, 0.05);
    let (va, vs, vc) = (near(&mut rng), near(&mut rng), near(&mut rng));
    let caller = w.caller();
    caller
        .store_embedding(w.tenant, acme_version.id, &acme_version.text_hash, &va)
        .await
        .unwrap();
    caller
        .store_summary_embedding(w.tenant, s, &vs)
        .await
        .unwrap();
    caller
        .store_community_embedding(w.tenant, c, &vc)
        .await
        .unwrap();

    let query = SearchRequest::text("widgets", current()).with_embedding(&centre);
    let results = search(&w, &query, 10).await;

    let hits = [
        node_hit(acme_version.id, acme),
        SearchHit::Summary {
            summary: s,
            node: acme,
        },
        SearchHit::Community(c),
    ];
    assert_eq!(results.len(), 3, "{results:#?}");
    for hit in &hits {
        let r = find(&results, hit).unwrap_or_else(|| panic!("{hit:?} not in {results:#?}"));
        assert_eq!(r.scope, Scope::new(SCOPE));
        let vector = r.vector.expect("the vector branch ranks it");
        let full_text = r.full_text.expect("the full-text branch ranks it");
        assert!(vector.value >= 0.0 && vector.value < 0.1, "{r:?}");
        assert!(full_text.value > 0.0, "{r:?}");
        assert!((1..=3).contains(&vector.rank) && (1..=3).contains(&full_text.rank));
    }
    // The three ranks of each branch are 1, 2 and 3, and the fused order is
    // by score, from 1.
    let mut vector_ranks: Vec<i64> = results.iter().map(|r| r.vector.unwrap().rank).collect();
    vector_ranks.sort();
    assert_eq!(vector_ranks, [1, 2, 3]);
    let mut text_ranks: Vec<i64> = results.iter().map(|r| r.full_text.unwrap().rank).collect();
    text_ranks.sort();
    assert_eq!(text_ranks, [1, 2, 3]);
    assert_eq!(
        results.iter().map(|r| r.rank).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert!(results.windows(2).all(|p| p[0].score >= p[1].score));
    // The Summary's text, "Acme sells widgets to schools.", is five words,
    // and the Version's searchable text, its name, two: BM25 puts the
    // shorter text holding the word first.
    let version = find(&results, &hits[0]).unwrap();
    let summary = find(&results, &hits[1]).unwrap();
    assert!(version.full_text.unwrap().value > summary.full_text.unwrap().value);
}

#[tokio::test]
async fn a_summary_or_community_is_found_only_when_its_recorded_time_holds_known_at_and_valid_at_is_on_or_after_as_of_valid()
 {
    let w = World::new().await;
    let (acme, e) = named(&w, "Acme").await;
    let old = summary(&w, acme, "Acme sells gadgets.", &[e]).await;
    let c = community(&w, &[acme], "The gadgets circle.", &[e]).await;
    let between = w
        .write(acme, e, ValidTime::UNKNOWN, json!({ "plan": "basic" }))
        .await;
    // Regenerating the Summary closes the old row (ADR-0015).
    let new = summary(&w, acme, "Acme sells gadgets and gizmos.", &[e]).await;

    let derived = |results: &[SearchResult]| -> Vec<SearchHit> {
        let mut hits: Vec<SearchHit> = results
            .iter()
            .map(|r| r.hit)
            .filter(|h| !matches!(h, SearchHit::Version { .. }))
            .collect();
        hits.sort_by_key(|h| format!("{h:?}"));
        hits
    };
    let at_instants = |as_of: AsOf| {
        let w = &w;
        async move { derived(&search(w, &SearchRequest::text("gadgets", as_of), 10).await) }
    };
    let s = |summary| SearchHit::Summary {
        summary,
        node: acme,
    };
    let mut expected_now = vec![s(new), SearchHit::Community(c)];
    expected_now.sort_by_key(|h| format!("{h:?}"));
    assert_eq!(
        at_instants(current()).await,
        expected_now,
        "the latest, now"
    );
    let mut expected_between = vec![s(old), SearchHit::Community(c)];
    expected_between.sort_by_key(|h| format!("{h:?}"));
    // `between` is the Recorded time of a write after the old Summary and
    // before the new one.
    assert_eq!(
        at_instants(AsOf::at(now(), between)).await,
        expected_between,
        "the old Summary at a known-at instant it held"
    );
    // At valid-at instants before the one they were written for, Search finds
    // neither, and at that instant both.
    assert_eq!(at_instants(AsOf::current(at(2024, 5, 31))).await, vec![]);
    assert_eq!(
        at_instants(AsOf::current(at(2024, 6, 1))).await,
        expected_now
    );
    // Before either was recorded, neither.
    assert_eq!(at_instants(AsOf::at(now(), at(2020, 1, 1))).await, vec![]);

    // The vector branch keeps the same rules. The old Summary's embedding,
    // stored after a regeneration closed it, carries its Recorded time.
    let mut rng = Rng::new(5);
    let v = rng.unit();
    for id in [old, new] {
        w.caller()
            .store_summary_embedding(w.tenant, id, &v)
            .await
            .unwrap();
    }
    let by_vector = |as_of: AsOf| {
        let w = &w;
        let v = &v;
        async move {
            let results = search(w, &SearchRequest::text("", as_of).with_embedding(v), 10).await;
            results
                .iter()
                .filter(|r| r.vector.is_some())
                .map(|r| r.hit)
                .filter(|h| matches!(h, SearchHit::Summary { .. }))
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(by_vector(current()).await, vec![s(new)]);
    assert_eq!(by_vector(AsOf::at(now(), between)).await, vec![s(old)]);
    assert_eq!(by_vector(AsOf::current(at(2024, 5, 31))).await, vec![]);
}

/// An Edge `owns` from `source` to `target`, written by a new Episode.
async fn owns(w: &World, source: NodeId, target: NodeId) -> EdgeId {
    let e = w.episode(at(2024, 1, 1), None).await;
    w.edge(source, target, e, ValidTime::UNKNOWN, json!({ "share": 1 }))
        .await
}

/// One more Episode citing Edge `edge`, a mention.
async fn mention_edge(w: &World, edge: EdgeId, share: i64) {
    let e = w.episode(at(2024, 2, 1), None).await;
    w.write_edge(edge, e, ValidTime::UNKNOWN, json!({ "share": share }))
        .await;
}

fn edges_of(results: &[SearchResult]) -> Vec<EdgeId> {
    let mut edges: Vec<EdgeId> = results
        .iter()
        .filter_map(|r| match r.hit {
            SearchHit::Version {
                of: Versioned::Edge(e),
                ..
            } => Some(e),
            _ => None,
        })
        .collect();
    edges.sort_by_key(|e| e.0);
    edges
}

fn sqlstate<T: std::fmt::Debug>(r: Result<T, gnomon::Error>) -> String {
    match r {
        Err(gnomon::Error::Database(e)) => {
            e.code().map(|c| c.code().to_owned()).unwrap_or_default()
        }
        other => panic!("expected a database error, got {other:?}"),
    }
}

#[tokio::test]
async fn the_neighbourhood_keeps_the_fifty_most_mentioned_edges_per_node_to_its_depth_and_the_same_ones_each_time()
 {
    let w = World::new().await;
    let (hub, _) = named(&w, "Hub").await;
    let mut edges = vec![];
    let mut companies = vec![];
    for i in 0..60 {
        let c = company(&w, &format!("Company {i}")).await;
        companies.push(c);
        edges.push(owns(&w, hub, c).await);
    }
    // Five of the later Edges have more mentions than the rest, so they are
    // kept although 50 Edges have lower IDs.
    let boosted: Vec<EdgeId> = edges[55..].to_vec();
    for (i, e) in boosted.iter().enumerate() {
        mention_edge(&w, *e, 2 + i as i64).await;
    }
    // Beyond the first Company the hub owns, another account owns it too.
    let (far, _) = named(&w, "Far").await;
    let far_edge = owns(&w, far, companies[59]).await;

    let seeds = [hub];
    let query = SearchRequest::text("", current()).seeds(&seeds);
    let first = search(&w, &query, 300).await;
    let mut expected: Vec<EdgeId> = boosted.clone();
    expected.extend(edges[..45].iter().copied());
    expected.sort_by_key(|e| e.0);
    assert_eq!(
        edges_of(&first),
        expected,
        "the five most mentioned and the 45 lowest IDs"
    );
    // Every result came from the neighbourhood, at depth 1: the 50 Edges and
    // the 50 Companies at their other ends.
    assert_eq!(first.len(), 100);
    for r in &first {
        let n = r.neighbourhood.expect("the neighbourhood found it");
        assert_eq!(n.value, 1, "{r:?}");
        assert!(r.vector.is_none() && r.full_text.is_none());
    }
    assert!(!edges_of(&first).contains(&far_edge));
    // The same call follows the same Edges, in the same order.
    assert_eq!(search(&w, &query, 300).await, first);
    // Depth 1 is the default.
    assert_eq!(search(&w, &query.clone().depth(1), 300).await, first);

    // At depth 2 the neighbourhood goes on from each Company it reached, so
    // it reaches the other owner of the last Company, through an Edge at
    // depth 2.
    let deeper = search(&w, &query.clone().depth(2), 300).await;
    assert!(edges_of(&deeper).contains(&far_edge));
    let far_hit = deeper
        .iter()
        .find(
            |r| matches!(r.hit, SearchHit::Version { of: Versioned::Edge(e), .. } if e == far_edge),
        )
        .unwrap();
    assert_eq!(far_hit.neighbourhood.unwrap().value, 2);
    let far_node = deeper
        .iter()
        .find(|r| matches!(r.hit, SearchHit::Version { of: Versioned::Node(n), .. } if n == far))
        .expect("the other owner is reached");
    assert_eq!(far_node.neighbourhood.unwrap().value, 2);
    // The Edges the hub kept are still the same 50, at depth 1.
    for e in &expected {
        let r = deeper
            .iter()
            .find(|r| matches!(r.hit, SearchHit::Version { of: Versioned::Edge(x), .. } if x == *e))
            .unwrap();
        assert_eq!(r.neighbourhood.unwrap().value, 1);
    }

    // Seeded from the last Company, depth 1 reaches its two owners, and
    // depth 2 keeps the hub's 50 Edges again: the Edge back to the seed,
    // which depth 1 followed, and 49 more. The seed is not a result.
    let company_seed = [companies[59]];
    let from_company = search(
        &w,
        &SearchRequest::text("", current())
            .seeds(&company_seed)
            .depth(2),
        300,
    )
    .await;
    let mut expected_from_company = expected.clone();
    expected_from_company.push(far_edge);
    expected_from_company.sort_by_key(|e| e.0);
    assert_eq!(edges_of(&from_company), expected_from_company);
    assert!(!from_company.iter().any(
        |r| matches!(r.hit, SearchHit::Version { of: Versioned::Node(n), .. } if n == companies[59])
    ));
    let back = from_company
        .iter()
        .find(|r| matches!(r.hit, SearchHit::Version { of: Versioned::Edge(e), .. } if e == edges[59]))
        .unwrap();
    assert_eq!(back.neighbourhood.unwrap().value, 1);
    // At depth 2 from the hub, the hub is not a result either.
    assert!(
        !deeper.iter().any(
            |r| matches!(r.hit, SearchHit::Version { of: Versioned::Node(n), .. } if n == hub)
        )
    );

    // No other depth.
    for depth in [0, 3] {
        let r = w
            .caller()
            .search(w.tenant, &query.clone().depth(depth), 10)
            .await;
        assert_eq!(sqlstate(r), "22023", "depth {depth}");
    }
}

fn hits(results: &[SearchResult]) -> Vec<SearchHit> {
    results.iter().map(|r| r.hit).collect()
}

#[tokio::test]
async fn with_no_seeds_the_neighbourhood_expands_from_the_top_node_hits_of_the_other_branches() {
    let w = World::new().await;
    let (hub, _) = named(&w, "Hub").await;
    let c = company(&w, "Plain").await;
    let edge = owns(&w, hub, c).await;
    // A Node no branch finds, joined to a Node no branch finds.
    let (lone, _) = named(&w, "Lone").await;
    let d = company(&w, "Quiet").await;
    owns(&w, lone, d).await;

    let results = search(&w, &SearchRequest::text("hub", current()), 10).await;
    let hub_version = version_of(&w, hub).await.id;
    let plain_version = version_of(&w, c).await.id;
    let edge_version = w.current_edge_version(edge, now()).await;
    let mut found = hits(&results);
    found.sort_by_key(|h| format!("{h:?}"));
    let mut expected = vec![
        node_hit(hub_version, hub),
        node_hit(plain_version, c),
        SearchHit::Version {
            version: edge_version,
            of: Versioned::Edge(edge),
        },
    ];
    expected.sort_by_key(|h| format!("{h:?}"));
    assert_eq!(found, expected);
    let plain = find(&results, &node_hit(plain_version, c)).unwrap();
    assert!(plain.full_text.is_none() && plain.vector.is_none());
    assert_eq!(plain.neighbourhood.unwrap().value, 1);
    // The hub itself is a seed, not a neighbour.
    assert!(
        find(&results, &node_hit(hub_version, hub))
            .unwrap()
            .neighbourhood
            .is_none()
    );
}

#[tokio::test]
async fn graph_signals_never_add_a_candidate_the_branches_did_not_find() {
    let w = World::new().await;
    // A hub, much mentioned and much connected, that no branch finds.
    let (popular, e) = named(&w, "Popular").await;
    for i in 0..5 {
        let c = company(&w, &format!("Holding {i}")).await;
        let edge = owns(&w, popular, c).await;
        mention_edge(&w, edge, 9).await;
        let more = w.episode(at(2024, 3, 1), None).await;
        w.write(
            popular,
            more,
            ValidTime::UNKNOWN,
            json!({ "plan": format!("p{i}") }),
        )
        .await;
    }
    let _ = e;
    let (gizmo, _) = named(&w, "Gizmo").await;
    let gizmo_version = version_of(&w, gizmo).await.id;

    for (mentions, connectedness) in [(0.0, 0.0), (0.5, 0.5), (100.0, 100.0)] {
        let weights = SearchWeights {
            mentions,
            connectedness,
            ..SearchWeights::default()
        };
        let results = search(
            &w,
            &SearchRequest::text("gizmo", current()).weights(weights),
            10,
        )
        .await;
        assert_eq!(
            hits(&results),
            vec![node_hit(gizmo_version, gizmo)],
            "signal weights {mentions} and {connectedness}"
        );
    }
    // The hub is ranked by the signals once a branch finds it.
    let results = search(&w, &SearchRequest::text("popular gizmo", current()), 10).await;
    let popular_version = version_of(&w, popular).await.id;
    let p = find(&results, &node_hit(popular_version, popular)).unwrap();
    assert_eq!(p.mentions.count, 6, "its Episode and five more");
    assert_eq!(p.mentions.rank, Some(1));
    assert_eq!(p.connectedness.count, 5);
    assert_eq!(p.connectedness.rank, Some(1));
}

#[tokio::test]
async fn per_query_weights_move_the_fused_order_by_their_share_of_each_reciprocal_rank() {
    let w = World::new().await;
    let mut rng = Rng::new(7);
    let query_vector = rng.unit();
    // A is found by full text alone, and B by vector alone, each at rank 1.
    let (a, _) = named(&w, "Sprocket").await;
    let (b, _) = named(&w, "Other").await;
    let b_version = version_of(&w, b).await;
    w.caller()
        .store_embedding(w.tenant, b_version.id, &b_version.text_hash, &query_vector)
        .await
        .unwrap();
    let a_version = version_of(&w, a).await.id;
    let order = |weights: SearchWeights| {
        let w = &w;
        let query_vector = &query_vector;
        async move {
            hits(
                &search(
                    w,
                    &SearchRequest::text("sprocket", current())
                        .with_embedding(query_vector)
                        .weights(weights),
                    10,
                )
                .await,
            )
        }
    };
    let (ha, hb) = (node_hit(a_version, a), node_hit(b_version.id, b));
    let by_id = if a_version.0 < b_version.id.0 {
        vec![ha, hb]
    } else {
        vec![hb, ha]
    };
    assert_eq!(
        order(SearchWeights::default()).await,
        by_id,
        "a tie goes to the lower ID"
    );
    let full_text = SearchWeights {
        full_text: 2.0,
        ..SearchWeights::default()
    };
    assert_eq!(order(full_text).await, vec![ha, hb]);
    let vector = SearchWeights {
        vector: 2.0,
        ..SearchWeights::default()
    };
    assert_eq!(order(vector).await, vec![hb, ha]);
    // A branch weighted 0 adds no result: B, which only the vector branch
    // finds, is gone.
    let no_vector = SearchWeights {
        vector: 0.0,
        ..SearchWeights::default()
    };
    assert_eq!(order(no_vector).await, vec![ha]);
    let results = search(
        &w,
        &SearchRequest::text("sprocket", current())
            .with_embedding(&query_vector)
            .weights(full_text),
        10,
    )
    .await;
    // The fused score is the weighted sum of 1 / (60 + rank) over the lists
    // that ranked it: A is first in full text at weight 2 and has one
    // mention, as B has, and no Edge.
    let first = &results[0];
    assert_eq!(first.mentions.rank, Some(1));
    assert!(
        (first.score - (2.0 / 61.0 + 0.5 / 61.0)).abs() < 1e-12,
        "{first:?}"
    );

    // Two Nodes the full-text branch ties, one more mentioned and one more
    // connected than the other.
    let (c, _) = named(&w, "Gear one").await;
    let (d, _) = named(&w, "Gear two").await;
    for share in ["x", "y"] {
        let e = w.episode(at(2024, 3, 1), None).await;
        w.write(d, e, ValidTime::UNKNOWN, json!({ "plan": share }))
            .await;
    }
    let (c_version, d_version) = (version_of(&w, c).await.id, version_of(&w, d).await.id);
    let gear = |weights: SearchWeights| {
        let w = &w;
        async move {
            hits(
                &search(
                    w,
                    &SearchRequest::text("gear", current()).weights(weights),
                    10,
                )
                .await,
            )
        }
    };
    let no_signals = SearchWeights {
        mentions: 0.0,
        connectedness: 0.0,
        ..SearchWeights::default()
    };
    let (hc, hd) = (node_hit(c_version, c), node_hit(d_version, d));
    assert!(c_version.0 < d_version.0, "C's Version is older");
    assert_eq!(gear(no_signals).await, vec![hc, hd]);
    // The full-text branch ranks the tie by Version ID, C first, and at the
    // signals' default weight that rank still decides.
    assert_eq!(gear(SearchWeights::default()).await, vec![hc, hd]);
    let mentions = SearchWeights {
        mentions: 2.0,
        connectedness: 0.0,
        ..SearchWeights::default()
    };
    assert_eq!(gear(mentions).await, vec![hd, hc], "D is more mentioned");
    // Now E, which ties with F as C with D, owns a Company, so it is the more
    // connected, and F the more mentioned.
    let (f, _) = named(&w, "Cog one").await;
    let (e, _) = named(&w, "Cog two").await;
    let co = company(&w, "Cogs Ltd").await;
    owns(&w, e, co).await;
    let (f_version, e_version) = (version_of(&w, f).await.id, version_of(&w, e).await.id);
    let cog = |weights: SearchWeights| {
        let w = &w;
        async move {
            hits(&search(w, &SearchRequest::text("cog", current()).weights(weights), 10).await)
                .into_iter()
                .filter(|h| matches!(h, SearchHit::Version { of: Versioned::Node(n), .. } if *n == e || *n == f))
                .collect::<Vec<_>>()
        }
    };
    let (he, hf) = (node_hit(e_version, e), node_hit(f_version, f));
    assert_eq!(cog(no_signals).await, vec![hf, he]);
    let connected = SearchWeights {
        mentions: 0.0,
        connectedness: 2.0,
        ..SearchWeights::default()
    };
    assert_eq!(cog(connected).await, vec![he, hf], "E is more connected");

    // A weight below 0 or not finite is refused.
    for bad in [-1.0, f64::NAN, f64::INFINITY] {
        let weights = SearchWeights {
            vector: bad,
            ..SearchWeights::default()
        };
        let r = w
            .caller()
            .search(
                w.tenant,
                &SearchRequest::text("gear", current()).weights(weights),
                10,
            )
            .await;
        assert_eq!(sqlstate(r), "22023", "weight {bad}");
    }
    let r = w
        .caller()
        .search(w.tenant, &SearchRequest::text("gear", current()), 0)
        .await;
    assert_eq!(sqlstate(r), "22023", "limit 0");
}

#[tokio::test]
async fn an_episode_edge_or_summary_in_a_scope_the_caller_cannot_see_changes_no_ranking() {
    let w = World::new().await;
    let hidden = "billing";
    let (x, ex) = named(&w, "Sprocket x").await;
    let (y, _) = named(&w, "Sprocket y").await;
    let co = company(&w, "Sprocket works").await;
    owns(&w, y, co).await;
    async fn only_support(w: &World) -> Vec<SearchResult> {
        w.seeing(&[SCOPE])
            .search(w.tenant, &SearchRequest::text("sprocket", current()), 20)
            .await
            .unwrap()
    }
    let before = only_support(&w).await;
    assert!(!before.is_empty());
    let _ = ex;

    // In the hidden Scope: Edges from X to Companies there, each cited by
    // more Episodes there, and a Summary of X there that says the query's
    // word.
    let mut hidden_episodes = vec![];
    for i in 0..3 {
        let e = w
            .episode_in(hidden, at(2024, 1, 1), "a hidden Episode")
            .await;
        hidden_episodes.push(e);
        let c = w
            .node_in(
                hidden,
                "company",
                e,
                json!({ "name": format!("Sprocket {i}") }),
            )
            .await;
        let edge = w.edge_in(hidden, x, c, e, json!({ "share": i })).await;
        for _ in 0..2 {
            let more = w.episode_in(hidden, at(2024, 2, 1), "another").await;
            w.seeing(ALL_SCOPES)
                .write_edge(
                    w.tenant,
                    edge,
                    &gnomon::Patch::new(
                        more,
                        ValidTime::UNKNOWN,
                        support::erasure::values(json!({ "share": 10 + i })),
                    ),
                )
                .await
                .unwrap();
        }
    }
    w.seeing(ALL_SCOPES)
        .write_summary(
            w.tenant,
            &NewSummary {
                scope: Scope::new(hidden),
                node: x,
                content: "Sprocket x, much discussed in billing.".into(),
                as_of_valid: at(2024, 6, 1),
                citations: hidden_episodes
                    .iter()
                    .map(|e| DerivedCitation::of(*e))
                    .collect(),
            },
        )
        .await
        .unwrap();

    // The caller of the visible Scope gets the same results, every score,
    // rank and count included.
    assert_eq!(only_support(&w).await, before);

    // A caller who sees both Scopes counts the hidden rows.
    let both = w
        .seeing(&[SCOPE, hidden])
        .search(w.tenant, &SearchRequest::text("sprocket", current()), 20)
        .await
        .unwrap();
    let x_version = version_of(&w, x).await.id;
    let seen = find(&both, &node_hit(x_version, x)).unwrap();
    assert_eq!(seen.connectedness.count, 3);
    let unseen = find(&before, &node_hit(x_version, x)).unwrap();
    assert_eq!(unseen.connectedness.count, 0);
    assert_ne!(both, before);
    assert!(
        both.iter()
            .any(|r| matches!(r.hit, SearchHit::Summary { node, .. } if node == x))
    );
}

#[tokio::test]
async fn a_retracted_episode_is_a_mention_only_at_a_known_at_before_its_retraction() {
    let w = World::new().await;
    let (a, e1) = named(&w, "Cog a").await;
    let e2 = w.episode(at(2024, 2, 1), None).await;
    w.write(a, e2, ValidTime::UNKNOWN, json!({ "plan": "basic" }))
        .await;
    let e3 = w.episode(at(2024, 3, 1), None).await;
    let before_retraction = w
        .write(a, e3, ValidTime::UNKNOWN, json!({ "plan": "pro" }))
        .await;
    for e in [e1, e2] {
        w.caller().retract_episode(w.tenant, e).await.unwrap();
    }

    let mentions = |as_of: AsOf| {
        let w = &w;
        async move {
            let results = search(w, &SearchRequest::text("cog", as_of), 10).await;
            assert_eq!(results.len(), 1, "{results:#?}");
            results[0].mentions
        }
    };
    // Before the Retractions were recorded, all three Episodes mention A.
    assert_eq!(
        mentions(AsOf::at(now(), before_retraction)).await,
        gnomon::SignalRank {
            count: 3,
            rank: Some(1)
        }
    );
    // Now e1 and e2 are retracted, and only e3 mentions it.
    assert_eq!(
        mentions(current()).await,
        gnomon::SignalRank {
            count: 1,
            rank: Some(1)
        }
    );
}

#[tokio::test]
async fn erasure_removes_the_search_rows_of_every_summary_and_community_it_removes_in_any_scope() {
    let w = World::new().await;
    let hidden = "billing";
    let (acme, e) = named(&w, "Acme").await;
    let c = community(&w, &[acme], "The anvils circle.", &[e]).await;
    // A Summary of Acme in a Scope the worker cannot see.
    let he = w.episode_in(hidden, at(2024, 1, 1), "billing's").await;
    let s = w
        .seeing(ALL_SCOPES)
        .write_summary(
            w.tenant,
            &NewSummary {
                scope: Scope::new(hidden),
                node: acme,
                content: "Acme buys anvils.".into(),
                as_of_valid: at(2024, 6, 1),
                citations: vec![DerivedCitation::of(he)],
            },
        )
        .await
        .unwrap();
    let mut rng = Rng::new(3);
    w.seeing(ALL_SCOPES)
        .store_summary_embedding(w.tenant, s, &rng.unit())
        .await
        .unwrap();
    w.caller()
        .store_community_embedding(w.tenant, c, &rng.unit())
        .await
        .unwrap();
    fn everyone(w: &World) -> gnomon::Caller<'_> {
        w.seeing(ALL_SCOPES)
    }
    let found = everyone(&w)
        .search(w.tenant, &SearchRequest::text("anvils", current()), 10)
        .await
        .unwrap();
    assert_eq!(found.len(), 2, "{found:#?}");

    // The worker of Acme's Scope erases Acme. Its Community and its Summary
    // in the hidden Scope go, with their token rows and embeddings.
    w.erase_node(acme).await;
    let admin = w.db.admin().await;
    for table in ["derived_search_token", "derived_embedding"] {
        let n: i64 = admin
            .query_typed(&format!("select count(*) from gnomon.{table}"), &[])
            .await
            .unwrap()[0]
            .get(0);
        assert_eq!(
            n, 0,
            "gnomon.{table} keeps rows of what the Erasure removed"
        );
    }
    support::erasure::assert_search_counts_match_the_token_rows(&admin).await;
    assert!(
        everyone(&w)
            .search(w.tenant, &SearchRequest::text("anvils", current()), 10)
            .await
            .unwrap()
            .is_empty()
    );
}

/// A smoke check, not a performance test: ticket #21 owns those. It loads
/// `GNOMON_SEARCH_SMOKE_ROWS` texts (default 2,000) into one Scope as
/// prototype 5's generator lays them out, with their token rows, one Edge
/// from each Node to another, each cited by an Episode, and prints the p50
/// and p95 of 20 fused Searches by text and vector at the current belief,
/// next to prototype 5's 66 ms p95 at 50,000 visible rows. It asserts no
/// timing.
#[tokio::test]
async fn smoke_check_prints_the_time_of_a_fused_search() {
    let rows: usize = std::env::var("GNOMON_SEARCH_SMOKE_ROWS")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(2_000);
    let w = World::new().await;
    let mut rng = Rng::new(66);
    let centres = support::vectors::subtopics(&mut rng, 20, 10);
    let mut corpus = support::vectors::Corpus::default();
    support::vectors::load(
        &w.db,
        &mut corpus,
        w.tenant,
        SCOPE,
        rows,
        &centres,
        &mut rng,
    )
    .await;

    let admin = w.db.admin().await;
    // The token rows, which the loader's inserts with triggers off did not
    // write, through the function the search index trigger calls, under the
    // worker's claims so the count changes it records name the Tenant.
    let mut tx_client = w.db.admin().await;
    let tx = tx_client.transaction().await.unwrap();
    tx.query_typed(
        "select set_config('request.jwt.claims', $1, true)",
        &[(&w.claims(), tokio_postgres::types::Type::TEXT)],
    )
    .await
    .unwrap();
    tx.execute_typed(
        "select gnomon.index_search_text($1, array(select id from gnomon.version
                                                  where tenant_id = $1), false)",
        &[(&w.tenant.0, tokio_postgres::types::Type::UUID)],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    // One Edge from each Node to the next, current and valid always, each
    // cited by one Episode.
    let e = w.episode(at(2024, 1, 1), None).await;
    let mut tx_client = w.db.admin().await;
    let tx = tx_client.transaction().await.unwrap();
    tx.batch_execute("set local session_replication_role = replica")
        .await
        .unwrap();
    tx.execute_typed(
        "with nodes as (
           select id, row_number() over (order by id) as i from gnomon.node
           where tenant_id = $1 and scope = $2 and kind = 'company'),
         pairs as (
           select gnomon.uuid_v7() as edge, a.id as source, b.id as target
           from nodes a join nodes b on b.i = a.i % (select count(*) from nodes) + 1),
         edges as (
           insert into gnomon.edge (tenant_id, scope, id, kind, source_id, target_id)
           select $1, $2, p.edge, 'edge', p.source, p.target from pairs p returning id),
         versions as (
           insert into gnomon.version (tenant_id, scope, id, edge_id, props, valid_from,
                                       valid_from_precision, valid_to, valid_to_precision,
                                       recorded_from, merge_rule)
           select $1, $2, gnomon.uuid_v7(), x.id, '{}', '-infinity', 'unknown', 'infinity',
                  'unknown', '2024-01-01', 'recency'
           from edges x returning id)
         insert into gnomon.citation (tenant_id, scope, version_id, episode_id, property,
                                      recorded_at, episode_reference_time, episode_recorded_at)
         select $1, $2, v.id, ep.id, 'holds', '2024-01-01', ep.reference_time, ep.recorded_at
         from versions v cross join gnomon.episode ep where ep.tenant_id = $1 and ep.id = $3",
        &[
            (&w.tenant.0, tokio_postgres::types::Type::UUID),
            (&SCOPE, tokio_postgres::types::Type::TEXT),
            (&e.0, tokio_postgres::types::Type::UUID),
        ],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    // Prototype 5 timed vacuumed tables, whose index-only scans need the
    // visibility map.
    admin.batch_execute("vacuum (analyze)").await.unwrap();

    let visible = corpus.visible(&[SCOPE], now(), None);
    let mut times = vec![];
    for i in 0..21 {
        let centre = rng.below(centres.len());
        let q = rng.near(&centres[centre], 0.6);
        // A name's number, which one text holds. Every name holds "text",
        // and a word every text holds makes the full-text branch score every
        // posting of the Scope (migration 0016), as prototype 5 avoided by
        // querying mid-frequency words.
        let text = format!("{}", rng.below(rows));
        let started = std::time::Instant::now();
        let results = search(
            &w,
            &SearchRequest::text(&text, current()).with_embedding(&q),
            10,
        )
        .await;
        let took = started.elapsed();
        assert!(!results.is_empty());
        // The first call warms the caches, as prototype 5 ran each query once
        // untimed.
        if i > 0 {
            times.push(took.as_secs_f64() * 1000.0);
        }
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p = |q: f64| times[((times.len() as f64 * q).ceil() as usize).saturating_sub(1)];
    println!(
        "fused Search over {rows} texts, {} Versions, {visible} visible now: \
         p50 {:.1} ms, p95 {:.1} ms (prototype 5: 66 ms p95 at 50,000 visible rows)",
        corpus.versions.len(),
        p(0.5),
        p(0.95)
    );
}

#[tokio::test]
async fn the_neighbourhood_follows_a_same_as_edge_as_any_other_and_keeps_both_nodes() {
    let w = World::new().await;
    let (x, e) = named(&w, "Quill").await;
    let (y, _) = named(&w, "Feather").await;
    let edge = w
        .caller()
        .create_edge(
            &gnomon::NewEdge {
                tenant: Some(w.tenant),
                scope: Scope::new(SCOPE),
                kind: "same_as".into(),
                source: x,
                target: y,
            },
            &gnomon::Patch::new(e, ValidTime::UNKNOWN, serde_json::Map::new()),
        )
        .await
        .unwrap()
        .0;
    let results = search(&w, &SearchRequest::text("quill", current()), 10).await;
    let (xv, yv) = (version_of(&w, x).await.id, version_of(&w, y).await.id);
    let mut found = hits(&results);
    found.sort_by_key(|h| format!("{h:?}"));
    let mut expected = vec![
        node_hit(xv, x),
        node_hit(yv, y),
        SearchHit::Version {
            version: w.current_edge_version(edge, now()).await,
            of: Versioned::Edge(edge),
        },
    ];
    expected.sort_by_key(|h| format!("{h:?}"));
    assert_eq!(found, expected, "the two Nodes stay two results (ADR-0012)");
    let feather = find(&results, &node_hit(yv, y)).unwrap();
    assert_eq!(feather.neighbourhood.unwrap().value, 1);
    assert!(feather.full_text.is_none());
}

#[tokio::test]
async fn mentions_count_the_citations_of_the_versions_valid_at_the_valid_at_instant() {
    let w = World::new().await;
    let (a, _) = named(&w, "Bolt").await;
    // A second Episode states a plan for 2020 alone, so only the Version of
    // 2020 cites it.
    let e2 = w.episode(at(2024, 2, 1), None).await;
    w.write(
        a,
        e2,
        ValidTime::between(at(2020, 1, 1), at(2021, 1, 1)),
        json!({ "plan": "trial" }),
    )
    .await;
    let mentions = |valid_at: DateTime<Utc>| {
        let w = &w;
        async move {
            let results =
                search(w, &SearchRequest::text("bolt", AsOf::current(valid_at)), 10).await;
            assert_eq!(results.len(), 1, "{results:#?}");
            results[0].mentions.count
        }
    };
    assert_eq!(mentions(at(2020, 6, 1)).await, 2);
    assert_eq!(mentions(now()).await, 1);
}
