//! Seam 1, the full-text branch of Search (ADR-0010): query text at a
//! valid-at and a known-at instant returns Versions ranked by BM25, filtered
//! by Tenant, Scope and the As-of instants, through the Rust core's Store API
//! against real Postgres. The adversarial cases live in sql_boundary.rs.

mod support;

use gnomon::{AsOf, Caller, FullTextCandidate, KindOf, NewKind, ValidTime, Versioned};
use gnomon::{NewEdge, NewNode, Patch, Scope};
use serde_json::json;
use support::erasure::{SCOPE, World, assert_search_counts_match_the_token_rows, at, values};
use tokio_postgres::Client;
use tokio_postgres::types::Type;

/// Registers the Kinds these tests search, beside the `account`, `company`
/// and `owns` of the shared World:
///
/// - `customer`, a Node Kind whose `plan` is searchable and whose `code` is
///   not;
/// - `partner`, below `customer`, which inherits that mark and adds `motto`;
/// - `supplies`, an Edge Kind from `customer` to `company` whose `terms` are
///   searchable and whose `note` is not.
async fn searchable_kinds(w: &World) {
    for (name, of, parent, properties, search) in [
        (
            "customer",
            KindOf::Node,
            None,
            json!({ "plan": { "type": "string" }, "code": { "type": "string" } }),
            vec!["plan"],
        ),
        (
            "partner",
            KindOf::Node,
            Some("customer"),
            json!({ "motto": { "type": "string" } }),
            vec!["motto"],
        ),
        (
            "supplies",
            KindOf::Edge {
                source: vec!["customer".into()],
                target: vec!["company".into()],
            },
            None,
            json!({ "terms": { "type": "string" }, "note": { "type": "string" } }),
            vec!["terms"],
        ),
    ] {
        w.caller()
            .register_kind(&NewKind {
                tenant: Some(w.tenant),
                name: name.into(),
                of,
                parent: parent.map(Into::into),
                properties: json!({ "type": "object", "properties": properties }),
                search: search.into_iter().map(Into::into).collect(),
            })
            .await
            .unwrap();
    }
}

/// What a caller finds for `query`, As-of `as_of`, in rank order.
async fn found(caller: &Caller<'_>, w: &World, query: &str, as_of: AsOf) -> Vec<Versioned> {
    candidates(caller, w, query, as_of)
        .await
        .into_iter()
        .map(|c| c.of)
        .collect()
}

async fn candidates(
    caller: &Caller<'_>,
    w: &World,
    query: &str,
    as_of: AsOf,
) -> Vec<FullTextCandidate> {
    let hits = caller
        .search_full_text(w.tenant, query, as_of, 50)
        .await
        .unwrap();
    for (i, c) in hits.iter().enumerate() {
        assert_eq!(c.rank, i as i64 + 1, "ranks run from 1 in order");
    }
    hits
}

fn now() -> AsOf {
    AsOf::current(at(2024, 6, 1))
}

#[tokio::test]
async fn a_node_is_found_by_a_word_of_its_name() {
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    let ada = w
        .node(
            "account",
            e,
            ValidTime::UNKNOWN,
            json!({ "name": "Ada Lovelace" }),
        )
        .await;

    let found = w
        .caller()
        .search_full_text(w.tenant, "lovelace", now(), 10)
        .await
        .unwrap();

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].of, Versioned::Node(ada));
    assert_eq!(found[0].rank, 1);
    assert!(found[0].score > 0.0);
}

#[tokio::test]
async fn the_score_is_bm25_over_the_scope() {
    // Two Nodes of two words each. For "alpha": N = 2, df = 1, so
    // idf = ln(1 + (2 - 1 + 0.5) / (1 + 0.5)) = ln 2; tf = 1 and the length is
    // the mean, so the word's part is 1 * 2.2 / (1 + 1.2) = 1. The score is ln 2.
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    let a = w
        .node(
            "company",
            e,
            ValidTime::UNKNOWN,
            json!({ "name": "alpha beta" }),
        )
        .await;
    w.node(
        "company",
        e,
        ValidTime::UNKNOWN,
        json!({ "name": "gamma delta" }),
    )
    .await;

    let found = candidates(&w.caller(), &w, "Alpha", now()).await;

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].of, Versioned::Node(a));
    assert!(
        (found[0].score - std::f64::consts::LN_2).abs() < 1e-12,
        "score {} is not ln 2",
        found[0].score
    );
}

#[tokio::test]
async fn only_what_a_kind_marks_as_searchable_is_indexed() {
    let w = World::new().await;
    searchable_kinds(&w).await;
    let e = w.episode(at(2024, 1, 1), None).await;
    let customer = w
        .node(
            "customer",
            e,
            ValidTime::UNKNOWN,
            json!({ "name": "Northwind", "plan": "platinum", "code": "zx81" }),
        )
        .await;
    let partner = w
        .node(
            "partner",
            e,
            ValidTime::UNKNOWN,
            json!({ "name": "Contoso", "aliases": ["Contoso Ltd", "CTS"],
                    "plan": "bronze", "code": "qq42", "motto": "always shipping" }),
        )
        .await;
    // `account` marks nothing of its own, so its `plan` is not searched.
    w.node(
        "account",
        e,
        ValidTime::UNKNOWN,
        json!({ "name": "Fabrikam", "plan": "platinum" }),
    )
    .await;
    let acme = w
        .node("company", e, ValidTime::UNKNOWN, json!({ "name": "Acme" }))
        .await;
    let supplies = w
        .caller()
        .create_edge(
            &NewEdge {
                tenant: Some(w.tenant),
                scope: Scope::new(SCOPE),
                kind: "supplies".into(),
                source: customer,
                target: acme,
            },
            &Patch::new(
                e,
                ValidTime::UNKNOWN,
                values(json!({ "terms": "net thirty", "note": "hush" })),
            ),
        )
        .await
        .unwrap()
        .0;

    let caller = w.caller();
    // A property the Kind marks, inherited marks, the name and the Aliases.
    assert_eq!(
        found(&caller, &w, "platinum", now()).await,
        vec![Versioned::Node(customer)]
    );
    assert_eq!(
        found(&caller, &w, "bronze", now()).await,
        vec![Versioned::Node(partner)]
    );
    assert_eq!(
        found(&caller, &w, "shipping", now()).await,
        vec![Versioned::Node(partner)]
    );
    assert_eq!(
        found(&caller, &w, "cts", now()).await,
        vec![Versioned::Node(partner)]
    );
    assert_eq!(
        found(&caller, &w, "northwind", now()).await,
        vec![Versioned::Node(customer)]
    );
    assert_eq!(
        found(&caller, &w, "thirty", now()).await,
        vec![Versioned::Edge(supplies)]
    );
    // Properties no Kind marks.
    for word in ["zx81", "qq42", "hush"] {
        assert_eq!(found(&caller, &w, word, now()).await, vec![], "{word}");
    }
    // Property names are not text.
    assert_eq!(found(&caller, &w, "plan", now()).await, vec![]);
}

#[tokio::test]
async fn an_edge_version_is_found_by_its_sentence_once_one_is_stored() {
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    let acct = w
        .node("account", e, ValidTime::UNKNOWN, json!({ "name": "Ada" }))
        .await;
    let acme = w
        .node("company", e, ValidTime::UNKNOWN, json!({ "name": "Acme" }))
        .await;
    let owns = w
        .edge(acct, acme, e, ValidTime::UNKNOWN, json!({ "share": 7 }))
        .await;
    let caller = w.caller();
    assert_eq!(found(&caller, &w, "holds", now()).await, vec![]);

    let version = w.current_edge_version(owns, at(2024, 6, 1)).await;
    w.store_sentence(version, "Ada holds seven percent of Acme")
        .await;

    let hits = candidates(&caller, &w, "holds percent", now()).await;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].of, Versioned::Edge(owns));
    assert_eq!(hits[0].version, version);
    // The Sentence is part of the Version's text with its Kind's marks, so a
    // word of the Sentence and a name find it and the Nodes alike.
    let acme_hits = found(&caller, &w, "acme", now()).await;
    assert!(acme_hits.contains(&Versioned::Edge(owns)));
    assert!(acme_hits.contains(&Versioned::Node(acme)));
}

/// The words of a Version's token rows, read past RLS.
async fn words_of(admin: &Client, version: uuid::Uuid) -> Vec<String> {
    admin
        .query_typed(
            "select token from gnomon.search_token where version_id = $1 order by 1",
            &[(&version, Type::UUID)],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect()
}

#[tokio::test]
async fn a_batch_is_indexed_in_its_own_transaction_and_a_replaced_version_leaves_no_tokens() {
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    let a = w
        .node(
            "company",
            e,
            ValidTime::UNKNOWN,
            json!({ "name": "Initech" }),
        )
        .await;
    let b = w
        .node(
            "company",
            e,
            ValidTime::UNKNOWN,
            json!({ "name": "Globex" }),
        )
        .await;
    let admin = w.db.admin().await;

    // One transaction writes three patches. The second write to `a` replaces
    // the Version the first opened, in place (ADR-0008).
    let f = w.episode(at(2024, 2, 1), None).await;
    let outcomes = w
        .caller()
        .write_nodes(
            w.tenant,
            &[
                (
                    a,
                    Patch::new(f, ValidTime::UNKNOWN, values(json!({ "name": "Initrode" }))),
                ),
                (
                    b,
                    Patch::new(
                        f,
                        ValidTime::UNKNOWN,
                        values(json!({ "name": "Globex Hooli" })),
                    ),
                ),
                (
                    a,
                    Patch::new(
                        f,
                        ValidTime::UNKNOWN,
                        values(json!({ "name": "Initech Penetrode" })),
                    ),
                ),
            ],
        )
        .await
        .unwrap();
    let caller = w.caller();
    assert_eq!(
        found(&caller, &w, "penetrode", now()).await,
        vec![Versioned::Node(a)]
    );
    assert_eq!(
        found(&caller, &w, "hooli", now()).await,
        vec![Versioned::Node(b)]
    );
    // The Version the batch replaced is gone with its words.
    assert_eq!(found(&caller, &w, "initrode", now()).await, vec![]);
    let replaced = outcomes[0].opened[0].0;
    assert!(!outcomes[2].opened.iter().any(|v| v.0 == replaced));
    assert_eq!(words_of(&admin, replaced).await, Vec::<String>::new());
    assert_eq!(
        words_of(&admin, outcomes[2].opened[0].0).await,
        vec!["initech", "penetrode"]
    );
    assert_search_counts_match_the_token_rows(&admin).await;

    // Inside the write's transaction the rows are there already, and a
    // rolled-back write leaves none.
    let mut client = w.db.login().await;
    let tx = w.raw(&mut client).await;
    tx.query_typed(
        "select * from gnomon.write_node($1, $2, $3, null, 'unknown', null, 'unknown', $4, null)",
        &[
            (&w.tenant.0, Type::UUID),
            (&b.0, Type::UUID),
            (&f.0, Type::UUID),
            (&json!({ "name": "Vandelay" }), Type::JSONB),
        ],
    )
    .await
    .unwrap();
    let seen: i64 = tx
        .query_typed(
            "select count(*) from gnomon.search_full_text($1, 'vandelay', $2, null, 10)",
            &[
                (&w.tenant.0, Type::UUID),
                (&at(2024, 6, 1), Type::TIMESTAMPTZ),
            ],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(seen, 1);
    tx.rollback().await.unwrap();
    assert_eq!(found(&caller, &w, "vandelay", now()).await, vec![]);
    assert_search_counts_match_the_token_rows(&admin).await;
}

#[tokio::test]
async fn results_honour_the_as_of_instants() {
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    // Valid from March to June 2024.
    let n = w
        .node(
            "company",
            e,
            ValidTime::between(at(2024, 3, 1), at(2024, 6, 1)),
            json!({ "name": "Umbrella" }),
        )
        .await;
    let first = w.read(n, at(2024, 4, 1)).await.unwrap();
    // Renamed later, over the same Valid time.
    let f = w.episode(at(2024, 2, 1), None).await;
    let renamed_at = w
        .write(
            n,
            f,
            ValidTime::between(at(2024, 3, 1), at(2024, 6, 1)),
            json!({ "name": "Tricell" }),
        )
        .await;
    let caller = w.caller();

    // Valid time: inside the Version's Valid time and outside it.
    assert_eq!(
        found(&caller, &w, "tricell", AsOf::current(at(2024, 4, 1))).await,
        vec![Versioned::Node(n)]
    );
    assert_eq!(
        found(&caller, &w, "tricell", AsOf::current(at(2024, 7, 1))).await,
        vec![]
    );
    assert_eq!(
        found(&caller, &w, "tricell", AsOf::current(at(2024, 2, 1))).await,
        vec![]
    );
    // Recorded time: the current belief no longer holds the first name, a
    // known-at before the rename does, and one before the first write holds
    // neither.
    assert_eq!(
        found(&caller, &w, "umbrella", AsOf::current(at(2024, 4, 1))).await,
        vec![]
    );
    let before_rename = AsOf::at(at(2024, 4, 1), first.recorded_from);
    let hits = candidates(&caller, &w, "umbrella", before_rename).await;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].version, first.id);
    assert_eq!(found(&caller, &w, "tricell", before_rename).await, vec![]);
    assert_eq!(
        found(&caller, &w, "tricell", AsOf::at(at(2024, 4, 1), renamed_at)).await,
        vec![Versioned::Node(n)]
    );
    let before_all = AsOf::at(
        at(2024, 4, 1),
        first.recorded_from - chrono::Duration::seconds(1),
    );
    assert_eq!(found(&caller, &w, "umbrella", before_all).await, vec![]);
}

#[tokio::test]
async fn a_version_written_in_the_same_session_is_found_before_any_embedding_exists() {
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    // One Store, one connection: the write commits, and the next call on the
    // same session finds it. Nothing has stored an embedding: no gnomon table
    // with a pgvector column holds a row.
    let caller = w.caller();
    let (n, _) = caller
        .create_node(
            &NewNode {
                tenant: Some(w.tenant),
                scope: Scope::new(SCOPE),
                kind: "company".into(),
            },
            &Patch::new(
                e,
                ValidTime::UNKNOWN,
                values(json!({ "name": "Cyberdyne" })),
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        found(&caller, &w, "cyberdyne", now()).await,
        vec![Versioned::Node(n)]
    );
    let admin = w.db.admin().await;
    let vector_tables: Vec<String> = admin
        .query_typed(
            "select distinct c.relname::text from pg_attribute a
             join pg_class c on c.oid = a.attrelid
             join pg_namespace n on n.oid = c.relnamespace
             join pg_type t on t.oid = a.atttypid
             join pg_extension x on x.extname = 'vector' and t.typnamespace = x.extnamespace
             where n.nspname = 'gnomon' and c.relkind = 'r' and not a.attisdropped",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    for table in vector_tables {
        let rows: i64 = admin
            .query_typed(&format!("select count(*) from gnomon.{table}"), &[])
            .await
            .unwrap()[0]
            .get(0);
        assert_eq!(rows, 0, "gnomon.{table} holds an embedding");
    }
}

#[tokio::test]
async fn ranking_does_not_change_when_a_scope_outside_the_callers_set_gains_the_query_terms() {
    let w = World::new().await;
    // In the caller's Scope, "alpha" is rarer than "beta", so the Node that
    // says "alpha" ranks first for "alpha beta".
    let e = w.episode(at(2024, 1, 1), None).await;
    let alpha = w
        .node(
            "company",
            e,
            ValidTime::UNKNOWN,
            json!({ "name": "alpha one" }),
        )
        .await;
    let beta = w
        .node(
            "company",
            e,
            ValidTime::UNKNOWN,
            json!({ "name": "beta two" }),
        )
        .await;
    w.node(
        "company",
        e,
        ValidTime::UNKNOWN,
        json!({ "name": "beta three" }),
    )
    .await;
    let caller = w.caller();
    let before = candidates(&caller, &w, "alpha beta", now()).await;
    assert_eq!(before[0].of, Versioned::Node(alpha));
    assert!(before.iter().any(|c| c.of == Versioned::Node(beta)));

    // Another Scope of the Tenant gains many Versions that say "alpha",
    // longer ones and shorter ones, which would make "alpha" common and the
    // mean length different if its counts were summed in.
    let g = w
        .episode_in("billing", at(2024, 1, 1), "billing notes")
        .await;
    for i in 0..12 {
        let name = if i % 2 == 0 {
            "alpha"
        } else {
            "alpha alpha alpha and more words here"
        };
        w.node_in("billing", "company", g, json!({ "name": name }))
            .await;
    }

    let after = candidates(&caller, &w, "alpha beta", now()).await;
    assert_eq!(after, before, "the order and every score are unchanged");
    // A caller who sees both Scopes ranks with both, so the counts do differ.
    let both = candidates(&w.seeing(&[SCOPE, "billing"]), &w, "alpha beta", now()).await;
    let alpha_both = both
        .iter()
        .find(|c| c.of == Versioned::Node(alpha))
        .unwrap();
    assert!(alpha_both.score < before[0].score);
    assert_search_counts_match_the_token_rows(&w.db.admin().await).await;
}

/// The BM25 counts as stored, read past RLS.
async fn counts(admin: &Client) -> (Vec<String>, Vec<String>) {
    let frequencies = admin
        .query_typed(
            "select format('%s %s %s', scope, token, versions)
             from gnomon.search_token_frequency order by 1",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    let lengths = admin
        .query_typed(
            "select format('%s %s %s', scope, versions, words)
             from gnomon.search_scope_length order by 1",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    (frequencies, lengths)
}

#[tokio::test]
async fn erasing_an_episode_removes_its_words_and_their_counts() {
    let w = World::new().await;
    let f = w.episode(at(2024, 1, 1), None).await;
    let hooli = w
        .node("company", f, ValidTime::UNKNOWN, json!({ "name": "Hooli" }))
        .await;
    let admin = w.db.admin().await;
    let before = counts(&admin).await;

    // E renames Hooli over part of its Valid time, which closes F's Version,
    // and describes a company of its own.
    let e = w.episode(at(2024, 2, 1), None).await;
    w.write(
        hooli,
        e,
        ValidTime::between(at(2024, 3, 1), at(2024, 6, 1)),
        json!({ "name": "Hooli Xyzzy" }),
    )
    .await;
    let only_e = w
        .node("company", e, ValidTime::UNKNOWN, json!({ "name": "Plugh" }))
        .await;
    let caller = w.caller();
    assert_eq!(
        found(&caller, &w, "xyzzy", AsOf::current(at(2024, 4, 1))).await,
        vec![Versioned::Node(hooli)]
    );
    assert_eq!(
        found(&caller, &w, "plugh", now()).await,
        vec![Versioned::Node(only_e)]
    );

    w.erase(e).await;

    // Nothing E said is found, at any known-at instant, and the counts are
    // what they were before E: its words are gone from them too.
    assert_eq!(
        found(&caller, &w, "xyzzy", AsOf::current(at(2024, 4, 1))).await,
        vec![]
    );
    assert_eq!(found(&caller, &w, "plugh", now()).await, vec![]);
    assert_eq!(counts(&admin).await, before);
    assert_search_counts_match_the_token_rows(&admin).await;
    // Hooli's Version from before E holds again, and is found by its name.
    let hits = candidates(&caller, &w, "hooli", AsOf::current(at(2024, 4, 1))).await;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].of, Versioned::Node(hooli));
    assert_eq!(
        hits[0].version,
        w.read(hooli, at(2024, 4, 1)).await.unwrap().id
    );
}

#[tokio::test]
async fn erasing_a_node_lowers_the_counts_of_a_scope_the_caller_cannot_see_and_an_erased_scope_leaves_none()
 {
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    let acct = w
        .node("account", e, ValidTime::UNKNOWN, json!({ "name": "Wonka" }))
        .await;
    // HR's Edge joins the worker's account to an HR company. The worker
    // cannot see HR.
    let h = w.episode_in("hr", at(2024, 1, 1), "hr notes").await;
    let hr_company = w
        .node_in("hr", "company", h, json!({ "name": "Slugworth" }))
        .await;
    let hr_edge = w
        .edge_in("hr", acct, hr_company, h, json!({ "share": 3 }))
        .await;
    let hr_version = w.versions_of(&[hr_edge.0]).await[0];
    w.store_sentence_in("hr", hr_version, "Wonka holds three percent of Slugworth")
        .await;
    let hr = w.seeing(&["hr"]);
    assert_eq!(
        found(&hr, &w, "percent", now()).await,
        vec![Versioned::Edge(hr_edge)]
    );

    w.erase_node(acct).await;

    // The Edge went with the Node, and HR's counts no longer hold its words.
    assert_eq!(found(&hr, &w, "percent", now()).await, vec![]);
    let admin = w.db.admin().await;
    assert_search_counts_match_the_token_rows(&admin).await;
    let hr_counts: Vec<String> = counts(&admin)
        .await
        .0
        .into_iter()
        .filter(|c| c.starts_with("hr "))
        .collect();
    assert_eq!(hr_counts, vec!["hr slugworth 1"]);

    // Erasing a Scope leaves no count of it.
    w.erase_scope("hr").await;
    let (frequencies, lengths) = counts(&admin).await;
    assert!(
        frequencies.iter().all(|c| !c.starts_with("hr ")),
        "{frequencies:?}"
    );
    assert!(lengths.iter().all(|c| !c.starts_with("hr ")), "{lengths:?}");
    assert_search_counts_match_the_token_rows(&admin).await;
}

#[tokio::test]
async fn writers_in_one_scope_sharing_words_all_commit_and_the_counts_stay_exact() {
    // Four Stores, each on its own connection, write Nodes in the same Scope
    // at once. Every name shares words with the others, in a different
    // order, so their counts' rows are the same rows. The counts lock them in
    // the order of their keys, so the writers wait for each other and none
    // deadlocks. Nothing here measures time.
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    let words = ["amber", "basalt", "cobalt", "dolomite", "ember"];
    let mut writers = Vec::new();
    for i in 0..4usize {
        let url = w.db.login_url();
        let tenant = w.tenant;
        writers.push(tokio::spawn(async move {
            let store = gnomon::Store::connect(&url, &support::connect_options())
                .await
                .unwrap();
            let caller = store.as_worker(tenant, Scope::new(SCOPE), None);
            for j in 0..10usize {
                let mut name: Vec<&str> = words.to_vec();
                name.rotate_left((i + j) % words.len());
                if (i + j) % 2 == 1 {
                    name.reverse();
                }
                caller
                    .create_node(
                        &NewNode {
                            tenant: Some(tenant),
                            scope: Scope::new(SCOPE),
                            kind: "company".into(),
                        },
                        &Patch::new(
                            e,
                            ValidTime::UNKNOWN,
                            values(json!({ "name": format!("{} writer{i}", name.join(" ")) })),
                        ),
                    )
                    .await
                    .unwrap();
            }
        }));
    }
    for writer in writers {
        writer.await.unwrap();
    }
    let admin = w.db.admin().await;
    assert_search_counts_match_the_token_rows(&admin).await;
    let amber = found(&w.caller(), &w, "amber", now()).await;
    assert_eq!(amber.len(), 40);
}

/// Writes a Node's name in a raw transaction of the worker's, as the Store
/// writes one patch.
async fn write_name(
    tx: &tokio_postgres::Transaction<'_>,
    w: &World,
    episode: gnomon::EpisodeId,
    node: gnomon::NodeId,
    name: &str,
) -> Result<Vec<tokio_postgres::Row>, tokio_postgres::Error> {
    tx.query_typed(
        "select * from gnomon.write_node($1, $2, $3, null, 'unknown', null, 'unknown', $4, null)",
        &[
            (&w.tenant.0, Type::UUID),
            (&node.0, Type::UUID),
            (&episode.0, Type::UUID),
            (&json!({ "name": name }), Type::JSONB),
        ],
    )
    .await
}

/// Runs `statement` and fails as soon as any backend of the test's database
/// waits on a lock, as the stage tests watch a claim. It asserts no timing.
async fn without_waiting<T>(
    admin: &Client,
    statement: impl std::future::Future<Output = Result<T, tokio_postgres::Error>>,
) -> T {
    let watch = async {
        loop {
            let waiting: i64 = admin
                .query_typed(
                    "select count(*) from pg_stat_activity
                     where datname = current_database() and wait_event_type = 'Lock'",
                    &[],
                )
                .await
                .unwrap()[0]
                .get(0);
            if waiting > 0 {
                return;
            }
            tokio::task::yield_now().await;
        }
    };
    tokio::select! {
        done = statement => done.expect("the write succeeds"),
        _ = watch => panic!("a writer waited on another writer's lock"),
    }
}

#[tokio::test]
async fn two_writers_of_several_statements_in_one_scope_never_wait_on_each_others_counts() {
    // Each transaction writes two Nodes, one statement each, in the same
    // Scope, and the second Node of each says the first word of the other.
    // Counts changed statement by statement would lock the word's row and the
    // Scope's row until commit, so the two transactions would wait on each
    // other: a deadlock. The counts change at commit instead, in key order,
    // and no writer waits on the other.
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    let mut nodes = Vec::new();
    for _ in 0..4 {
        nodes.push(
            w.node("company", e, ValidTime::UNKNOWN, json!({ "name": "seed" }))
                .await,
        );
    }
    let admin = w.db.admin().await;
    let mut c1 = w.db.login().await;
    let mut c2 = w.db.login().await;
    let t1 = w.raw(&mut c1).await;
    let t2 = w.raw(&mut c2).await;
    without_waiting(&admin, write_name(&t1, &w, e, nodes[0], "xenon")).await;
    without_waiting(&admin, write_name(&t2, &w, e, nodes[1], "yttrium")).await;
    without_waiting(&admin, write_name(&t1, &w, e, nodes[2], "yttrium krypton")).await;
    without_waiting(&admin, write_name(&t2, &w, e, nodes[3], "xenon krypton")).await;
    t1.commit().await.unwrap();
    t2.commit().await.unwrap();

    assert_search_counts_match_the_token_rows(&admin).await;
    let caller = w.caller();
    assert_eq!(found(&caller, &w, "krypton", now()).await.len(), 2);
    assert_eq!(found(&caller, &w, "xenon", now()).await.len(), 2);
    assert_eq!(found(&caller, &w, "seed", now()).await, vec![]);
}
