//! Seam 1, the vacuum call after Erasure (ADR-0003): an Erasure is complete
//! at commit, but copies of the erased bytes stay in dead tuples and index
//! pages until the tables it touched are vacuumed. `VACUUM` cannot run in a
//! transaction, so the Store offers it as a call of its own, which runs as a
//! login role that holds `gnomon_maintainer` and its `MAINTAIN` privilege.
//! These tests drive it through the Rust core's Store API against real
//! Postgres, and through the pooler in lane 4. The adversarial cases for the
//! grants live in sql_boundary.rs.
//!
//! Dead tuples are counted with `pgstattuple`, which reads the pages
//! themselves, so no statistics lag. Autovacuum is off on the gnomon tables
//! of each test's database, so only the call removes what an Erasure left.

mod support;

use std::collections::{BTreeMap, BTreeSet};

use gnomon::{Bound, BoundSide, BoundStatement, EndOfValidity, Precision, VacuumMode, ValidTime};
use serde_json::json;
use support::erasure::{at, gnomon_tables, World, SCOPE};
use tokio_postgres::types::Type;
use tokio_postgres::Client;

/// Text only the erased rows hold, so a scan of every page can find a copy.
const MARKER: &str = "vacuumed-7f3e";

/// The tables an Erasure deletes from or rewrites today: those migration
/// 0013's header lists, the token table and BM25 counts of migration 0016,
/// the full and quantized embeddings of migration 0017, and the Summaries
/// and Communities of migration 0018.
const ERASED_TABLES: &[&str] = &[
    "gnomon.bound_statement",
    "gnomon.citation",
    "gnomon.community",
    "gnomon.community_citation",
    "gnomon.community_member",
    "gnomon.contribution",
    "gnomon.edge",
    "gnomon.embedding",
    "gnomon.embedding_bit",
    "gnomon.episode",
    "gnomon.episode_retraction",
    "gnomon.erasure_reach",
    "gnomon.node",
    "gnomon.search_scope_length",
    "gnomon.search_token",
    "gnomon.search_token_frequency",
    "gnomon.sentence",
    "gnomon.stage_record",
    "gnomon.stage_usage",
    "gnomon.statement",
    "gnomon.summary",
    "gnomon.summary_citation",
    "gnomon.version",
];

/// The stage tables (migration 0014), which Erase Episode deletes the
/// Episode's rows from.
const STAGE_TABLES: &[&str] = &["gnomon.stage_record", "gnomon.stage_usage"];

/// Turns autovacuum off on every gnomon table of this database and installs
/// `pgstattuple`, so a test sees the dead tuples an Erasure leaves until the
/// call removes them.
async fn hold_dead_tuples(admin: &Client) {
    admin
        .batch_execute("create extension if not exists pgstattuple")
        .await
        .expect("pgstattuple is on both images");
    for table in gnomon_tables(admin).await {
        admin
            .batch_execute(&format!(
                "alter table gnomon.{table} set (autovacuum_enabled = false, \
                 toast.autovacuum_enabled = false)"
            ))
            .await
            .unwrap();
    }
}

/// The dead tuples in each of these tables, read from its pages.
async fn dead_tuples(admin: &Client, tables: &[String]) -> BTreeMap<String, i64> {
    let mut out = BTreeMap::new();
    for table in tables {
        let row = admin
            .query_typed(
                "select dead_tuple_count from pgstattuple($1::text::regclass)",
                &[(table, Type::TEXT)],
            )
            .await
            .unwrap();
        out.insert(table.clone(), row[0].get(0));
    }
    out
}

/// An Episode that states a plan, ends it, bounds a Version, writes an Edge
/// with a Sentence and has stage records with usage counts, beside another
/// Episode whose values stay. Returns the Episode to erase.
async fn an_episode_written_everywhere(w: &World) -> gnomon::EpisodeId {
    let e = w
        .episode_saying(at(2024, 1, 1), None, &format!("{MARKER} said so"))
        .await;
    let f = w.episode(at(2024, 1, 2), None).await;
    let acct = w
        .node("account", f, ValidTime::UNKNOWN, json!({ "plan": "basic" }))
        .await;
    w.write(
        acct,
        e,
        ValidTime::between(at(2024, 3, 1), at(2024, 9, 1)),
        json!({ "plan": MARKER }),
    )
    .await;
    let acme = w
        .node("company", f, ValidTime::UNKNOWN, json!({ "name": "Acme" }))
        .await;
    let owns = w
        .edge(acct, acme, e, ValidTime::UNKNOWN, json!({ "share": 7 }))
        .await;
    let edge_version = w.current_edge_version(owns, at(2024, 4, 1)).await;
    w.store_sentence(
        edge_version,
        &format!("{MARKER}: the account owns 7% of Acme"),
    )
    .await;
    w.caller()
        .end_node_validity(
            w.tenant,
            acct,
            &EndOfValidity {
                episode: e,
                at: Bound::At(at(2025, 1, 1)),
            },
        )
        .await
        .unwrap();
    let version = w.read(acct, at(2024, 1, 1)).await.unwrap().id;
    w.caller()
        .state_bound(
            w.tenant,
            acct,
            &BoundStatement {
                episode: e,
                version,
                side: BoundSide::Start,
                bound: Bound::Period {
                    at: at(2020, 1, 1),
                    precision: Precision::Year,
                },
            },
        )
        .await
        .unwrap();
    w.write_stage_records(SCOPE, e, &format!("{MARKER}-model"))
        .await;
    // A Summary of the account and a Community of it and Acme, each citing E
    // (migration 0018).
    w.caller()
        .write_summary(
            w.tenant,
            &gnomon::NewSummary {
                scope: gnomon::Scope::new(SCOPE),
                node: acct,
                content: format!("{MARKER}: the account's plan"),
                as_of_valid: at(2024, 4, 1),
                citations: vec![gnomon::DerivedCitation::of(e)],
            },
        )
        .await
        .unwrap();
    w.caller()
        .write_community(
            w.tenant,
            &gnomon::NewCommunity {
                scope: gnomon::Scope::new(SCOPE),
                replaces: vec![],
                members: vec![acct, acme],
                content: format!("{MARKER}: the account and Acme"),
                as_of_valid: at(2024, 4, 1),
                citations: vec![gnomon::DerivedCitation::of(e)],
            },
        )
        .await
        .unwrap();
    // The embedding of every Version's values (ADR-0010).
    let mut rng = support::vectors::Rng::new(13);
    for row in
        w.db.admin()
            .await
            .query_typed(
                "select id, text_hash from gnomon.version where scope = $1",
                &[(&SCOPE, tokio_postgres::types::Type::TEXT)],
            )
            .await
            .unwrap()
    {
        w.caller()
            .store_embedding(
                w.tenant,
                gnomon::VersionId(row.get(0)),
                row.get(1),
                &rng.unit(),
            )
            .await
            .unwrap();
    }
    e
}

#[tokio::test]
async fn the_dead_tuples_an_episode_erasure_leaves_are_gone_after_the_call() {
    let w = World::new().await;
    let admin = w.db.admin().await;
    hold_dead_tuples(&admin).await;
    let e = an_episode_written_everywhere(&w).await;

    let erasure = w.erase(e).await;

    let before = dead_tuples(&admin, &erasure.touched_tables).await;
    assert!(
        before["gnomon.version"] > 0 && before["gnomon.episode"] > 0,
        "the Erasure leaves dead tuples: {before:?}"
    );
    for table in STAGE_TABLES {
        assert!(
            before[*table] > 0,
            "the Erasure leaves dead stage rows: {before:?}"
        );
    }
    println!("dead tuples after the Erasure: {before:?}");

    let maintenance = w.db.maintenance_store().await;
    let vacuum = maintenance
        .vacuum_erasure(&erasure.touched_tables, VacuumMode::Plain)
        .await
        .expect("the vacuum call runs as the maintenance role");

    assert_eq!(vacuum.tables, erasure.touched_tables);
    assert!(vacuum.finished_at >= erasure.erased_at);
    let after = dead_tuples(&admin, &erasure.touched_tables).await;
    println!("dead tuples after the call: {after:?}");
    assert!(after.values().all(|&n| n == 0), "{after:?}");
}

#[tokio::test]
async fn the_call_after_erasures_of_every_kind_vacuums_every_table_they_touched() {
    let w = World::new().await;
    let admin = w.db.admin().await;
    hold_dead_tuples(&admin).await;
    let mut touched = BTreeSet::new();

    // Erase Episode, of an Episode Retract Episode retracted first, so its
    // Retraction mark goes too.
    let e = an_episode_written_everywhere(&w).await;
    w.caller().retract_episode(w.tenant, e).await.unwrap();
    touched.extend(w.erase(e).await.touched_tables);

    // A Node joined by an Edge of billing, a Scope the worker cannot see, so
    // the Erasure reaches it through `gnomon.erasure_reach`.
    let f = w.episode(at(2024, 2, 1), None).await;
    let billing = w.episode_in("billing", at(2024, 2, 2), "billing's").await;
    let x = w
        .node("account", f, ValidTime::UNKNOWN, json!({ "plan": MARKER }))
        .await;
    let acme = w
        .node_in("billing", "company", billing, json!({ "name": "Acme" }))
        .await;
    w.edge_in("billing", x, acme, billing, json!({ "share": 3 }))
        .await;
    let node_erasure = w.erase_node(x).await;
    let reach = dead_tuples(&admin, &["gnomon.erasure_reach".to_owned()]).await;
    assert!(
        reach["gnomon.erasure_reach"] > 0,
        "the reach list's dead rows hold the erased IDs: {reach:?}"
    );
    touched.extend(node_erasure.touched_tables);

    // An Edge, and a Scope with its Episodes.
    let y = w
        .node("account", f, ValidTime::UNKNOWN, json!({ "plan": "basic" }))
        .await;
    let globex = w
        .node(
            "company",
            f,
            ValidTime::UNKNOWN,
            json!({ "name": "Globex" }),
        )
        .await;
    let owns = w
        .edge(y, globex, f, ValidTime::UNKNOWN, json!({ "share": 9 }))
        .await;
    touched.extend(w.erase_edge(owns).await.touched_tables);
    let hr = w.episode_in("hr", at(2024, 2, 3), "hr's").await;
    w.node_in("hr", "account", hr, json!({ "plan": MARKER }))
        .await;
    touched.extend(w.erase_scope("hr").await.touched_tables);

    let maintenance = w.db.maintenance_store().await;
    let vacuum = maintenance
        .vacuum_erasures(VacuumMode::Plain)
        .await
        .expect("the vacuum call runs as the maintenance role");

    let observed: Vec<&str> = touched.iter().map(String::as_str).collect();
    for table in ERASED_TABLES {
        assert!(
            observed.contains(table),
            "no Erasure touched {table}: {observed:?}"
        );
    }
    for table in &touched {
        assert!(
            vacuum.tables.contains(table),
            "an Erasure touched {table}, which the call does not vacuum: {:?}",
            vacuum.tables
        );
    }
    let after = dead_tuples(&admin, &vacuum.tables).await;
    println!("dead tuples after the call: {after:?}");
    assert!(after.values().all(|&n| n == 0), "{after:?}");
}

/// A table a later migration adds, which Erasure deletes from by a trigger
/// on the Episode, and a table whose rows go with it by a cascading foreign
/// key. The migration grants what migrations 0011 and 0013 tell it to:
/// DELETE to the definer on the first, and MAINTAIN on both.
async fn a_later_table_erasure_deletes_from(w: &World, grant_maintain: bool) {
    let operator = w.db.operator().await;
    operator
        .batch_execute(
            "create table gnomon.later_summary (
               tenant_id uuid not null, episode_id uuid not null, body text not null,
               primary key (tenant_id, episode_id));
             create table gnomon.later_summary_token (
               tenant_id uuid not null, episode_id uuid not null, token text not null,
               foreign key (tenant_id, episode_id)
                 references gnomon.later_summary (tenant_id, episode_id) on delete cascade);
             create function gnomon.erase_later_summary() returns trigger
               language plpgsql set search_path = '' as $$
               begin
                 delete from gnomon.later_summary s
                 where s.tenant_id = old.tenant_id and s.episode_id = old.id;
                 return old;
               end $$;
             create trigger erase_later_summary after delete on gnomon.episode
               for each row execute function gnomon.erase_later_summary();
             grant select, delete on gnomon.later_summary to gnomon_definer;",
        )
        .await
        .unwrap();
    if grant_maintain {
        operator
            .batch_execute(
                "grant maintain on gnomon.later_summary, gnomon.later_summary_token
                   to gnomon_maintainer",
            )
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn a_table_a_later_migration_adds_is_vacuumed_with_no_change_to_the_call() {
    let w = World::new().await;
    a_later_table_erasure_deletes_from(&w, true).await;
    let admin = w.db.admin().await;
    hold_dead_tuples(&admin).await;
    let e = w.episode_saying(at(2024, 1, 1), None, MARKER).await;
    w.node("account", e, ValidTime::UNKNOWN, json!({ "plan": MARKER }))
        .await;
    for table in ["later_summary", "later_summary_token"] {
        admin
            .query_typed(
                &format!("insert into gnomon.{table} values ($1, $2, $3)"),
                &[
                    (&w.tenant.0, Type::UUID),
                    (&e.0, Type::UUID),
                    (&MARKER, Type::TEXT),
                ],
            )
            .await
            .unwrap();
    }

    let erasure = w.erase(e).await;

    let later = [
        "gnomon.later_summary".to_owned(),
        "gnomon.later_summary_token".to_owned(),
    ];
    for table in &later {
        assert!(
            erasure.touched_tables.contains(table),
            "the Erasure's result names {table}: {:?}",
            erasure.touched_tables
        );
    }
    let maintenance = w.db.maintenance_store().await;
    let all = maintenance
        .vacuum_erasures(VacuumMode::Plain)
        .await
        .unwrap();
    for table in &later {
        assert!(all.tables.contains(table), "{table}: {:?}", all.tables);
    }
    let after = dead_tuples(&admin, &later).await;
    assert!(after.values().all(|&n| n == 0), "{after:?}");
}

#[tokio::test]
async fn the_call_refuses_and_vacuums_nothing_when_a_table_lacks_maintain() {
    let w = World::new().await;
    a_later_table_erasure_deletes_from(&w, false).await;
    let admin = w.db.admin().await;
    hold_dead_tuples(&admin).await;
    let e = an_episode_written_everywhere(&w).await;
    let erasure = w.erase(e).await;
    let maintenance = w.db.maintenance_store().await;

    let refused = maintenance
        .vacuum_erasures(VacuumMode::Plain)
        .await
        .expect_err("a table the call must vacuum lacks MAINTAIN");

    match &refused {
        gnomon::Error::VacuumRefused(m) => assert!(
            m.contains("gnomon.later_summary") && m.contains("gnomon.later_summary_token"),
            "{m}"
        ),
        other => panic!("{other:?}"),
    }
    let left = dead_tuples(&admin, &erasure.touched_tables).await;
    assert!(left["gnomon.version"] > 0, "nothing was vacuumed: {left:?}");
}

#[tokio::test]
async fn the_call_refuses_without_maintain_on_a_stage_table_and_vacuums_the_stage_tables_with_it() {
    let w = World::new().await;
    let admin = w.db.admin().await;
    hold_dead_tuples(&admin).await;
    let e = an_episode_written_everywhere(&w).await;
    let erasure = w.erase(e).await;
    let maintenance = w.db.maintenance_store().await;

    // Without the grant the call refuses, naming the stage table.
    let operator = w.db.operator().await;
    operator
        .batch_execute("revoke maintain on gnomon.stage_record from gnomon_maintainer")
        .await
        .unwrap();
    match maintenance.vacuum_erasures(VacuumMode::Plain).await {
        Err(gnomon::Error::VacuumRefused(m)) => assert!(m.contains("gnomon.stage_record"), "{m}"),
        other => panic!("the call ran without MAINTAIN on gnomon.stage_record: {other:?}"),
    }
    let left = dead_tuples(&admin, &[STAGE_TABLES[0].to_owned()]).await;
    assert!(left[STAGE_TABLES[0]] > 0, "nothing was vacuumed: {left:?}");

    // With the migration's grant it runs and leaves no dead stage row.
    operator
        .batch_execute("grant maintain on gnomon.stage_record to gnomon_maintainer")
        .await
        .unwrap();
    let vacuum = maintenance
        .vacuum_erasure(&erasure.touched_tables, VacuumMode::Plain)
        .await
        .unwrap();
    let stage: Vec<String> = STAGE_TABLES.iter().map(|t| t.to_string()).collect();
    for table in &stage {
        assert!(
            vacuum.tables.contains(table),
            "{table}: {:?}",
            vacuum.tables
        );
    }
    let after = dead_tuples(&admin, &stage).await;
    assert!(after.values().all(|&n| n == 0), "{after:?}");
}

#[tokio::test]
async fn the_call_refuses_a_table_no_erasure_touches_and_a_role_without_the_maintenance_role() {
    let w = World::new().await;
    let maintenance = w.db.maintenance_store().await;
    for table in [
        "gnomon.kind",
        "pg_catalog.pg_class",
        "gnomon.version; select 1",
    ] {
        let refused = maintenance
            .vacuum_erasure(&[table.to_owned()], VacuumMode::Plain)
            .await
            .expect_err(table);
        assert!(
            matches!(&refused, gnomon::Error::VacuumRefused(m) if m.contains(table)),
            "{table}: {refused:?}"
        );
    }

    // The Store's own login role holds the caller roles and not
    // gnomon_maintainer, so it may not even list the tables.
    let refused = w
        .store
        .vacuum_erasures(VacuumMode::Plain)
        .await
        .expect_err("the caller login role does not hold gnomon_maintainer");
    assert_eq!(
        refused.sqlstate(),
        Some(&tokio_postgres::error::SqlState::INSUFFICIENT_PRIVILEGE),
        "{refused:?}"
    );
}

/// The pages of every relation of these tables that hold `marker`: the
/// table, its TOAST table and every index of either.
async fn pages_holding(admin: &Client, tables: &[String], marker: &str) -> Vec<String> {
    admin
        .batch_execute("create extension if not exists pageinspect")
        .await
        .expect("pageinspect is on both images");
    let mut out = Vec::new();
    for table in tables {
        let rows = admin
            .query_typed(
                "with t as (select $1::text::regclass::oid as oid),
                 rels as (
                   select t.oid from t
                   union all select c.reltoastrelid from pg_class c, t
                     where c.oid = t.oid and c.reltoastrelid <> 0
                 )
                 select r.oid::regclass::text, b
                 from (select oid from rels
                       union all select i.indexrelid from pg_index i join rels on i.indrelid = rels.oid) r,
                      generate_series(0, pg_relation_size(r.oid) / current_setting('block_size')::int - 1) b
                 where position(convert_to($2, 'UTF8') in get_raw_page(r.oid::regclass::text, b::int)) > 0",
                &[(table, Type::TEXT), (&marker, Type::TEXT)],
            )
            .await
            .unwrap();
        for row in rows {
            out.push(format!(
                "{} block {}",
                row.get::<_, String>(0),
                row.get::<_, i64>(1)
            ));
        }
    }
    out
}

#[tokio::test]
async fn a_plain_vacuum_may_leave_the_erased_bytes_in_free_space_and_a_full_one_leaves_none() {
    let w = World::new().await;
    let admin = w.db.admin().await;
    hold_dead_tuples(&admin).await;
    // Rows that stay, written first, then a Node whose rows are the newest
    // on their pages. Erasing it deletes and writes nothing new, so the
    // space its rows held stays free after a plain VACUUM.
    let f = w.episode(at(2024, 1, 1), None).await;
    for plan in ["basic", "gold", "silver"] {
        w.node("account", f, ValidTime::UNKNOWN, json!({ "plan": plan }))
            .await;
    }
    let x = w
        .node("account", f, ValidTime::UNKNOWN, json!({ "plan": MARKER }))
        .await;
    let erasure = w.erase_node(x).await;
    let maintenance = w.db.maintenance_store().await;
    let before = pages_holding(&admin, &erasure.touched_tables, MARKER).await;
    assert!(!before.is_empty(), "the dead tuples hold the marker");

    let plain = maintenance
        .vacuum_erasure(&erasure.touched_tables, VacuumMode::Plain)
        .await
        .unwrap();
    assert_eq!(
        dead_tuples(&admin, &plain.tables)
            .await
            .values()
            .sum::<i64>(),
        0
    );
    let after_plain = pages_holding(&admin, &plain.tables, MARKER).await;
    println!("pages holding the marker after the plain call: {after_plain:?}");
    assert!(
        !after_plain.is_empty(),
        "a plain VACUUM frees the space and does not overwrite it. If this fails, \
         Postgres now overwrites freed space, and VacuumMode::Full may no longer be \
         needed to leave no copy on a page"
    );

    let full = maintenance
        .vacuum_erasure(&erasure.touched_tables, VacuumMode::Full)
        .await
        .unwrap();

    assert_eq!(full.mode, VacuumMode::Full);
    let after_full = pages_holding(&admin, &full.tables, MARKER).await;
    assert!(after_full.is_empty(), "{after_full:?}");
}
