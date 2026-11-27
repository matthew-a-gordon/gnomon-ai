//! Seam 2: the SQL boundary, adversarially.
//!
//! These tests bypass the Rust core. They connect as the login role, switch
//! to a caller role with claims set, as any SQL client could, and try to
//! cross a Tenant or Scope. Each attempt must fail: RLS, grants and invoker
//! functions are the barrier, not the Rust core (ADR-0005, ADR-0009).

mod support;

use gnomon::{EpisodeId, NewEpisode, Scope, Store, TenantId};
use serde_json::json;
use support::TestDb;
use support::erasure::BM25_COUNT_TABLES;
use support::timestamptz::Timestamptz;
use tokio_postgres::error::SqlState;
use tokio_postgres::types::Type;
use tokio_postgres::{Client, Transaction};
use uuid::Uuid;

const USER: &str = "authenticated";
const WORKER: &str = "gnomon_worker";

fn claims(tenant: Uuid, scopes: &[&str]) -> String {
    json!({ "role": "authenticated", "app_metadata": { "tenant_id": tenant.to_string(), "scopes": scopes } })
        .to_string()
}

/// Opens a transaction as `role` with `claims`, the way a caller would.
async fn as_caller<'c>(client: &'c mut Client, role: &str, claims: &str) -> Transaction<'c> {
    let tx = client.transaction().await.unwrap();
    tx.batch_execute(&format!("set local role {role}"))
        .await
        .unwrap();
    tx.query_typed(
        "select set_config('request.jwt.claims', $1, true)",
        &[(&claims, Type::TEXT)],
    )
    .await
    .unwrap();
    tx
}

fn assert_refused(result: Result<impl std::fmt::Debug, tokio_postgres::Error>, what: &str) {
    match result {
        Err(e) => assert_eq!(
            e.code(),
            Some(&SqlState::INSUFFICIENT_PRIVILEGE),
            "{what}: expected a privilege or RLS refusal, got {e:?}"
        ),
        Ok(v) => panic!("{what} was allowed: {v:?}"),
    }
}

/// Two Tenants, each with an Episode in `support` and one in `billing`.
struct World {
    db: TestDb,
    t1: Uuid,
    t2: Uuid,
    t1_support: Uuid,
    t1_billing: Uuid,
    t2_support: Uuid,
}

async fn world() -> World {
    let db = TestDb::migrated().await;
    let store = Store::connect(&db.login_url(), &support::connect_options())
        .await
        .unwrap();
    let (t1, t2) = (Uuid::new_v4(), Uuid::new_v4());
    let record = |t: Uuid, scope: &str| {
        let store = &store;
        let scope = scope.to_owned();
        async move {
            let EpisodeId(id) = store
                .as_worker(TenantId(t), Scope::new(scope.clone()), None)
                .record_episode(&NewEpisode {
                    tenant: Some(TenantId(t)),
                    scope: Scope::new(scope.clone()),
                    content: format!("{t} {scope}"),
                    reference_time: "2024-01-01T00:00:00Z".parse().unwrap(),
                    authority: None,
                })
                .await
                .unwrap();
            id
        }
    };
    let t1_support = record(t1, "support").await;
    let t1_billing = record(t1, "billing").await;
    let t2_support = record(t2, "support").await;
    record(t2, "billing").await;
    World {
        db,
        t1,
        t2,
        t1_support,
        t1_billing,
        t2_support,
    }
}

async fn visible_ids(
    tx: &Transaction<'_>,
    sql: &str,
    params: &[(&(dyn tokio_postgres::types::ToSql + Sync), Type)],
) -> Vec<Uuid> {
    tx.query_typed(sql, params)
        .await
        .unwrap()
        .iter()
        .map(|r| r.get::<_, Uuid>(0))
        .collect()
}

#[tokio::test]
async fn a_caller_selecting_the_table_directly_sees_only_its_own_tenant_and_scopes() {
    let w = world().await;
    for role in [USER, WORKER] {
        let mut client = w.db.login().await;
        let tx = as_caller(&mut client, role, &claims(w.t1, &["support"])).await;
        let ids = visible_ids(&tx, "select id from gnomon.episode", &[]).await;
        assert_eq!(ids, vec![w.t1_support], "{role} saw {ids:?}");

        let by_id = visible_ids(
            &tx,
            "select id from gnomon.episode where id = any($1)",
            &[(&vec![w.t1_billing, w.t2_support], Type::UUID_ARRAY)],
        )
        .await;
        assert!(
            by_id.is_empty(),
            "{role} read another Tenant's or Scope's Episode: {by_id:?}"
        );
    }
}

#[tokio::test]
async fn a_caller_cannot_read_another_tenant_or_scope_through_read_episode() {
    let w = world().await;
    let mut client = w.db.login().await;
    let tx = as_caller(&mut client, USER, &claims(w.t1, &["support"])).await;
    let sql = "select id from gnomon.read_episode($1, $2)";
    for (tenant, id) in [
        (w.t2, w.t2_support),
        (w.t1, w.t2_support),
        (w.t1, w.t1_billing),
    ] {
        let ids = visible_ids(&tx, sql, &[(&tenant, Type::UUID), (&id, Type::UUID)]).await;
        assert!(
            ids.is_empty(),
            "read_episode({tenant}, {id}) returned {ids:?}"
        );
    }
    let own = visible_ids(
        &tx,
        sql,
        &[(&w.t1, Type::UUID), (&w.t1_support, Type::UUID)],
    )
    .await;
    assert_eq!(own, vec![w.t1_support]);
}

#[tokio::test]
async fn a_caller_cannot_record_an_episode_into_another_tenant_or_scope_through_record_episode() {
    let w = world().await;
    let attempts = [
        (w.t2, "support", "another Tenant"),
        (w.t1, "billing", "another Scope"),
    ];
    for (tenant, scope, what) in attempts {
        let mut client = w.db.login().await;
        let tx = as_caller(&mut client, USER, &claims(w.t1, &["support"])).await;
        let call = tx
            .query_typed(
                "select gnomon.record_episode($1, $2, 'forged', now(), null)",
                &[(&tenant, Type::UUID), (&scope, Type::TEXT)],
            )
            .await;
        assert_refused(call, &format!("record_episode into {what}"));
    }
}

#[tokio::test]
async fn a_caller_with_no_claims_or_malformed_claims_sees_nothing() {
    let w = world().await;
    let no_tenant = json!({ "app_metadata": { "scopes": ["support"] } }).to_string();
    let scopes_not_a_list =
        json!({ "app_metadata": { "tenant_id": w.t1.to_string(), "scopes": "support" } })
            .to_string();
    for c in ["", "{}", no_tenant.as_str(), scopes_not_a_list.as_str()] {
        let mut client = w.db.login().await;
        let tx = as_caller(&mut client, USER, c).await;
        let ids = visible_ids(&tx, "select id from gnomon.episode", &[]).await;
        assert!(ids.is_empty(), "claims {c:?} saw {ids:?}");
    }
}

#[tokio::test]
async fn the_login_role_itself_cannot_read_or_write_episodes() {
    let w = world().await;
    let client = w.db.login().await;
    assert_refused(
        client
            .query_typed("select id from gnomon.episode", &[])
            .await,
        "the login role reading",
    );
    assert_refused(
        client
            .query_typed(
                "select gnomon.record_episode($1, 'support', 'x', now(), null)",
                &[(&w.t1, Type::UUID)],
            )
            .await,
        "the login role calling record_episode",
    );
}

#[tokio::test]
async fn a_caller_cannot_redirect_the_store_functions_through_its_search_path() {
    let w = world().await;
    let mut client = w.db.login().await;
    let tx = as_caller(&mut client, USER, &claims(w.t1, &["support"])).await;
    // Shadow the table and the helper functions in the caller's own temporary
    // schema, and put that schema first.
    tx.batch_execute(
        "create temporary table episode (tenant_id uuid, scope text, id uuid, content text,
           reference_time timestamptz, authority integer, recorded_at timestamptz);
         set local search_path = pg_temp, public;",
    )
    .await
    .unwrap();
    tx.query_typed(
        "insert into pg_temp.episode values ($1, 'support', $2, 'shadow', now(), null, now())",
        &[(&w.t1, Type::UUID), (&w.t1_billing, Type::UUID)],
    )
    .await
    .unwrap();

    let read = tx
        .query_typed(
            "select content from gnomon.read_episode($1, $2)",
            &[(&w.t1, Type::UUID), (&w.t1_billing, Type::UUID)],
        )
        .await
        .unwrap();
    assert!(
        read.is_empty(),
        "read_episode read the caller's shadow table"
    );

    let id: Uuid = tx
        .query_typed(
            "select gnomon.record_episode($1, 'support', 'real', now(), null)",
            &[(&w.t1, Type::UUID)],
        )
        .await
        .unwrap()[0]
        .get(0);
    let real = visible_ids(
        &tx,
        "select id from gnomon.episode where id = $1",
        &[(&id, Type::UUID)],
    )
    .await;
    assert_eq!(
        real,
        vec![id],
        "record_episode wrote somewhere other than gnomon.episode"
    );
}

// The catalog, read as the caller would see it. These pin the acceptance
// criteria for every table and function a later migration adds.

/// The policies' conditions on the Tenant, the readable Scopes and the
/// writable Scopes, as PostgreSQL prints them: each a sub-select (ADR-0005).
const TENANT_CONDITION: &str = "(tenant_id = ( SELECT gnomon.tenant_id() AS tenant_id))";
const READABLE_CONDITION: &str =
    "(scope = ANY (( SELECT gnomon.allowed_scopes() AS allowed_scopes)::text[]))";
const WRITABLE_CONDITION: &str =
    "(scope = ANY (( SELECT gnomon.writable_scopes() AS writable_scopes)::text[]))";

/// The Ontology's tables. They carry a Tenant and no Scope, because every
/// Scope of a Tenant shares one Ontology, and their policies check the Tenant
/// alone.
const ONTOLOGY_TABLES: &[&str] = &["kind", "kind_rule", "kind_merge"];

/// Every table has RLS and a Tenant, and every policy reads only the
/// caller's Tenant and Scopes, with two exceptions. The eraser role's policies
/// check the Tenant alone, because an Erasure removes what depends on what it
/// erases in every Scope (#83). The search index trigger's policies on the
/// BM25 counts reach every Scope whose counts its own transaction changed.
/// No other role gains reach outside the caller's Scopes.
#[tokio::test]
async fn every_gnomon_table_has_rls_and_every_policy_but_the_erasers_and_the_counts_reads_only_the_callers_scopes()
 {
    let w = world().await;
    let client = w.db.login().await;
    let tables = client
        .query_typed(
            "select c.relname, c.relrowsecurity,
                    exists (select from pg_attribute a where a.attrelid = c.oid and a.attname = 'tenant_id' and not a.attisdropped),
                    exists (select from pg_attribute a where a.attrelid = c.oid and a.attname = 'scope' and not a.attisdropped)
             from pg_class c join pg_namespace n on n.oid = c.relnamespace
             where n.nspname = 'gnomon' and c.relkind in ('r', 'p')
             order by 1",
            &[],
        )
        .await
        .unwrap();
    assert!(tables.len() >= 4);
    for t in &tables {
        let (name, rls, tenant, scope): (String, bool, bool, bool) =
            (t.get(0), t.get(1), t.get(2), t.get(3));
        assert!(rls, "gnomon.{name} has no RLS");
        if name == "schema_version" || name == "tenancy" {
            // The ledger describes the schema, and the tenancy row
            // describes the deployment. Neither holds a graph.
            continue;
        }
        assert!(tenant, "gnomon.{name} lacks a tenant_id column");
        let policies = client
            .query_typed(
                "select qual, with_check, roles::text[] from pg_policies
                 where schemaname = 'gnomon' and tablename = $1",
                &[(&name, Type::TEXT)],
            )
            .await
            .unwrap();
        assert!(!policies.is_empty(), "gnomon.{name} has no policy");
        let ontology = ONTOLOGY_TABLES.contains(&name.as_str());
        assert_eq!(
            scope, !ontology,
            "gnomon.{name}: graph tables have a scope column and Ontology tables none"
        );
        let expected = if ontology {
            TENANT_CONDITION.to_owned()
        } else {
            format!("({TENANT_CONDITION} AND {READABLE_CONDITION})")
        };
        for p in &policies {
            let (using, check, roles): (Option<String>, Option<String>, Vec<String>) =
                (p.get(0), p.get(1), p.get(2));
            assert!(
                using.is_some() || check.is_some(),
                "gnomon.{name} has a policy that checks nothing"
            );
            // The eraser, for Erasure alone: every Scope of the caller's
            // Tenant, and no other Tenant (#83). Its functions check the
            // Scopes of what they erase and of what they return.
            // The Ontology's tables check the Tenant alone for every role.
            if roles == ["gnomon_eraser"] && !ontology {
                let tenant = Some(TENANT_CONDITION.to_owned());
                assert_eq!(using, tenant, "gnomon.{name}: the eraser's USING");
                assert_eq!(check, tenant, "gnomon.{name}: the eraser's CHECK");
                continue;
            }
            // The search index function, a definer, changes the BM25 counts
            // only of a Scope whose changes its own transaction recorded,
            // and records only its own transaction's changes (migration
            // 0016). No caller writes either table.
            if BM25_COUNT_TABLES.contains(&name.as_str()) && roles == ["gnomon_definer"] {
                let pending = format!(
                    "({TENANT_CONDITION} AND (EXISTS ( SELECT\n   \
                     FROM gnomon.search_count_delta d\n  \
                     WHERE ((d.tenant_id = {name}.tenant_id) AND \
                     (d.written_in = pg_current_xact_id_if_assigned()) AND \
                     (d.scope = {name}.scope)))))"
                );
                let (own_using, own_check) = if name == "search_count_delta" {
                    (
                        format!(
                            "({TENANT_CONDITION} AND \
                             (written_in = pg_current_xact_id_if_assigned()))"
                        ),
                        format!("({TENANT_CONDITION} AND (written_in = pg_current_xact_id()))"),
                    )
                } else {
                    (pending.clone(), pending)
                };
                assert_eq!(using, Some(own_using), "gnomon.{name} the definer's USING");
                assert_eq!(check, Some(own_check), "gnomon.{name} the definer's CHECK");
                continue;
            }
            // The writer role writes only a Scope the caller may both read
            // and write, and some of its policies add a condition (#81).
            if roles == ["gnomon_writer"] {
                let writable = format!(
                    "({TENANT_CONDITION} AND {READABLE_CONDITION} AND {WRITABLE_CONDITION}"
                );
                for e in [&using, &check].into_iter().flatten() {
                    assert!(
                        e.starts_with(&writable),
                        "gnomon.{name}: the writer's policy is {e}"
                    );
                }
                continue;
            }
            if let Some(using) = using {
                assert_eq!(using, expected, "gnomon.{name} USING");
            }
            if let Some(check) = check {
                assert_eq!(check, expected, "gnomon.{name} WITH CHECK");
            }
        }
    }
}

/// The functions each deployment supplies to tell a policy who is calling
/// (ADR-0005), and whether each returns a set of Scope keys.
const POLICY_FUNCTIONS: &[(&str, bool)] = &[
    ("tenant_id", false),
    ("allowed_scopes", true),
    ("writable_scopes", true),
    ("user_id", false),
];

/// Every place a gnomon policy calls one of [`POLICY_FUNCTIONS`] other than
/// as a sub-select of its own, as `table.policy USING|CHECK: function`. A
/// call in a sub-select runs once for the statement, and a call outside one
/// runs for every row the policy checks (ADR-0005). A call inside a larger
/// sub-select counts as outside one. The Scope functions' sub-selects are
/// cast to `text[]`, the one form `any` reads as an array.
async fn policy_calls_outside_a_sub_select(
    client: &impl tokio_postgres::GenericClient,
) -> Vec<String> {
    let policies = client
        .query_typed(
            "select tablename::text, policyname::text, qual, with_check from pg_policies
             where schemaname = 'gnomon' order by 1, 2",
            &[],
        )
        .await
        .unwrap();
    assert!(policies.len() > 40, "too few policies: {}", policies.len());
    let mut found = vec![];
    for p in &policies {
        let (table, name): (String, String) = (p.get(0), p.get(1));
        for (clause, expr) in [
            ("USING", p.get::<_, Option<String>>(2)),
            ("CHECK", p.get(3)),
        ] {
            let Some(expr) = expr else { continue };
            for (function, array) in POLICY_FUNCTIONS {
                let call = format!("gnomon.{function}()");
                let sub_select = if *array {
                    format!("( SELECT {call} AS {function})::text[]")
                } else {
                    format!("( SELECT {call} AS {function})")
                };
                let calls = expr.matches(&call).count();
                let in_sub_selects = expr.matches(&sub_select).count();
                if calls != in_sub_selects {
                    found.push(format!("{table}.{name} {clause}: {function}"));
                }
            }
        }
    }
    found
}

/// Every policy calls the policy functions in sub-selects of their own
/// (ADR-0005), and the check names each policy that calls one otherwise.
#[tokio::test]
async fn every_policy_calls_the_policy_functions_in_a_sub_select() {
    let db = TestDb::migrated().await;
    let mut admin = db.admin().await;
    let tx = admin.transaction().await.unwrap();
    assert_eq!(
        policy_calls_outside_a_sub_select(&tx).await,
        Vec::<String>::new()
    );

    tx.batch_execute(
        "create policy probe_bare on gnomon.node for select to authenticated
           using (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()));
         create policy probe_one_bare on gnomon.node for insert to gnomon_writer
           with check (tenant_id = (select gnomon.tenant_id())
                       and scope = any ((select gnomon.allowed_scopes())::text[])
                       and scope = any (gnomon.writable_scopes()));
         create policy probe_correlated on gnomon.episode for select to authenticated
           using (exists (select from gnomon.node n
                          where n.tenant_id = episode.tenant_id
                            and n.scope = any (gnomon.allowed_scopes())));
         create policy probe_user on gnomon.episode for update to gnomon_writer
           using (true) with check (recorded_by = gnomon.user_id());
         create policy probe_good on gnomon.episode for delete to gnomon_writer
           using (tenant_id = (select gnomon.tenant_id())
                  and recorded_by = (select gnomon.user_id()));",
    )
    .await
    .unwrap();
    assert_eq!(
        policy_calls_outside_a_sub_select(&tx).await,
        vec![
            "episode.probe_correlated USING: allowed_scopes".to_owned(),
            "episode.probe_user CHECK: user_id".to_owned(),
            "node.probe_bare USING: tenant_id".to_owned(),
            "node.probe_bare USING: allowed_scopes".to_owned(),
            "node.probe_one_bare CHECK: writable_scopes".to_owned(),
        ]
    );
    tx.rollback().await.unwrap();
}

/// The SECURITY DEFINER functions `gnomon_definer` owns (ADR-0009): Kind
/// registration, the setting of a Kind's merge rule, and the trigger function
/// that writes full-text Search's token rows and counts.
const DEFINERS: &[&str] = &[
    "gnomon.maintain_search_index(",
    "gnomon.register_kind(",
    "gnomon.set_merge_rule(",
];

/// The SECURITY DEFINER functions `gnomon_eraser` owns: the four Erasures
/// (ADR-0009, #83).
const ERASERS: &[&str] = &[
    "gnomon.erase_episode(",
    "gnomon.erase_node(",
    "gnomon.erase_edge(",
    "gnomon.erase_scope(",
];

/// The tables of history that only Erasure deletes or rewrites. Neither
/// `gnomon_definer` nor a caller role holds a write privilege on any of them
/// (ADR-0009).
const HISTORY_TABLES: &[&str] = &[
    "gnomon.episode",
    "gnomon.episode_retraction",
    "gnomon.version",
    "gnomon.citation",
    "gnomon.contribution",
    "gnomon.statement",
    "gnomon.bound_statement",
];

/// The functions a caller writes through, each a definer owned by
/// `gnomon_writer` (#81).
const WRITERS: &[&str] = &[
    "gnomon.record_episode(uuid,text,text,timestamp with time zone,integer)",
    "gnomon.create_node(uuid,text,text)",
    "gnomon.create_edge(uuid,text,text,uuid,uuid)",
    "gnomon.write_node(uuid,uuid,uuid,timestamp with time zone,text,timestamp with time zone,text,jsonb,jsonb)",
    "gnomon.write_edge(uuid,uuid,uuid,timestamp with time zone,text,timestamp with time zone,text,jsonb,jsonb)",
    "gnomon.correct_node(uuid,uuid,uuid,timestamp with time zone,text,timestamp with time zone,text,jsonb,jsonb)",
    "gnomon.correct_edge(uuid,uuid,uuid,timestamp with time zone,text,timestamp with time zone,text,jsonb,jsonb)",
    "gnomon.state_bound(uuid,uuid,uuid,uuid,text,timestamp with time zone,text)",
    "gnomon.state_edge_bound(uuid,uuid,uuid,uuid,text,timestamp with time zone,text)",
    "gnomon.end_validity(uuid,uuid,boolean,uuid,timestamp with time zone,text)",
    "gnomon.retract_version(uuid,uuid,boolean,uuid,uuid)",
    "gnomon.retract_episode(uuid,uuid)",
    "gnomon.stage_pending(uuid,uuid,text)",
    "gnomon.claim_stage(uuid,uuid,interval)",
    "gnomon.finish_stage(uuid,uuid,text,integer,gnomon.stage_status,text,integer,interval)",
    "gnomon.add_stage_usage(uuid,uuid,text,text,integer,bigint,bigint,numeric)",
    "gnomon.write_summary(uuid,text,uuid,text,timestamp with time zone,uuid[],text[],text[])",
    "gnomon.write_community(uuid,text,uuid[],uuid[],text,timestamp with time zone,uuid[],text[],text[])",
    "gnomon.store_embedding(uuid,uuid,text,real[])",
    "gnomon.store_name_embedding(uuid,uuid,text,real[])",
    "gnomon.store_summary_embedding(uuid,uuid,real[])",
    "gnomon.store_community_embedding(uuid,uuid,real[])",
    "gnomon.store_sentence(uuid,uuid,text)",
];

/// The read functions and the helpers they call as the caller, which the
/// caller roles execute besides [`WRITERS`] (#81). Kind registration, the
/// merge-rule setter and Erasure are the deployment's to grant.
const CALLER_READS: &[&str] = &[
    "gnomon.allowed_scopes()",
    "gnomon.call_tenant(uuid)",
    "gnomon.default_tenant()",
    "gnomon.embedding_dimension()",
    "gnomon.embedding_vector(real[])",
    "gnomon.is_symmetric_kind(text)",
    "gnomon.kind_lineage(uuid,text)",
    "gnomon.kind_merge_rule(uuid,text)",
    "gnomon.list_citations(uuid,uuid)",
    "gnomon.list_gaps(uuid,text,text,uuid,text,integer)",
    "gnomon.list_node_episodes(uuid,uuid)",
    "gnomon.list_statements(uuid,uuid)",
    "gnomon.neighbourhood_edges(uuid,uuid[],timestamp with time zone,timestamp with time zone)",
    "gnomon.period_end(timestamp with time zone,gnomon.time_precision)",
    "gnomon.read_communities(uuid,uuid,timestamp with time zone,timestamp with time zone)",
    "gnomon.read_edge(uuid,uuid,timestamp with time zone,timestamp with time zone)",
    "gnomon.read_episode(uuid,uuid)",
    "gnomon.read_kind(uuid,text)",
    "gnomon.read_node(uuid,uuid,timestamp with time zone,timestamp with time zone)",
    "gnomon.read_nodes(uuid,uuid[],timestamp with time zone,timestamp with time zone)",
    "gnomon.read_stage_records(uuid,uuid)",
    "gnomon.read_stage_usage(uuid,uuid)",
    "gnomon.read_summaries(uuid,uuid,timestamp with time zone,timestamp with time zone)",
    "gnomon.reserved_kinds()",
    "gnomon.resolve(uuid,text,real[],timestamp with time zone,integer)",
    "gnomon.resolve_full_text(uuid,text,timestamp with time zone,integer)",
    "gnomon.resolve_vector(uuid,real[],timestamp with time zone,integer)",
    "gnomon.root_kinds()",
    "gnomon.search(uuid,text,real[],timestamp with time zone,timestamp with time zone,uuid[],integer,double precision,double precision,double precision,double precision,double precision,integer)",
    "gnomon.search_connectedness(uuid,uuid[],uuid[],uuid[],timestamp with time zone,timestamp with time zone)",
    "gnomon.search_full_text(uuid,text,timestamp with time zone,timestamp with time zone,integer)",
    "gnomon.search_full_text_branch(uuid,text,timestamp with time zone,timestamp with time zone,integer)",
    "gnomon.search_full_text_derived(uuid,text,timestamp with time zone,timestamp with time zone,integer)",
    "gnomon.search_mentions(uuid,uuid[],text[],uuid[],text[],timestamp with time zone,timestamp with time zone)",
    "gnomon.search_neighbourhood(uuid,uuid[],integer,timestamp with time zone,timestamp with time zone,integer)",
    "gnomon.search_query_words(uuid,text)",
    "gnomon.search_vector(uuid,real[],timestamp with time zone,timestamp with time zone,integer)",
    "gnomon.search_vector_branch(uuid,real[],timestamp with time zone,timestamp with time zone,integer)",
    "gnomon.search_vector_derived(uuid,real[],timestamp with time zone,timestamp with time zone,integer)",
    "gnomon.sentence_text_hash(text)",
    "gnomon.tenant_id()",
    "gnomon.traverse(uuid,uuid,timestamp with time zone,timestamp with time zone,text,text[])",
    "gnomon.uncertain_at(timestamp with time zone,timestamp with time zone,gnomon.time_precision)",
];

/// The writes no caller role may make on any gnomon table, or any column.
const WRITE_PRIVILEGES: &[&str] = &["INSERT", "UPDATE", "DELETE", "TRUNCATE"];

/// The table and column write privileges, and the functions, `role` holds
/// in `client`'s database, directly or through a role it belongs to.
async fn caller_rights(client: &Client, role: &str) -> (Vec<String>, Vec<String>) {
    let writes: Vec<String> = client
        .query_typed(
            "select format('%s %s', c.oid::regclass, p)
             from pg_class c, unnest($2::text[]) p
             where c.relnamespace = 'gnomon'::regnamespace and c.relkind in ('r', 'p')
               and (has_table_privilege($1, c.oid, p)
                    or (p in ('INSERT', 'UPDATE') and has_any_column_privilege($1, c.oid, p)))
             order by 1",
            &[(&role, Type::TEXT), (&WRITE_PRIVILEGES, Type::TEXT_ARRAY)],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    let mut functions: Vec<String> = client
        .query_typed(
            "select replace(p.oid::regprocedure::text, ', ', ',')
             from pg_proc p
             where p.pronamespace = 'gnomon'::regnamespace
               and has_function_privilege($1, p.oid, 'EXECUTE')",
            &[(&role, Type::TEXT)],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    functions.sort();
    (writes, functions)
}

#[tokio::test]
async fn no_caller_role_holds_a_write_privilege_and_each_executes_only_the_store_functions() {
    let mut expected: Vec<String> = WRITERS
        .iter()
        .chain(CALLER_READS)
        .map(|f| f.to_string())
        .collect();
    expected.sort();
    // Once as the files leave it, and once after the role that applies them
    // granted callers everything on every object it creates.
    let plain = TestDb::migrated().await;
    let granting = TestDb::empty_with_login().await;
    let operator = granting.operator().await;
    for objects in ["tables", "sequences", "functions", "schemas"] {
        operator
            .batch_execute(&format!(
                "alter default privileges grant all on {objects} to authenticated, gnomon_worker"
            ))
            .await
            .unwrap();
    }
    for (file, sql) in support::migration_files(support::DEFAULT_DIMENSION) {
        operator
            .batch_execute(&sql)
            .await
            .unwrap_or_else(|e| panic!("apply {file}: {e:?}"));
    }
    for db in [&plain, &granting] {
        let admin = db.admin().await;
        // The search index trigger's owner writes the search tables and the
        // Ontology, and no table of history. Only the eraser is let past the
        // triggers that check a write, so this is a privilege, not a
        // convention (ADR-0009, #83).
        let (writes, _) = caller_rights(&admin, "gnomon_definer").await;
        let history: Vec<_> = writes
            .iter()
            .filter(|w| {
                HISTORY_TABLES
                    .iter()
                    .any(|t| w.split(' ').next() == Some(t))
            })
            .collect();
        assert!(history.is_empty(), "gnomon_definer may write {history:#?}");
        for role in [USER, WORKER, support::LOGIN_ROLE] {
            let (writes, functions) = caller_rights(&admin, role).await;
            assert!(writes.is_empty(), "{role} may write: {writes:#?}");
            if role == support::LOGIN_ROLE {
                // NOINHERIT: it reaches the caller roles' rights only by
                // switching to one.
                assert!(functions.is_empty(), "{role} executes {functions:#?}");
                continue;
            }
            let extra: Vec<_> = functions.iter().filter(|f| !expected.contains(f)).collect();
            let missing: Vec<_> = expected.iter().filter(|f| !functions.contains(f)).collect();
            assert!(
                extra.is_empty() && missing.is_empty(),
                "{role} executes functions beyond the Store's: {extra:#?}\n\
                 and lacks: {missing:#?}"
            );
        }
    }
}

#[tokio::test]
async fn every_gnomon_function_runs_as_the_invoker_with_a_search_path_ending_in_pg_temp_but_the_definers_and_writers()
 {
    let w = world().await;
    let client = w.db.login().await;
    let functions = client
        .query_typed(
            "select p.oid::regprocedure::text, p.prosecdef, coalesce(p.proconfig, '{}'),
                    p.proowner::regrole::text
             from pg_proc p join pg_namespace n on n.oid = p.pronamespace
             where n.nspname = 'gnomon'",
            &[],
        )
        .await
        .unwrap();
    let names: Vec<String> = functions.iter().map(|f| f.get(0)).collect();
    for expected in [
        "gnomon.record_episode(",
        "gnomon.read_episode(",
        "gnomon.tenant_id()",
        "gnomon.allowed_scopes()",
        "gnomon.register_kind(",
        "gnomon.read_kind(",
        "gnomon.create_node(",
        "gnomon.write_node(",
        "gnomon.state_bound(",
        "gnomon.set_merge_rule(",
        "gnomon.kind_merge_rule(",
        "gnomon.read_node(",
        "gnomon.read_nodes(",
        "gnomon.create_edge(",
        "gnomon.write_edge(",
        "gnomon.state_edge_bound(",
        "gnomon.read_edge(",
        "gnomon.traverse(",
        "gnomon.list_citations(",
        "gnomon.list_node_episodes(",
        "gnomon.default_tenant()",
        "gnomon.call_tenant(",
        "gnomon.switch_to_multi_tenant()",
        "gnomon.erase_episode(",
        "gnomon.erase_node(",
        "gnomon.erase_edge(",
        "gnomon.erase_scope(",
        "gnomon.store_sentence(",
        "gnomon.list_gaps(",
    ] {
        assert!(
            names.iter().any(|n| n.starts_with(expected)),
            "{expected} is missing from {names:?}"
        );
    }
    for f in &functions {
        let (name, definer, config, owner): (String, bool, Vec<String>, String) =
            (f.get(0), f.get(1), f.get(2), f.get(3));
        let compact = name.replace(", ", ",");
        let owned_by = if DEFINERS.iter().any(|d| name.starts_with(d)) {
            Some("gnomon_definer")
        } else if ERASERS.iter().any(|d| name.starts_with(d)) {
            Some("gnomon_eraser")
        } else if WRITERS.contains(&compact.as_str()) {
            Some("gnomon_writer")
        } else {
            None
        };
        assert_eq!(
            definer,
            owned_by.is_some(),
            "{name}: SECURITY DEFINER is {definer}"
        );
        if let Some(role) = owned_by {
            assert_eq!(owner, role, "{name} is owned by {owner}");
        }
        // With an empty path, Postgres still searches the temporary schema
        // first for tables and types, so every path lists it, last.
        let path = config
            .iter()
            .find_map(|c| c.strip_prefix("search_path="))
            .unwrap_or_else(|| panic!("{name} does not pin search_path: {config:?}"));
        let schemas: Vec<&str> = path
            .split(',')
            .map(|p| p.trim().trim_matches('"'))
            .collect();
        assert_eq!(
            schemas.first(),
            Some(&"pg_catalog"),
            "{name}: search_path {path:?} does not start with pg_catalog"
        );
        assert_eq!(
            schemas.last(),
            Some(&"pg_temp"),
            "{name}: search_path {path:?} does not end with pg_temp"
        );
    }
}

#[tokio::test]
async fn the_definer_roles_cannot_log_in_or_bypass_rls_and_no_role_belongs_to_one_or_one_to_a_role()
{
    let w = world().await;
    let client = w.db.login().await;
    // A migrating role acts as each definer role for one step, under the
    // server's lock when a test applies the files (tests/install.rs).
    let _lock = support::Server::get().await.lock();
    // `gnomon_writer` owns every function a caller writes through,
    // `gnomon_definer` Kind registration, the merge-rule setter and the
    // search index trigger, and `gnomon_eraser` Erasure (ADR-0009, #81,
    // #83).
    for role in ["gnomon_definer", "gnomon_writer", "gnomon_eraser"] {
        let row = client
            .query_typed(
                "select rolcanlogin, rolinherit, rolsuper or rolbypassrls,
                        exists (select from pg_auth_members m where m.member = r.oid),
                        array(select m.member::regrole::text from pg_auth_members m
                              where m.roleid = r.oid and (m.set_option or m.inherit_option)),
                        array(select m.member::regrole::text from pg_auth_members m
                              where m.roleid = r.oid),
                        array(select c.oid::regclass::text from pg_class c where c.relowner = r.oid)
                 from pg_roles r where rolname = $1",
                &[(&role, Type::TEXT)],
            )
            .await
            .unwrap();
        assert_eq!(row.len(), 1, "the role {role} exists");
        let (login, inherit, bypass, member): (bool, bool, bool, bool) =
            (row[0].get(0), row[0].get(1), row[0].get(2), row[0].get(3));
        assert!(!login, "{role} can log in");
        assert!(!inherit, "{role} inherits");
        assert!(!bypass, "{role} can bypass RLS");
        assert!(!member, "{role} is a member of another role");
        // The migration role may act as the role only while it hands it the
        // functions. A CREATEROLE role keeps ADMIN on a role it created, which
        // lets it grant the role but not act as it.
        let members: Vec<String> = row[0].get(4);
        assert!(members.is_empty(), "{members:?} can act as {role}");
        let all_members: Vec<String> = row[0].get(5);
        for caller in [USER, WORKER, support::LOGIN_ROLE] {
            assert!(
                !all_members.iter().any(|m| m == caller),
                "{caller} is a member of {role}"
            );
        }
        // Row-level security applies to a role that owns no table.
        let owned: Vec<String> = row[0].get(6);
        assert!(owned.is_empty(), "{role} owns {owned:?}");
    }
}

#[tokio::test]
async fn the_migrations_grant_kind_registration_to_no_role_but_its_owner() {
    let w = world().await;
    let client = w.db.admin().await;
    let grantees: Vec<String> = client
        .query_typed(
            &format!(
                "select coalesce(nullif(a.grantee, 0)::regrole::text, 'public')
                 from pg_proc p, aclexplode(p.proacl) a
                 where p.oid = '{}'::regprocedure and a.grantee <> p.proowner",
                support::REGISTER_KIND
            ),
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert!(
        grantees.is_empty(),
        "Kind registration is granted to {grantees:?}"
    );
}

#[tokio::test]
async fn the_caller_roles_cannot_bypass_rls() {
    let w = world().await;
    let client = w.db.login().await;
    let rows = client
        .query_typed(
            "select rolname::text, rolsuper or rolbypassrls from pg_roles where rolname = any($1)",
            &[(&vec![USER, WORKER], Type::TEXT_ARRAY)],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    for r in rows {
        let (name, bypass): (String, bool) = (r.get(0), r.get(1));
        assert!(!bypass, "{name} can bypass RLS");
    }
}

// Kind registration, a definer function (ADR-0009). The migrations grant it
// to no one, so the deployment decides who may register Kinds.

const REGISTER_PERSON: &str = "select gnomon.register_kind($1, 'person', 'node', null, '{\"properties\": {\"role\": {\"type\": \"string\"}}}', '{}', null, null)";

/// The Kinds of `tenant`, counted past RLS.
async fn kind_rows(db: &TestDb, tenant: Uuid) -> i64 {
    db.admin()
        .await
        .query_typed(
            "select (select count(*) from gnomon.kind where tenant_id = $1)
                  + (select count(*) from gnomon.kind_rule where tenant_id = $1)",
            &[(&tenant, Type::UUID)],
        )
        .await
        .unwrap()[0]
        .get(0)
}

#[tokio::test]
async fn a_caller_role_without_the_grant_cannot_register_a_kind() {
    let w = world().await;
    for role in [USER, WORKER] {
        let mut client = w.db.login().await;
        let tx = as_caller(&mut client, role, &claims(w.t1, &["support"])).await;
        let call = tx
            .query_typed(REGISTER_PERSON, &[(&w.t1, Type::UUID)])
            .await;
        assert_refused(
            call,
            &format!("{role} registering a Kind without the grant"),
        );
    }
    let client = w.db.login().await;
    assert_refused(
        client
            .query_typed(REGISTER_PERSON, &[(&w.t1, Type::UUID)])
            .await,
        "the login role registering a Kind",
    );
    assert_eq!(
        kind_rows(&w.db, w.t1).await,
        0,
        "a refused call wrote a Kind"
    );
}

#[tokio::test]
async fn a_caller_with_the_grant_cannot_register_a_kind_in_another_tenant_or_with_no_tenant() {
    let w = world().await;
    w.db.grant_kind_registration(USER).await;
    let no_tenant = json!({ "app_metadata": { "scopes": ["support"] } }).to_string();
    let attempts = [
        (claims(w.t1, &["support"]), Some(w.t2), "another Tenant"),
        (claims(w.t1, &["support"]), None, "a null Tenant"),
        (no_tenant, Some(w.t2), "claims with no Tenant"),
        (String::new(), Some(w.t2), "no claims"),
    ];
    for (c, tenant, what) in attempts {
        let mut client = w.db.login().await;
        let tx = as_caller(&mut client, USER, &c).await;
        let call = tx
            .query_typed(REGISTER_PERSON, &[(&tenant, Type::UUID)])
            .await;
        assert_refused(call, &format!("registering a Kind into {what}"));
    }
    assert_eq!(kind_rows(&w.db, w.t2).await, 0);

    // The grant does let the caller register in its own Tenant.
    let mut client = w.db.login().await;
    let tx = as_caller(&mut client, USER, &claims(w.t1, &["support"])).await;
    tx.query_typed(REGISTER_PERSON, &[(&w.t1, Type::UUID)])
        .await
        .expect("registering in its own Tenant");
    tx.commit().await.unwrap();
    assert_eq!(kind_rows(&w.db, w.t1).await, 2, "one Kind and its one rule");
}

#[tokio::test]
async fn a_caller_cannot_read_another_tenants_kinds() {
    let w = world().await;
    w.db.grant_kind_registration(USER).await;
    {
        let mut client = w.db.login().await;
        let tx = as_caller(&mut client, USER, &claims(w.t2, &["support"])).await;
        tx.query_typed(REGISTER_PERSON, &[(&w.t2, Type::UUID)])
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }
    for role in [USER, WORKER] {
        let mut client = w.db.login().await;
        let tx = as_caller(&mut client, role, &claims(w.t1, &["support"])).await;
        for sql in [
            "select count(*) from gnomon.kind",
            "select count(*) from gnomon.kind_rule",
        ] {
            let n: i64 = tx.query_typed(sql, &[]).await.unwrap()[0].get(0);
            assert_eq!(n, 0, "{role}: {sql}");
        }
        let read = tx
            .query_typed(
                "select name from gnomon.read_kind($1, 'person')",
                &[(&w.t2, Type::UUID)],
            )
            .await
            .unwrap();
        assert!(
            read.is_empty(),
            "{role} read Tenant two's Kind through read_kind"
        );
    }
}

#[tokio::test]
async fn a_caller_cannot_redirect_kind_registration_through_its_search_path() {
    let w = world().await;
    w.db.grant_kind_registration(USER).await;
    let mut client = w.db.login().await;
    let tx = as_caller(&mut client, USER, &claims(w.t1, &["support"])).await;
    // Shadow the Kind tables and the root Kinds in the caller's temporary
    // schema, and put that schema first.
    tx.batch_execute(
        "create temporary table kind (like gnomon.kind);
         create temporary table kind_rule (tenant_id uuid, kind text, property text, rule text);
         create function pg_temp.root_kinds() returns table (name text, kind_of text, search text[],
           source_kinds text[], target_kinds text[])
           language sql as $$ values ('person', 'node', '{}'::text[], null::text[], null::text[]) $$;
         set local search_path = pg_temp, public;",
    )
    .await
    .unwrap();
    tx.query_typed(REGISTER_PERSON, &[(&w.t1, Type::UUID)])
        .await
        .expect("registration ignores the caller's search path");
    let shadow: i64 = tx
        .query_typed("select count(*) from pg_temp.kind", &[])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(shadow, 0, "registration wrote the caller's shadow table");
    let real = tx
        .query_typed(
            "select name from gnomon.read_kind($1, 'person')",
            &[(&w.t1, Type::UUID)],
        )
        .await
        .unwrap();
    assert_eq!(
        real.len(),
        1,
        "registration wrote somewhere other than gnomon.kind"
    );
}

/// The roles the type-shadowing test's domain check ran as, in order.
async fn recorded_roles(tx: &Transaction<'_>) -> String {
    let r: Option<String> = tx
        .query_typed(
            "select pg_catalog.current_setting('gnomon_test.roles', true)",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    r.unwrap_or_default()
}

/// Built-in types a function body names without a schema. The temporary
/// schema holds a domain of each name in the type-shadowing test.
const SHADOWED_TYPES: &[&str] = &[
    "text",
    "jsonb",
    "uuid",
    "timestamptz",
    "int4",
    "int8",
    "bool",
    "float8",
    "varbit",
    "tsvector",
    "xid8",
    "name",
    "regclass",
    "oid",
];

#[tokio::test]
async fn temporary_types_named_like_built_in_types_run_nothing_of_the_callers_as_a_definer_role() {
    // With `search_path = ''`, Postgres still searches the temporary schema
    // first for tables and types. A caller creates a domain named like each
    // built-in type, whose check records the role it runs as, then calls
    // every group of definer functions: Kind registration, the merge-rule
    // setter, a write that fires the search index trigger, an Erasure, which
    // runs as `gnomon_eraser` (#83), and the write functions, which run as
    // `gnomon_writer` (#81). A function that resolved one of those names to
    // the caller's domain would run the caller's check as one of those roles.
    let w = world().await;
    w.db.grant_kind_registration(USER).await;
    w.db.grant_erasure(USER).await;
    let mut client = w.db.login().await;
    let tx = as_caller(&mut client, USER, &claims(w.t1, &["support", "billing"])).await;
    let mut setup = String::from(
        "create function pg_temp.record_role() returns pg_catalog.bool
           language plpgsql set search_path = pg_catalog
           as $$ begin
             perform set_config('gnomon_test.roles',
               coalesce(current_setting('gnomon_test.roles', true), '') || ' ' || current_user,
               false);
             return true;
           end $$;",
    );
    for t in SHADOWED_TYPES {
        setup.push_str(&format!(
            "create domain pg_temp.{t} as pg_catalog.{t} check (pg_temp.record_role());"
        ));
    }
    tx.batch_execute(&setup).await.unwrap();
    // The caller's own statements resolve the names to its domains, so the
    // check runs, as the caller.
    tx.batch_execute("select 'armed'::text").await.unwrap();
    assert!(
        recorded_roles(&tx).await.contains(USER),
        "the domains did not shadow the built-in types: {:?}",
        recorded_roles(&tx).await
    );

    tx.query_typed(REGISTER_PERSON, &[(&w.t1, Type::UUID)])
        .await
        .expect("Kind registration");
    tx.query_typed(
        "select gnomon.set_merge_rule($1, 'person', 'ranked')",
        &[(&w.t1, Type::UUID)],
    )
    .await
    .expect("the merge-rule setter");
    let node: Uuid = tx
        .query_typed(
            "select gnomon.create_node($1, 'support', 'person')",
            &[(&w.t1, Type::UUID)],
        )
        .await
        .unwrap()[0]
        .get(0);
    write_node(
        &tx,
        w.t1,
        node,
        Some(w.t1_support),
        json!({ "name": "Ada Lovelace", "role": "engineer" }),
    )
    .await
    .expect("a write that fires the search index trigger");
    let indexed: i64 = tx
        .query_typed(
            "select count(*) from gnomon.search_token where tenant_id = $1",
            &[(&w.t1, Type::UUID)],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert!(indexed > 0, "the write wrote no token rows");
    tx.query_typed(
        "select gnomon.record_episode($1, 'support', 'another Episode', now(), 1)",
        &[(&w.t1, Type::UUID)],
    )
    .await
    .expect("recording an Episode");
    tx.query_typed(
        "select gnomon.write_summary($1, 'support', $2, 'Ada, an engineer.', now(), $3,
           null, null)",
        &[
            (&w.t1, Type::UUID),
            (&node, Type::UUID),
            (&vec![w.t1_support], Type::UUID_ARRAY),
        ],
    )
    .await
    .expect("a Summary");
    tx.query_typed(
        ERASE_EPISODE,
        &[(&w.t1, Type::UUID), (&w.t1_billing, Type::UUID)],
    )
    .await
    .expect("an Erasure");

    let ran_as = recorded_roles(&tx).await;
    for definer in ["gnomon_definer", "gnomon_writer", "gnomon_eraser"] {
        let times = ran_as.split_whitespace().filter(|r| *r == definer).count();
        assert_eq!(
            times, 0,
            "the caller's domain check ran {times} times as {definer}"
        );
    }
}

#[tokio::test]
async fn a_caller_calling_register_kind_directly_cannot_give_a_node_kind_endpoints() {
    let w = world().await;
    w.db.grant_kind_registration(USER).await;
    let mut client = w.db.login().await;
    let tx = as_caller(&mut client, USER, &claims(w.t1, &["support"])).await;
    let err = tx
        .query_typed(
            "select gnomon.register_kind($1, 'person', 'node', null, '{}', '{}', array['node'], null)",
            &[(&w.t1, Type::UUID)],
        )
        .await
        .expect_err("a Node Kind with endpoints was registered");
    assert_eq!(err.code().map(|c| c.code()), Some(gnomon::INVALID_KIND));
    assert!(
        err.as_db_error()
            .unwrap()
            .message()
            .contains("only an Edge Kind names the Node Kinds"),
        "{err:?}"
    );
}

// Nodes, Contributions, Versions and Citations (ADR-0007, ADR-0008,
// ADR-0009). Callers write them as the invoker, so the grants, the guard
// triggers and RLS are the barrier.

/// Two Tenants with Episodes as in `world`, and in each Tenant's `support`
/// Scope a Node written twice: once to open a Version and once to close it
/// and open another. All of it is committed.
struct Nodes {
    w: World,
    n1: Uuid,
    /// Tenant one's closed Version and its open one.
    closed: Uuid,
    open: Uuid,
    n2: Uuid,
    n2_open: Uuid,
}

async fn nodes() -> Nodes {
    let w = world().await;
    let store = Store::connect(&w.db.login_url(), &support::connect_options())
        .await
        .unwrap();
    let mut made = Vec::new();
    for (t, ep) in [(w.t1, w.t1_support), (w.t2, w.t2_support)] {
        let caller = store.as_worker(TenantId(t), Scope::new("support"), None);
        let patch = |n: &str| {
            gnomon::Patch::new(
                EpisodeId(ep),
                gnomon::ValidTime::UNKNOWN,
                match json!({ "name": n }) {
                    serde_json::Value::Object(m) => m,
                    _ => unreachable!(),
                },
            )
        };
        let (node, first) = caller
            .create_node(
                &gnomon::NewNode {
                    tenant: Some(TenantId(t)),
                    scope: Scope::new("support"),
                    kind: gnomon::ROOT_NODE_KIND.into(),
                },
                &patch("first"),
            )
            .await
            .unwrap();
        let second = caller
            .write_node(TenantId(t), node, &patch("second"))
            .await
            .unwrap();
        made.push((node.0, first.opened[0].0, second.opened[0].0));
    }
    Nodes {
        w,
        n1: made[0].0,
        closed: made[0].1,
        open: made[0].2,
        n2: made[1].0,
        n2_open: made[1].2,
    }
}

const WRITE_NODE: &str =
    "select status from gnomon.write_node($1, $2, $3, null, null, null, null, $4, null)";

async fn write_node(
    tx: &Transaction<'_>,
    tenant: Uuid,
    node: Uuid,
    episode: Option<Uuid>,
    values: serde_json::Value,
) -> Result<Vec<tokio_postgres::Row>, tokio_postgres::Error> {
    tx.query_typed(
        WRITE_NODE,
        &[
            (&tenant, Type::UUID),
            (&node, Type::UUID),
            (&episode, Type::UUID),
            (&values, Type::JSONB),
        ],
    )
    .await
}

/// The Version rows of a Node and its Citations, counted past RLS.
async fn version_rows(db: &TestDb, node: Uuid) -> (i64, i64) {
    let row = db
        .admin()
        .await
        .query_typed(
            "select (select count(*) from gnomon.version where node_id = $1),
                    (select count(*) from gnomon.citation c join gnomon.version v on v.id = c.version_id
                     where v.node_id = $1)",
            &[(&node, Type::UUID)],
        )
        .await
        .unwrap();
    (row[0].get(0), row[0].get(1))
}

/// Opens a transaction as `gnomon_writer` with `claims`, as a write
/// function runs. No caller may do this: the superuser does it for the test.
async fn as_writer<'c>(client: &'c mut Client, claims: &str) -> Transaction<'c> {
    let tx = client.transaction().await.unwrap();
    tx.batch_execute("set local role gnomon_writer")
        .await
        .unwrap();
    tx.query_typed(
        "select set_config('request.jwt.claims', $1, true)",
        &[(&claims, Type::TEXT)],
    )
    .await
    .unwrap();
    tx
}

/// Every row of the tables a write may delete from or update, past RLS.
async fn history_rows(db: &TestDb) -> Vec<String> {
    let admin = db.admin().await;
    let mut rows = Vec::new();
    for table in [
        "version",
        "citation",
        "summary",
        "summary_citation",
        "community",
        "community_member",
        "community_citation",
    ] {
        rows.extend(
            admin
                .query_typed(
                    &format!("select t::text from gnomon.{table} t order by 1"),
                    &[],
                )
                .await
                .unwrap()
                .iter()
                .map(|r| format!("{table}: {}", r.get::<_, String>(0))),
        );
    }
    rows
}

#[tokio::test]
async fn as_the_writer_role_deleting_or_rewriting_committed_history_changes_nothing() {
    // The write functions run as `gnomon_writer` (#81). Its policies let it
    // delete only the rows its own transaction wrote, and change a
    // committed row only by closing it at the start of the current
    // transaction. Its grants let it update only the Recorded-time end.
    let n = nodes().await;
    let store = Store::connect(&n.w.db.login_url(), &support::connect_options())
        .await
        .unwrap();
    let caller = store.as_worker(TenantId(n.w.t1), Scope::new("support"), None);
    let cites = vec![gnomon::DerivedCitation::of(EpisodeId(n.w.t1_support))];
    let gnomon::SummaryId(summary) = caller
        .write_summary(
            TenantId(n.w.t1),
            &gnomon::NewSummary {
                scope: Scope::new("support"),
                node: gnomon::NodeId(n.n1),
                content: "a Summary".into(),
                as_of_valid: "2024-06-01T00:00:00Z".parse().unwrap(),
                citations: cites.clone(),
            },
        )
        .await
        .unwrap();
    let gnomon::CommunityId(community) = caller
        .write_community(
            TenantId(n.w.t1),
            &gnomon::NewCommunity {
                scope: Scope::new("support"),
                replaces: vec![],
                members: vec![gnomon::NodeId(n.n1)],
                content: "a Community".into(),
                as_of_valid: "2024-06-01T00:00:00Z".parse().unwrap(),
                citations: cites,
            },
        )
        .await
        .unwrap();
    let before = history_rows(&n.w.db).await;
    let claims = claims(n.w.t1, &["support"]);

    // Statements that reach no row: deletes of committed rows, and updates
    // of a row that is already closed.
    let none = [
        ("delete from gnomon.version where node_id = $1", n.n1),
        ("delete from gnomon.citation where version_id = $1", n.open),
        (
            "delete from gnomon.citation where version_id = $1",
            n.closed,
        ),
        ("delete from gnomon.summary where id = $1", summary),
        (
            "delete from gnomon.summary_citation where summary_id = $1",
            summary,
        ),
        ("delete from gnomon.community where id = $1", community),
        (
            "delete from gnomon.community_member where community_id = $1",
            community,
        ),
        (
            "delete from gnomon.community_citation where community_id = $1",
            community,
        ),
        (
            "update gnomon.version set recorded_to = 'infinity' where id = $1",
            n.closed,
        ),
        (
            "update gnomon.version set recorded_to = transaction_timestamp() where id = $1",
            n.closed,
        ),
    ];
    // Statements the policy's check or the grants refuse.
    let refused = [
        (
            "update gnomon.version set recorded_to = now() - interval '1 second' where id = $1",
            n.open,
        ),
        (
            "update gnomon.version set recorded_to = now() + interval '1 day' where id = $1",
            n.open,
        ),
        (
            "update gnomon.version set props = '{\"name\": \"forged\"}' where id = $1",
            n.open,
        ),
        (
            "update gnomon.version set recorded_from = '2000-01-01' where id = $1",
            n.open,
        ),
        (
            "update gnomon.version set closed_in = null where id = $1",
            n.closed,
        ),
        (
            "update gnomon.summary set recorded_to = now() - interval '1 second' where id = $1",
            summary,
        ),
        (
            "update gnomon.summary set content = 'forged' where id = $1",
            summary,
        ),
        (
            "update gnomon.community set recorded_to = now() + interval '1 day' where id = $1",
            community,
        ),
        (
            "update gnomon.contribution set value = '\"forged\"' where node_id = $1",
            n.n1,
        ),
        ("delete from gnomon.contribution where node_id = $1", n.n1),
        (
            "update gnomon.citation set place = 'forged' where version_id = $1",
            n.open,
        ),
        ("delete from gnomon.node where id = $1", n.n1),
        ("delete from gnomon.episode where tenant_id = $1", n.w.t1),
        ("truncate gnomon.version cascade", n.n1),
    ];
    for (sql, target) in none {
        let mut client = n.w.db.admin().await;
        let tx = as_writer(&mut client, &claims).await;
        let params: &[(&(dyn tokio_postgres::types::ToSql + Sync), Type)] =
            &[(&target, Type::UUID)];
        let changed = tx.execute_typed(sql, params).await.unwrap();
        assert_eq!(changed, 0, "{sql} changed {changed} rows");
        tx.commit().await.unwrap();
    }
    for (sql, target) in refused {
        let mut client = n.w.db.admin().await;
        let tx = as_writer(&mut client, &claims).await;
        let r = if sql.starts_with("truncate") {
            tx.batch_execute(sql).await.map(|()| 0)
        } else {
            tx.execute_typed(sql, &[(&target, Type::UUID)]).await
        };
        match r {
            Err(e) => assert!(
                e.code() == Some(&SqlState::INSUFFICIENT_PRIVILEGE)
                    || e.code() == Some(&SqlState::CHECK_VIOLATION),
                "{sql}: {e:?}"
            ),
            Ok(changed) => panic!("{sql} was allowed and changed {changed} rows"),
        }
    }
    assert_eq!(history_rows(&n.w.db).await, before);

    // In its own transaction the writer closes the open Version, and deletes
    // the Version and Citations its own write made, as the write path does.
    let mut client = n.w.db.admin().await;
    let tx = as_writer(&mut client, &claims).await;
    let closed = tx
        .execute_typed(
            "update gnomon.version set recorded_to = transaction_timestamp() where id = $1",
            &[(&n.open, Type::UUID)],
        )
        .await
        .unwrap();
    assert_eq!(closed, 1);
    let closed_in: bool = tx
        .query_typed(
            "select closed_in = pg_current_xact_id() from gnomon.version where id = $1",
            &[(&n.open, Type::UUID)],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert!(closed_in, "the closing transaction is recorded");
    let reopened = tx
        .execute_typed(
            "update gnomon.version set recorded_to = 'infinity' where id = $1",
            &[(&n.open, Type::UUID)],
        )
        .await
        .unwrap();
    assert_eq!(reopened, 1, "a Version this transaction closed reopens");
    write_node(
        &tx,
        n.w.t1,
        n.n1,
        Some(n.w.t1_support),
        json!({ "name": "third" }),
    )
    .await
    .unwrap();
    let fresh: Uuid = tx
        .query_typed(
            "select id from gnomon.version where node_id = $1 and recorded_to = 'infinity'",
            &[(&n.n1, Type::UUID)],
        )
        .await
        .unwrap()[0]
        .get(0);
    let deleted = tx
        .execute_typed(
            "delete from gnomon.citation where version_id = $1",
            &[(&fresh, Type::UUID)],
        )
        .await
        .unwrap();
    assert_eq!(deleted, 1, "the transaction's own Citation");
    let deleted = tx
        .execute_typed(
            "delete from gnomon.version where id = $1",
            &[(&fresh, Type::UUID)],
        )
        .await
        .unwrap();
    assert_eq!(deleted, 1, "the transaction's own Version");
    let deleted = tx
        .execute_typed(
            "delete from gnomon.version where id = $1",
            &[(&n.open, Type::UUID)],
        )
        .await
        .unwrap();
    assert_eq!(deleted, 0, "a committed Version, after deleting its own");
    drop(tx);
    assert_eq!(history_rows(&n.w.db).await, before);
}

#[tokio::test]
async fn a_write_with_no_episode_fails() {
    let n = nodes().await;
    let mut client = n.w.db.login().await;
    let tx = as_caller(&mut client, USER, &claims(n.w.t1, &["support"])).await;
    let err = write_node(&tx, n.w.t1, n.n1, None, json!({ "name": "anonymous" }))
        .await
        .expect_err("a write with no Episode was accepted");
    assert_eq!(err.code().map(|c| c.code()), Some(gnomon::INVALID_WRITE));
    assert_eq!(
        err.as_db_error().unwrap().message(),
        "every write cites an Episode"
    );
}

#[tokio::test]
async fn the_write_patch_retract_and_erasure_paths_commit_versions_that_episodes_cite() {
    // The check that refuses an uncited Version at commit lets every Store
    // path through: a write, a patch, two writes to one Node in one
    // transaction, Retract Episode and Erase Episode, which deletes
    // Citations as its replay requires.
    let n = nodes().await;
    n.w.db.grant_erasure(WORKER).await;
    let store = Store::connect(&n.w.db.login_url(), &support::connect_options())
        .await
        .unwrap();
    let t = TenantId(n.w.t1);
    let caller = store.as_worker(t, Scope::new("support"), None);
    let mut episodes = Vec::new();
    for i in 0..4 {
        episodes.push(
            caller
                .record_episode(&NewEpisode {
                    tenant: Some(t),
                    scope: Scope::new("support"),
                    content: format!("Episode {i}"),
                    reference_time: "2024-01-01T00:00:00Z".parse().unwrap(),
                    authority: None,
                })
                .await
                .unwrap(),
        );
    }
    let day = |m: i8| support::utc(2024, m, 1);
    let name = |e: EpisodeId, valid: gnomon::ValidTime, name: &str| {
        gnomon::Patch::new(e, valid, support::erasure::values(json!({ "name": name })))
    };
    let new_node = gnomon::NewNode {
        tenant: Some(t),
        scope: Scope::new("support"),
        kind: gnomon::ROOT_NODE_KIND.into(),
    };
    let (node, _) = caller
        .create_node(
            &new_node,
            &name(episodes[0], gnomon::ValidTime::UNKNOWN, "Ada"),
        )
        .await
        .expect("a write");
    caller
        .write_node(
            t,
            node,
            &name(
                episodes[1],
                gnomon::ValidTime::between(day(1), day(6)),
                "Ada L",
            ),
        )
        .await
        .expect("a patch");
    caller
        .write_nodes(
            t,
            &[
                (
                    node,
                    name(episodes[2], gnomon::ValidTime::between(day(2), day(3)), "B"),
                ),
                (
                    node,
                    name(episodes[2], gnomon::ValidTime::between(day(2), day(4)), "C"),
                ),
            ],
        )
        .await
        .expect("two writes to one Node in one transaction");
    let (alone, _) = caller
        .create_node(
            &new_node,
            &name(episodes[3], gnomon::ValidTime::UNKNOWN, "only one Episode"),
        )
        .await
        .unwrap();
    let retraction = caller
        .retract_episode(t, episodes[3])
        .await
        .expect("Retract Episode");
    assert_eq!(retraction.retracted.len(), 1);
    let erasure = caller
        .erase_episode(t, episodes[1])
        .await
        .expect("Erase Episode");
    assert_eq!(erasure.recomputed_nodes, vec![node]);
    caller
        .erase_node(t, alone)
        .await
        .expect("Erasure of a Node");
    let uncited: i64 = n
        .w
        .db
        .admin()
        .await
        .query_typed(
            "select count(*) from gnomon.version v
             where v.tenant_id = $1 and not exists (
               select from gnomon.citation c where c.tenant_id = v.tenant_id and c.version_id = v.id)",
            &[(&n.w.t1, Type::UUID)],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(uncited, 0);
}

#[tokio::test]
async fn a_caller_cannot_read_or_write_another_tenants_or_scopes_nodes() {
    let n = nodes().await;
    for role in [USER, WORKER] {
        let mut client = n.w.db.login().await;
        let tx = as_caller(&mut client, role, &claims(n.w.t1, &["support"])).await;
        for table in ["node", "contribution", "version", "citation"] {
            let n_other: i64 = tx
                .query_typed(
                    &format!("select count(*) from gnomon.{table} where tenant_id <> $1"),
                    &[(&n.w.t1, Type::UUID)],
                )
                .await
                .unwrap()[0]
                .get(0);
            assert_eq!(n_other, 0, "{role} saw another Tenant's {table} rows");
        }
        let read = tx
            .query_typed(
                "select id from gnomon.read_node($1, $2, now(), null)",
                &[(&n.w.t2, Type::UUID), (&n.n2, Type::UUID)],
            )
            .await
            .unwrap();
        assert!(read.is_empty(), "{role} read Tenant two's Node");
        let cites = tx
            .query_typed(
                "select episode_id from gnomon.list_citations($1, $2)",
                &[(&n.w.t2, Type::UUID), (&n.n2_open, Type::UUID)],
            )
            .await
            .unwrap();
        assert!(cites.is_empty(), "{role} listed Tenant two's Citations");
    }

    // Writes into another Tenant fail at the Tenant check, and writes that
    // name this Tenant but another's Node or Episode find nothing to write.
    let attempts = [
        (
            n.w.t2,
            n.n2,
            n.w.t2_support,
            "another Tenant's Node",
            Some(SqlState::INSUFFICIENT_PRIVILEGE),
        ),
        (
            n.w.t1,
            n.n2,
            n.w.t1_support,
            "another Tenant's Node named in this Tenant",
            None,
        ),
        (
            n.w.t1,
            n.n1,
            n.w.t2_support,
            "citing another Tenant's Episode",
            None,
        ),
        (
            n.w.t1,
            n.n1,
            n.w.t1_billing,
            "citing another Scope's Episode",
            None,
        ),
    ];
    for (tenant, node, episode, what, code) in attempts {
        let mut client = n.w.db.login().await;
        let tx = as_caller(&mut client, WORKER, &claims(n.w.t1, &["support"])).await;
        let err = write_node(
            &tx,
            tenant,
            node,
            Some(episode),
            json!({ "name": "forged" }),
        )
        .await
        .expect_err(what);
        match code {
            Some(c) => assert_eq!(err.code(), Some(&c), "{what}: {err:?}"),
            None => assert_eq!(
                err.code().map(|c| c.code()),
                Some(gnomon::INVALID_WRITE),
                "{what}: {err:?}"
            ),
        }
    }
    let mut client = n.w.db.login().await;
    let tx = as_caller(&mut client, USER, &claims(n.w.t1, &["support"])).await;
    let r = tx
        .query_typed(
            "select gnomon.create_node($1, 'billing', 'node')",
            &[(&n.w.t1, Type::UUID)],
        )
        .await;
    assert_refused(r, "creating a Node in another Scope");
    assert_eq!(
        version_rows(&n.w.db, n.n2).await.0,
        2,
        "Tenant two's Node is untouched"
    );
}

#[tokio::test]
async fn a_write_that_started_before_a_committed_write_to_the_node_fails_and_can_be_retried() {
    let n = nodes().await;
    let mut early = n.w.db.login().await;
    let tx = as_caller(&mut early, USER, &claims(n.w.t1, &["support"])).await;
    // The early transaction's time is fixed when it starts. A write to the
    // Node then commits.
    let store = Store::connect(&n.w.db.login_url(), &support::connect_options())
        .await
        .unwrap();
    store
        .as_worker(TenantId(n.w.t1), Scope::new("support"), None)
        .write_node(
            TenantId(n.w.t1),
            gnomon::NodeId(n.n1),
            &gnomon::Patch::new(
                EpisodeId(n.w.t1_support),
                gnomon::ValidTime::UNKNOWN,
                match json!({ "name": "later" }) {
                    serde_json::Value::Object(m) => m,
                    _ => unreachable!(),
                },
            ),
        )
        .await
        .unwrap();
    let err = write_node(
        &tx,
        n.w.t1,
        n.n1,
        Some(n.w.t1_support),
        json!({ "name": "earlier" }),
    )
    .await
    .expect_err("a write recorded before a committed one");
    assert_eq!(
        err.code(),
        Some(&SqlState::T_R_SERIALIZATION_FAILURE),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_caller_cannot_redirect_the_write_through_its_search_path() {
    let n = nodes().await;
    let mut client = n.w.db.login().await;
    let tx = as_caller(&mut client, USER, &claims(n.w.t1, &["support"])).await;
    tx.batch_execute(
        "create temporary table version (like gnomon.version);
         create temporary table contribution (like gnomon.contribution);
         create function pg_temp.jsonb_path_match(jsonb, jsonpath) returns boolean
           language sql as $$ select true $$;
         set local search_path = pg_temp, public;",
    )
    .await
    .unwrap();
    write_node(
        &tx,
        n.w.t1,
        n.n1,
        Some(n.w.t1_support),
        json!({ "name": "real" }),
    )
    .await
    .unwrap();
    let shadow: i64 = tx
        .query_typed("select (select count(*) from pg_temp.version) + (select count(*) from pg_temp.contribution)", &[])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(shadow, 0, "the write wrote the caller's shadow tables");
    let err = write_node(
        &tx,
        n.w.t1,
        n.n1,
        Some(n.w.t1_support),
        json!({ "name": 7 }),
    )
    .await
    .expect_err("the write used the caller's shadow rule check");
    assert_eq!(err.code().map(|c| c.code()), Some(gnomon::INVALID_WRITE));
}

#[tokio::test]
async fn a_caller_cannot_recompute_another_tenants_node() {
    let n = nodes().await;
    let before = version_rows(&n.w.db, n.n2).await;
    for (tenant, what) in [
        (n.w.t2, "naming Tenant two"),
        (n.w.t1, "naming its own Tenant"),
    ] {
        let mut client = n.w.db.login().await;
        let tx = as_caller(&mut client, USER, &claims(n.w.t1, &["support"])).await;
        let err = tx
            .query_typed(
                "select * from gnomon.recompute_versions($1, $2, false, '-infinity', 'infinity')",
                &[(&tenant, Type::UUID), (&n.n2, Type::UUID)],
            )
            .await
            .expect_err(what);
        let code = err.code().map(|c| c.code().to_owned());
        assert!(
            code.as_deref() == Some("42501") || code.as_deref() == Some(gnomon::INVALID_WRITE),
            "{what}: {err:?}"
        );
    }
    assert_eq!(version_rows(&n.w.db, n.n2).await, before);
}

// Tenancy modes (ADR-0005). The mode is one row that no caller role may
// write, and multi-tenant mode never falls back to the default Tenant, not
// even for a caller that bypasses the Rust core.

#[tokio::test]
async fn a_caller_cannot_switch_the_tenancy_mode_or_write_the_tenancy_row() {
    let w = world().await;
    for multi in [false, true] {
        if multi {
            w.db.switch_to_multi_tenant().await;
        }
        for role in [USER, WORKER] {
            for sql in [
                "select gnomon.switch_to_multi_tenant()",
                "update gnomon.tenancy set mode = 'single'",
                "update gnomon.tenancy set mode = 'multi'",
                "update gnomon.tenancy set default_tenant_id = gen_random_uuid()",
                "insert into gnomon.tenancy (singleton, mode, default_tenant_id) values (false, 'single', gen_random_uuid())",
                "delete from gnomon.tenancy",
                "truncate gnomon.tenancy",
            ] {
                let mut client = w.db.login().await;
                let tx = as_caller(&mut client, role, &claims(w.t1, &["support"])).await;
                assert_refused(tx.batch_execute(sql).await, &format!("{role}: {sql}"));
            }
        }
    }
    let mode: String =
        w.db.operator()
            .await
            .query_typed("select mode from gnomon.tenancy", &[])
            .await
            .unwrap()[0]
            .get(0);
    assert_eq!(mode, "multi", "only the operator's switch took effect");
}

/// Every gnomon table with a Tenant, from the catalog.
async fn tenant_tables(client: &Client) -> Vec<String> {
    client
        .query_typed(
            "select c.relname::text
             from pg_class c join pg_namespace n on n.oid = c.relnamespace
             where n.nspname = 'gnomon' and c.relkind in ('r', 'p')
               and exists (select from pg_attribute a
                           where a.attrelid = c.oid and a.attname = 'tenant_id'
                             and not a.attisdropped)
             order by 1",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect()
}

#[tokio::test]
async fn in_multi_tenant_mode_claims_that_bind_no_tenant_reach_no_row_of_the_former_default_tenant()
{
    let db = TestDb::migrated().await;
    db.grant_kind_registration("authenticated").await;
    let first = db.default_tenant().await.0;
    // A single-tenant deployment writes through the Store, naming no Tenant.
    let store = Store::connect(&db.login_url(), &support::connect_options())
        .await
        .unwrap();
    let single = store.as_worker(None, Scope::new("support"), None);
    let episode = single
        .record_episode(&NewEpisode {
            tenant: None,
            scope: Scope::new("support"),
            content: "the default Tenant's".into(),
            reference_time: "2024-01-01T00:00:00Z".parse().unwrap(),
            authority: None,
        })
        .await
        .unwrap();
    single
        .create_node(
            &gnomon::NewNode {
                tenant: None,
                scope: Scope::new("support"),
                kind: gnomon::ROOT_NODE_KIND.into(),
            },
            &gnomon::Patch::new(
                episode,
                gnomon::ValidTime::UNKNOWN,
                serde_json::from_value(json!({ "name": "Ada" })).unwrap(),
            ),
        )
        .await
        .unwrap();
    let no_tenant =
        json!({ "role": "authenticated", "app_metadata": { "scopes": ["support"] } }).to_string();

    // Before the switch, such claims act in the default Tenant.
    let mut client = db.login().await;
    let tables = tenant_tables(&client).await;
    {
        let tx = as_caller(&mut client, USER, &no_tenant).await;
        let tenant: Option<Uuid> = tx
            .query_typed("select gnomon.tenant_id()", &[])
            .await
            .unwrap()[0]
            .get(0);
        assert_eq!(tenant, Some(first));
        let n: i64 = tx
            .query_typed("select count(*) from gnomon.node", &[])
            .await
            .unwrap()[0]
            .get(0);
        assert_eq!(n, 1);
    }

    db.switch_to_multi_tenant().await;

    let tx = as_caller(&mut client, USER, &no_tenant).await;
    let tenant: Option<Uuid> = tx
        .query_typed("select gnomon.tenant_id()", &[])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(
        tenant, None,
        "multi-tenant mode fell back to the default Tenant"
    );
    for t in &tables {
        let n: i64 = tx
            .query_typed(&format!("select count(*) from gnomon.{t}"), &[])
            .await
            .unwrap()[0]
            .get(0);
        assert_eq!(n, 0, "claims with no Tenant see {n} rows of gnomon.{t}");
    }
    let read = visible_ids(
        &tx,
        "select id from gnomon.read_episode($1, $2)",
        &[(&first, Type::UUID), (&episode.0, Type::UUID)],
    )
    .await;
    assert!(read.is_empty(), "read_episode returned {read:?}");
    tx.rollback().await.unwrap();

    let tx = as_caller(&mut client, USER, &no_tenant).await;
    assert_refused(
        tx.query_typed(
            "select gnomon.record_episode($1, 'support', 'into the first Tenant', now(), null)",
            &[(&first, Type::UUID)],
        )
        .await,
        "recording into the first Tenant with claims that bind no Tenant",
    );
}

#[tokio::test]
async fn in_single_tenant_mode_claims_that_bind_no_tenant_reach_only_the_default_tenant() {
    let w = world().await;
    let no_tenant =
        json!({ "role": "authenticated", "app_metadata": { "scopes": ["support", "billing"] } })
            .to_string();
    let mut client = w.db.login().await;
    let tx = as_caller(&mut client, USER, &no_tenant).await;
    let ids = visible_ids(&tx, "select id from gnomon.episode", &[]).await;
    assert!(ids.is_empty(), "the default Tenant saw {ids:?}");
    let read = visible_ids(
        &tx,
        "select id from gnomon.read_episode($1, $2)",
        &[(&w.t1, Type::UUID), (&w.t1_support, Type::UUID)],
    )
    .await;
    assert!(read.is_empty(), "read_episode returned {read:?}");
    assert_refused(
        tx.query_typed(
            "select gnomon.record_episode($1, 'support', 'into t1', now(), null)",
            &[(&w.t1, Type::UUID)],
        )
        .await,
        "recording into another Tenant with claims that bind no Tenant",
    );
}

#[tokio::test]
async fn after_the_switch_a_caller_of_the_former_default_tenant_sees_every_row_of_every_table() {
    let db = TestDb::migrated().await;
    db.grant_kind_registration("authenticated").await;
    let first = db.default_tenant().await.0;
    // A single-tenant deployment writes a row into every table through the
    // Store, naming no Tenant.
    let store = Store::connect(&db.login_url(), &support::connect_options())
        .await
        .unwrap();
    let single = store.as_user(
        serde_json::from_value(
            json!({ "role": "authenticated", "app_metadata": { "scopes": ["support"] } }),
        )
        .unwrap(),
    );
    single
        .register_kind(&gnomon::NewKind {
            tenant: None,
            name: "person".into(),
            of: gnomon::KindOf::Node,
            parent: None,
            properties: json!({ "properties": { "age": { "type": "integer" } } }),
            search: vec![],
        })
        .await
        .unwrap();
    single
        .set_merge_rule(None, "person", gnomon::MergeRule::Ranked)
        .await
        .unwrap();
    let episode = single
        .record_episode(&NewEpisode {
            tenant: None,
            scope: Scope::new("support"),
            content: "Ada, 36.".into(),
            reference_time: "2024-01-01T00:00:00Z".parse().unwrap(),
            authority: None,
        })
        .await
        .unwrap();
    let (node, written) = single
        .create_node(
            &gnomon::NewNode {
                tenant: None,
                scope: Scope::new("support"),
                kind: "person".into(),
            },
            &gnomon::Patch::new(
                episode,
                gnomon::ValidTime::UNKNOWN,
                serde_json::from_value(json!({ "name": "Ada", "age": 36 })).unwrap(),
            ),
        )
        .await
        .unwrap();
    // A Refinement writes a bound statement.
    single
        .state_bound(
            None,
            node,
            &gnomon::BoundStatement {
                episode,
                version: written.opened[0],
                side: gnomon::BoundSide::Start,
                bound: gnomon::Bound::Period {
                    at: "2024-03-01T00:00:00Z".parse().unwrap(),
                    precision: gnomon::Precision::Month,
                },
            },
        )
        .await
        .unwrap();
    // An End of validity writes a statement.
    single
        .end_node_validity(
            None,
            node,
            &gnomon::EndOfValidity {
                episode,
                at: gnomon::Bound::At("2024-06-01T00:00:00Z".parse().unwrap()),
            },
        )
        .await
        .unwrap();
    // Retract Episode writes a mark. An Episode nothing cites needs no
    // Retraction.
    let spare = single
        .record_episode(&NewEpisode {
            tenant: None,
            scope: Scope::new("support"),
            content: "retracted".into(),
            reference_time: "2024-02-01T00:00:00Z".parse().unwrap(),
            authority: None,
        })
        .await
        .unwrap();
    single.retract_episode(None, spare).await.unwrap();
    // A stage record with usage counts (ADR-0011).
    single
        .write_stages(None, &[gnomon::StageWrite::pending(episode, "graph")])
        .await
        .unwrap();
    let claim = single
        .claim_stage(None, Some(episode), std::time::Duration::from_secs(3600))
        .await
        .unwrap()
        .unwrap();
    single
        .write_stages(
            None,
            &[gnomon::StageWrite::finish(
                &claim,
                gnomon::StageOutcome::Succeeded,
                vec![gnomon::StageUsage {
                    model: "claude-sonnet-5-5".into(),
                    calls: 1,
                    input_tokens: 10,
                    output_tokens: 1,
                    cost_usd: None,
                }],
            )],
        )
        .await
        .unwrap();
    // An Edge from the Node to itself, and its Version's Sentence.
    let (_, edge) = single
        .create_edge(
            &gnomon::NewEdge {
                tenant: None,
                scope: Scope::new("support"),
                kind: gnomon::ROOT_EDGE_KIND.into(),
                source: node,
                target: node,
            },
            &gnomon::Patch::new(episode, gnomon::ValidTime::UNKNOWN, serde_json::Map::new()),
        )
        .await
        .unwrap();

    let operator = db.operator().await;
    support::store_sentence(
        &db,
        TenantId(first),
        "support",
        edge.opened[0].0,
        "Ada knows herself.",
    )
    .await;
    // An embedding of the Node's current Version (ADR-0010).
    let current = single
        .read_node(
            None,
            node,
            gnomon::AsOf::current("2024-04-01T00:00:00Z".parse().unwrap()),
        )
        .await
        .unwrap()
        .unwrap();
    single
        .store_embedding(
            None,
            current.id,
            &current.text_hash,
            &support::vectors::Rng::new(1).unit(),
        )
        .await
        .unwrap();
    // A Summary of the Node and a Community holding it, each citing the
    // Episode (ADR-0014, `0003_reads_and_writes.sql`).
    single
        .write_summary(
            None,
            &gnomon::NewSummary {
                scope: Scope::new("support"),
                node,
                content: "Ada is 36.".into(),
                as_of_valid: "2024-04-01T00:00:00Z".parse().unwrap(),
                citations: vec![gnomon::DerivedCitation::of(episode)],
            },
        )
        .await
        .unwrap();
    let community = single
        .write_community(
            None,
            &gnomon::NewCommunity {
                scope: Scope::new("support"),
                replaces: vec![],
                members: vec![node],
                content: "Ada, on her own.".into(),
                as_of_valid: "2024-04-01T00:00:00Z".parse().unwrap(),
                citations: vec![gnomon::DerivedCitation::of(episode)],
            },
        )
        .await
        .unwrap();
    // Its embedding, for Search (`0004_search.sql`). Its token rows and the
    // Summary's are written with them.
    single
        .store_community_embedding(None, community, &support::vectors::Rng::new(8).unit())
        .await
        .unwrap();
    let tables = tenant_tables(&operator).await;
    for expected in [
        "citation",
        "contribution",
        "episode",
        "kind",
        "kind_rule",
        "kind_merge",
        "node",
        "version",
        "bound_statement",
        "edge",
        "episode_retraction",
        "sentence",
        "stage_record",
        "stage_usage",
        "statement",
        "embedding",
        "embedding_bit",
        "summary",
        "summary_citation",
        "community",
        "community_member",
        "community_citation",
        "derived_search_token",
        "derived_embedding",
    ] {
        assert!(
            tables.iter().any(|t| t == expected),
            "{expected} is not in {tables:?}"
        );
    }
    let mut before = Vec::new();
    for t in &tables {
        let n: i64 = operator
            .query_typed(&format!("select count(*) from gnomon.{t}"), &[])
            .await
            .unwrap()[0]
            .get(0);
        // A transaction's changes to the BM25 counts go at its commit.
        if t == "search_count_delta" {
            assert_eq!(n, 0, "committed changes to the counts are left");
            before.push(n);
            continue;
        }
        assert!(
            n > 0,
            "the single-tenant deployment wrote no row into gnomon.{t}"
        );
        before.push(n);
    }

    db.switch_to_multi_tenant().await;

    let mut client = db.login().await;
    let tx = as_caller(&mut client, USER, &claims(first, &["support"])).await;
    for (t, n) in tables.iter().zip(before) {
        let seen: i64 = tx
            .query_typed(
                &format!("select count(*) from gnomon.{t} where tenant_id = $1"),
                &[(&first, Type::UUID)],
            )
            .await
            .unwrap()[0]
            .get(0);
        assert_eq!(
            seen, n,
            "gnomon.{t}: the first Tenant sees {seen} of {n} rows"
        );
    }
}

// Bound precision, Refinement and the Correction a contradiction becomes
// (ADR-0007, ADR-0008). A bound statement is written only through the Store,
// its kind is decided by a trigger, and a statement moves no committed row.

#[tokio::test]
async fn a_caller_cannot_state_a_bound_in_another_tenant_or_scope_or_of_closed_history() {
    let n = nodes().await;
    let call =
        "select change from gnomon.state_bound($1, $2, $3, $4, 'from', '2024-03-01', 'month')";
    // Another Tenant's claims, naming tenant one.
    for role in [USER, WORKER] {
        let mut client = n.w.db.login().await;
        let tx = as_caller(&mut client, role, &claims(n.w.t2, &["support"])).await;
        let r = tx
            .query_typed(
                call,
                &[
                    (&n.w.t1, Type::UUID),
                    (&n.n1, Type::UUID),
                    (&n.w.t1_support, Type::UUID),
                    (&n.open, Type::UUID),
                ],
            )
            .await;
        assert_refused(
            r,
            &format!("{role}: state a bound of another Tenant's Node"),
        );
        // The right Tenant, but a Scope the claims do not grant: the Node is
        // not visible.
        let mut client = n.w.db.login().await;
        let tx = as_caller(&mut client, role, &claims(n.w.t1, &["billing"])).await;
        let err = tx
            .query_typed(
                call,
                &[
                    (&n.w.t1, Type::UUID),
                    (&n.n1, Type::UUID),
                    (&n.w.t1_billing, Type::UUID),
                    (&n.open, Type::UUID),
                ],
            )
            .await
            .unwrap_err();
        assert_eq!(
            err.code().map(|c| c.code()),
            Some("GNW01"),
            "{role}: {err:?}"
        );
    }
    // A closed Version is history: a statement about it is refused.
    let mut client = n.w.db.login().await;
    let tx = as_caller(&mut client, USER, &claims(n.w.t1, &["support"])).await;
    let err = tx
        .query_typed(
            call,
            &[
                (&n.w.t1, Type::UUID),
                (&n.n1, Type::UUID),
                (&n.w.t1_support, Type::UUID),
                (&n.closed, Type::UUID),
            ],
        )
        .await
        .unwrap_err();
    assert_eq!(err.code().map(|c| c.code()), Some("GNW01"), "{err:?}");
}

#[tokio::test]
async fn a_refinement_leaves_committed_history_as_it_was_and_gives_callers_no_new_rights() {
    let n = nodes().await;
    let admin = n.w.db.admin().await;
    let snapshot = || async {
        admin
            .query(
                "select id::text, props::text, valid_from::text, valid_from_precision::text,
                        valid_to::text, recorded_from::text, recorded_to::text, text_hash
                 from gnomon.version where node_id = $1 order by recorded_from, id",
                &[&n.n1],
            )
            .await
            .unwrap()
            .iter()
            .map(|r| {
                (0..8)
                    .map(|i| r.get::<_, Option<String>>(i))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>()
    };
    let before = snapshot().await;

    let mut client = n.w.db.login().await;
    let tx = as_caller(&mut client, USER, &claims(n.w.t1, &["support"])).await;
    let row = tx
        .query_typed(
            "select change, versions from gnomon.state_bound($1, $2, $3, $4, 'from', '2024-03-17', 'day')",
            &[
                (&n.w.t1, Type::UUID),
                (&n.n1, Type::UUID),
                (&n.w.t1_support, Type::UUID),
                (&n.open, Type::UUID),
            ],
        )
        .await
        .unwrap();
    assert_eq!(row[0].get::<_, String>(0), "refinement");
    tx.commit().await.unwrap();

    let after = snapshot().await;
    // The Versions that were there before are the same rows, apart from the
    // end of the Recorded time of the one the statement closed.
    for old in &before {
        let new = after
            .iter()
            .find(|r| r[0] == old[0])
            .expect("a row survives");
        assert_eq!(new[..6], old[..6], "committed history is not rewritten");
        assert_eq!(new[7], old[7]);
    }
    assert_eq!(after.len(), before.len() + 1);
    let opened = after
        .iter()
        .find(|r| !before.iter().any(|b| b[0] == r[0]))
        .unwrap();
    let was = before
        .iter()
        .find(|b| b[0] == Some(n.open.to_string()))
        .unwrap();
    assert_eq!(opened[7], was[7], "a Refinement shares the text hash");

    // The statement added no right to rewrite history.
    for role in [USER, WORKER] {
        let rights = admin
            .query_typed(
                "select has_table_privilege($1, 'gnomon.bound_statement', 'UPDATE'),
                        has_table_privilege($1, 'gnomon.bound_statement', 'DELETE'),
                        has_table_privilege($1, 'gnomon.bound_statement', 'TRUNCATE'),
                        has_table_privilege($1, 'gnomon.version', 'DELETE'),
                        has_column_privilege($1, 'gnomon.version', 'valid_from', 'UPDATE'),
                        has_column_privilege($1, 'gnomon.version', 'valid_from_precision', 'UPDATE'),
                        has_column_privilege($1, 'gnomon.version', 'props', 'UPDATE'),
                        has_column_privilege($1, 'gnomon.version', 'recorded_to', 'UPDATE'),
                        has_column_privilege($1, 'gnomon.version', 'text_hash', 'INSERT'),
                        has_column_privilege($1, 'gnomon.version', 'text_hash', 'UPDATE'),
                        has_column_privilege($1, 'gnomon.contribution', 'valid_from', 'UPDATE')",
                &[(&role, Type::TEXT)],
            )
            .await
            .unwrap();
        let got: Vec<bool> = (0..11).map(|i| rights[0].get(i)).collect();
        assert_eq!(got, [false; 11], "{role}");
    }
    // Nor can a caller rewrite a Version's bound by hand.
    for (sql, what) in [
        (
            "update gnomon.version set valid_from = '2030-01-01' where id = $1",
            "move a bound",
        ),
        (
            "update gnomon.version set valid_from_precision = 'year' where id = $1",
            "change a bound's precision",
        ),
    ] {
        let mut client = n.w.db.login().await;
        let tx = as_caller(&mut client, USER, &claims(n.w.t1, &["support"])).await;
        assert_refused(
            tx.execute_typed(sql, &[(&n.closed, Type::UUID)]).await,
            what,
        );
    }
}

#[tokio::test]
async fn a_stated_bound_is_the_start_of_its_period_in_utc_whatever_the_sessions_time_zone() {
    let n = nodes().await;
    let mut client = n.w.db.login().await;
    let tx = as_caller(&mut client, USER, &claims(n.w.t1, &["support"])).await;
    // 23:00 UTC on March 31 is April 1 in Auckland.
    tx.batch_execute("set local timezone = 'Pacific/Auckland'")
        .await
        .unwrap();
    let node: Uuid = tx
        .query_typed(
            "select gnomon.create_node($1, 'support', 'node')",
            &[(&n.w.t1, Type::UUID)],
        )
        .await
        .unwrap()[0]
        .get(0);
    tx.query_typed(
        "select status from gnomon.write_node($1, $2, $3,
           '2024-03-31 23:00+00', 'month', '2025-12-31 23:00+00', 'year', '{\"name\": \"x\"}', null)",
        &[
            (&n.w.t1, Type::UUID),
            (&node, Type::UUID),
            (&n.w.t1_support, Type::UUID),
        ],
    )
    .await
    .unwrap();
    let row = tx
        .query_typed(
            "select (valid_from at time zone 'UTC')::text, valid_from_precision::text,
                    (valid_to at time zone 'UTC')::text, valid_to_precision::text
             from gnomon.version where node_id = $1",
            &[(&node, Type::UUID)],
        )
        .await
        .unwrap();
    assert_eq!(row[0].get::<_, String>(0), "2024-03-01 00:00:00");
    assert_eq!(row[0].get::<_, String>(1), "month");
    assert_eq!(row[0].get::<_, String>(2), "2025-01-01 00:00:00");
    assert_eq!(row[0].get::<_, String>(3), "year");
}

#[tokio::test]
async fn a_write_with_a_precision_that_does_not_fit_its_bound_is_refused() {
    let n = nodes().await;
    for (bounds, what) in [
        (
            "'2024-03-01', 'unknown', null, null",
            "a stated bound of unknown precision",
        ),
        (
            "null, 'month', null, null",
            "an unknown bound with a precision",
        ),
        (
            "'2024-03-01', 'fortnight', null, null",
            "a precision that does not exist",
        ),
        (
            "'2024-03-17', 'month', '2024-03-20', 'month'",
            "a period that is empty once truncated",
        ),
    ] {
        let mut client = n.w.db.login().await;
        let tx = as_caller(&mut client, USER, &claims(n.w.t1, &["support"])).await;
        let err = tx
            .query_typed(
                &format!(
                    "select status from gnomon.write_node($1, $2, $3, {bounds}, '{{\"name\": \"x\"}}', null)"
                ),
                &[
                    (&n.w.t1, Type::UUID),
                    (&n.n1, Type::UUID),
                    (&n.w.t1_support, Type::UUID),
                ],
            )
            .await
            .unwrap_err();
        assert_eq!(
            err.code().map(|c| c.code()),
            Some("GNW01"),
            "{what}: {err:?}"
        );
    }
}

// Ranked merge and Authority (ADR-0007). A Kind's merge rule is written only
// by the definer function that sets it, each Version records the rule of its
// Node's Kind even when a caller inserts it directly, and a caller gets no
// right to change an Episode's Authority or a Version beyond what Nodes and
// Versions already granted.

#[tokio::test]
async fn no_caller_may_set_a_merge_rule_until_the_deployment_grants_it_and_then_only_in_its_own_tenant()
 {
    let n = nodes().await;
    let set = "select gnomon.set_merge_rule($1, 'person', 'ranked')";
    n.w.db.grant_kind_registration(USER).await;
    {
        let mut client = n.w.db.login().await;
        let tx = as_caller(&mut client, USER, &claims(n.w.t1, &["support"])).await;
        tx.query_typed(
            "select gnomon.register_kind($1, 'person', 'node', null, '{}', '{}', null, null)",
            &[(&n.w.t1, Type::UUID)],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }
    // The worker role was never granted the right: it is refused.
    {
        let mut client = n.w.db.login().await;
        let tx = as_caller(&mut client, WORKER, &claims(n.w.t1, &["support"])).await;
        assert_refused(
            tx.query_typed(set, &[(&n.w.t1, Type::UUID)]).await,
            "the worker setting a merge rule without the grant",
        );
    }
    // The role that has it cannot name another Tenant, whose Kind it can neither see nor change.
    for tenant in [n.w.t2] {
        let mut client = n.w.db.login().await;
        let tx = as_caller(&mut client, USER, &claims(n.w.t1, &["support"])).await;
        assert_refused(
            tx.query_typed(set, &[(&tenant, Type::UUID)]).await,
            "setting a rule in another Tenant",
        );
    }
    // Within its own Tenant it can, and root Kinds have no rule to set.
    let mut client = n.w.db.login().await;
    let tx = as_caller(&mut client, USER, &claims(n.w.t1, &["support"])).await;
    tx.query_typed(set, &[(&n.w.t1, Type::UUID)]).await.unwrap();
    let err = tx
        .query_typed(
            "select gnomon.set_merge_rule($1, 'node', 'ranked')",
            &[(&n.w.t1, Type::UUID)],
        )
        .await
        .expect_err("a root Kind has no rule of its own");
    assert_eq!(err.code().map(|c| c.code()), Some(gnomon::INVALID_KIND));
}

#[tokio::test]
async fn a_caller_cannot_write_the_merge_rule_table_directly() {
    let n = nodes().await;
    n.w.db.grant_kind_registration(USER).await;
    {
        let mut client = n.w.db.login().await;
        let tx = as_caller(&mut client, USER, &claims(n.w.t1, &["support"])).await;
        tx.query_typed(
            "select gnomon.register_kind($1, 'person', 'node', null, '{}', '{}', null, null)",
            &[(&n.w.t1, Type::UUID)],
        )
        .await
        .unwrap();
        tx.query_typed(
            "select gnomon.set_merge_rule($1, 'person', 'ranked')",
            &[(&n.w.t1, Type::UUID)],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }
    for role in [USER, WORKER] {
        for sql in [
            "insert into gnomon.kind_merge (tenant_id, kind, merge_rule) select tenant_id, 'person', 'recency' from gnomon.kind",
            "update gnomon.kind_merge set merge_rule = 'recency'",
            "delete from gnomon.kind_merge",
            "truncate gnomon.kind_merge",
        ] {
            let mut client = n.w.db.login().await;
            let tx = as_caller(&mut client, role, &claims(n.w.t1, &["support"])).await;
            assert_refused(tx.batch_execute(sql).await, &format!("{role}: {sql}"));
        }
    }
    let rows: i64 =
        n.w.db
            .admin()
            .await
            .query_typed("select count(*) from gnomon.kind_merge", &[])
            .await
            .unwrap()[0]
            .get(0);
    assert_eq!(rows, 1, "only the definer function's row exists");
}

/// Every grant on the gnomon schema and on each relation and function in
/// it, to any role but the object's owner. An object with no ACL holds its
/// built-in default.
async fn gnomon_grants(client: &Client) -> Vec<String> {
    client
        .query_typed(
            "with objects as (
               select 'schema' as kind, n.nspname::text as name,
                      coalesce(n.nspacl, acldefault('n'::\"char\", n.nspowner)) as acl, n.nspowner as owner
               from pg_namespace n where n.nspname = 'gnomon'
               union all
               select 'relation', c.oid::regclass::text,
                      coalesce(c.relacl, acldefault(case when c.relkind = 'S' then 's' else 'r' end::\"char\",
                                                    c.relowner)),
                      c.relowner
               from pg_class c
               where c.relnamespace = 'gnomon'::regnamespace
                 and c.relkind in ('r', 'p', 'v', 'm', 'S', 'f')
               union all
               select 'function', p.oid::regprocedure::text,
                      coalesce(p.proacl, acldefault('f'::\"char\", p.proowner)), p.proowner
               from pg_proc p where p.pronamespace = 'gnomon'::regnamespace
             )
             select format('%s %s: %s %s', o.kind, o.name,
                           case when a.grantee = 0 then 'public' else a.grantee::regrole::text end,
                           a.privilege_type)
             from objects o, aclexplode(o.acl) a
             where a.grantee <> o.owner
             order by 1",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect()
}

#[tokio::test]
async fn global_default_privileges_give_callers_no_right_the_migrations_do_not() {
    // The role that applies the migrations grants callers everything on
    // every table, sequence, function and schema it creates, in any
    // schema, before the first migration runs. The last file revokes what
    // that grants on gnomon's objects, so the chain ends with the grants a
    // chain without default privileges has, and an Episode stays immutable.
    let db = TestDb::empty_with_login().await;
    let operator = db.operator().await;
    for objects in ["tables", "sequences", "functions", "schemas"] {
        operator
            .batch_execute(&format!(
                "alter default privileges grant all on {objects} to authenticated, gnomon_worker"
            ))
            .await
            .unwrap();
    }
    for (file, sql) in support::migration_files(support::DEFAULT_DIMENSION) {
        operator
            .batch_execute(&sql)
            .await
            .unwrap_or_else(|e| panic!("apply {file}: {e:?}"));
    }

    let plain = TestDb::migrated().await;
    let expected = gnomon_grants(&plain.admin().await).await;
    let granted = gnomon_grants(&db.admin().await).await;
    let extra: Vec<_> = granted.iter().filter(|g| !expected.contains(g)).collect();
    let missing: Vec<_> = expected.iter().filter(|g| !granted.contains(g)).collect();
    assert!(
        extra.is_empty() && missing.is_empty(),
        "grants beyond the migrations': {extra:#?}\ngrants the migrations give that are missing: {missing:#?}"
    );

    let store = Store::connect(&db.login_url(), &support::connect_options())
        .await
        .unwrap();
    let t = Uuid::new_v4();
    let EpisodeId(episode) = store
        .as_worker(TenantId(t), Scope::new("support"), None)
        .record_episode(&NewEpisode {
            tenant: Some(TenantId(t)),
            scope: Scope::new("support"),
            content: "an Episode".into(),
            reference_time: "2024-01-01T00:00:00Z".parse().unwrap(),
            authority: Some(1),
        })
        .await
        .unwrap();
    for role in [USER, WORKER] {
        for sql in [
            "update gnomon.episode set authority = 99, content = 'rewritten'",
            "delete from gnomon.episode",
        ] {
            let mut client = db.login().await;
            let tx = as_caller(&mut client, role, &claims(t, &["support"])).await;
            assert_refused(tx.batch_execute(sql).await, &format!("{role}: {sql}"));
        }
    }
    let row = db
        .admin()
        .await
        .query_typed(
            "select content, authority from gnomon.episode where id = $1",
            &[(&episode, Type::UUID)],
        )
        .await
        .unwrap();
    assert_eq!(row.len(), 1);
    assert_eq!(row[0].get::<_, String>(0), "an Episode");
    assert_eq!(row[0].get::<_, Option<i32>>(1), Some(1));
}

// Edges (ADR-0005, ADR-0008, ADR-0009). An Edge's endpoints are Nodes of its
// own Tenant that the caller can see, of Kinds its Edge Kind allows. Edge
// Versions are Versions: the guard triggers, the grants and RLS that keep a
// Node's history keep an Edge's, and no caller gets a new right to change
// them. A caller stores a Sentence of an Edge Version it sees (migration
// 0021), which the Sentences tests below cover.

/// The Nodes of `nodes`, an Edge in each Tenant's `support` Scope from its
/// Node to itself, written once, and a Node of Tenant one in `billing`.
struct Edges {
    n: Nodes,
    e1: Uuid,
    e1_open: Uuid,
    e2: Uuid,
    billing_node: Uuid,
}

async fn edges() -> Edges {
    let n = nodes().await;
    let store = Store::connect(&n.w.db.login_url(), &support::connect_options())
        .await
        .unwrap();
    let mut made = Vec::new();
    for (t, ep, node) in [
        (n.w.t1, n.w.t1_support, n.n1),
        (n.w.t2, n.w.t2_support, n.n2),
    ] {
        let (edge, written) = store
            .as_worker(TenantId(t), Scope::new("support"), None)
            .create_edge(
                &gnomon::NewEdge {
                    tenant: Some(TenantId(t)),
                    scope: Scope::new("support"),
                    kind: gnomon::ROOT_EDGE_KIND.into(),
                    source: gnomon::NodeId(node),
                    target: gnomon::NodeId(node),
                },
                &gnomon::Patch::new(
                    EpisodeId(ep),
                    gnomon::ValidTime::UNKNOWN,
                    Default::default(),
                ),
            )
            .await
            .unwrap();
        made.push((edge.0, written.opened[0].0));
    }
    let (billing_node, _) = store
        .as_worker(TenantId(n.w.t1), Scope::new("billing"), None)
        .create_node(
            &gnomon::NewNode {
                tenant: Some(TenantId(n.w.t1)),
                scope: Scope::new("billing"),
                kind: gnomon::ROOT_NODE_KIND.into(),
            },
            &gnomon::Patch::new(
                EpisodeId(n.w.t1_billing),
                gnomon::ValidTime::UNKNOWN,
                serde_json::from_value(json!({ "name": "billing" })).unwrap(),
            ),
        )
        .await
        .unwrap();
    Edges {
        e1: made[0].0,
        e1_open: made[0].1,
        e2: made[1].0,
        billing_node: billing_node.0,
        n,
    }
}

fn assert_invalid_write(
    result: Result<impl std::fmt::Debug, tokio_postgres::Error>,
    message: &str,
) {
    match result {
        Err(e) => {
            assert_eq!(
                e.code().map(|c| c.code()),
                Some(gnomon::INVALID_WRITE),
                "{message}: {e:?}"
            );
            assert_eq!(e.as_db_error().unwrap().message(), message);
        }
        Ok(v) => panic!("allowed, expected {message:?}: {v:?}"),
    }
}

const CREATE_EDGE: &str = "select gnomon.create_edge($1, 'support', $2, $3, $4)";
const INSERT_EDGE: &str = "insert into gnomon.edge (tenant_id, scope, kind, source_id, target_id)
     values ($1, 'support', $2, $3, $4)";

#[tokio::test]
async fn a_caller_cannot_join_a_node_in_another_tenant_or_a_scope_it_cannot_see_with_an_edge() {
    let e = edges().await;
    let (t1, n1, n2) = (e.n.w.t1, e.n.n1, e.n.n2);
    for role in [USER, WORKER] {
        for sql in [CREATE_EDGE] {
            for (source, target, side, other) in [
                (n1, n2, "target", n2),
                (n2, n1, "source", n2),
                (n1, e.billing_node, "target", e.billing_node),
                (e.billing_node, n1, "source", e.billing_node),
            ] {
                let mut client = e.n.w.db.login().await;
                let tx = as_caller(&mut client, role, &claims(t1, &["support"])).await;
                let r = tx
                    .execute_typed(
                        sql,
                        &[
                            (&t1, Type::UUID),
                            (&"edge", Type::TEXT),
                            (&source, Type::UUID),
                            (&target, Type::UUID),
                        ],
                    )
                    .await;
                assert_invalid_write(
                    r,
                    &format!("the {side} Node {other} is not visible to the caller"),
                );
            }
        }
        // An Edge filed under the other Tenant, to that Tenant's Node, finds
        // no Node the caller can see, before RLS refuses the row itself.
        let mut client = e.n.w.db.login().await;
        let tx = as_caller(&mut client, role, &claims(t1, &["support"])).await;
        let r = tx
            .execute_typed(
                CREATE_EDGE,
                &[
                    (&e.n.w.t2, Type::UUID),
                    (&"edge", Type::TEXT),
                    (&n2, Type::UUID),
                    (&n2, Type::UUID),
                ],
            )
            .await;
        assert_invalid_write(
            r,
            &format!("the source Node {n2} is not visible to the caller"),
        );
    }

    // Under the trigger, the foreign keys refuse a cross-Tenant endpoint
    // whatever RLS allows: here the owner, who bypasses RLS, inserts one with
    // the trigger turned off for its own transaction.
    let mut operator = e.n.w.db.operator().await;
    let tx = operator.transaction().await.unwrap();
    tx.batch_execute("alter table gnomon.edge disable trigger edge_check_insert")
        .await
        .unwrap();
    let err = tx
        .execute_typed(
            INSERT_EDGE,
            &[
                (&t1, Type::UUID),
                (&"edge", Type::TEXT),
                (&n1, Type::UUID),
                (&n2, Type::UUID),
            ],
        )
        .await
        .unwrap_err();
    assert_eq!(
        err.code(),
        Some(&SqlState::FOREIGN_KEY_VIOLATION),
        "{err:?}"
    );
    drop(tx);

    let rows: i64 =
        e.n.w
            .db
            .admin()
            .await
            .query_typed("select count(*) from gnomon.edge", &[])
            .await
            .unwrap()[0]
            .get(0);
    assert_eq!(rows, 2, "only the fixture's Edges exist");
}

#[tokio::test]
async fn a_caller_cannot_read_or_traverse_another_tenants_or_scopes_edges() {
    let e = edges().await;
    let (t1, t2) = (e.n.w.t1, e.n.w.t2);
    // A caller who sees both of Tenant one's Scopes joins its support Node to
    // its billing Node with an Edge in support.
    let store = Store::connect(&e.n.w.db.login_url(), &support::connect_options())
        .await
        .unwrap();
    let (crossing, _) = store
        .as_user(support::user(TenantId(t1), &["support", "billing"]))
        .create_edge(
            &gnomon::NewEdge {
                tenant: Some(TenantId(t1)),
                scope: Scope::new("support"),
                kind: gnomon::ROOT_EDGE_KIND.into(),
                source: gnomon::NodeId(e.n.n1),
                target: gnomon::NodeId(e.billing_node),
            },
            &gnomon::Patch::new(
                EpisodeId(e.n.w.t1_support),
                gnomon::ValidTime::UNKNOWN,
                Default::default(),
            ),
        )
        .await
        .unwrap();
    for role in [USER, WORKER] {
        let mut client = e.n.w.db.login().await;
        let tx = as_caller(&mut client, role, &claims(t1, &["support"])).await;
        for table in ["edge", "sentence"] {
            let other: i64 = tx
                .query_typed(
                    &format!("select count(*) from gnomon.{table} where tenant_id <> $1"),
                    &[(&t1, Type::UUID)],
                )
                .await
                .unwrap()[0]
                .get(0);
            assert_eq!(other, 0, "{role} saw another Tenant's {table} rows");
        }
        for (sql, id) in [
            ("select id from gnomon.read_edge($1, $2, now(), null)", e.e2),
            (
                "select id from gnomon.traverse($1, $2, now(), null, 'both', null)",
                e.n.n2,
            ),
            (
                "select id from gnomon.read_nodes($1, array[$2::uuid], now(), null)",
                e.n.n2,
            ),
        ] {
            for tenant in [t1, t2] {
                let rows = tx
                    .query_typed(sql, &[(&tenant, Type::UUID), (&id, Type::UUID)])
                    .await
                    .unwrap();
                assert!(rows.is_empty(), "{role} read Tenant two through {sql}");
            }
        }
        // A caller of support alone reads no Edge that names the billing
        // Node, by ID or by traversal.
        let rows = tx
            .query_typed(
                "select id from gnomon.read_edge($1, $2, now(), null)",
                &[(&t1, Type::UUID), (&crossing.0, Type::UUID)],
            )
            .await
            .unwrap();
        assert!(
            rows.is_empty(),
            "{role} read an Edge to a Node outside its Scopes"
        );
        let rows = tx
            .query_typed(
                "select neighbour_id from gnomon.traverse($1, $2, now(), null, 'both', null)",
                &[(&t1, Type::UUID), (&e.n.n1, Type::UUID)],
            )
            .await
            .unwrap();
        let neighbours: Vec<Uuid> = rows.iter().map(|r| r.get(0)).collect();
        assert_eq!(
            neighbours,
            vec![e.n.n1],
            "{role} traversed out of its Scopes"
        );
        // Tenant one's own Node in billing is out of this caller's Scopes.
        let rows = tx
            .query_typed(
                "select id from gnomon.read_nodes($1, array[$2::uuid, $3::uuid], now(), null)",
                &[
                    (&t1, Type::UUID),
                    (&e.billing_node, Type::UUID),
                    (&e.n.n1, Type::UUID),
                ],
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "{role} read a Node outside its Scopes");
    }
}

#[tokio::test]
async fn a_caller_cannot_write_another_tenants_edge_or_change_an_edge() {
    let e = edges().await;
    let t1 = e.n.w.t1;
    for role in [USER, WORKER] {
        // Sentences are covered with Sentences, below.
        let attempts: [(&str, Vec<Uuid>, &str); 7] = [
            (
                "select gnomon.write_edge($1, $2, $3, null, null, null, null, null, null)",
                vec![e.n.w.t2, e.e2, e.n.w.t2_support],
                "writing another Tenant's Edge",
            ),
            (
                "update gnomon.edge set target_id = $2 where id = $3 and $1::uuid is not null",
                vec![t1, e.n.n1, e.e1],
                "moving an Edge's endpoint",
            ),
            (
                "delete from gnomon.edge where id = $2 and $1::uuid is not null and $3::uuid is not null",
                vec![t1, e.e1, e.e1],
                "deleting an Edge",
            ),
            (
                "insert into gnomon.edge (tenant_id, scope, kind, source_id, target_id, created_at)
                 values ($1, 'support', 'edge', $2, $3, '2000-01-01')",
                vec![t1, e.n.n1, e.n.n1],
                "choosing an Edge's creation time",
            ),
            (
                "delete from gnomon.version where id = $2 and $1::uuid is not null and $3::uuid is not null",
                vec![t1, e.e1_open, e.e1_open],
                "deleting a committed Edge Version",
            ),
            (
                "update gnomon.version set props = '{\"forged\": true}' where id = $2
                 and $1::uuid is not null and $3::uuid is not null",
                vec![t1, e.e1_open, e.e1_open],
                "rewriting an Edge Version's values",
            ),
            (
                "update gnomon.version set recorded_to = '2000-01-01' where id = $2
                 and $1::uuid is not null and $3::uuid is not null",
                vec![t1, e.e1_open, e.e1_open],
                "rewriting an Edge Version's Recorded time",
            ),
        ];
        for (sql, params, what) in attempts {
            let mut client = e.n.w.db.login().await;
            let tx = as_caller(&mut client, role, &claims(t1, &["support"])).await;
            let r = tx
                .execute_typed(
                    sql,
                    &[
                        (&params[0], Type::UUID),
                        (&params[1], Type::UUID),
                        (&params[2], Type::UUID),
                    ],
                )
                .await;
            assert_refused(r, &format!("{role} {what}"));
        }
    }
    let rows =
        e.n.w
            .db
            .admin()
            .await
            .query_typed(
                "select (select count(*) from gnomon.version where edge_id = any($1)),
                    (select count(*) from gnomon.sentence),
                    (select count(*) from gnomon.edge)",
                &[(&vec![e.e1, e.e2], Type::UUID_ARRAY)],
            )
            .await
            .unwrap();
    assert_eq!(
        (
            rows[0].get::<_, i64>(0),
            rows[0].get::<_, i64>(1),
            rows[0].get::<_, i64>(2)
        ),
        (2, 0, 2),
        "the Edges and their Versions are untouched"
    );
}

// The reserved Edge Kinds `same_as` and `different_from` (ADR-0012). They
// are ordinary Edges, so the Edge rules hold for them: endpoints of the
// caller's Tenant that it can see, and nothing of another Tenant or Scope
// to read or traverse. Both are symmetric, so the Store keeps one Edge of
// each per pair of Nodes and Scope whatever order a caller gives, and no
// caller can register, rewrite or remove either Kind, with or without the
// grant to register Kinds.

const RESERVED: [&str; 2] = [gnomon::SAME_AS_KIND, gnomon::DIFFERENT_FROM_KIND];

fn assert_code(
    result: Result<impl std::fmt::Debug, tokio_postgres::Error>,
    code: &str,
    what: &str,
) {
    match result {
        Err(e) => assert_eq!(e.code().map(|c| c.code()), Some(code), "{what}: {e:?}"),
        Ok(v) => panic!("{what} was allowed: {v:?}"),
    }
}

/// A second Node of Tenant one in `support`, written as a worker.
async fn second_support_node(e: &Edges) -> Uuid {
    let store = Store::connect(&e.n.w.db.login_url(), &support::connect_options())
        .await
        .unwrap();
    store
        .as_worker(TenantId(e.n.w.t1), Scope::new("support"), None)
        .create_node(
            &gnomon::NewNode {
                tenant: Some(TenantId(e.n.w.t1)),
                scope: Scope::new("support"),
                kind: gnomon::ROOT_NODE_KIND.into(),
            },
            &gnomon::Patch::new(
                EpisodeId(e.n.w.t1_support),
                gnomon::ValidTime::UNKNOWN,
                serde_json::from_value(json!({ "name": "second" })).unwrap(),
            ),
        )
        .await
        .unwrap()
        .0
        .0
}

#[tokio::test]
async fn a_caller_cannot_join_a_node_of_another_tenant_or_hidden_scope_or_a_node_to_itself_with_a_reserved_edge()
 {
    let e = edges().await;
    let (t1, n1, n2) = (e.n.w.t1, e.n.n1, e.n.n2);
    for role in [USER, WORKER] {
        for sql in [CREATE_EDGE] {
            for kind in RESERVED {
                for (source, target, message) in [
                    (
                        n1,
                        n2,
                        format!("the target Node {n2} is not visible to the caller"),
                    ),
                    (
                        n2,
                        n1,
                        format!("the source Node {n2} is not visible to the caller"),
                    ),
                    (
                        n1,
                        e.billing_node,
                        format!(
                            "the target Node {} is not visible to the caller",
                            e.billing_node
                        ),
                    ),
                    (
                        e.billing_node,
                        n1,
                        format!(
                            "the source Node {} is not visible to the caller",
                            e.billing_node
                        ),
                    ),
                    (
                        n1,
                        n1,
                        format!(
                            "an Edge of the symmetric Kind \"{kind}\" joins two different Nodes"
                        ),
                    ),
                ] {
                    let mut client = e.n.w.db.login().await;
                    let tx = as_caller(&mut client, role, &claims(t1, &["support"])).await;
                    let r = tx
                        .execute_typed(
                            sql,
                            &[
                                (&t1, Type::UUID),
                                (&kind, Type::TEXT),
                                (&source, Type::UUID),
                                (&target, Type::UUID),
                            ],
                        )
                        .await;
                    assert_invalid_write(r, &message);
                }
            }
        }
    }
    let rows: i64 =
        e.n.w
            .db
            .admin()
            .await
            .query_typed("select count(*) from gnomon.edge", &[])
            .await
            .unwrap()[0]
            .get(0);
    assert_eq!(rows, 2, "only the fixture's Edges exist");
}

#[tokio::test]
async fn a_reserved_edge_is_one_edge_per_pair_and_scope_and_hidden_from_other_tenants_and_scopes() {
    let e = edges().await;
    let (t1, t2, n1) = (e.n.w.t1, e.n.w.t2, e.n.n1);
    for role in [USER, WORKER] {
        for kind in RESERVED {
            // A pair of Nodes of its own for each round.
            let m1 = second_support_node(&e).await;
            let (low, high) = (n1.min(m1), n1.max(m1));
            let mut client = e.n.w.db.login().await;
            let tx = as_caller(&mut client, role, &claims(t1, &["support", "billing"])).await;
            // An Edge created from the higher ID to the lower is stored the
            // other way round.
            let support_edge: Uuid = tx
                .query_typed(
                    CREATE_EDGE,
                    &[
                        (&t1, Type::UUID),
                        (&kind, Type::TEXT),
                        (&high, Type::UUID),
                        (&low, Type::UUID),
                    ],
                )
                .await
                .unwrap()[0]
                .get(0);
            let row = tx
                .query_typed(
                    "select source_id, target_id from gnomon.edge where id = $1",
                    &[(&support_edge, Type::UUID)],
                )
                .await
                .unwrap();
            let (source, target): (Uuid, Uuid) = (row[0].get(0), row[0].get(1));
            assert_eq!((source, target), (low, high), "{role} {kind}");
            // Creating it again in the Scope, in either order, returns the one
            // there is.
            for (s, t) in [(low, high), (high, low)] {
                let id: Uuid = tx
                    .query_typed(
                        CREATE_EDGE,
                        &[
                            (&t1, Type::UUID),
                            (&kind, Type::TEXT),
                            (&s, Type::UUID),
                            (&t, Type::UUID),
                        ],
                    )
                    .await
                    .unwrap()[0]
                    .get(0);
                assert_eq!(id, support_edge, "{role} {kind}");
            }
            // The same pair in billing is an Edge of its own.
            let billing_edge: Uuid = tx
                .query_typed(
                    "select gnomon.create_edge($1, 'billing', $2, $3, $4)",
                    &[
                        (&t1, Type::UUID),
                        (&kind, Type::TEXT),
                        (&low, Type::UUID),
                        (&high, Type::UUID),
                    ],
                )
                .await
                .unwrap()[0]
                .get(0);
            assert_ne!(billing_edge, support_edge);
            // Both hold, each citing an Episode of its own Scope.
            for (edge, episode) in [
                (support_edge, e.n.w.t1_support),
                (billing_edge, e.n.w.t1_billing),
            ] {
                tx.execute_typed(
                    "select gnomon.write_edge($1, $2, $3, null, null, null, null, null, null)",
                    &[
                        (&t1, Type::UUID),
                        (&edge, Type::UUID),
                        (&episode, Type::UUID),
                    ],
                )
                .await
                .unwrap();
            }
            tx.commit().await.unwrap();

            // A caller of support alone neither sees the billing Edge nor
            // collides with it: its Edges of the pair are support's.
            let mut client = e.n.w.db.login().await;
            let tx = as_caller(&mut client, role, &claims(t1, &["support"])).await;
            let id: Uuid = tx
                .query_typed(
                    CREATE_EDGE,
                    &[
                        (&t1, Type::UUID),
                        (&kind, Type::TEXT),
                        (&high, Type::UUID),
                        (&low, Type::UUID),
                    ],
                )
                .await
                .unwrap()[0]
                .get(0);
            assert_eq!(id, support_edge, "{role} {kind}");
            let seen = visible_ids(
                &tx,
                "select id from gnomon.edge where kind = $1 and $2 in (source_id, target_id)",
                &[(&kind, Type::TEXT), (&m1, Type::UUID)],
            )
            .await;
            assert_eq!(seen, vec![support_edge], "{role} saw billing's {kind} Edge");
            // Traversed from the round's own Node, which no earlier round's
            // Edge joins.
            let traversed = visible_ids(
                &tx,
                "select edge_id from gnomon.traverse($1, $2, now(), null, 'both', array[$3])",
                &[(&t1, Type::UUID), (&m1, Type::UUID), (&kind, Type::TEXT)],
            )
            .await;
            assert_eq!(
                traversed,
                vec![support_edge],
                "{role} traversed billing's {kind} Edge"
            );
            tx.rollback().await.unwrap();

            // Tenant two reads neither, by ID or by table, and addresses
            // neither by naming Tenant one's Nodes.
            let mut client = e.n.w.db.login().await;
            let tx = as_caller(&mut client, role, &claims(t2, &["support", "billing"])).await;
            let seen = visible_ids(
                &tx,
                "select id from gnomon.edge where id = any($1)",
                &[(&vec![support_edge, billing_edge], Type::UUID_ARRAY)],
            )
            .await;
            assert!(seen.is_empty(), "{role} of Tenant two saw {seen:?}");
            let r = tx
                .execute_typed(
                    CREATE_EDGE,
                    &[
                        (&t2, Type::UUID),
                        (&kind, Type::TEXT),
                        (&low, Type::UUID),
                        (&high, Type::UUID),
                    ],
                )
                .await;
            assert_invalid_write(
                r,
                &format!("the source Node {low} is not visible to the caller"),
            );
            tx.rollback().await.unwrap();
        }
    }
}

#[tokio::test]
async fn no_caller_can_register_rewrite_or_remove_a_reserved_kind_even_with_the_grant() {
    let w = world().await;
    w.db.grant_kind_registration(USER).await;
    let before = kind_rows(&w.db, w.t1).await;
    for kind in RESERVED {
        let mut client = w.db.login().await;
        let tx = as_caller(&mut client, USER, &claims(w.t1, &["support"])).await;
        for (sql, what) in [
            (
                "select gnomon.register_kind($1, $2, 'edge', null, '{}', '{}', null, null)",
                "register it",
            ),
            (
                "select gnomon.register_kind($1, $2, 'edge', 'edge', '{\"properties\": {\"weight\": {\"type\": \"number\"}}}', '{}', '{node}', '{node}')",
                "redefine it",
            ),
            (
                "select gnomon.register_kind($1, $2, 'node', null, '{}', '{}', null, null)",
                "reuse its name for a Node Kind",
            ),
            (
                "select gnomon.register_kind($1, 'duplicate_of', 'edge', $2, '{}', '{}', null, null)",
                "specialize it",
            ),
            (
                "select gnomon.set_merge_rule($1, $2, 'ranked')",
                "set its merge rule",
            ),
        ] {
            tx.batch_execute("savepoint attempt").await.unwrap();
            let r = tx
                .execute_typed(sql, &[(&w.t1, Type::UUID), (&kind, Type::TEXT)])
                .await;
            assert_code(r, gnomon::INVALID_KIND, &format!("{what}: {kind}"));
            tx.batch_execute("rollback to savepoint attempt")
                .await
                .unwrap();
        }
        for (sql, what) in [
            (
                "insert into gnomon.kind (tenant_id, name, kind_of, parent, ancestors, properties, search,
                   source_kinds, target_kinds, definition)
                 values ($1, $2, 'edge', 'edge', '{edge}', '{}', '{}', '{node}', '{node}', '{}')",
                "insert it",
            ),
            ("delete from gnomon.kind where tenant_id = $1 and name = $2", "delete it"),
            (
                "update gnomon.kind set name = $2 where tenant_id = $1",
                "rename a Kind to it",
            ),
            (
                "insert into gnomon.kind_merge (tenant_id, kind, merge_rule) values ($1, $2, 'ranked')",
                "give it a merge rule",
            ),
        ] {
            tx.batch_execute("savepoint attempt").await.unwrap();
            let r = tx
                .execute_typed(sql, &[(&w.t1, Type::UUID), (&kind, Type::TEXT)])
                .await;
            assert_refused(r, &format!("{what}: {kind}"));
            tx.batch_execute("rollback to savepoint attempt").await.unwrap();
        }
        // The trigger that refuses the names stays on.
        let r = tx
            .execute_typed(
                "alter table gnomon.kind disable trigger kind_check_reserved_name",
                &[],
            )
            .await;
        assert_refused(r, "turning the reserved-name check off");
        drop(tx);

        let mut client = w.db.login().await;
        let tx = as_caller(&mut client, USER, &claims(w.t1, &["support"])).await;
        let row = tx
            .query_typed(
                "select kind_of, parent, source_kinds, target_kinds, is_symmetric
                 from gnomon.read_kind($1, $2)",
                &[(&w.t1, Type::UUID), (&kind, Type::TEXT)],
            )
            .await
            .unwrap();
        assert_eq!(row.len(), 1, "{kind}");
        let read: (String, String, Vec<String>, Vec<String>, bool) = (
            row[0].get(0),
            row[0].get(1),
            row[0].get(2),
            row[0].get(3),
            row[0].get(4),
        );
        assert_eq!(
            read,
            (
                "edge".into(),
                "edge".into(),
                vec!["node".into()],
                vec!["node".into()],
                true
            ),
            "{kind}"
        );
    }
    assert_eq!(
        kind_rows(&w.db, w.t1).await,
        before,
        "a refused call wrote a Kind"
    );
}

// Correction, End of validity and Retraction (ADR-0007, ADR-0009). A
// statement is held like a Contribution: callers read it and write it only
// through the Store, and nothing changes or deletes it but Erasure. The merge
// reads a statement only through the Contributions that carry its ID.

const END_VALIDITY: &str =
    "select status from gnomon.end_validity($1, $2, false, $3, $4::text::timestamptz, 'instant')";
const RETRACT_VERSION: &str = "select status from gnomon.retract_version($1, $2, false, $3, $4)";

#[tokio::test]
async fn a_caller_cannot_end_or_retract_in_another_tenant_or_scope_or_a_version_that_is_not_current()
 {
    let n = nodes().await;
    let (t1, t2) = (n.w.t1, n.w.t2);
    for role in [USER, WORKER] {
        // Another Tenant's claims, naming tenant one.
        for (sql, version) in [(END_VALIDITY, None), (RETRACT_VERSION, Some(n.open))] {
            let mut client = n.w.db.login().await;
            let tx = as_caller(&mut client, role, &claims(t2, &["support"])).await;
            let r = match version {
                None => {
                    tx.query_typed(
                        sql,
                        &[
                            (&t1, Type::UUID),
                            (&n.n1, Type::UUID),
                            (&n.w.t1_support, Type::UUID),
                            (&"2024-03-01", Type::TEXT),
                        ],
                    )
                    .await
                }
                Some(v) => {
                    tx.query_typed(
                        sql,
                        &[
                            (&t1, Type::UUID),
                            (&n.n1, Type::UUID),
                            (&n.w.t1_support, Type::UUID),
                            (&v, Type::UUID),
                        ],
                    )
                    .await
                }
            };
            assert_refused(r, &format!("{role}: state about another Tenant's Node"));
        }
        // The right Tenant, but a Scope the claims do not grant.
        let mut client = n.w.db.login().await;
        let tx = as_caller(&mut client, role, &claims(t1, &["billing"])).await;
        let err = tx
            .query_typed(
                RETRACT_VERSION,
                &[
                    (&t1, Type::UUID),
                    (&n.n1, Type::UUID),
                    (&n.w.t1_billing, Type::UUID),
                    (&n.open, Type::UUID),
                ],
            )
            .await
            .unwrap_err();
        assert_eq!(
            err.code().map(|c| c.code()),
            Some("GNW01"),
            "{role}: {err:?}"
        );

        // A closed Version is history, and another Node's Version is not
        // this Node's.
        for version in [n.closed, n.n2_open] {
            let mut client = n.w.db.login().await;
            let tx = as_caller(&mut client, role, &claims(t1, &["support"])).await;
            let err = tx
                .query_typed(
                    RETRACT_VERSION,
                    &[
                        (&t1, Type::UUID),
                        (&n.n1, Type::UUID),
                        (&n.w.t1_support, Type::UUID),
                        (&version, Type::UUID),
                    ],
                )
                .await
                .unwrap_err();
            assert_eq!(
                err.code().map(|c| c.code()),
                Some("GNW01"),
                "{role}: {err:?}"
            );
        }
    }
    let rows =
        n.w.db
            .admin()
            .await
            .query_typed("select count(*) from gnomon.statement", &[])
            .await
            .unwrap();
    assert_eq!(
        rows[0].get::<_, i64>(0),
        0,
        "no refused call left a statement"
    );
}

#[tokio::test]
async fn a_correction_closes_the_earlier_version_and_no_caller_can_reopen_or_rewrite_it() {
    let n = nodes().await;
    let t1 = n.w.t1;
    let mut client = n.w.db.login().await;
    let tx = as_caller(&mut client, WORKER, &claims(t1, &["support"])).await;
    tx.query_typed(
        "select status from gnomon.correct_node($1, $2, $3, null, null, null, null,
           '{\"name\": \"corrected\"}', null)",
        &[
            (&t1, Type::UUID),
            (&n.n1, Type::UUID),
            (&n.w.t1_support, Type::UUID),
        ],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    // The earlier Version is closed, and its Recorded time is not rewritten.
    let rows =
        n.w.db
            .admin()
            .await
            .query_typed(
                "select recorded_to < 'infinity', recorded_from < recorded_to
             from gnomon.version where id = $1",
                &[(&n.open, Type::UUID)],
            )
            .await
            .unwrap();
    assert!(rows[0].get::<_, bool>(0));
    for sql in [
        "update gnomon.version set recorded_to = 'infinity' where id = $1",
        "update gnomon.version set recorded_to = recorded_from + interval '1 microsecond'
           where id = $1",
        "delete from gnomon.version where id = $1",
    ] {
        let mut client = n.w.db.login().await;
        let tx = as_caller(&mut client, WORKER, &claims(t1, &["support"])).await;
        let r = tx.execute_typed(sql, &[(&n.open, Type::UUID)]).await;
        assert!(r.is_err(), "{sql} was allowed: {r:?}");
    }
    let rows =
        n.w.db
            .admin()
            .await
            .query_typed(
                "select recorded_to < 'infinity' from gnomon.version where id = $1",
                &[(&n.open, Type::UUID)],
            )
            .await
            .unwrap();
    assert!(
        rows[0].get::<_, bool>(0),
        "the earlier Version is still closed"
    );
}

// Retract Episode (ADR-0003, ADR-0007, ADR-0009). It runs as the invoker, so
// a caller gets no UPDATE or DELETE right from it, deletes no committed
// history, and cannot retract an Episode outside its Tenant and Scopes. The
// marker that stops an Episode's statements counting is written only by it.

const RETRACT_EPISODE: &str = "select versions from gnomon.retract_episode($1, $2)";

#[tokio::test]
async fn retract_episode_deletes_no_committed_row_and_closes_the_version_in_recorded_time_only() {
    let n = nodes().await;
    let t1 = n.w.t1;
    let admin = n.w.db.admin().await;
    let counts = || async {
        admin
            .query_typed(
                "select (select count(*) from gnomon.version),
                        (select count(*) from gnomon.citation),
                        (select count(*) from gnomon.contribution),
                        (select count(*) from gnomon.episode)",
                &[],
            )
            .await
            .unwrap()
            .iter()
            .flat_map(|r| (0..4).map(|i| r.get::<_, i64>(i)).collect::<Vec<_>>())
            .collect::<Vec<_>>()
    };
    let before = counts().await;
    for role in [USER, WORKER] {
        let mut client = n.w.db.login().await;
        let tx = as_caller(&mut client, role, &claims(t1, &["support"])).await;
        let rows = tx
            .query_typed(
                RETRACT_EPISODE,
                &[(&t1, Type::UUID), (&n.w.t1_support, Type::UUID)],
            )
            .await
            .unwrap();
        let retracted: Vec<Uuid> = rows[0].get(0);
        // The first role retracts the open Version, the second finds none.
        assert_eq!(retracted.len(), usize::from(role == USER), "{role}");
        tx.commit().await.unwrap();
    }
    let after = counts().await;
    assert_eq!(after[0], before[0], "no Version row deleted or added");
    assert_eq!(after[1], before[1], "no Citation deleted");
    assert!(after[2] > before[2], "Contributions only added to");
    assert_eq!(after[3], before[3], "the Episode stays");
    let open = admin
        .query_typed(
            "select recorded_to <> 'infinity' from gnomon.version where id = $1",
            &[(&n.open, Type::UUID)],
        )
        .await
        .unwrap();
    assert!(open[0].get::<_, bool>(0), "closed in Recorded time");
    // Tenant two is untouched.
    let other = admin
        .query_typed(
            "select count(*) from gnomon.version where tenant_id = $1 and recorded_to = 'infinity'",
            &[(&n.w.t2, Type::UUID)],
        )
        .await
        .unwrap();
    assert_eq!(other[0].get::<_, i64>(0), 1);
}

#[tokio::test]
async fn a_caller_cannot_retract_an_episode_of_another_tenant_or_a_scope_it_cannot_see() {
    let n = nodes().await;
    let (t1, t2) = (n.w.t1, n.w.t2);
    for role in [USER, WORKER] {
        // Tenant two's Episode, named under Tenant one's claims.
        let mut client = n.w.db.login().await;
        let tx = as_caller(&mut client, role, &claims(t1, &["support"])).await;
        assert_refused(
            tx.query_typed(
                RETRACT_EPISODE,
                &[(&t2, Type::UUID), (&n.w.t2_support, Type::UUID)],
            )
            .await,
            &format!("{role}: another Tenant's Episode by its Tenant"),
        );
        let mut client = n.w.db.login().await;
        let tx = as_caller(&mut client, role, &claims(t1, &["support"])).await;
        assert_invalid_write(
            tx.query_typed(
                RETRACT_EPISODE,
                &[(&t1, Type::UUID), (&n.w.t2_support, Type::UUID)],
            )
            .await,
            &format!("Episode {} is not visible to the caller", n.w.t2_support),
        );
        // Tenant one's Episode in a Scope the claims do not name.
        let mut client = n.w.db.login().await;
        let tx = as_caller(&mut client, role, &claims(t1, &["support"])).await;
        assert_invalid_write(
            tx.query_typed(
                RETRACT_EPISODE,
                &[(&t1, Type::UUID), (&n.w.t1_billing, Type::UUID)],
            )
            .await,
            &format!("Episode {} is not visible to the caller", n.w.t1_billing),
        );
    }
    let rows =
        n.w.db
            .admin()
            .await
            .query_typed("select count(*) from gnomon.episode_retraction", &[])
            .await
            .unwrap();
    assert_eq!(rows[0].get::<_, i64>(0), 0);
}

// Erase Episode (ADR-0003, ADR-0009). The one definer function that deletes
// and rewrites committed history: granted to no caller role by the
// migrations, refused across Tenants and Scopes, and the guard triggers stand
// aside for it alone, because only the NOLOGIN definer role passes them.

const ERASE_EPISODE: &str = "select erased_nodes from gnomon.erase_episode($1, $2)";

/// Every row of every gnomon table, counted past RLS.
async fn row_count(db: &TestDb) -> i64 {
    let admin = db.admin().await;
    let mut total = 0;
    for table in tenant_tables(&admin).await {
        total += admin
            .query_typed(&format!("select count(*) from gnomon.{table}"), &[])
            .await
            .unwrap()[0]
            .get::<_, i64>(0);
    }
    total
}

#[tokio::test]
async fn the_migrations_grant_erasure_to_no_role_but_its_owner() {
    let w = world().await;
    let client = w.db.admin().await;
    for erasure in support::ERASURES {
        let rows = client
            .query_typed(
                &format!(
                    "select coalesce(nullif(a.grantee, 0)::regrole::text, 'public'),
                            p.proowner::regrole::text, p.prosecdef
                     from pg_proc p left join lateral aclexplode(p.proacl) a on true
                     where p.oid = '{erasure}'::regprocedure"
                ),
                &[],
            )
            .await
            .unwrap();
        let owner: String = rows[0].get(1);
        assert_eq!(owner, "gnomon_eraser", "{erasure}");
        assert!(
            rows[0].get::<_, bool>(2),
            "{erasure} is not SECURITY DEFINER"
        );
        let grantees: Vec<String> = rows
            .iter()
            .filter_map(|r| r.get::<_, Option<String>>(0))
            .filter(|g| g != "gnomon_eraser")
            .collect();
        assert!(grantees.is_empty(), "{erasure} is granted to {grantees:?}");
    }
}

#[tokio::test]
async fn a_caller_role_without_the_grant_cannot_erase() {
    let n = nodes().await;
    let before = row_count(&n.w.db).await;
    for role in [USER, WORKER] {
        let mut client = n.w.db.login().await;
        let tx = as_caller(&mut client, role, &claims(n.w.t1, &["support"])).await;
        let r = tx
            .query_typed(
                ERASE_EPISODE,
                &[(&n.w.t1, Type::UUID), (&n.w.t1_support, Type::UUID)],
            )
            .await;
        assert_refused(r, &format!("{role} erasing without the grant"));
    }
    assert_eq!(row_count(&n.w.db).await, before, "nothing was erased");
}

#[tokio::test]
async fn a_caller_with_the_grant_cannot_erase_in_another_tenant_or_a_scope_it_cannot_see() {
    let n = nodes().await;
    n.w.db.grant_erasure(WORKER).await;
    let before = row_count(&n.w.db).await;
    let cases = [
        // The call names another Tenant.
        (
            n.w.t2,
            n.w.t2_support,
            &["support"][..],
            "another Tenant",
            true,
        ),
        // The call names the caller's Tenant and another Tenant's Episode.
        (
            n.w.t1,
            n.w.t2_support,
            &["support"][..],
            "another Tenant's Episode",
            false,
        ),
        // The Episode is the caller's Tenant's, in a Scope it cannot see.
        (
            n.w.t1,
            n.w.t1_billing,
            &["support"][..],
            "a Scope the caller cannot see",
            false,
        ),
        // The call names the nil Tenant.
        (
            Uuid::nil(),
            n.w.t1_support,
            &["support"][..],
            "the nil Tenant",
            true,
        ),
    ];
    for (tenant, episode, scopes, what, privilege) in cases {
        let mut client = n.w.db.login().await;
        let tx = as_caller(&mut client, WORKER, &claims(n.w.t1, scopes)).await;
        let r = tx
            .query_typed(
                ERASE_EPISODE,
                &[(&tenant, Type::UUID), (&episode, Type::UUID)],
            )
            .await;
        if privilege {
            assert_refused(r, what);
        } else {
            match r {
                Err(e) => assert_eq!(e.code().map(|c| c.code()), Some("GNW01"), "{what}: {e:?}"),
                Ok(v) => panic!("erasing {what} was allowed: {v:?}"),
            }
        }
    }
    // Claims that bind another Tenant cannot erase in the caller's Tenant either.
    let mut client = n.w.db.login().await;
    let tx = as_caller(&mut client, WORKER, &claims(n.w.t2, &["support"])).await;
    let r = tx
        .query_typed(
            ERASE_EPISODE,
            &[(&n.w.t1, Type::UUID), (&n.w.t1_support, Type::UUID)],
        )
        .await;
    assert_refused(r, "claims of another Tenant");
    assert_eq!(row_count(&n.w.db).await, before, "nothing was erased");
}

/// The Nodes fixture with a second Episode of Tenant one that names Node
/// `n1` too, so that erasing the first rewrites `n1`'s history rather than
/// deleting it.
async fn erasable(n: &Nodes) -> Uuid {
    let store = Store::connect(&n.w.db.login_url(), &support::connect_options())
        .await
        .unwrap();
    let caller = store.as_worker(TenantId(n.w.t1), Scope::new("support"), None);
    let f = caller
        .record_episode(&NewEpisode {
            tenant: Some(TenantId(n.w.t1)),
            scope: Scope::new("support"),
            content: "another source".into(),
            reference_time: "2024-02-01T00:00:00Z".parse().unwrap(),
            authority: None,
        })
        .await
        .unwrap();
    caller
        .write_node(
            TenantId(n.w.t1),
            gnomon::NodeId(n.n1),
            &gnomon::Patch::new(
                f,
                gnomon::ValidTime::UNKNOWN,
                match json!({ "aliases": ["kept"] }) {
                    serde_json::Value::Object(m) => m,
                    _ => unreachable!(),
                },
            ),
        )
        .await
        .unwrap();
    n.w.db.grant_erasure(WORKER).await;
    f.0
}

#[tokio::test]
async fn a_caller_cannot_forge_the_erasure_bypass_or_delete_what_an_erasure_rewrote() {
    let n = nodes().await;
    erasable(&n).await;
    let t1 = n.w.t1;

    // The bypass is the eraser role itself, which no caller can become, nor
    // the other definer roles. No setting a caller can change turns it on,
    // and no caller may even ask whether it is on.
    for role in [USER, WORKER] {
        for definer in ["gnomon_eraser", "gnomon_definer", "gnomon_writer"] {
            let mut client = n.w.db.login().await;
            let tx = as_caller(&mut client, role, &claims(t1, &["support"])).await;
            let r = tx.batch_execute(&format!("set local role {definer}")).await;
            assert_refused(r.map(|_| ()), &format!("{role} becoming {definer}"));
        }
        let mut client = n.w.db.login().await;
        let tx = as_caller(&mut client, role, &claims(t1, &["support"])).await;
        tx.batch_execute("select set_config('gnomon.running_as_eraser', 'on', true)")
            .await
            .unwrap();
        let r = tx
            .query_typed("select gnomon.running_as_eraser()", &[])
            .await;
        assert_refused(r, &format!("{role} asking whether the bypass is on"));
        let mut client = n.w.db.login().await;
        let tx = as_caller(&mut client, role, &claims(t1, &["support"])).await;
        tx.batch_execute("select set_config('gnomon.running_as_eraser', 'on', true)")
            .await
            .unwrap();
        let r = tx
            .execute_typed(
                "delete from gnomon.version where id = $1",
                &[(&n.closed, Type::UUID)],
            )
            .await;
        assert_refused(r, &format!("{role} deleting a committed Version"));
    }

    // Erasure's parts are not callable on their own.
    for (sql, what) in [
        (
            "select gnomon.replay_versions($1, 'support', $2, false, 'node', '-infinity', '{}')",
            "the replay",
        ),
        (
            "select gnomon.erase_episode_rows($1, $2)",
            "the Episode's deletes",
        ),
        (
            "select gnomon.erase_version_text($1, array[$2])",
            "the text deletes",
        ),
    ] {
        let mut client = n.w.db.login().await;
        let tx = as_caller(&mut client, WORKER, &claims(t1, &["support"])).await;
        let r = tx
            .query_typed(sql, &[(&t1, Type::UUID), (&n.n1, Type::UUID)])
            .await;
        assert_refused(r, &format!("calling {what}"));
    }
    // No caller may run the shared merge, with a replay, another Recorded time
    // or none.
    for (at, replay, what) in [
        ("now()", true, "a replay"),
        (
            "now() - interval '1 day'",
            false,
            "an earlier Recorded time",
        ),
    ] {
        let mut client = n.w.db.login().await;
        let tx = as_caller(&mut client, WORKER, &claims(t1, &["support"])).await;
        let r = tx
            .query_typed(
                &format!(
                    "select gnomon.merge_versions($1, 'support', $2, false, '-infinity', \
                     'infinity', {at}, 'recency', $3)"
                ),
                &[
                    (&t1, Type::UUID),
                    (&n.n1, Type::UUID),
                    (&replay, Type::BOOL),
                ],
            )
            .await;
        assert_refused(r, &format!("merging {what}"));
    }

    // Nor to reach another Tenant's Node.
    let mut client = n.w.db.login().await;
    let tx = as_caller(&mut client, WORKER, &claims(t1, &["support"])).await;
    let r = tx
        .query_typed(
            "select gnomon.merge_versions($1, 'support', $2, false, '-infinity', 'infinity', \
             transaction_timestamp(), 'recency', false)",
            &[(&n.w.t2, Type::UUID), (&n.n2, Type::UUID)],
        )
        .await;
    assert_refused(r, "merging another Tenant's Node directly");

    // After an Erasure in the same transaction, what it rewrote is still
    // committed history to the caller: the replay marks it written by no
    // transaction.
    let before = version_rows(&n.w.db, n.n1).await;
    for (sql, what) in [
        (
            "delete from gnomon.version where id = $1",
            "delete a rewritten Version",
        ),
        (
            "delete from gnomon.citation where version_id = $1",
            "delete a rewritten Version's Citations",
        ),
        (
            "update gnomon.version set recorded_to = transaction_timestamp() - interval '1 day'
             where id = $1",
            "rewrite a rewritten Version's Recorded time",
        ),
    ] {
        let mut client = n.w.db.login().await;
        let tx = as_caller(&mut client, WORKER, &claims(t1, &["support"])).await;
        tx.query_typed(
            ERASE_EPISODE,
            &[(&t1, Type::UUID), (&n.w.t1_support, Type::UUID)],
        )
        .await
        .unwrap();
        let rewritten = tx
            .query_typed(
                "select id from gnomon.version where node_id = $1 and written_in = '0' limit 1",
                &[(&n.n1, Type::UUID)],
            )
            .await
            .unwrap();
        assert_eq!(
            rewritten.len(),
            1,
            "the Erasure replayed the other Episode's write"
        );
        let id: Uuid = rewritten[0].get(0);
        let r = tx.execute_typed(sql, &[(&id, Type::UUID)]).await;
        assert_refused(r, what);
    }
    assert_eq!(
        version_rows(&n.w.db, n.n1).await,
        before,
        "nothing was committed"
    );

    // A caller has no DELETE on what only Erasure deletes.
    for table in [
        "episode",
        "contribution",
        "statement",
        "bound_statement",
        "node",
        "edge",
    ] {
        let mut client = n.w.db.login().await;
        let tx = as_caller(&mut client, WORKER, &claims(t1, &["support"])).await;
        let r = tx
            .execute_typed(&format!("delete from gnomon.{table}"), &[])
            .await;
        assert_refused(r, &format!("deleting from gnomon.{table}"));
    }
}

#[tokio::test]
async fn an_erasure_refuses_a_transaction_that_already_wrote_what_the_episode_touched() {
    let n = nodes().await;
    let f = erasable(&n).await;
    let t1 = n.w.t1;
    let mut client = n.w.db.login().await;
    let tx = as_caller(&mut client, WORKER, &claims(t1, &["support"])).await;
    write_node(
        &tx,
        t1,
        n.n1,
        Some(f),
        json!({ "name": "same transaction" }),
    )
    .await
    .unwrap();
    let r = tx
        .query_typed(
            ERASE_EPISODE,
            &[(&t1, Type::UUID), (&n.w.t1_support, Type::UUID)],
        )
        .await;
    match r {
        Err(e) => assert_eq!(e.code().map(|c| c.code()), Some("GNW01"), "{e:?}"),
        Ok(v) => panic!("an Erasure after a write in its own transaction was allowed: {v:?}"),
    }
}

// Erase a Node, an Edge or a Scope (ADR-0003, ADR-0005, ADR-0009). Erasure
// as the eraser: granted to no caller role, refused across Tenants and for
// what the caller cannot see, and its reach into other Scopes, which erases
// the Edges joining an erased Node, is open to no caller.

const ERASE_NODE: &str = "select erased_nodes from gnomon.erase_node($1, $2)";
const ERASE_EDGE: &str = "select erased_edges from gnomon.erase_edge($1, $2)";
const ERASE_SCOPE: &str = "select erased_nodes from gnomon.erase_scope($1, $2)";

/// The Nodes fixture with more of Tenant one's graph: a Node of billing, an
/// Edge of billing that joins only it, an Edge of billing that joins `n1`,
/// and an Edge of Tenant two.
struct Graph {
    n: Nodes,
    billing_node: Uuid,
    billing_edge: Uuid,
    crossing_edge: Uuid,
    t2_edge: Uuid,
}

async fn graph() -> Graph {
    let n = nodes().await;
    let store = Store::connect(&n.w.db.login_url(), &support::connect_options())
        .await
        .unwrap();
    let t1 = TenantId(n.w.t1);
    let both = store.as_user(
        match json!({
            "role": "authenticated",
            "app_metadata": { "tenant_id": n.w.t1.to_string(), "scopes": ["support", "billing"] }
        }) {
            serde_json::Value::Object(m) => m,
            _ => unreachable!(),
        },
    );
    let patch = |episode: Uuid| {
        gnomon::Patch::new(
            EpisodeId(episode),
            gnomon::ValidTime::UNKNOWN,
            serde_json::Map::new(),
        )
    };
    let (billing_node, _) = both
        .create_node(
            &gnomon::NewNode {
                tenant: Some(t1),
                scope: Scope::new("billing"),
                kind: gnomon::ROOT_NODE_KIND.into(),
            },
            &gnomon::Patch::new(
                EpisodeId(n.w.t1_billing),
                gnomon::ValidTime::UNKNOWN,
                match json!({ "name": "billing's" }) {
                    serde_json::Value::Object(m) => m,
                    _ => unreachable!(),
                },
            ),
        )
        .await
        .unwrap();
    let edge = |scope: &'static str, source: Uuid, target: Uuid, episode: Uuid| {
        let both = &both;
        let patch = &patch;
        async move {
            both.create_edge(
                &gnomon::NewEdge {
                    tenant: Some(t1),
                    scope: Scope::new(scope),
                    kind: gnomon::ROOT_EDGE_KIND.into(),
                    source: gnomon::NodeId(source),
                    target: gnomon::NodeId(target),
                },
                &patch(episode),
            )
            .await
            .unwrap()
            .0
            .0
        }
    };
    let billing_edge = edge("billing", billing_node.0, billing_node.0, n.w.t1_billing).await;
    let crossing_edge = edge("billing", billing_node.0, n.n1, n.w.t1_billing).await;
    let t2_edge = store
        .as_worker(TenantId(n.w.t2), Scope::new("support"), None)
        .create_edge(
            &gnomon::NewEdge {
                tenant: Some(TenantId(n.w.t2)),
                scope: Scope::new("support"),
                kind: gnomon::ROOT_EDGE_KIND.into(),
                source: gnomon::NodeId(n.n2),
                target: gnomon::NodeId(n.n2),
            },
            &patch(n.w.t2_support),
        )
        .await
        .unwrap()
        .0
        .0;
    n.w.db.grant_erasure(WORKER).await;
    Graph {
        billing_node: billing_node.0,
        billing_edge,
        crossing_edge,
        t2_edge,
        n,
    }
}

#[tokio::test]
async fn a_caller_role_without_the_grant_cannot_erase_a_node_an_edge_or_a_scope() {
    let n = nodes().await;
    let before = row_count(&n.w.db).await;
    let t1 = n.w.t1;
    let edge: Uuid = {
        let mut client = n.w.db.login().await;
        let tx = as_caller(&mut client, WORKER, &claims(t1, &["support"])).await;
        let id = tx
            .query_typed(
                "select gnomon.create_edge($1, 'support', 'edge', $2, $2)",
                &[(&t1, Type::UUID), (&n.n1, Type::UUID)],
            )
            .await
            .unwrap()[0]
            .get(0);
        tx.commit().await.unwrap();
        id
    };
    let before = before + 1;
    for role in [USER, WORKER] {
        for (sql, id, what) in [
            (ERASE_NODE, Some(n.n1), "a Node"),
            (ERASE_EDGE, Some(edge), "an Edge"),
            (ERASE_SCOPE, None, "a Scope"),
        ] {
            let mut client = n.w.db.login().await;
            let tx = as_caller(&mut client, role, &claims(t1, &["support"])).await;
            let r = match id {
                Some(id) => {
                    tx.query_typed(sql, &[(&t1, Type::UUID), (&id, Type::UUID)])
                        .await
                }
                None => {
                    tx.query_typed(sql, &[(&t1, Type::UUID), (&"support", Type::TEXT)])
                        .await
                }
            };
            assert_refused(r, &format!("{role} erasing {what} without the grant"));
        }
    }
    assert_eq!(row_count(&n.w.db).await, before, "nothing was erased");
}

#[tokio::test]
async fn a_caller_with_the_grant_cannot_erase_another_tenants_node_edge_or_scope_or_one_it_cannot_see()
 {
    let g = graph().await;
    let (t1, t2) = (g.n.w.t1, g.n.w.t2);
    let before = row_count(&g.n.w.db).await;
    // (claims' Tenant, the call's Tenant, the call, the target, what, refused
    // for privilege rather than as not visible)
    enum Target {
        Id(Uuid),
        Scope(&'static str),
    }
    let cases = [
        (
            t1,
            t2,
            ERASE_NODE,
            Target::Id(g.n.n2),
            "another Tenant's Node",
            true,
        ),
        (
            t1,
            t1,
            ERASE_NODE,
            Target::Id(g.n.n2),
            "another Tenant's Node by ID",
            false,
        ),
        (
            t1,
            t1,
            ERASE_NODE,
            Target::Id(g.billing_node),
            "a Node of a Scope it cannot see",
            false,
        ),
        (
            t1,
            Uuid::nil(),
            ERASE_NODE,
            Target::Id(g.n.n1),
            "a Node of the nil Tenant",
            true,
        ),
        (
            t2,
            t1,
            ERASE_NODE,
            Target::Id(g.n.n1),
            "a Node under another Tenant's claims",
            true,
        ),
        (
            t1,
            t2,
            ERASE_EDGE,
            Target::Id(g.t2_edge),
            "another Tenant's Edge",
            true,
        ),
        (
            t1,
            t1,
            ERASE_EDGE,
            Target::Id(g.t2_edge),
            "another Tenant's Edge by ID",
            false,
        ),
        (
            t1,
            t1,
            ERASE_EDGE,
            Target::Id(g.billing_edge),
            "an Edge of a Scope it cannot see",
            false,
        ),
        (
            t1,
            t1,
            ERASE_EDGE,
            Target::Id(g.crossing_edge),
            "an Edge of a Scope it cannot see that joins its Node",
            false,
        ),
        (
            t1,
            t2,
            ERASE_SCOPE,
            Target::Scope("support"),
            "another Tenant's Scope",
            true,
        ),
        (
            t1,
            t1,
            ERASE_SCOPE,
            Target::Scope("billing"),
            "a Scope it cannot see",
            false,
        ),
        (
            t2,
            t1,
            ERASE_SCOPE,
            Target::Scope("support"),
            "a Scope under another Tenant's claims",
            true,
        ),
    ];
    for (claimed, tenant, sql, target, what, privilege) in cases {
        let mut client = g.n.w.db.login().await;
        let tx = as_caller(&mut client, WORKER, &claims(claimed, &["support"])).await;
        let r = match target {
            Target::Id(id) => {
                tx.query_typed(sql, &[(&tenant, Type::UUID), (&id, Type::UUID)])
                    .await
            }
            Target::Scope(s) => {
                tx.query_typed(sql, &[(&tenant, Type::UUID), (&s, Type::TEXT)])
                    .await
            }
        };
        if privilege {
            assert_refused(r, what);
        } else {
            match r {
                Err(e) => assert_eq!(e.code().map(|c| c.code()), Some("GNW01"), "{what}: {e:?}"),
                Ok(v) => panic!("erasing {what} was allowed: {v:?}"),
            }
        }
    }
    assert_eq!(row_count(&g.n.w.db).await, before, "nothing was erased");
}

#[tokio::test]
async fn no_caller_calls_erasures_parts_and_an_erasure_widens_nothing_after_it() {
    let g = graph().await;
    let t1 = g.n.w.t1;
    for role in [USER, WORKER] {
        for (sql, what) in [
            (
                "select gnomon.erase_versioned_rows($1, array[$2])",
                "the row deletes",
            ),
            (
                "select gnomon.erase_empty_nodes($1, array[$2])",
                "the deletes of the Nodes left with nothing",
            ),
            (
                "select gnomon.erase_graph($1, null, array[$2], '{}')",
                "the shared Erasure",
            ),
        ] {
            let mut client = g.n.w.db.login().await;
            let tx = as_caller(&mut client, role, &claims(t1, &["support", "billing"])).await;
            let r = tx
                .query_typed(sql, &[(&t1, Type::UUID), (&g.n.n1, Type::UUID)])
                .await;
            assert_refused(r, &format!("{role} {what}"));
        }
    }

    // The worker of support erases n1, which an Edge of billing joins, and
    // that Edge goes. In the same transaction the worker still sees nothing
    // of billing and still cannot erase billing's other Edge.
    let mut client = g.n.w.db.login().await;
    let tx = as_caller(&mut client, WORKER, &claims(t1, &["support"])).await;
    let edges: Vec<Uuid> = tx
        .query_typed(
            "select erased_edges from gnomon.erase_node($1, $2)",
            &[(&t1, Type::UUID), (&g.n.n1, Type::UUID)],
        )
        .await
        .unwrap()[0]
        .get(0);
    // The Edge went, and the result does not name it: it is billing's.
    assert!(
        edges.is_empty(),
        "the result names billing's Edge {edges:?}"
    );
    for table in ["edge", "contribution", "version", "citation", "node"] {
        let n: i64 = tx
            .query_typed(
                &format!("select count(*) from gnomon.{table} where scope = 'billing'"),
                &[],
            )
            .await
            .unwrap()[0]
            .get(0);
        assert_eq!(
            n, 0,
            "the worker sees billing's {table} rows after an Erasure"
        );
    }
    let mut tx = tx;
    let attempt = tx.savepoint("attempt").await.unwrap();
    let r = attempt
        .query_typed(
            ERASE_EDGE,
            &[(&t1, Type::UUID), (&g.billing_edge, Type::UUID)],
        )
        .await;
    match r {
        Err(e) => assert_eq!(e.code().map(|c| c.code()), Some("GNW01"), "{e:?}"),
        Ok(v) => panic!("an Erasure widened what the caller may erase: {v:?}"),
    }
    attempt.rollback().await.unwrap();
    // Once the Erasure commits, billing's Edge that joined n1 is gone.
    tx.commit().await.unwrap();
    let crossing: i64 =
        g.n.w
            .db
            .admin()
            .await
            .query_typed(
                "select count(*) from gnomon.edge where id = $1",
                &[(&g.crossing_edge, Type::UUID)],
            )
            .await
            .unwrap()[0]
            .get(0);
    assert_eq!(crossing, 0, "the Edge of billing that joined n1 stands");
    assert!(
        version_rows(&g.n.w.db, g.billing_node).await.0 > 0,
        "billing's Node keeps its Versions"
    );
}

/// The stage tables (ADR-0011), which `0001_schema.sql` creates.
const STAGE_TABLES: &[&str] = &["stage_record", "stage_usage"];

/// `world()` with stage records for its Episodes: each Episode's graph stage
/// failed once and is due again in an hour, with usage counts, and its
/// sentences stage is pending, so it is due now. Written through the Store
/// by the worker of each Episode's own Tenant and Scope.
async fn staged_world() -> World {
    let w = world().await;
    let store = Store::connect(&w.db.login_url(), &support::connect_options())
        .await
        .unwrap();
    let admin = w.db.admin().await;
    let episodes = admin
        .query_typed("select tenant_id, scope, id from gnomon.episode", &[])
        .await
        .unwrap();
    for row in &episodes {
        let (tenant, scope, episode): (Uuid, String, Uuid) = (row.get(0), row.get(1), row.get(2));
        let (tenant, episode) = (TenantId(tenant), EpisodeId(episode));
        let caller = store.as_worker(tenant, Scope::new(scope), None);
        caller
            .write_stages(tenant, &[gnomon::StageWrite::pending(episode, "graph")])
            .await
            .unwrap();
        let claim = caller
            .claim_stage(tenant, Some(episode), std::time::Duration::from_secs(3600))
            .await
            .unwrap()
            .unwrap();
        caller
            .write_stages(
                tenant,
                &[
                    gnomon::StageWrite::finish(
                        &claim,
                        gnomon::StageOutcome::Retry {
                            error: gnomon::StageError {
                                code: "rate_limited".into(),
                                status: Some(429),
                            },
                            after: std::time::Duration::from_secs(3600),
                        },
                        vec![gnomon::StageUsage {
                            model: "claude-sonnet-5-5".into(),
                            calls: 1,
                            input_tokens: 100,
                            output_tokens: 10,
                            cost_usd: None,
                        }],
                    ),
                    gnomon::StageWrite::pending(episode, "sentences"),
                ],
            )
            .await
            .unwrap();
    }
    w
}

/// Every row of the stage tables, read past RLS.
async fn stage_rows(db: &TestDb) -> Vec<String> {
    let admin = db.admin().await;
    let tables: Vec<String> = STAGE_TABLES.iter().map(|t| t.to_string()).collect();
    support::erasure::rows_of(&admin, &tables).await
}

#[tokio::test]
async fn a_caller_cannot_read_another_tenants_or_scopes_stage_records() {
    let w = staged_world().await;
    for role in [USER, WORKER] {
        let mut client = w.db.login().await;
        let tx = as_caller(&mut client, role, &claims(w.t1, &["support"])).await;
        for table in STAGE_TABLES {
            let ids = visible_ids(
                &tx,
                &format!("select distinct episode_id from gnomon.{table}"),
                &[],
            )
            .await;
            assert_eq!(ids, vec![w.t1_support], "{role} read {table}: {ids:?}");
        }
        for (tenant, episode) in [(w.t1, w.t1_billing), (w.t2, w.t2_support)] {
            for function in ["read_stage_records", "read_stage_usage"] {
                let rows = tx
                    .query_typed(
                        &format!("select * from gnomon.{function}($1, $2)"),
                        &[(&tenant, Type::UUID), (&episode, Type::UUID)],
                    )
                    .await
                    .unwrap();
                assert!(rows.is_empty(), "{role} read {episode} through {function}");
            }
        }
    }
}

#[tokio::test]
async fn a_caller_cannot_write_another_tenants_or_scopes_stage_records() {
    let w = staged_world().await;
    let before = stage_rows(&w.db).await;
    let mut client = w.db.login().await;
    let caller = claims(w.t1, &["support"]);

    // The stage functions, for another Tenant's or Scope's Episode.
    for (tenant, episode) in [(w.t1, w.t1_billing), (w.t2, w.t2_support)] {
        for (sql, code) in [
            ("select gnomon.stage_pending($1, $2, 'forged')", "GNW01"),
            (
                "select gnomon.add_stage_usage($1, $2, 'graph', 'forged', 1, 1, 1, null)",
                "GNW01",
            ),
            (
                "select gnomon.finish_stage($1, $2, 'graph', 1, 'succeeded', null, null, null)",
                "GNS01",
            ),
        ] {
            let tx = as_caller(&mut client, WORKER, &caller).await;
            let refused = tx
                .query_typed(sql, &[(&tenant, Type::UUID), (&episode, Type::UUID)])
                .await;
            match refused {
                Err(e) => assert_eq!(e.code().map(SqlState::code), Some(code), "{sql}: {e:?}"),
                Ok(_) => panic!("{sql} for {episode} was allowed"),
            }
        }
    }

    assert_eq!(stage_rows(&w.db).await, before);
}

#[tokio::test]
async fn a_caller_cannot_claim_another_tenants_or_scopes_stage_records() {
    let w = staged_world().await;
    let mut client = w.db.login().await;
    for (role, tenant, episode) in [
        (USER, w.t1, None),
        (WORKER, w.t1, Some(w.t1_billing)),
        (WORKER, w.t2, None),
        (WORKER, w.t2, Some(w.t2_support)),
    ] {
        let tx = as_caller(&mut client, role, &claims(w.t1, &["support"])).await;
        let claimed = visible_ids(
            &tx,
            "select episode_id from gnomon.claim_stage($1, $2, interval '1 hour')",
            &[(&tenant, Type::UUID), (&episode, Type::UUID)],
        )
        .await;
        if tenant == w.t1 && episode.is_none() {
            assert_eq!(claimed, vec![w.t1_support], "{role} claimed {claimed:?}");
        } else {
            assert!(claimed.is_empty(), "{role} claimed {claimed:?} in {tenant}");
        }
        tx.rollback().await.unwrap();
    }
    // Claims that bind no Tenant or Scope claim nothing.
    for no_claims in [
        "{}",
        r#"{"app_metadata": {"tenant_id": null, "scopes": []}}"#,
    ] {
        let tx = as_caller(&mut client, WORKER, no_claims).await;
        let claimed = visible_ids(
            &tx,
            "select episode_id from gnomon.claim_stage($1, null, interval '1 hour')",
            &[(&w.t1, Type::UUID)],
        )
        .await;
        assert!(claimed.is_empty(), "claims {no_claims} claimed {claimed:?}");
    }
    // The other Tenant's and Scope's pending records are still pending.
    let admin = w.db.admin().await;
    let pending: i64 = admin
        .query_typed(
            "select count(*) from gnomon.stage_record where stage = 'sentences' and status = 'pending'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(pending, 4);
}

#[tokio::test]
async fn a_caller_cannot_delete_a_stage_record_or_change_what_it_names() {
    let w = staged_world().await;
    let before = stage_rows(&w.db).await;
    let mut client = w.db.login().await;
    for sql in [
        "delete from gnomon.stage_record where episode_id = $1",
        "delete from gnomon.stage_usage where episode_id = $1",
        "update gnomon.stage_record set scope = 'billing' where episode_id = $1",
        "update gnomon.stage_record set episode_id = gnomon.uuid_v7() where episode_id = $1",
        "update gnomon.stage_record set tenant_id = gnomon.uuid_v7() where episode_id = $1",
        "update gnomon.stage_record set stage = 'renamed' where episode_id = $1",
        "update gnomon.stage_record set created_at = now() where episode_id = $1",
        "update gnomon.stage_usage set model = 'renamed' where episode_id = $1",
        "update gnomon.stage_usage set scope = 'billing' where episode_id = $1",
    ] {
        for role in [USER, WORKER] {
            let tx = as_caller(&mut client, role, &claims(w.t1, &["support"])).await;
            assert_refused(
                tx.execute_typed(sql, &[(&w.t1_support, Type::UUID)]).await,
                &format!("{role}: {sql}"),
            );
        }
    }
    assert_eq!(stage_rows(&w.db).await, before);
}

/// The token table of full-text Search, the BM25 counts of each Scope and
/// each transaction's changes to them (ADR-0010), which `0001_schema.sql`
/// creates.
const SEARCH_TABLES: &[&str] = &[
    "search_token",
    "search_token_frequency",
    "search_scope_length",
    "search_count_delta",
];

/// The one trigger function that writes them, a definer (ADR-0009).
const SEARCH_INDEX_FUNCTION: &str = "gnomon.maintain_search_index()";

/// `world()` with a Node named "zebra" and something of its own in each
/// Tenant's `support` and `billing` Scopes, written through the Store by
/// each Scope's worker.
async fn searchable_world() -> World {
    let w = world().await;
    let store = Store::connect(&w.db.login_url(), &support::connect_options())
        .await
        .unwrap();
    let admin = w.db.admin().await;
    for row in admin
        .query_typed("select tenant_id, scope, id from gnomon.episode", &[])
        .await
        .unwrap()
    {
        let (tenant, scope, episode): (Uuid, String, Uuid) = (row.get(0), row.get(1), row.get(2));
        let name = format!("zebra {scope}");
        store
            .as_worker(TenantId(tenant), Scope::new(scope.clone()), None)
            .create_node(
                &gnomon::NewNode {
                    tenant: Some(TenantId(tenant)),
                    scope: Scope::new(scope),
                    kind: gnomon::ROOT_NODE_KIND.into(),
                },
                &gnomon::Patch::new(
                    EpisodeId(episode),
                    gnomon::ValidTime::UNKNOWN,
                    support::erasure::values(json!({ "name": name })),
                ),
            )
            .await
            .unwrap();
    }
    w
}

#[tokio::test]
async fn a_caller_reads_only_its_own_tenants_and_scopes_token_rows_and_counts() {
    let w = searchable_world().await;
    for role in [USER, WORKER] {
        let mut client = w.db.login().await;
        let tx = as_caller(&mut client, role, &claims(w.t1, &["support"])).await;
        for table in SEARCH_TABLES {
            // Every transaction's changes to the counts go at its commit.
            if *table == "search_count_delta" {
                continue;
            }
            let seen: Vec<(Uuid, String)> = tx
                .query_typed(
                    &format!("select distinct tenant_id, scope from gnomon.{table}"),
                    &[],
                )
                .await
                .unwrap()
                .iter()
                .map(|r| (r.get(0), r.get(1)))
                .collect();
            assert_eq!(
                seen,
                vec![(w.t1, "support".to_owned())],
                "{role} read gnomon.{table}"
            );
        }
        // Search finds only the caller's own Scope, and nothing when it
        // names the other Tenant.
        let at: jiff::Timestamp = "2024-06-01T00:00:00Z".parse().unwrap();
        for (tenant, expected) in [(w.t1, vec!["support".to_owned()]), (w.t2, vec![])] {
            let scopes: Vec<String> = tx
                .query_typed(
                    "select scope from gnomon.search_full_text($1, 'zebra billing support', $2, null, 10)",
                    &[(&tenant, Type::UUID), (&Timestamptz(at), Type::TIMESTAMPTZ)],
                )
                .await
                .unwrap()
                .iter()
                .map(|r| r.get(0))
                .collect();
            assert_eq!(scopes, expected, "{role} searched {tenant}");
        }
        // The counts a caller reads are its own Scope's: one document, which
        // says "zebra" and "support".
        let documents: i64 = tx
            .query_typed(
                "select sum(versions)::bigint from gnomon.search_scope_length where tenant_id = $1",
                &[(&w.t1, Type::UUID)],
            )
            .await
            .unwrap()[0]
            .get(0);
        assert_eq!(documents, 1, "{role} counted documents of another Scope");
    }
}

#[tokio::test]
async fn a_caller_cannot_write_the_token_rows_or_the_counts_directly_even_in_its_own_scope() {
    let w = searchable_world().await;
    let admin = w.db.admin().await;
    let tables: Vec<String> = SEARCH_TABLES.iter().map(|t| t.to_string()).collect();
    let before = support::erasure::rows_of(&admin, &tables).await;
    let own_version: Uuid = admin
        .query_typed(
            "select id from gnomon.version where tenant_id = $1 and scope = 'support'",
            &[(&w.t1, Type::UUID)],
        )
        .await
        .unwrap()[0]
        .get(0);
    let t = w.t1;
    let forgeries = |scope: &str| {
        vec![
            // A forged word for the caller's own Version.
            format!(
                "insert into gnomon.search_token (tenant_id, scope, token, version_id, tf, dl)
                 values ('{t}', '{scope}', 'forged', '{own_version}', 1, 1)"
            ),
            format!("update gnomon.search_token set tf = 99 where scope = '{scope}'"),
            format!("delete from gnomon.search_token where scope = '{scope}'"),
            format!(
                "insert into gnomon.search_token_frequency (tenant_id, scope, token, versions)
                 values ('{t}', '{scope}', 'forged', 1000)"
            ),
            format!(
                "update gnomon.search_token_frequency set versions = 1000 where scope = '{scope}'"
            ),
            format!("delete from gnomon.search_token_frequency where scope = '{scope}'"),
            format!(
                "insert into gnomon.search_scope_length (tenant_id, scope, versions, words)
                 values ('{t}', 'elsewhere', 1000, 1000)"
            ),
            format!(
                "update gnomon.search_scope_length set versions = 1000 where scope = '{scope}'"
            ),
            format!("delete from gnomon.search_scope_length where scope = '{scope}'"),
            format!(
                "insert into gnomon.search_count_delta (tenant_id, scope, token, versions, words)
                 values ('{t}', '{scope}', null, 1000, 1000)"
            ),
            // The functions that write the token rows, called directly.
            format!("select gnomon.index_search_text('{t}', array['{own_version}'::uuid], true)"),
            format!("select * from gnomon.search_text_tokens('{t}', array['{own_version}'::uuid])"),
        ]
    };
    for role in [USER, WORKER] {
        for scope in ["support", "billing"] {
            for sql in forgeries(scope) {
                // As the caller's own statement, and from a trigger of the
                // caller's own on a temporary table, which every role may
                // create.
                let through_a_trigger = format!(
                    "create temp table pulled (i int);
                     create function pg_temp.pull() returns trigger language plpgsql as $f$
                     begin
                       execute $q${sql}$q$;
                       return null;
                     end $f$;
                     create trigger pull after insert on pulled
                       for each statement execute function pg_temp.pull();
                     insert into pulled values (1);"
                );
                for attempt in [sql.clone(), through_a_trigger] {
                    let mut client = w.db.login().await;
                    let tx = as_caller(&mut client, role, &claims(w.t1, &["support"])).await;
                    assert_refused(
                        tx.batch_execute(&attempt).await,
                        &format!("{role}: {attempt}"),
                    );
                }
            }
        }
        // No caller can call the trigger function, or attach it to a table
        // of its own.
        for sql in [
            "create temp table pulled (i int);
             create trigger pull after insert on pulled referencing new table as changed
               for each statement execute function gnomon.maintain_search_index();",
            "create trigger pull after insert on gnomon.search_token
               for each statement execute function gnomon.maintain_search_index();",
        ] {
            let mut client = w.db.login().await;
            let tx = as_caller(&mut client, role, &claims(w.t1, &["support"])).await;
            assert_refused(tx.batch_execute(sql).await, &format!("{role}: {sql}"));
        }
    }
    assert_eq!(support::erasure::rows_of(&admin, &tables).await, before);
}

#[tokio::test]
async fn the_search_index_function_is_a_definer_no_role_may_execute_and_callers_hold_only_select() {
    let w = world().await;
    let admin = w.db.admin().await;
    let row = admin
        .query_typed(
            &format!(
                "select p.prosecdef, p.proowner::regrole::text,
                        array(select coalesce(nullif(a.grantee, 0)::regrole::text, 'public')
                              from aclexplode(p.proacl) a where a.grantee <> p.proowner)
                 from pg_proc p where p.oid = '{SEARCH_INDEX_FUNCTION}'::regprocedure"
            ),
            &[],
        )
        .await
        .unwrap();
    let (definer, owner, grantees): (bool, String, Vec<String>) =
        (row[0].get(0), row[0].get(1), row[0].get(2));
    assert!(definer);
    assert_eq!(owner, "gnomon_definer");
    assert_eq!(grantees, Vec::<String>::new(), "who may execute it");
    for role in [USER, WORKER, support::LOGIN_ROLE] {
        for table in SEARCH_TABLES {
            let privileges: Vec<String> = admin
                .query_typed(
                    "select p from unnest(array['SELECT', 'INSERT', 'UPDATE', 'DELETE',
                                              'TRUNCATE', 'REFERENCES', 'TRIGGER', 'MAINTAIN']) p
                     where has_table_privilege($1, ('gnomon.' || $2)::regclass, p)",
                    &[(&role, Type::TEXT), (table, Type::TEXT)],
                )
                .await
                .unwrap()
                .iter()
                .map(|r| r.get(0))
                .collect();
            let expected: Vec<String> = if role == support::LOGIN_ROLE {
                vec![]
            } else {
                vec!["SELECT".into()]
            };
            assert_eq!(privileges, expected, "{role} on gnomon.{table}");
        }
    }
}

/// The full vectors and quantized rows of vector Search (ADR-0010), which
/// `0001_schema.sql` creates.
const EMBEDDING_TABLES: &[&str] = &["embedding", "embedding_bit"];

/// `searchable_world()` with an embedding stored for every Version, as each
/// Scope's worker, and a Node named "twin" in Tenant 1's `support` and
/// `billing` Scopes, whose Versions have the same text. Only billing's twin
/// is embedded.
struct EmbeddedWorld {
    w: World,
    billing_twin: Uuid,
    twin_hash: String,
    /// A Version of Tenant 1's `support` with an embedding, and its hash.
    support_version: Uuid,
    support_hash: String,
}

async fn embedded_world() -> EmbeddedWorld {
    let w = searchable_world().await;
    let store = Store::connect(&w.db.login_url(), &support::connect_options())
        .await
        .unwrap();
    let mut rng = support::vectors::Rng::new(18);
    let admin = w.db.admin().await;
    for row in admin
        .query_typed(
            "select tenant_id, scope, id, text_hash from gnomon.version",
            &[],
        )
        .await
        .unwrap()
    {
        let (tenant, scope, id, hash): (Uuid, String, Uuid, String) =
            (row.get(0), row.get(1), row.get(2), row.get(3));
        store
            .as_worker(TenantId(tenant), Scope::new(scope), None)
            .store_embedding(TenantId(tenant), gnomon::VersionId(id), &hash, &rng.unit())
            .await
            .unwrap();
    }
    let support_row = admin
        .query_typed(
            "select id, text_hash from gnomon.version where tenant_id = $1 and scope = 'support'",
            &[(&w.t1, Type::UUID)],
        )
        .await
        .unwrap();
    let mut twins = Vec::new();
    for (scope, episode) in [("support", w.t1_support), ("billing", w.t1_billing)] {
        let caller = store.as_worker(TenantId(w.t1), Scope::new(scope), None);
        let (node, _) = caller
            .create_node(
                &gnomon::NewNode {
                    tenant: Some(TenantId(w.t1)),
                    scope: Scope::new(scope),
                    kind: gnomon::ROOT_NODE_KIND.into(),
                },
                &gnomon::Patch::new(
                    EpisodeId(episode),
                    gnomon::ValidTime::UNKNOWN,
                    support::erasure::values(json!({ "name": "twin" })),
                ),
            )
            .await
            .unwrap();
        let version = caller
            .read_node(
                TenantId(w.t1),
                node,
                gnomon::AsOf::current("2024-06-01T00:00:00Z".parse().unwrap()),
            )
            .await
            .unwrap()
            .unwrap();
        twins.push((caller, version));
    }
    let (billing_caller, billing_twin) = twins.pop().unwrap();
    let (_, support_twin) = twins.pop().unwrap();
    assert_eq!(support_twin.text_hash, billing_twin.text_hash);
    billing_caller
        .store_embedding(
            TenantId(w.t1),
            billing_twin.id,
            &billing_twin.text_hash,
            &rng.unit(),
        )
        .await
        .unwrap();
    drop(billing_caller);
    EmbeddedWorld {
        billing_twin: billing_twin.id.0,
        twin_hash: support_twin.text_hash,
        support_version: support_row[0].get(0),
        support_hash: support_row[0].get(1),
        w,
    }
}

async fn embedding_rows(db: &TestDb) -> Vec<String> {
    let tables: Vec<String> = EMBEDDING_TABLES.iter().map(|t| t.to_string()).collect();
    support::erasure::rows_of(&db.admin().await, &tables).await
}

/// A unit vector of the embedding dimension, as SQL text.
fn sql_vector() -> String {
    let v = support::vectors::Rng::new(5).unit();
    format!(
        "array[{}]::real[]",
        v.iter()
            .map(|x| x.to_string())
            .collect::<Vec<_>>()
            .join(",")
    )
}

#[tokio::test]
async fn a_caller_reads_only_its_own_tenants_and_scopes_embeddings() {
    let ew = embedded_world().await;
    let w = &ew.w;
    let query = sql_vector();
    for role in [USER, WORKER] {
        let mut client = w.db.login().await;
        let tx = as_caller(&mut client, role, &claims(w.t1, &["support"])).await;
        for table in EMBEDDING_TABLES {
            let seen: Vec<(Uuid, String)> = tx
                .query_typed(
                    &format!("select distinct tenant_id, scope from gnomon.{table}"),
                    &[],
                )
                .await
                .unwrap()
                .iter()
                .map(|r| (r.get(0), r.get(1)))
                .collect();
            assert_eq!(
                seen,
                vec![(w.t1, "support".to_owned())],
                "{role} read gnomon.{table}"
            );
        }
        // Search finds only the caller's own Scope, and nothing when it
        // names the other Tenant.
        let at: jiff::Timestamp = "2024-06-01T00:00:00Z".parse().unwrap();
        for (tenant, expected) in [(w.t1, vec!["support".to_owned()]), (w.t2, vec![])] {
            let scopes: Vec<String> = tx
                .query_typed(
                    &format!(
                        "select distinct scope from gnomon.search_vector($1, {query}, $2, null, 400)"
                    ),
                    &[(&tenant, Type::UUID), (&Timestamptz(at), Type::TIMESTAMPTZ)],
                )
                .await
                .unwrap()
                .iter()
                .map(|r| r.get(0))
                .collect();
            assert_eq!(scopes, expected, "{role} searched {tenant}");
        }
    }
}

/// Fails unless `result` is a refusal: a privilege or RLS refusal, the
/// Store's refusal of an invalid write, or a foreign key.
fn assert_refused_write(result: Result<impl std::fmt::Debug, tokio_postgres::Error>, what: &str) {
    match result {
        Err(e) => assert!(
            [
                SqlState::INSUFFICIENT_PRIVILEGE,
                SqlState::from_code(gnomon::INVALID_WRITE),
                SqlState::FOREIGN_KEY_VIOLATION,
            ]
            .iter()
            .any(|c| e.code() == Some(c)),
            "{what}: expected a refusal, got {e:?}"
        ),
        Ok(v) => panic!("{what} was allowed: {v:?}"),
    }
}

#[tokio::test]
async fn a_caller_cannot_write_an_embedding_of_another_tenant_or_scope_or_attach_another_scopes_text()
 {
    let ew = embedded_world().await;
    let w = &ew.w;
    let before = embedding_rows(&w.db).await;
    let (t1, t2) = (w.t1, w.t2);
    let (own, own_hash) = (ew.support_version, &ew.support_hash);
    let (billing_twin, twin_hash) = (ew.billing_twin, &ew.twin_hash);
    let v = sql_vector();
    // Another Scope's Version, another Tenant, and the caller's own Version
    // moved to a text that is not its own, through the Store's function.
    // Callers write nothing directly (#81).
    let attempts = [
        format!("select gnomon.store_embedding('{t1}', '{billing_twin}', '{twin_hash}', {v})"),
        format!("select gnomon.store_embedding('{t2}', '{own}', '{own_hash}', {v})"),
        format!("select gnomon.store_embedding('{t1}', '{own}', '{twin_hash}', {v})"),
    ];
    for role in [USER, WORKER] {
        for sql in &attempts {
            let mut client = w.db.login().await;
            let tx = as_caller(&mut client, role, &claims(w.t1, &["support"])).await;
            assert_refused_write(tx.batch_execute(sql).await, &format!("{role}: {sql}"));
        }
    }
    assert_eq!(embedding_rows(&w.db).await, before);
}

#[tokio::test]
async fn callers_hold_only_select_on_the_embedding_tables_and_the_writer_insert_and_a_text_hash_update()
 {
    let w = world().await;
    let admin = w.db.admin().await;
    for role in [
        USER,
        WORKER,
        support::LOGIN_ROLE,
        "gnomon_writer",
        "gnomon_definer",
        "gnomon_eraser",
    ] {
        for table in EMBEDDING_TABLES {
            let privileges: Vec<String> = admin
                .query_typed(
                    "select p from unnest(array['SELECT', 'INSERT', 'UPDATE', 'DELETE',
                                              'TRUNCATE', 'REFERENCES', 'TRIGGER', 'MAINTAIN']) p
                     where has_table_privilege($1, ('gnomon.' || $2)::regclass, p)
                     union all
                     select 'UPDATE (' || a.attname || ')'
                     from pg_attribute a
                     where a.attrelid = ('gnomon.' || $2)::regclass and a.attnum > 0
                       and not a.attisdropped
                       and has_column_privilege($1, a.attrelid, a.attnum, 'UPDATE')",
                    &[(&role, Type::TEXT), (table, Type::TEXT)],
                )
                .await
                .unwrap()
                .iter()
                .map(|r| r.get(0))
                .collect();
            let expected: Vec<&str> = match (role, *table) {
                (r, _) if r == support::LOGIN_ROLE => vec![],
                ("gnomon_definer" | "gnomon_eraser", _) => vec!["SELECT", "DELETE"],
                ("gnomon_writer", "embedding") => vec!["SELECT", "INSERT"],
                ("gnomon_writer", _) => vec!["SELECT", "INSERT", "UPDATE (text_hash)"],
                _ => vec!["SELECT"],
            };
            assert_eq!(privileges, expected, "{role} on gnomon.{table}");
        }
    }
    // The trigger functions run for no caller of their own.
    for function in [
        "gnomon.embedding_bit_derive()",
        "gnomon.embedding_check_used()",
    ] {
        for role in [USER, WORKER] {
            let may: bool = admin
                .query_typed(
                    "select has_function_privilege($1, $2, 'EXECUTE')",
                    &[(&role, Type::TEXT), (&function, Type::TEXT)],
                )
                .await
                .unwrap()[0]
                .get(0);
            assert!(!may, "{role} may execute {function}");
        }
    }
}

#[tokio::test]
async fn vector_search_works_with_pgvector_in_a_schema_of_its_own_whatever_the_callers_search_path()
{
    // A deployment whose pgvector lives in a schema the caller roles cannot
    // reach, as Supabase keeps it in `extensions`, before the migrations.
    // The schema belongs to the role that applies the migrations, so the
    // migration can grant the caller roles USAGE on it.
    let db = TestDb::empty_with_login().await;
    let operator = db.operator().await;
    let migrator: String = operator
        .query_typed("select current_user::text", &[])
        .await
        .unwrap()[0]
        .get(0);
    db.admin()
        .await
        .batch_execute(&format!(
            "create schema vecs authorization {}; revoke all on schema vecs from public;
             create extension vector with schema vecs",
            support::quote_ident(&migrator)
        ))
        .await
        .unwrap();
    for (file, sql) in support::migration_files(support::DEFAULT_DIMENSION) {
        operator
            .batch_execute(&sql)
            .await
            .unwrap_or_else(|e| panic!("apply {file}: {e:?}"));
    }
    let store = Store::connect(&db.login_url(), &support::connect_options())
        .await
        .unwrap();
    let t = TenantId(Uuid::new_v4());
    let caller = store.as_worker(t, Scope::new("support"), None);
    let episode = caller
        .record_episode(&NewEpisode {
            tenant: Some(t),
            scope: Scope::new("support"),
            content: "an Episode".into(),
            reference_time: "2024-01-01T00:00:00Z".parse().unwrap(),
            authority: None,
        })
        .await
        .unwrap();
    let (node, _) = caller
        .create_node(
            &gnomon::NewNode {
                tenant: Some(t),
                scope: Scope::new("support"),
                kind: gnomon::ROOT_NODE_KIND.into(),
            },
            &gnomon::Patch::new(
                episode,
                gnomon::ValidTime::UNKNOWN,
                support::erasure::values(json!({ "name": "Ada" })),
            ),
        )
        .await
        .unwrap();
    let as_of = gnomon::AsOf::current("2024-06-01T00:00:00Z".parse().unwrap());
    let version = caller.read_node(t, node, as_of).await.unwrap().unwrap();
    let embedding = support::vectors::Rng::new(1).unit();
    caller
        .store_embedding(t, version.id, &version.text_hash, &embedding)
        .await
        .unwrap();
    let found = caller.search_vector(t, &embedding, as_of, 5).await.unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].version, version.id);

    // A caller whose own search path leads to a schema of look-alikes
    // reaches the same functions: every name the functions use is qualified.
    let mut client = db.login().await;
    let tx = as_caller(
        &mut client,
        WORKER,
        &json!({ "role": "gnomon_worker", "app_metadata": { "tenant_id": t.0.to_string(), "scopes": ["support"] } })
            .to_string(),
    )
    .await;
    tx.batch_execute(
        "create function pg_temp.binary_quantize(real[]) returns bit language sql as $$ select B'1' $$;
         set local search_path = pg_temp, public",
    )
    .await
    .unwrap();
    let ids: Vec<Uuid> = tx
        .query_typed(
            "select version_id from gnomon.search_vector($1, $2, $3, null, 5)",
            &[
                (&t.0, Type::UUID),
                (&embedding, Type::FLOAT4_ARRAY),
                (&Timestamptz(as_of.valid_at), Type::TIMESTAMPTZ),
            ],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(ids, vec![version.id.0]);

    // Resolution too: the search index trigger quantizes a name's full
    // vector as the definer role, and the caller's look-alike is not used.
    caller
        .store_name_embedding(t, node, "Ada", &embedding)
        .await
        .unwrap();
    let names: Vec<String> = tx
        .query_typed(
            "select name from gnomon.resolve($1, 'ada', $2, null, 5)",
            &[(&t.0, Type::UUID), (&embedding, Type::FLOAT4_ARRAY)],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(names, vec!["Ada".to_owned()]);
    tx.commit().await.unwrap();
    let twin = caller
        .create_node(
            &gnomon::NewNode {
                tenant: Some(t),
                scope: Scope::new("support"),
                kind: gnomon::ROOT_NODE_KIND.into(),
            },
            &gnomon::Patch::new(
                episode,
                gnomon::ValidTime::UNKNOWN,
                support::erasure::values(json!({ "name": "Ada" })),
            ),
        )
        .await
        .unwrap()
        .0;
    let found = caller
        .resolve(t, &gnomon::Mention::embedding(&embedding), 5)
        .await
        .unwrap();
    assert_eq!(
        found.len(),
        2,
        "the twin's name took its bits from the stored full vector"
    );
    assert!(found.iter().any(|c| c.node == twin));
}

// Summaries and Communities (ADR-0009, ADR-0014, ADR-0015, `0003_reads_and_writes.sql`).
// Callers write them through invoker functions or directly, and every check
// is the tables' own: RLS, column grants, foreign keys that carry the Scope,
// and guard triggers. Only Erasure deletes committed rows, and its reach
// into a Scope the caller cannot see covers only the Summaries of a Node it
// listed.

/// The Graph fixture with a committed Summary of `n1` in support and one in
/// billing, a Summary of billing's Node in billing, and a Community of
/// support holding `n1`, each citing its own Scope's Episode.
struct Summarized {
    g: Graph,
    support_summary: Uuid,
    billing_summary: Uuid,
    billing_node_summary: Uuid,
    community: Uuid,
}

async fn summarized() -> Summarized {
    let g = graph().await;
    let store = Store::connect(&g.n.w.db.login_url(), &support::connect_options())
        .await
        .unwrap();
    let t1 = TenantId(g.n.w.t1);
    let both = store.as_user(
        serde_json::from_value(json!({
            "role": "authenticated",
            "app_metadata": { "tenant_id": g.n.w.t1.to_string(), "scopes": ["support", "billing"] }
        }))
        .unwrap(),
    );
    let at: jiff::Timestamp = "2024-06-01T00:00:00Z".parse().unwrap();
    let summary = |scope: &str, node: Uuid, episode: Uuid, content: &str| gnomon::NewSummary {
        scope: Scope::new(scope),
        node: gnomon::NodeId(node),
        content: content.into(),
        as_of_valid: at,
        citations: vec![gnomon::DerivedCitation::of(EpisodeId(episode))],
    };
    let mut ids = Vec::new();
    for s in [
        summary("support", g.n.n1, g.n.w.t1_support, "n1 in support"),
        summary("billing", g.n.n1, g.n.w.t1_billing, "n1 in billing"),
        summary(
            "billing",
            g.billing_node,
            g.n.w.t1_billing,
            "billing's Node",
        ),
    ] {
        ids.push(both.write_summary(t1, &s).await.unwrap().0);
    }
    let community = both
        .write_community(
            t1,
            &gnomon::NewCommunity {
                scope: Scope::new("support"),
                replaces: vec![],
                members: vec![gnomon::NodeId(g.n.n1)],
                content: "n1 alone".into(),
                as_of_valid: at,
                citations: vec![gnomon::DerivedCitation::of(EpisodeId(g.n.w.t1_support))],
            },
        )
        .await
        .unwrap()
        .0;
    Summarized {
        support_summary: ids[0],
        billing_summary: ids[1],
        billing_node_summary: ids[2],
        community,
        g,
    }
}

/// Every row of the five tables, counted past RLS.
async fn summary_and_community_rows(db: &TestDb) -> Vec<i64> {
    let admin = db.admin().await;
    let mut counts = Vec::new();
    for t in [
        "summary",
        "summary_citation",
        "community",
        "community_member",
        "community_citation",
    ] {
        counts.push(
            admin
                .query_typed(&format!("select count(*) from gnomon.{t}"), &[])
                .await
                .unwrap()[0]
                .get(0),
        );
    }
    counts
}

#[tokio::test]
async fn no_function_commits_a_summary_or_community_without_a_citation_or_a_community_without_a_node()
 {
    // #75: a commit check that a Summary or Community is cited skipped a row
    // the claims at commit no longer showed. Callers now write only through
    // the functions, which refuse what has no Citation or no Node as they
    // run, so no check at commit is left to skip.
    let n = nodes().await;
    let t1 = n.w.t1;
    let before = summary_and_community_rows(&n.w.db).await;
    let summary = "select gnomon.write_summary($1, 'support', $2, 'prose', '2024-06-01', $3,
                     null, null)";
    let community = "select gnomon.write_community($1, 'support', '{}', $2, 'prose',
                       '2024-06-01', $3, null, null)";
    for role in [USER, WORKER] {
        for (sql, members, episodes, message) in [
            (
                summary,
                None,
                Some(vec![]),
                "a Summary or Community cites at least one Episode",
            ),
            (
                summary,
                None,
                None,
                "a Summary or Community cites at least one Episode",
            ),
            (
                community,
                Some(vec![n.n1]),
                Some(vec![]),
                "a Summary or Community cites at least one Episode",
            ),
            (
                community,
                Some(vec![]),
                Some(vec![n.w.t1_support]),
                "a Community holds at least one Node, and names each",
            ),
            (
                community,
                None,
                Some(vec![n.w.t1_support]),
                "a Community holds at least one Node, and names each",
            ),
        ] {
            let mut client = n.w.db.login().await;
            let tx = as_caller(&mut client, role, &claims(t1, &["support"])).await;
            let r = if sql == summary {
                tx.query_typed(
                    sql,
                    &[
                        (&t1, Type::UUID),
                        (&n.n1, Type::UUID),
                        (&episodes, Type::UUID_ARRAY),
                    ],
                )
                .await
            } else {
                tx.query_typed(
                    sql,
                    &[
                        (&t1, Type::UUID),
                        (&members, Type::UUID_ARRAY),
                        (&episodes, Type::UUID_ARRAY),
                    ],
                )
                .await
            };
            assert_invalid_write(r, message);
        }
        // Written with its Citations and Nodes, then the claims change before
        // the commit: what commits is cited and holds its Node, and the
        // caller cannot delete either in between.
        let mut client = n.w.db.login().await;
        let tx = as_caller(&mut client, role, &claims(t1, &["support"])).await;
        let s: Uuid = tx
            .query_typed(
                summary,
                &[
                    (&t1, Type::UUID),
                    (&n.n1, Type::UUID),
                    (&vec![n.w.t1_support], Type::UUID_ARRAY),
                ],
            )
            .await
            .unwrap()[0]
            .get(0);
        let k: Uuid = tx
            .query_typed(
                community,
                &[
                    (&t1, Type::UUID),
                    (&vec![n.n1], Type::UUID_ARRAY),
                    (&vec![n.w.t1_support], Type::UUID_ARRAY),
                ],
            )
            .await
            .unwrap()[0]
            .get(0);
        for sql in [
            "delete from gnomon.summary_citation",
            "delete from gnomon.community_citation",
            "delete from gnomon.community_member",
        ] {
            tx.batch_execute("savepoint attempt").await.unwrap();
            assert_refused(tx.batch_execute(sql).await, &format!("{role}: {sql}"));
            tx.batch_execute("rollback to savepoint attempt")
                .await
                .unwrap();
        }
        tx.query_typed(
            "select set_config('request.jwt.claims', $1, true)",
            &[(&claims(t1, &[]), Type::TEXT)],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let held =
            n.w.db
                .admin()
                .await
                .query_typed(
                    "select (select count(*) from gnomon.summary_citation where summary_id = $1),
                        (select count(*) from gnomon.community_citation where community_id = $2),
                        (select count(*) from gnomon.community_member where community_id = $2)",
                    &[(&s, Type::UUID), (&k, Type::UUID)],
                )
                .await
                .unwrap();
        let counts: (i64, i64, i64) = (held[0].get(0), held[0].get(1), held[0].get(2));
        assert_eq!(counts, (1, 1, 1), "{role}");
    }
    // Each caller committed one row of each table and nothing more.
    let after = summary_and_community_rows(&n.w.db).await;
    let added: Vec<i64> = after.iter().zip(&before).map(|(a, b)| a - b).collect();
    assert_eq!(added, [2, 2, 2, 2, 2]);
}

#[tokio::test]
async fn a_caller_sees_and_writes_no_summary_or_community_of_another_tenant_or_scope() {
    let d = summarized().await;
    let w = &d.g.n.w;
    // Each caller sees the Summaries of its own Scopes, and reads only those
    // of n1 through the read function, which takes the Tenant it is given
    // and still reads under the caller's claims.
    let mut billing = vec![d.billing_summary, d.billing_node_summary];
    billing.sort();
    for (tenant, scopes, expected, of_n1) in [
        (w.t1, vec!["support"], vec![d.support_summary], 1),
        (w.t1, vec!["billing"], billing, 1),
        (w.t2, vec!["support", "billing"], vec![], 0),
    ] {
        for role in [USER, WORKER] {
            let mut client = w.db.login().await;
            let tx = as_caller(&mut client, role, &claims(tenant, &scopes)).await;
            let seen = visible_ids(&tx, "select id from gnomon.summary order by id", &[]).await;
            assert_eq!(seen, expected, "{role} of {scopes:?}");
            for t in [
                "summary_citation",
                "community",
                "community_member",
                "community_citation",
            ] {
                let n: i64 = tx
                    .query_typed(
                        &format!(
                            "select count(*) from gnomon.{t} where tenant_id <> $1 \
                             or not (scope = any($2))"
                        ),
                        &[(&tenant, Type::UUID), (&scopes, Type::TEXT_ARRAY)],
                    )
                    .await
                    .unwrap()[0]
                    .get(0);
                assert_eq!(
                    n, 0,
                    "{role} of {scopes:?} sees gnomon.{t} rows outside them"
                );
            }
            let read: i64 = tx
                .query_typed(
                    "select count(*) from gnomon.read_summaries($1, $2, 'infinity', null)",
                    &[(&w.t1, Type::UUID), (&d.g.n.n1, Type::UUID)],
                )
                .await
                .unwrap()[0]
                .get(0);
            assert_eq!(read, of_n1, "{role} of {scopes:?} reads n1's Summaries");
        }
    }

    // The write functions refuse another Tenant and a Scope the caller
    // cannot see.
    for (tenant, scope, what) in [
        (w.t2, "support", "another Tenant"),
        (w.t1, "billing", "a Scope it cannot see"),
    ] {
        let mut client = w.db.login().await;
        let tx = as_caller(&mut client, WORKER, &claims(w.t1, &["support"])).await;
        let r = tx
            .query_typed(
                "select gnomon.write_summary($1, $2, $3, 'forged', '2024-06-01', array[$4], null, null)",
                &[
                    (&tenant, Type::UUID),
                    (&scope, Type::TEXT),
                    (&d.g.n.n1, Type::UUID),
                    (&w.t1_support, Type::UUID),
                ],
            )
            .await;
        match r {
            Err(e) => assert!(
                e.code() == Some(&SqlState::INSUFFICIENT_PRIVILEGE)
                    || e.code().map(|c| c.code()) == Some(gnomon::INVALID_WRITE),
                "{what}: {e:?}"
            ),
            Ok(v) => panic!("a Summary was written in {what}: {v:?}"),
        }
    }
}

#[tokio::test]
async fn no_caller_runs_the_summary_deletes_and_an_erasure_removes_only_its_nodes_summaries_in_every_scope()
 {
    let d = summarized().await;
    let w = &d.g.n.w;
    let t1 = w.t1;
    for role in [USER, WORKER] {
        let mut client = w.db.login().await;
        let tx = as_caller(&mut client, role, &claims(t1, &["support", "billing"])).await;
        let r = tx
            .query_typed(
                "select gnomon.erase_summaries_and_communities($1, array[$2], '{}')",
                &[(&t1, Type::UUID), (&d.billing_summary, Type::UUID)],
            )
            .await;
        assert_refused(r, &format!("{role} the Summary deletes"));
    }

    // The worker of support erases n1. In the same transaction it still sees
    // no Summary of billing, though its Erasure removed one.
    let mut client = w.db.login().await;
    let tx = as_caller(&mut client, WORKER, &claims(t1, &["support"])).await;
    tx.query_typed(ERASE_NODE, &[(&t1, Type::UUID), (&d.g.n.n1, Type::UUID)])
        .await
        .unwrap();
    let seen: i64 = tx
        .query_typed(
            "select count(*) from gnomon.summary where scope = 'billing'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(
        seen, 0,
        "the worker sees billing's Summaries after an Erasure"
    );
    tx.commit().await.unwrap();

    // n1's Summaries in both Scopes and its Community are gone. billing's
    // Node, whose Edge to n1 went too, was not erased, so its Summary stands.
    let admin = w.db.admin().await;
    for (table, id, stands) in [
        ("summary", d.support_summary, false),
        ("summary", d.billing_summary, false),
        ("community", d.community, false),
        ("summary", d.billing_node_summary, true),
    ] {
        let n = admin
            .query_typed(
                &format!("select from gnomon.{table} where id = $1"),
                &[(&id, Type::UUID)],
            )
            .await
            .unwrap()
            .len();
        assert_eq!(n == 1, stands, "{table} {id}");
    }
    let cited: i64 = admin
        .query_typed(
            "select count(*) from gnomon.summary_citation where summary_id = $1",
            &[(&d.billing_node_summary, Type::UUID)],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(
        cited, 1,
        "the Summary of a Node not erased lost its Citation"
    );
}

// Resolution (ADR-0009, ADR-0010, `0004_search.sql`). The search index trigger
// writes the name rows, and callers update only a name row's bits in their
// own Scopes, which a trigger derives from the stored full vector whatever
// the caller supplies.

/// The name table of Resolution, and the full vectors its rows use.
const NAME_TABLES: &[&str] = &["node_name", "embedding"];

/// `searchable_world()` with the embedding of every Node's name stored, as
/// each Scope's worker, and a Node named "plain" in Tenant 1's `support`
/// whose name has none.
async fn named_world() -> World {
    let w = searchable_world().await;
    let store = Store::connect(&w.db.login_url(), &support::connect_options())
        .await
        .unwrap();
    let mut rng = support::vectors::Rng::new(19);
    let admin = w.db.admin().await;
    for row in admin
        .query_typed(
            "select tenant_id, scope, node_id, name from gnomon.node_name",
            &[],
        )
        .await
        .unwrap()
    {
        let (tenant, scope, node, name): (Uuid, String, Uuid, String) =
            (row.get(0), row.get(1), row.get(2), row.get(3));
        store
            .as_worker(TenantId(tenant), Scope::new(scope), None)
            .store_name_embedding(TenantId(tenant), gnomon::NodeId(node), &name, &rng.unit())
            .await
            .unwrap();
    }
    store
        .as_worker(TenantId(w.t1), Scope::new("support"), None)
        .create_node(
            &gnomon::NewNode {
                tenant: Some(TenantId(w.t1)),
                scope: Scope::new("support"),
                kind: gnomon::ROOT_NODE_KIND.into(),
            },
            &gnomon::Patch::new(
                EpisodeId(w.t1_support),
                gnomon::ValidTime::UNKNOWN,
                support::erasure::values(json!({ "name": "plain" })),
            ),
        )
        .await
        .unwrap();
    w
}

async fn name_rows(db: &TestDb) -> Vec<String> {
    let tables: Vec<String> = NAME_TABLES.iter().map(|t| t.to_string()).collect();
    support::erasure::rows_of(&db.admin().await, &tables).await
}

#[tokio::test]
async fn a_caller_reads_only_its_own_tenants_and_scopes_name_rows_and_resolves_only_them() {
    let w = named_world().await;
    let query = sql_vector();
    for role in [USER, WORKER] {
        let mut client = w.db.login().await;
        let tx = as_caller(&mut client, role, &claims(w.t1, &["support"])).await;
        let seen: Vec<(Uuid, String)> = tx
            .query_typed(
                "select distinct tenant_id, scope from gnomon.node_name",
                &[],
            )
            .await
            .unwrap()
            .iter()
            .map(|r| (r.get(0), r.get(1)))
            .collect();
        assert_eq!(seen, vec![(w.t1, "support".to_owned())], "{role}");
        // Both halves find only the caller's own Scope, and nothing when the
        // call names the other Tenant.
        for (tenant, expected) in [(w.t1, vec!["support".to_owned()]), (w.t2, vec![])] {
            let scopes: Vec<String> = tx
                .query_typed(
                    &format!(
                        "select distinct scope from gnomon.resolve($1, 'zebra billing support',
                                                                   {query}, null, 400)"
                    ),
                    &[(&tenant, Type::UUID)],
                )
                .await
                .unwrap()
                .iter()
                .map(|r| r.get(0))
                .collect();
            assert_eq!(scopes, expected, "{role} resolved in {tenant}");
        }
    }
}

#[tokio::test]
async fn a_caller_cannot_write_the_name_rows_directly_with_another_scopes_data_or_forge_them() {
    let w = named_world().await;
    let before = name_rows(&w.db).await;
    let admin = w.db.admin().await;
    let id_of = |scope: &'static str, tenant: Uuid| {
        let admin = &admin;
        async move {
            let r = admin
                .query_typed(
                    "select version_id, node_id, text_hash from gnomon.node_name
                     where tenant_id = $1 and scope = $2 and name like 'zebra%'",
                    &[(&tenant, Type::UUID), (&scope, Type::TEXT)],
                )
                .await
                .unwrap();
            (
                r[0].get::<_, Uuid>(0),
                r[0].get::<_, Uuid>(1),
                r[0].get::<_, String>(2),
            )
        }
    };
    let (t1, t2) = (w.t1, w.t2);
    let (own_version, own_node, _) = id_of("support", t1).await;
    let (_, billing_node, billing_hash) = id_of("billing", t1).await;
    let (_, other_tenant_node, _) = id_of("support", t2).await;
    let v = sql_vector();
    let row = |scope: &str, tenant: Uuid| {
        format!(
            "insert into gnomon.node_name (tenant_id, scope, version_id, node_id, text_hash, name,
                                           alias, valid_from, valid_to, recorded_to)
             values ('{tenant}', '{scope}', '{own_version}', '{own_node}', repeat('ab', 32),
                     'forged', true, '-infinity', 'infinity', 'infinity')"
        )
    };
    let attempts = [
        // A forged Alias of the caller's own Version, in its own Scope,
        // another Scope and another Tenant: callers hold no INSERT.
        row("support", t1),
        row("billing", t1),
        row("support", t2),
        // Billing's name in the caller's own Scope.
        format!(
            "insert into gnomon.node_name (tenant_id, scope, version_id, node_id, text_hash, name,
                                           alias, valid_from, valid_to, recorded_to)
             values ('{t1}', 'support', '{own_version}', '{billing_node}', '{billing_hash}',
                     'zebra billing', false, '-infinity', 'infinity', 'infinity')"
        ),
        // What only the Store derives, and what only Erasure deletes.
        "update gnomon.node_name set name = 'forged'".to_owned(),
        "update gnomon.node_name set alias = not alias".to_owned(),
        format!("update gnomon.node_name set text_hash = '{billing_hash}'"),
        "update gnomon.node_name set scope = 'billing'".to_owned(),
        format!("update gnomon.node_name set tenant_id = '{t2}'"),
        format!("update gnomon.node_name set node_id = '{billing_node}'"),
        "update gnomon.node_name set recorded_to = 'infinity'".to_owned(),
        "update gnomon.node_name set valid_from = '2001-01-01'".to_owned(),
        "delete from gnomon.node_name".to_owned(),
        "truncate gnomon.node_name".to_owned(),
        // Bits for a name whose text has no stored full vector.
        "update gnomon.node_name set q = null where name = 'plain'".to_owned(),
        // The Store's function, for a Node of another Scope or Tenant, and
        // for a name the caller's own Node does not have.
        format!(
            "select gnomon.store_name_embedding('{t1}', '{billing_node}', 'zebra billing', {v})"
        ),
        format!(
            "select gnomon.store_name_embedding('{t2}', '{other_tenant_node}', 'zebra support', {v})"
        ),
        format!("select gnomon.store_name_embedding('{t1}', '{own_node}', 'zebra billing', {v})"),
        // The functions that derive the name rows, called directly.
        format!("select gnomon.index_node_names('{t1}', array['{own_version}'::uuid])"),
        format!("select * from gnomon.node_names('{t1}', array['{own_version}'::uuid])"),
    ];
    for role in [USER, WORKER] {
        for sql in &attempts {
            // As the caller's own statement, and from a trigger of the
            // caller's own on a temporary table, which every role may create.
            let through_a_trigger = format!(
                "create temp table pulled (i int);
                 create function pg_temp.pull() returns trigger language plpgsql as $f$
                 begin
                   execute $q${sql}$q$;
                   return null;
                 end $f$;
                 create trigger pull after insert on pulled
                   for each statement execute function pg_temp.pull();
                 insert into pulled values (1);"
            );
            for attempt in [sql.clone(), through_a_trigger] {
                let mut client = w.db.login().await;
                let tx = as_caller(&mut client, role, &claims(t1, &["support"])).await;
                assert_refused_write(
                    tx.batch_execute(&attempt).await,
                    &format!("{role}: {attempt}"),
                );
            }
        }
    }
    assert_eq!(name_rows(&w.db).await, before);
}

#[tokio::test]
async fn callers_hold_only_select_on_the_name_table_and_the_writer_a_bits_update_and_the_definer_no_more_than_it_needs()
 {
    let w = world().await;
    let admin = w.db.admin().await;
    for role in [
        USER,
        WORKER,
        support::LOGIN_ROLE,
        "gnomon_writer",
        "gnomon_definer",
        "gnomon_eraser",
    ] {
        let privileges: Vec<String> = admin
            .query_typed(
                "select p from unnest(array['SELECT', 'INSERT', 'UPDATE', 'DELETE',
                                          'TRUNCATE', 'REFERENCES', 'TRIGGER', 'MAINTAIN']) p
                 where has_table_privilege($1, 'gnomon.node_name'::regclass, p)
                 union all
                 select 'UPDATE (' || a.attname || ')'
                 from pg_attribute a
                 where a.attrelid = 'gnomon.node_name'::regclass and a.attnum > 0
                   and not a.attisdropped
                   and has_column_privilege($1, a.attrelid, a.attnum, 'UPDATE')",
                &[(&role, Type::TEXT)],
            )
            .await
            .unwrap()
            .iter()
            .map(|r| r.get(0))
            .collect();
        let expected: Vec<&str> = match role {
            r if r == support::LOGIN_ROLE => vec![],
            "gnomon_definer" => vec!["SELECT", "INSERT", "DELETE"],
            "gnomon_eraser" => vec!["SELECT", "DELETE"],
            "gnomon_writer" => vec!["SELECT", "UPDATE (q)"],
            _ => vec!["SELECT"],
        };
        assert_eq!(privileges, expected, "{role} on gnomon.node_name");
    }
    // Callers reach Resolution through its functions, and no caller runs
    // the functions that derive the name rows or their bits.
    for (function, callers_may) in [
        (
            "gnomon.resolve(uuid, text, real[], timestamptz, integer)",
            true,
        ),
        (
            "gnomon.resolve_full_text(uuid, text, timestamptz, integer)",
            true,
        ),
        (
            "gnomon.resolve_vector(uuid, real[], timestamptz, integer)",
            true,
        ),
        (
            "gnomon.store_name_embedding(uuid, uuid, text, real[])",
            true,
        ),
        ("gnomon.node_names(uuid, uuid[])", false),
        ("gnomon.index_node_names(uuid, uuid[])", false),
        ("gnomon.node_name_derive()", false),
    ] {
        for role in [USER, WORKER] {
            let may: bool = admin
                .query_typed(
                    "select has_function_privilege($1, $2, 'EXECUTE')",
                    &[(&role, Type::TEXT), (&function, Type::TEXT)],
                )
                .await
                .unwrap()[0]
                .get(0);
            assert_eq!(may, callers_may, "{role} may execute {function}");
        }
    }
    // The caller roles' only policy is a read of their Tenant and Scopes,
    // which the writer shares, and the writer updates only rows of a Scope
    // it may write. The eraser reads and deletes in every Scope of the
    // Tenant.
    let policies: Vec<(String, Vec<String>, String)> = admin
        .query_typed(
            "select policyname::text, array(select r::text from unnest(roles) r order by 1), cmd::text
             from pg_policies
             where schemaname = 'gnomon' and tablename = 'node_name' order by 1",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect();
    assert_eq!(
        policies,
        vec![
            (
                "node_name_definer".to_owned(),
                vec!["gnomon_definer".to_owned()],
                "ALL".to_owned()
            ),
            (
                "node_name_eraser".to_owned(),
                vec!["gnomon_eraser".to_owned()],
                "ALL".to_owned()
            ),
            (
                "node_name_tenant_scope".to_owned(),
                vec![
                    "authenticated".to_owned(),
                    "gnomon_worker".to_owned(),
                    "gnomon_writer".to_owned()
                ],
                "SELECT".to_owned()
            ),
            (
                "node_name_writer_update".to_owned(),
                vec!["gnomon_writer".to_owned()],
                "UPDATE".to_owned()
            ),
        ]
    );
}

// Search (ADR-0010, `0004_search.sql`). The fused call, its token rows and
// embeddings of Summaries and Communities, and its graph signals run as the
// caller under RLS. No caller writes a token row, and an embedding row a
// caller inserts is derived from the stored row whatever it supplied.

/// The token rows and embeddings of Summaries and Communities.
const DERIVED_SEARCH_TABLES: &[&str] = &["derived_search_token", "derived_embedding"];

/// `searchable_world()` with a Summary and a Community of each Scope's Node,
/// "zebra summary <scope>" and "zebra community <scope>", each citing the
/// Scope's Episode and embedded by the Scope's worker, and a Summary of
/// Tenant 1's `support` Node with no embedding.
struct DerivedWorld {
    w: World,
    /// Tenant 1's rows: its `support` Summary, its `billing` Summary and
    /// Community, and Tenant 2's `support` Summary.
    support_summary: Uuid,
    unembedded_summary: Uuid,
    billing_summary: Uuid,
    billing_community: Uuid,
    other_tenant_summary: Uuid,
    support_node: Uuid,
}

async fn derived_world() -> DerivedWorld {
    let w = searchable_world().await;
    let store = Store::connect(&w.db.login_url(), &support::connect_options())
        .await
        .unwrap();
    let admin = w.db.admin().await;
    let mut rng = support::vectors::Rng::new(23);
    let mut summaries = std::collections::HashMap::new();
    let mut communities = std::collections::HashMap::new();
    let mut nodes = std::collections::HashMap::new();
    for row in admin
        .query_typed(
            "select n.tenant_id, n.scope, n.id, e.id from gnomon.node n
             join gnomon.episode e on e.tenant_id = n.tenant_id and e.scope = n.scope",
            &[],
        )
        .await
        .unwrap()
    {
        let (tenant, scope, node, episode): (Uuid, String, Uuid, Uuid) =
            (row.get(0), row.get(1), row.get(2), row.get(3));
        let caller = store.as_worker(TenantId(tenant), Scope::new(scope.clone()), None);
        let at = "2024-01-01T00:00:00Z".parse().unwrap();
        let summary = caller
            .write_summary(
                TenantId(tenant),
                &gnomon::NewSummary {
                    scope: Scope::new(scope.clone()),
                    node: gnomon::NodeId(node),
                    content: format!("zebra summary {scope}"),
                    as_of_valid: at,
                    citations: vec![gnomon::DerivedCitation::of(EpisodeId(episode))],
                },
            )
            .await
            .unwrap();
        let community = caller
            .write_community(
                TenantId(tenant),
                &gnomon::NewCommunity {
                    scope: Scope::new(scope.clone()),
                    replaces: vec![],
                    members: vec![gnomon::NodeId(node)],
                    content: format!("zebra community {scope}"),
                    as_of_valid: at,
                    citations: vec![gnomon::DerivedCitation::of(EpisodeId(episode))],
                },
            )
            .await
            .unwrap();
        caller
            .store_summary_embedding(TenantId(tenant), summary, &rng.unit())
            .await
            .unwrap();
        caller
            .store_community_embedding(TenantId(tenant), community, &rng.unit())
            .await
            .unwrap();
        summaries.insert((tenant, scope.clone()), summary.0);
        communities.insert((tenant, scope.clone()), community.0);
        nodes.insert((tenant, scope), node);
    }
    let support_node = nodes[&(w.t1, "support".to_owned())];
    // A second Node in Tenant 1's `support`, with a Summary and no embedding.
    let (other, _) = store
        .as_worker(TenantId(w.t1), Scope::new("support"), None)
        .create_node(
            &gnomon::NewNode {
                tenant: Some(TenantId(w.t1)),
                scope: Scope::new("support"),
                kind: gnomon::ROOT_NODE_KIND.into(),
            },
            &gnomon::Patch::new(
                EpisodeId(w.t1_support),
                gnomon::ValidTime::UNKNOWN,
                support::erasure::values(json!({ "name": "plain" })),
            ),
        )
        .await
        .unwrap();
    let unembedded = store
        .as_worker(TenantId(w.t1), Scope::new("support"), None)
        .write_summary(
            TenantId(w.t1),
            &gnomon::NewSummary {
                scope: Scope::new("support"),
                node: other,
                content: "plain prose".into(),
                as_of_valid: "2024-01-01T00:00:00Z".parse().unwrap(),
                citations: vec![gnomon::DerivedCitation::of(EpisodeId(w.t1_support))],
            },
        )
        .await
        .unwrap();
    DerivedWorld {
        support_summary: summaries[&(w.t1, "support".to_owned())],
        unembedded_summary: unembedded.0,
        billing_summary: summaries[&(w.t1, "billing".to_owned())],
        billing_community: communities[&(w.t1, "billing".to_owned())],
        other_tenant_summary: summaries[&(w.t2, "support".to_owned())],
        support_node,
        w,
    }
}

async fn derived_search_rows(db: &TestDb) -> Vec<String> {
    let tables: Vec<String> = DERIVED_SEARCH_TABLES
        .iter()
        .map(|t| t.to_string())
        .collect();
    support::erasure::rows_of(&db.admin().await, &tables).await
}

/// `gnomon.search` as SQL, at valid-at 2025-01-01, reading the current
/// belief, with the default seeds, depth and weights.
fn search_sql(query_vector: &str) -> String {
    format!(
        "select kind, id, scope, score, rank, vector_rank, full_text_rank, neighbourhood_rank,
                mentions, mentions_rank, connectedness, connectedness_rank
         from gnomon.search($1, 'zebra', {query_vector}, '2025-01-01', null, null, null,
                            null, null, null, null, null, 50)"
    )
}

#[tokio::test]
async fn a_caller_reads_only_its_own_tenants_and_scopes_summary_search_rows_and_searches_only_them()
{
    let dw = derived_world().await;
    let w = &dw.w;
    let search = search_sql(&sql_vector());
    for role in [USER, WORKER] {
        let mut client = w.db.login().await;
        let tx = as_caller(&mut client, role, &claims(w.t1, &["support"])).await;
        for table in DERIVED_SEARCH_TABLES {
            let seen: Vec<(Uuid, String)> = tx
                .query_typed(
                    &format!("select distinct tenant_id, scope from gnomon.{table}"),
                    &[],
                )
                .await
                .unwrap()
                .iter()
                .map(|r| (r.get(0), r.get(1)))
                .collect();
            assert_eq!(
                seen,
                vec![(w.t1, "support".to_owned())],
                "{role} read gnomon.{table}"
            );
        }
        // Search finds Versions, Summaries and Communities of the caller's
        // own Scope only, and nothing when it names the other Tenant.
        for (tenant, expected) in [(w.t1, vec!["support".to_owned()]), (w.t2, vec![])] {
            let rows = tx
                .query_typed(&search, &[(&tenant, Type::UUID)])
                .await
                .unwrap();
            let mut scopes: Vec<String> = rows.iter().map(|r| r.get(2)).collect();
            scopes.dedup();
            assert_eq!(scopes, expected, "{role} searched in {tenant}");
            if tenant == w.t1 {
                let mut kinds: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
                kinds.sort();
                kinds.dedup();
                assert_eq!(kinds, ["community", "summary", "version"], "{role}");
            }
        }
    }
}

#[tokio::test]
async fn a_caller_cannot_write_the_summary_search_rows_or_forge_an_embedding_of_one() {
    let dw = derived_world().await;
    let w = &dw.w;
    let before = derived_search_rows(&w.db).await;
    let (t1, t2) = (w.t1, w.t2);
    let (own, billing, billing_community, other_tenant) = (
        dw.support_summary,
        dw.billing_summary,
        dw.billing_community,
        dw.other_tenant_summary,
    );
    let v = sql_vector();
    let dim = support::vectors::DIM;
    let embedding_row = |tenant: Uuid, scope: &str, summary: Uuid| {
        format!(
            "insert into gnomon.derived_embedding (tenant_id, scope, summary_id, embedding, q,
                                                   as_of_valid, recorded_from, recorded_to)
             values ('{tenant}', '{scope}', '{summary}', gnomon.embedding_vector({v}),
                     repeat('1', {dim})::bit({dim}), '-infinity', '-infinity', 'infinity')"
        )
    };
    let attempts = [
        // Token rows, in the caller's own Scope, another Scope and another
        // Tenant: callers hold no INSERT.
        format!(
            "insert into gnomon.derived_search_token (tenant_id, scope, token, summary_id, tf, dl)
             values ('{t1}', 'support', 'forged', '{own}', 1, 1)"
        ),
        format!(
            "insert into gnomon.derived_search_token (tenant_id, scope, token, summary_id, tf, dl)
             values ('{t1}', 'billing', 'forged', '{billing}', 1, 1)"
        ),
        format!(
            "insert into gnomon.derived_search_token (tenant_id, scope, token, summary_id, tf, dl)
             values ('{t2}', 'support', 'forged', '{other_tenant}', 1, 1)"
        ),
        "update gnomon.derived_search_token set tf = tf + 1".to_owned(),
        "delete from gnomon.derived_search_token".to_owned(),
        "truncate gnomon.derived_search_token".to_owned(),
        // An embedding row of another Scope's or Tenant's Summary, and of the
        // caller's own Summary in another Scope.
        embedding_row(t1, "billing", billing),
        embedding_row(t2, "support", other_tenant),
        embedding_row(t1, "support", billing),
        // What only the Store derives, and what only Erasure deletes.
        "update gnomon.derived_embedding set recorded_to = '2001-01-01'".to_owned(),
        "update gnomon.derived_embedding set q = ~q".to_owned(),
        "update gnomon.derived_embedding set embedding = embedding".to_owned(),
        "delete from gnomon.derived_embedding".to_owned(),
        "truncate gnomon.derived_embedding".to_owned(),
        // The Store's functions, for another Scope's or Tenant's rows.
        format!("select gnomon.store_summary_embedding('{t1}', '{billing}', {v})"),
        format!("select gnomon.store_community_embedding('{t1}', '{billing_community}', {v})"),
        format!("select gnomon.store_summary_embedding('{t2}', '{other_tenant}', {v})"),
        format!("select gnomon.store_summary_embedding('{t1}', '{other_tenant}', {v})"),
        // The function that derives the token rows, called directly, and the
        // search index trigger attached to a table of the caller's own.
        format!(
            "select * from gnomon.derived_text_tokens('{t1}', 'summary', array['{own}'::uuid])"
        ),
        "create temp table mine (id uuid, tenant_id uuid);
         create trigger mine after insert on mine referencing new table as changed
           for each statement execute function gnomon.maintain_search_index()"
            .to_owned(),
    ];
    for role in [USER, WORKER] {
        for sql in &attempts {
            let through_a_trigger = format!(
                "create temp table pulled (i int);
                 create function pg_temp.pull() returns trigger language plpgsql as $f$
                 begin
                   execute $q${sql}$q$;
                   return null;
                 end $f$;
                 create trigger pull after insert on pulled
                   for each statement execute function pg_temp.pull();
                 insert into pulled values (1);"
            );
            for attempt in [sql.clone(), through_a_trigger] {
                let mut client = w.db.login().await;
                let tx = as_caller(&mut client, role, &claims(t1, &["support"])).await;
                assert_refused_write(
                    tx.batch_execute(&attempt).await,
                    &format!("{role}: {attempt}"),
                );
            }
        }
    }
    assert_eq!(derived_search_rows(&w.db).await, before);

    // An embedding row the writer role inserts for a Summary holds the bits
    // of its own vector and the Summary's time columns, whatever the insert
    // supplied.
    let mut client = w.db.admin().await;
    let tx = as_writer(&mut client, &claims(t1, &["support"])).await;
    tx.batch_execute(&embedding_row(t1, "support", dw.unembedded_summary))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let admin = w.db.admin().await;
    let ext: String = admin
        .query_typed(
            "select n.nspname::text from pg_extension e
             join pg_namespace n on n.oid = e.extnamespace where e.extname = 'vector'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    let dim = support::vectors::DIM;
    let row = admin
        .query_typed(
            &format!(
                "select d.q = {}.binary_quantize(d.embedding)::bit({dim}),
                        d.as_of_valid = s.as_of_valid, d.recorded_from = s.recorded_from,
                        d.recorded_to = s.recorded_to
                 from gnomon.derived_embedding d
                 join gnomon.summary s on s.id = d.summary_id
                 where d.summary_id = $1",
                support::quote_ident(&ext)
            ),
            &[(&dw.unembedded_summary, Type::UUID)],
        )
        .await
        .unwrap();
    let (bits, valid, from, to): (bool, bool, bool, bool) =
        (row[0].get(0), row[0].get(1), row[0].get(2), row[0].get(3));
    assert!(bits, "the bits are those of the caller's own vector");
    assert!(valid && from && to, "the time columns are the Summary's");
}

#[tokio::test]
async fn callers_hold_only_select_on_the_summary_token_rows_and_their_embeddings_and_the_writer_inserts_embeddings()
 {
    let w = world().await;
    let admin = w.db.admin().await;
    for (table, writer, definer) in [
        (
            "gnomon.derived_search_token",
            vec![],
            vec!["SELECT", "INSERT", "DELETE"],
        ),
        ("gnomon.derived_embedding", vec!["SELECT", "INSERT"], vec![]),
    ] {
        for role in [
            USER,
            WORKER,
            support::LOGIN_ROLE,
            "gnomon_writer",
            "gnomon_definer",
            "gnomon_eraser",
        ] {
            let privileges: Vec<String> = admin
                .query_typed(
                    "select p from unnest(array['SELECT', 'INSERT', 'UPDATE', 'DELETE',
                                              'TRUNCATE', 'REFERENCES', 'TRIGGER', 'MAINTAIN']) p
                     where has_table_privilege($1, $2::regclass, p)
                     union all
                     select 'UPDATE (' || a.attname || ')'
                     from pg_attribute a
                     where a.attrelid = $2::regclass and a.attnum > 0 and not a.attisdropped
                       and has_column_privilege($1, a.attrelid, a.attnum, 'UPDATE')",
                    &[(&role, Type::TEXT), (&table, Type::TEXT)],
                )
                .await
                .unwrap()
                .iter()
                .map(|r| r.get(0))
                .collect();
            let expected = match role {
                r if r == support::LOGIN_ROLE => vec![],
                "gnomon_definer" => definer.clone(),
                "gnomon_eraser" => vec!["SELECT", "DELETE"],
                "gnomon_writer" => writer.clone(),
                _ => vec!["SELECT"],
            };
            assert_eq!(privileges, expected, "{role} on {table}");
        }
    }
    // Callers reach Search through its functions, and no caller runs the
    // functions that derive the token rows or an embedding row's bits.
    for (function, callers_may) in [
        (
            "gnomon.search(uuid, text, real[], timestamptz, timestamptz, uuid[], integer, \
          double precision, double precision, double precision, double precision, \
          double precision, integer)",
            true,
        ),
        ("gnomon.store_summary_embedding(uuid, uuid, real[])", true),
        ("gnomon.store_community_embedding(uuid, uuid, real[])", true),
        (
            "gnomon.search_neighbourhood(uuid, uuid[], integer, timestamptz, timestamptz, integer)",
            true,
        ),
        ("gnomon.derived_text_tokens(uuid, text, uuid[])", false),
        ("gnomon.derived_embedding_derive()", false),
        ("gnomon.maintain_search_index()", false),
    ] {
        for role in [USER, WORKER] {
            let may: bool = admin
                .query_typed(
                    "select has_function_privilege($1, $2, 'EXECUTE')",
                    &[(&role, Type::TEXT), (&function, Type::TEXT)],
                )
                .await
                .unwrap()[0]
                .get(0);
            assert_eq!(may, callers_may, "{role} may execute {function}");
        }
    }
    // The search index trigger is still the one definer that writes the
    // search tables, owned by the definer role.
    let owner: String = admin
        .query_typed(
            "select p.proowner::regrole::text from pg_proc p
             where p.oid = 'gnomon.maintain_search_index()'::regprocedure and p.prosecdef",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(owner, "gnomon_definer");
    // The callers' only policy is a read of their Tenant and Scopes, and the
    // writer inserts embeddings only in a Scope it may write. The definer
    // writes the token rows in the caller's Scopes and has no policy on the
    // embeddings, and the eraser reads and deletes both in every Scope of the
    // Tenant.
    for table in DERIVED_SEARCH_TABLES {
        let policies: Vec<(String, Vec<String>, String)> = admin
            .query_typed(
                "select policyname::text, array(select r::text from unnest(roles) r order by 1),
                        cmd::text
                 from pg_policies
                 where schemaname = 'gnomon' and tablename = $1 order by 1",
                &[(table, Type::TEXT)],
            )
            .await
            .unwrap()
            .iter()
            .map(|r| (r.get(0), r.get(1), r.get(2)))
            .collect();
        let embeddings = *table == "derived_embedding";
        let mut readers = vec!["authenticated".to_owned(), "gnomon_worker".to_owned()];
        if embeddings {
            readers.push("gnomon_writer".to_owned());
        }
        let mut expected = vec![];
        if !embeddings {
            expected.push((
                format!("{table}_definer"),
                vec!["gnomon_definer".to_owned()],
                "ALL".to_owned(),
            ));
        }
        expected.extend([
            (
                format!("{table}_eraser"),
                vec!["gnomon_eraser".to_owned()],
                "ALL".to_owned(),
            ),
            (
                format!("{table}_tenant_scope"),
                readers,
                "SELECT".to_owned(),
            ),
        ]);
        if embeddings {
            expected.push((
                format!("{table}_writer_insert"),
                vec!["gnomon_writer".to_owned()],
                "INSERT".to_owned(),
            ));
        }
        assert_eq!(policies, expected, "gnomon.{table}");
    }
}

#[tokio::test]
async fn an_episode_edge_or_summary_of_a_scope_the_caller_cannot_see_changes_no_ranking_of_search()
{
    let dw = derived_world().await;
    let w = &dw.w;
    let search = search_sql(&sql_vector());
    let ranked = |scopes: &'static [&'static str]| {
        let search = &search;
        async move {
            let mut client = w.db.login().await;
            let tx = as_caller(&mut client, USER, &claims(w.t1, scopes)).await;
            let rows = tx
                .query_typed(search, &[(&w.t1, Type::UUID)])
                .await
                .unwrap();
            rows.iter()
                .map(|r| {
                    format!(
                        "{} {} {} {:?} {} {:?} {:?} {:?} {} {:?} {} {:?}",
                        r.get::<_, String>(0),
                        r.get::<_, Uuid>(1),
                        r.get::<_, String>(2),
                        r.get::<_, f64>(3).to_bits(),
                        r.get::<_, i64>(4),
                        r.get::<_, Option<i64>>(5),
                        r.get::<_, Option<i64>>(6),
                        r.get::<_, Option<i64>>(7),
                        r.get::<_, i64>(8),
                        r.get::<_, Option<i64>>(9),
                        r.get::<_, i64>(10),
                        r.get::<_, Option<i64>>(11),
                    )
                })
                .collect::<Vec<String>>()
        }
    };
    let before = ranked(&["support"]).await;
    let both_before = ranked(&["support", "billing"]).await;
    assert!(!before.is_empty());

    // In `billing`, which the caller cannot see: three Episodes, each the
    // source of an Edge from the caller's `support` Node to a `billing` Node,
    // and of another Summary of the `support` Node, written by a user who
    // sees both Scopes.
    let store = Store::connect(&w.db.login_url(), &support::connect_options())
        .await
        .unwrap();
    let t1 = TenantId(w.t1);
    let both = store.as_user(support::erasure::values(json!({
        "role": "authenticated",
        "app_metadata": { "tenant_id": w.t1.to_string(), "scopes": ["support", "billing"] }
    })));
    let mut episodes = vec![];
    for i in 0..3 {
        let e = both
            .record_episode(&NewEpisode {
                tenant: Some(t1),
                scope: Scope::new("billing"),
                content: format!("hidden {i}"),
                reference_time: "2024-01-01T00:00:00Z".parse().unwrap(),
                authority: None,
            })
            .await
            .unwrap();
        episodes.push(e);
        let (far, _) = both
            .create_node(
                &gnomon::NewNode {
                    tenant: Some(t1),
                    scope: Scope::new("billing"),
                    kind: gnomon::ROOT_NODE_KIND.into(),
                },
                &gnomon::Patch::new(
                    e,
                    gnomon::ValidTime::UNKNOWN,
                    support::erasure::values(json!({ "name": format!("far {i}") })),
                ),
            )
            .await
            .unwrap();
        both.create_edge(
            &gnomon::NewEdge {
                tenant: Some(t1),
                scope: Scope::new("billing"),
                kind: gnomon::ROOT_EDGE_KIND.into(),
                source: gnomon::NodeId(dw.support_node),
                target: far,
            },
            &gnomon::Patch::new(e, gnomon::ValidTime::UNKNOWN, serde_json::Map::new()),
        )
        .await
        .unwrap();
    }
    both.write_summary(
        t1,
        &gnomon::NewSummary {
            scope: Scope::new("billing"),
            node: gnomon::NodeId(dw.support_node),
            content: "zebra zebra, as billing tells it".into(),
            as_of_valid: "2024-01-01T00:00:00Z".parse().unwrap(),
            citations: episodes
                .iter()
                .map(|e| gnomon::DerivedCitation::of(*e))
                .collect(),
        },
    )
    .await
    .unwrap();

    // The caller of `support` alone sees the same ranking, every score,
    // rank and count included, while a caller of both Scopes sees another.
    assert_eq!(ranked(&["support"]).await, before);
    assert_ne!(ranked(&["support", "billing"]).await, both_before);
}

// Sentences and the gap listing (ADR-0009, ADR-0013, `0004_search.sql`).

/// The Edges of `edges()`, each Edge Version's ID, and an Edge of Tenant
/// one's `billing` Scope from its Node to itself. Tenant two's Edge and the
/// billing Edge each have a Sentence, stored by their own Scope's worker.
struct Sentenced {
    e: Edges,
    e2_open: Uuid,
    billing_open: Uuid,
}

const STORE_SENTENCE: &str = "select gnomon.store_sentence($1, $2, $3)";

async fn sentenced() -> Sentenced {
    let e = edges().await;
    let store = Store::connect(&e.n.w.db.login_url(), &support::connect_options())
        .await
        .unwrap();
    let (_, written) = store
        .as_worker(TenantId(e.n.w.t1), Scope::new("billing"), None)
        .create_edge(
            &gnomon::NewEdge {
                tenant: Some(TenantId(e.n.w.t1)),
                scope: Scope::new("billing"),
                kind: gnomon::ROOT_EDGE_KIND.into(),
                source: gnomon::NodeId(e.billing_node),
                target: gnomon::NodeId(e.billing_node),
            },
            &gnomon::Patch::new(
                EpisodeId(e.n.w.t1_billing),
                gnomon::ValidTime::UNKNOWN,
                Default::default(),
            ),
        )
        .await
        .unwrap();
    let billing_open = written.opened[0];
    let e2_open =
        e.n.w
            .db
            .admin()
            .await
            .query_typed(
                "select id from gnomon.version where edge_id = $1",
                &[(&e.e2, Type::UUID)],
            )
            .await
            .unwrap()[0]
            .get(0);
    for (t, scope, version) in [
        (e.n.w.t2, "support", e2_open),
        (e.n.w.t1, "billing", billing_open.0),
    ] {
        store
            .as_worker(TenantId(t), Scope::new(scope), None)
            .store_sentence(TenantId(t), gnomon::VersionId(version), "Its own Sentence.")
            .await
            .unwrap();
    }
    Sentenced {
        e,
        e2_open,
        billing_open: billing_open.0,
    }
}

/// Every Version and Sentence row, as text, read past RLS.
async fn versions_and_sentences(db: &TestDb) -> Vec<String> {
    db.admin()
        .await
        .query_typed(
            "select format('%s %s %s %s', id, recorded_from, recorded_to, props)
             from gnomon.version
             union all
             select format('%s %s %s %s', tenant_id, scope, version_id, sentence)
             from gnomon.sentence
             order by 1",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect()
}

/// Fails unless `result` is a refusal: those of [`assert_refused_write`], or
/// a check constraint or a bad parameter.
fn assert_refused_sentence(
    result: Result<impl std::fmt::Debug, tokio_postgres::Error>,
    what: &str,
) {
    match result {
        Err(e)
            if [SqlState::CHECK_VIOLATION, SqlState::INVALID_PARAMETER_VALUE]
                .iter()
                .any(|c| e.code() == Some(c)) => {}
        other => assert_refused_write(other, what),
    }
}

#[tokio::test]
async fn a_caller_stores_a_sentence_only_of_an_edge_version_it_sees() {
    let s = sentenced().await;
    let w = &s.e.n.w;
    let (t1, t2) = (w.t1, w.t2);
    let before = versions_and_sentences(&w.db).await;
    for role in [USER, WORKER] {
        let calls: [(Uuid, Uuid, &str, &str); 6] = [
            (
                t2,
                s.e2_open,
                "A forged Sentence.",
                "another Tenant's Edge Version",
            ),
            (
                t1,
                s.e2_open,
                "A forged Sentence.",
                "another Tenant's Edge Version as its own",
            ),
            (
                t1,
                s.billing_open,
                "A forged Sentence.",
                "an Edge Version of a Scope it cannot see",
            ),
            (t1, s.e.n.open, "A forged Sentence.", "a Node Version"),
            (t1, Uuid::new_v4(), "A forged Sentence.", "no Version"),
            (t1, s.e.e1_open, " \t", "a Sentence with no word"),
        ];
        for (tenant, version, sentence, what) in calls {
            let mut client = w.db.login().await;
            let tx = as_caller(&mut client, role, &claims(t1, &["support"])).await;
            let r = tx
                .execute_typed(
                    STORE_SENTENCE,
                    &[
                        (&tenant, Type::UUID),
                        (&version, Type::UUID),
                        (&sentence, Type::TEXT),
                    ],
                )
                .await;
            assert_refused_sentence(r, &format!("{role} storing through the function {what}"));
        }
        // Another Tenant's and another Scope's Sentences are not there to
        // read.
        let mut client = w.db.login().await;
        let tx = as_caller(&mut client, role, &claims(t1, &["support"])).await;
        let seen: i64 = tx
            .query_typed("select count(*) from gnomon.sentence", &[])
            .await
            .unwrap()[0]
            .get(0);
        assert_eq!(seen, 0, "{role} saw another Scope's or Tenant's Sentences");
        tx.rollback().await.unwrap();
    }
    assert_eq!(
        versions_and_sentences(&w.db).await,
        before,
        "no Version, Recorded time or Sentence changed"
    );
}

#[tokio::test]
async fn storing_a_sentence_writes_no_version_and_changes_no_recorded_time() {
    let s = sentenced().await;
    let w = &s.e.n.w;
    let versions = || async {
        w.db.admin()
            .await
            .query_typed(
                "select format('%s %s %s %s %s', id, recorded_from, recorded_to, props, written_in)
                 from gnomon.version order by 1",
                &[],
            )
            .await
            .unwrap()
            .iter()
            .map(|r| r.get::<_, String>(0))
            .collect::<Vec<_>>()
    };
    let before = versions().await;
    for (role, sentence) in [(USER, "First."), (WORKER, "Second."), (USER, "Second.")] {
        let mut client = w.db.login().await;
        let tx = as_caller(&mut client, role, &claims(w.t1, &["support"])).await;
        tx.execute_typed(
            STORE_SENTENCE,
            &[
                (&w.t1, Type::UUID),
                (&s.e.e1_open, Type::UUID),
                (&sentence, Type::TEXT),
            ],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }
    assert_eq!(versions().await, before);
}

/// The quantized rows and full vectors of a Tenant's Scope, read past RLS.
async fn scope_embedding_rows(
    db: &TestDb,
    tenant: Uuid,
    scope: &str,
) -> (Vec<String>, Vec<String>) {
    let admin = db.admin().await;
    let read = |sql: &'static str| {
        let admin = &admin;
        let scope = scope.to_owned();
        async move {
            admin
                .query_typed(sql, &[(&tenant, Type::UUID), (&scope, Type::TEXT)])
                .await
                .unwrap()
                .iter()
                .map(|r| r.get::<_, String>(0))
                .collect::<Vec<_>>()
        }
    };
    (
        read(
            "select version_id || ' ' || text_hash from gnomon.embedding_bit
             where tenant_id = $1 and scope = $2 order by 1",
        )
        .await,
        read(
            "select text_hash from gnomon.embedding where tenant_id = $1 and scope = $2
             order by 1",
        )
        .await,
    )
}

#[tokio::test]
async fn a_replaced_sentence_leaves_no_embedding_of_its_old_text_and_touches_no_other_scope() {
    let s = sentenced().await;
    let w = &s.e.n.w;
    let store = Store::connect(&w.db.login_url(), &support::connect_options())
        .await
        .unwrap();
    let mut rng = support::vectors::Rng::new(260);
    let vector = rng.unit();
    let text = "Its own Sentence.";
    let hash: String =
        w.db.admin()
            .await
            .query_typed(
                "select gnomon.sentence_text_hash($1)",
                &[(&text, Type::TEXT)],
            )
            .await
            .unwrap()[0]
            .get(0);
    // The same text, embedded with the same vector, in Tenant one's support
    // and billing and in Tenant two's support.
    for (t, scope, version) in [
        (w.t1, "support", s.e.e1_open),
        (w.t1, "billing", s.billing_open),
        (w.t2, "support", s.e2_open),
    ] {
        let caller = store.as_worker(TenantId(t), Scope::new(scope), None);
        caller
            .store_sentence(TenantId(t), gnomon::VersionId(version), text)
            .await
            .unwrap();
        caller
            .store_embedding(TenantId(t), gnomon::VersionId(version), &hash, &vector)
            .await
            .unwrap();
    }
    let billing = scope_embedding_rows(&w.db, w.t1, "billing").await;
    let other_tenant = scope_embedding_rows(&w.db, w.t2, "support").await;
    assert_eq!(billing.1, vec![hash.clone()]);

    for role in [USER, WORKER] {
        // A caller deletes no embedding row itself, its own Scope's included.
        for table in ["embedding", "embedding_bit"] {
            let mut client = w.db.login().await;
            let tx = as_caller(&mut client, role, &claims(w.t1, &["support"])).await;
            let r = tx
                .execute_typed(&format!("delete from gnomon.{table}"), &[])
                .await;
            assert_refused(r, &format!("{role} deleting from gnomon.{table}"));
        }
        let mut client = w.db.login().await;
        let tx = as_caller(&mut client, role, &claims(w.t1, &["support"])).await;
        let r = tx.execute_typed("truncate gnomon.embedding_bit", &[]).await;
        assert_refused(r, &format!("{role} truncating gnomon.embedding_bit"));

        // Replacing its Sentence removes the old text's embedding, in the
        // same transaction, and only its own.
        let mut client = w.db.login().await;
        let tx = as_caller(&mut client, role, &claims(w.t1, &["support"])).await;
        tx.execute_typed(
            STORE_SENTENCE,
            &[
                (&w.t1, Type::UUID),
                (&s.e.e1_open, Type::UUID),
                (&"A new Sentence.", Type::TEXT),
            ],
        )
        .await
        .unwrap();
        let left: i64 = tx
            .query_typed(
                "select (select count(*) from gnomon.embedding_bit where version_id = $1)
                      + (select count(*) from gnomon.embedding where text_hash = $2)",
                &[(&s.e.e1_open, Type::UUID), (&hash, Type::TEXT)],
            )
            .await
            .unwrap()[0]
            .get(0);
        assert_eq!(left, 0, "{role} still sees the old text's embedding");
        let hits = tx
            .query_typed(
                "select version_id from gnomon.search_vector($1, $2, now(), null, 10)",
                &[(&w.t1, Type::UUID), (&vector, Type::FLOAT4_ARRAY)],
            )
            .await
            .unwrap();
        assert!(hits.is_empty(), "{role} found the old text by vector");
        tx.rollback().await.unwrap();
    }

    // Committed by the Store, as the Pipeline would.
    store
        .as_worker(TenantId(w.t1), Scope::new("support"), None)
        .store_sentence(
            TenantId(w.t1),
            gnomon::VersionId(s.e.e1_open),
            "A new Sentence.",
        )
        .await
        .unwrap();
    assert_eq!(
        scope_embedding_rows(&w.db, w.t1, "support").await,
        (vec![], vec![])
    );
    assert_eq!(scope_embedding_rows(&w.db, w.t1, "billing").await, billing);
    assert_eq!(
        scope_embedding_rows(&w.db, w.t2, "support").await,
        other_tenant
    );
    // No row of any table names the old text in that Scope.
    let admin = w.db.admin().await;
    let residue: Vec<String> = support::erasure::every_row(&admin)
        .await
        .into_iter()
        .filter(|r| r.contains(&hash) && r.contains(&s.e.e1_open.to_string()))
        .collect();
    assert_eq!(residue, Vec::<String>::new());
    support::erasure::assert_no_unused_full_vector(&admin).await;
}

const LIST_GAPS: &str = "select kind, id from gnomon.list_gaps($1, $2, null, null, null, 1000)";

/// Every ID of a row of a Tenant's Scope that a gap can name: its Versions,
/// Nodes, Summaries and Communities, read past RLS.
async fn ids_in(db: &TestDb, tenant: Uuid, scope: &str) -> std::collections::HashSet<Uuid> {
    db.admin()
        .await
        .query_typed(
            "select id from gnomon.version where tenant_id = $1 and scope = $2
             union all select id from gnomon.node where tenant_id = $1 and scope = $2
             union all select id from gnomon.summary where tenant_id = $1 and scope = $2
             union all select id from gnomon.community where tenant_id = $1 and scope = $2",
            &[(&tenant, Type::UUID), (&scope, Type::TEXT)],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect()
}

#[tokio::test]
async fn the_gap_listing_names_only_the_callers_tenant_and_scope() {
    let s = sentenced().await;
    let w = &s.e.n.w;
    let (t1, t2) = (w.t1, w.t2);
    // Each other Scope has gaps of its own: an Edge Version of billing has
    // no Sentence and the other's Sentence no embedding, and Tenant two's
    // Node has no Summary.
    let store = Store::connect(&w.db.login_url(), &support::connect_options())
        .await
        .unwrap();
    let (_, unsentenced) = store
        .as_worker(TenantId(t1), Scope::new("billing"), None)
        .create_edge(
            &gnomon::NewEdge {
                tenant: Some(TenantId(t1)),
                scope: Scope::new("billing"),
                kind: gnomon::ROOT_EDGE_KIND.into(),
                source: gnomon::NodeId(s.e.billing_node),
                target: gnomon::NodeId(s.e.billing_node),
            },
            &gnomon::Patch::new(
                EpisodeId(w.t1_billing),
                gnomon::ValidTime::UNKNOWN,
                Default::default(),
            ),
        )
        .await
        .unwrap();
    let billing_gaps: Vec<String> = store
        .as_worker(TenantId(t1), Scope::new("billing"), None)
        .list_gaps(TenantId(t1), &Scope::new("billing"), None, 1000)
        .await
        .unwrap()
        .gaps
        .iter()
        .map(|g| format!("{g:?}"))
        .collect();
    let unsentenced = unsentenced.opened[0].0.to_string();
    assert!(
        billing_gaps
            .iter()
            .any(|g| g.starts_with("Sentence ") && g.contains(&unsentenced)),
        "{billing_gaps:?}"
    );
    assert!(!ids_in(&w.db, t2, "support").await.is_empty());
    let support_ids = ids_in(&w.db, t1, "support").await;

    for role in [USER, WORKER] {
        let mut listings = Vec::new();
        for scopes in [&["support"][..], &["support", "billing"][..]] {
            let mut client = w.db.login().await;
            let tx = as_caller(&mut client, role, &claims(t1, scopes)).await;
            let rows = tx
                .query_typed(LIST_GAPS, &[(&t1, Type::UUID), (&"support", Type::TEXT)])
                .await
                .unwrap();
            let listed: Vec<(String, Uuid)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
            assert!(!listed.is_empty(), "{role} {scopes:?} listed nothing");
            for (kind, id) in &listed {
                assert!(
                    support_ids.contains(id),
                    "{role} {scopes:?} listed a {kind} gap {id} outside Tenant one's support"
                );
            }
            listings.push(listed);
        }
        assert_eq!(
            listings[0], listings[1],
            "{role}: a second Scope changed the listing"
        );

        // A Scope it cannot see, another Tenant, claims of no Tenant, and a
        // page token of no kind are refused.
        for (tenant, scope, claims, what) in [
            (
                t1,
                "billing",
                claims(t1, &["support"]),
                "a Scope it cannot see",
            ),
            (t2, "support", claims(t1, &["support"]), "another Tenant"),
            (
                t2,
                "support",
                claims(t2, &[]),
                "another Tenant's claims with no Scope",
            ),
            (
                t1,
                "support",
                json!({ "role": "authenticated" }).to_string(),
                "claims of no Tenant",
            ),
        ] {
            let mut client = w.db.login().await;
            let tx = as_caller(&mut client, role, &claims).await;
            let r = tx
                .query_typed(LIST_GAPS, &[(&tenant, Type::UUID), (&scope, Type::TEXT)])
                .await;
            assert_refused_write(r, &format!("{role} listing the gaps of {what}"));
        }
        let mut client = w.db.login().await;
        let tx = as_caller(&mut client, role, &claims(t1, &["support"])).await;
        let r = tx
            .query_typed(
                "select id from gnomon.list_gaps($1, 'support', 'version', $2, null, 10)",
                &[(&t1, Type::UUID), (&s.e.e1_open, Type::UUID)],
            )
            .await;
        assert_eq!(
            r.unwrap_err().code(),
            Some(&SqlState::INVALID_PARAMETER_VALUE),
            "{role}: a page token of no kind"
        );
        // A token of an embedding's kind names its text's hash, and one of
        // another kind names none.
        for (kind, hash) in [("node_embedding", None), ("sentence", Some("ab"))] {
            let mut client = w.db.login().await;
            let tx = as_caller(&mut client, role, &claims(t1, &["support"])).await;
            let r = tx
                .query_typed(
                    "select id from gnomon.list_gaps($1, 'support', $2, $3, $4, 10)",
                    &[
                        (&t1, Type::UUID),
                        (&kind, Type::TEXT),
                        (&s.e.e1_open, Type::UUID),
                        (&hash, Type::TEXT),
                    ],
                )
                .await;
            assert_eq!(
                r.unwrap_err().code(),
                Some(&SqlState::INVALID_PARAMETER_VALUE),
                "{role}: a {kind} token with hash {hash:?}"
            );
        }
    }
}

#[tokio::test]
async fn the_writer_inserts_a_sentence_and_changes_only_its_text_and_store_sentence_is_its_definer()
{
    let w = world().await;
    let admin = w.db.admin().await;
    for role in ["gnomon_writer"] {
        let table: Vec<String> = admin
            .query_typed(
                "select p from unnest(array['SELECT', 'INSERT', 'UPDATE', 'DELETE',
                                          'TRUNCATE', 'REFERENCES', 'TRIGGER', 'MAINTAIN']) p
                 where has_table_privilege($1, 'gnomon.sentence', p)",
                &[(&role, Type::TEXT)],
            )
            .await
            .unwrap()
            .iter()
            .map(|r| r.get(0))
            .collect();
        assert_eq!(table, ["SELECT"], "{role} on gnomon.sentence");
        let columns: Vec<String> = admin
            .query_typed(
                "select c || ' ' || p
                 from unnest(array['tenant_id', 'scope', 'version_id', 'sentence']) c,
                      unnest(array['INSERT', 'UPDATE']) p
                 where has_column_privilege($1, 'gnomon.sentence', c, p)
                 order by 1",
                &[(&role, Type::TEXT)],
            )
            .await
            .unwrap()
            .iter()
            .map(|r| r.get(0))
            .collect();
        assert_eq!(
            columns,
            [
                "scope INSERT",
                "sentence INSERT",
                "sentence UPDATE",
                "tenant_id INSERT",
                "version_id INSERT"
            ],
            "{role}'s column privileges on gnomon.sentence"
        );
        for table in ["embedding", "embedding_bit"] {
            let deletes: bool = admin
                .query_typed(
                    "select has_table_privilege($1, ('gnomon.' || $2)::regclass, 'DELETE')
                         or has_table_privilege($1, ('gnomon.' || $2)::regclass, 'TRUNCATE')",
                    &[(&role, Type::TEXT), (&table, Type::TEXT)],
                )
                .await
                .unwrap()[0]
                .get(0);
            assert!(!deletes, "{role} may delete from gnomon.{table}");
        }
    }
    let functions = admin
        .query_typed(
            "select p.oid::regprocedure::text, p.prosecdef, p.proconfig,
                    array(select coalesce(nullif(a.grantee, 0)::regrole::text, 'public')
                          from aclexplode(p.proacl) a where a.grantee <> p.proowner
                          order by 1)
             from pg_proc p
             where p.oid in ('gnomon.store_sentence(uuid, uuid, text)'::regprocedure,
                             'gnomon.list_gaps(uuid, text, text, uuid, text, integer)'::regprocedure)
             order by 1",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(functions.len(), 2);
    for f in functions {
        let (name, definer, config, grantees): (String, bool, Vec<String>, Vec<String>) =
            (f.get(0), f.get(1), f.get(2), f.get(3));
        // Storing a Sentence is a write, so it runs as `gnomon_writer`; the
        // gap listing is a read, and runs as the caller.
        assert_eq!(
            definer,
            name.starts_with("gnomon.store_sentence("),
            "{name}"
        );
        assert_eq!(config, ["search_path=pg_catalog, pg_temp"], "{name}");
        assert_eq!(grantees, ["authenticated", "gnomon_worker"], "{name}");
    }
}
