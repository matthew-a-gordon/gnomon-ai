//! Seam 2: the SQL boundary, adversarially.
//!
//! These tests bypass the Rust core. They connect as the login role, switch
//! to a caller role with claims set, as any SQL client could, and try to
//! cross a Tenant or Scope. Each attempt must fail: RLS, grants and invoker
//! functions are the barrier, not the Rust core (ADR-0005, ADR-0009).

mod support;

use chrono::{TimeZone, Utc};
use gnomon::{EpisodeId, NewEpisode, Scope, Store, TenantId};
use serde_json::json;
use support::TestDb;
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
                .as_worker(TenantId(t), Scope::new(scope.clone()))
                .record_episode(&NewEpisode {
                    tenant: Some(TenantId(t)),
                    scope: Scope::new(scope.clone()),
                    content: format!("{t} {scope}"),
                    reference_time: Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
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
async fn a_caller_cannot_insert_an_episode_into_another_tenant_or_scope() {
    let w = world().await;
    let attempts = [
        (w.t2, "support", "another Tenant"),
        (w.t1, "billing", "another Scope"),
    ];
    for role in [USER, WORKER] {
        for (tenant, scope, what) in attempts {
            let mut client = w.db.login().await;
            let tx = as_caller(&mut client, role, &claims(w.t1, &["support"])).await;
            let insert = tx
                .query_typed(
                    "insert into gnomon.episode (tenant_id, scope, id, content, reference_time)
                     values ($1, $2, gen_random_uuid(), 'forged', now()) returning id",
                    &[(&tenant, Type::UUID), (&scope, Type::TEXT)],
                )
                .await;
            assert_refused(insert, &format!("{role} inserting into {what}"));
        }
    }
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
async fn a_caller_cannot_update_or_delete_any_episode_even_its_own() {
    let w = world().await;
    let statements = [
        (
            "update gnomon.episode set content = 'rewritten' where id = $1",
            "update",
        ),
        (
            "update gnomon.episode set tenant_id = gen_random_uuid() where id = $1",
            "move Tenant",
        ),
        (
            "update gnomon.episode set scope = 'billing' where id = $1",
            "move Scope",
        ),
        ("delete from gnomon.episode where id = $1", "delete"),
    ];
    for role in [USER, WORKER] {
        for target in [w.t1_support, w.t2_support] {
            for (sql, what) in statements {
                let mut client = w.db.login().await;
                let tx = as_caller(&mut client, role, &claims(w.t1, &["support"])).await;
                let r = tx.execute_typed(sql, &[(&target, Type::UUID)]).await;
                assert_refused(r, &format!("{role} {what} of {target}"));
            }
        }
    }

    // Nothing changed.
    let store = Store::connect(&w.db.login_url(), &support::connect_options())
        .await
        .unwrap();
    let read = store
        .as_worker(TenantId(w.t2), Scope::new("support"))
        .read_episode(TenantId(w.t2), EpisodeId(w.t2_support))
        .await
        .unwrap()
        .expect("Tenant two's Episode survives");
    assert_eq!(read.content, format!("{} support", w.t2));
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

/// The Ontology's tables. They carry a Tenant and no Scope, because every
/// Scope of a Tenant shares one Ontology, and their policies check the Tenant
/// alone.
const ONTOLOGY_TABLES: &[&str] = &["kind", "kind_rule"];

#[tokio::test]
async fn every_gnomon_table_has_rls_and_every_graph_table_has_a_tenant_and_the_tenant_scope_policy()
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
        let ontology = ONTOLOGY_TABLES.contains(&name.as_str());
        assert_eq!(
            scope, !ontology,
            "gnomon.{name}: graph tables have a scope column and Ontology tables none"
        );
        let policies = client
            .query_typed(
                "select qual, with_check from pg_policies where schemaname = 'gnomon' and tablename = $1",
                &[(&name, Type::TEXT)],
            )
            .await
            .unwrap();
        assert!(!policies.is_empty(), "gnomon.{name} has no policy");
        let expected = if ontology {
            "(tenant_id = gnomon.tenant_id())"
        } else {
            "((tenant_id = gnomon.tenant_id()) AND (scope = ANY (gnomon.allowed_scopes())))"
        };
        for p in &policies {
            let (using, check): (Option<String>, Option<String>) = (p.get(0), p.get(1));
            assert!(
                using.is_some() || check.is_some(),
                "gnomon.{name} has a policy that checks nothing"
            );
            if let Some(using) = using {
                assert_eq!(using, expected, "gnomon.{name} USING");
            }
            if let Some(check) = check {
                assert_eq!(check, expected, "gnomon.{name} WITH CHECK");
            }
        }
    }
}

/// The only SECURITY DEFINER functions ADR-0009 allows. Erasure joins Kind
/// registration in a later migration.
const DEFINERS: &[&str] = &["gnomon.register_kind("];

#[tokio::test]
async fn every_gnomon_function_runs_as_the_invoker_with_an_empty_search_path_but_the_definers() {
    let w = world().await;
    let client = w.db.login().await;
    let functions = client
        .query_typed(
            "select p.oid::regprocedure::text, p.prosecdef, coalesce(p.proconfig, '{}'),
                    p.proowner::regrole::text
             from pg_proc p join pg_namespace n on n.oid = p.pronamespace
             where n.nspname = 'gnomon'
               -- The constructors Postgres creates with gnomon.uuid_range
               -- and its multirange are internal functions, which take no
               -- search path and run no SQL.
               and not (
                 p.prolang = (select oid from pg_language where lanname = 'internal')
                 and exists (
                   select from pg_range r
                   where r.rngtypid = p.prorettype or r.rngmultitypid = p.prorettype
                 )
               )",
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
        "gnomon.read_node(",
        "gnomon.list_citations(",
        "gnomon.default_tenant()",
        "gnomon.call_tenant(",
        "gnomon.switch_to_multi_tenant()",
    ] {
        assert!(
            names.iter().any(|n| n.starts_with(expected)),
            "{expected} is missing from {names:?}"
        );
    }
    for f in &functions {
        let (name, definer, config, owner): (String, bool, Vec<String>, String) =
            (f.get(0), f.get(1), f.get(2), f.get(3));
        let allowed = DEFINERS.iter().any(|d| name.starts_with(d));
        assert_eq!(definer, allowed, "{name}: SECURITY DEFINER is {definer}");
        if definer {
            assert_eq!(owner, "gnomon_definer", "{name} is owned by {owner}");
        }
        assert!(
            config
                .iter()
                .any(|c| c == "search_path=\"\"" || c == "search_path="),
            "{name} does not pin search_path to '': {config:?}"
        );
    }
}

#[tokio::test]
async fn the_definer_role_cannot_log_in_or_bypass_rls_and_no_role_belongs_to_it_or_it_to_one() {
    let w = world().await;
    let client = w.db.login().await;
    let row = client
        .query_typed(
            "select rolcanlogin, rolsuper or rolbypassrls,
                    exists (select from pg_auth_members m where m.member = r.oid),
                    array(select m.member::regrole::text from pg_auth_members m
                          where m.roleid = r.oid and (m.set_option or m.inherit_option))
             from pg_roles r where rolname = 'gnomon_definer'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(row.len(), 1, "the definer role exists");
    let (login, bypass, member): (bool, bool, bool) = (row[0].get(0), row[0].get(1), row[0].get(2));
    assert!(!login, "gnomon_definer can log in");
    assert!(!bypass, "gnomon_definer can bypass RLS");
    assert!(!member, "gnomon_definer inherits another role's privileges");
    // The migration role may act as the definer role only while it hands it
    // the function. A CREATEROLE role keeps ADMIN on a role it created, which
    // lets it grant the role but not act as it.
    let members: Vec<String> = row[0].get(3);
    assert!(members.is_empty(), "{members:?} can act as gnomon_definer");
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

const REGISTER_PERSON: &str =
    "select gnomon.register_kind($1, 'person', 'node', null, '{\"properties\": {\"role\": {\"type\": \"string\"}}}', '{}', null, null)";

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
async fn a_caller_cannot_write_the_kind_tables_directly_even_with_the_grant() {
    let w = world().await;
    w.db.grant_kind_registration(USER).await;
    w.db.grant_kind_registration(WORKER).await;
    {
        let mut client = w.db.login().await;
        let tx = as_caller(&mut client, USER, &claims(w.t1, &["support"])).await;
        tx.query_typed(REGISTER_PERSON, &[(&w.t1, Type::UUID)])
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }
    let statements = [
        (
            "insert into gnomon.kind (tenant_id, name, kind_of, parent, ancestors, properties, search, definition)
             values ($1, 'forged', 'node', 'node', '{node}', '{}', '{}', '{}')",
            "insert a Kind",
        ),
        (
            "insert into gnomon.kind_rule (tenant_id, kind, property, rule) values ($1, 'person', 'age', '1 == 1')",
            "insert a rule",
        ),
        (
            "update gnomon.kind_rule set rule = '1 == 1' where tenant_id = $1",
            "weaken a rule",
        ),
        (
            "update gnomon.kind set search = '{}' where tenant_id = $1",
            "rewrite a Kind",
        ),
        ("delete from gnomon.kind_rule where tenant_id = $1", "delete a rule"),
        ("delete from gnomon.kind where tenant_id = $1", "delete a Kind"),
    ];
    for role in [USER, WORKER] {
        for (sql, what) in statements {
            let mut client = w.db.login().await;
            let tx = as_caller(&mut client, role, &claims(w.t1, &["support"])).await;
            let r = tx.execute_typed(sql, &[(&w.t1, Type::UUID)]).await;
            assert_refused(r, &format!("{role}: {what}"));
        }
    }
    let rule: String = w
        .db
        .admin()
        .await
        .query_typed(
            "select rule::text from gnomon.kind_rule where tenant_id = $1 and kind = 'person' and property = 'role'",
            &[(&w.t1, Type::UUID)],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert!(rule.contains("\"string\""), "the rule changed: {rule}");
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
        let caller = store.as_worker(TenantId(t), Scope::new("support"));
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

const WRITE_NODE: &str = "select status from gnomon.write_node($1, $2, $3, null, null, $4, null)";

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

#[tokio::test]
async fn a_caller_cannot_delete_a_committed_version_or_citation() {
    let n = nodes().await;
    let before = version_rows(&n.w.db, n.n1).await;
    let statements = [
        (
            "delete from gnomon.version where id = $1",
            n.open,
            "the open Version",
        ),
        (
            "delete from gnomon.version where id = $1",
            n.closed,
            "a closed Version",
        ),
        (
            "delete from gnomon.citation where version_id = $1",
            n.open,
            "the open Version's Citations",
        ),
        (
            "delete from gnomon.citation where version_id = $1",
            n.closed,
            "a closed Version's Citations",
        ),
        (
            "delete from gnomon.version where node_id = $1",
            n.n1,
            "every Version of the Node",
        ),
    ];
    for role in [USER, WORKER] {
        for (sql, target, what) in statements {
            let mut client = n.w.db.login().await;
            let tx = as_caller(&mut client, role, &claims(n.w.t1, &["support"])).await;
            let r = tx.execute_typed(sql, &[(&target, Type::UUID)]).await;
            assert_refused(r, &format!("{role} deleting {what}"));
        }
    }
    assert_eq!(
        version_rows(&n.w.db, n.n1).await,
        before,
        "nothing was deleted"
    );
}

#[tokio::test]
async fn a_caller_may_delete_only_the_versions_and_citations_its_own_transaction_wrote() {
    let n = nodes().await;
    let before = version_rows(&n.w.db, n.n1).await;
    for role in [USER, WORKER] {
        let mut client = n.w.db.login().await;
        let tx = as_caller(&mut client, role, &claims(n.w.t1, &["support"])).await;
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
        let cites = tx
            .execute_typed(
                "delete from gnomon.citation where version_id = $1",
                &[(&fresh, Type::UUID)],
            )
            .await
            .expect("deleting the transaction's own Citations");
        assert_eq!(cites, 1);
        let versions = tx
            .execute_typed(
                "delete from gnomon.version where id = $1",
                &[(&fresh, Type::UUID)],
            )
            .await
            .expect("deleting the transaction's own Version");
        assert_eq!(versions, 1);
        // The committed Versions stay out of reach in the same transaction.
        let r = tx
            .execute_typed(
                "delete from gnomon.version where id = $1",
                &[(&n.open, Type::UUID)],
            )
            .await;
        assert_refused(
            r,
            &format!("{role} deleting a committed Version after its own"),
        );
    }
    assert_eq!(version_rows(&n.w.db, n.n1).await, before);
}

#[tokio::test]
async fn a_caller_cannot_rewrite_the_history_of_a_version() {
    let n = nodes().await;
    let statements = [
        ("update gnomon.version set recorded_to = now() where id = $1", n.closed, "move a closed end to now"),
        ("update gnomon.version set recorded_to = 'infinity' where id = $1", n.closed, "reopen a closed Version"),
        (
            "update gnomon.version set recorded_to = recorded_to + interval '1 second' where id = $1",
            n.closed,
            "move a closed end later",
        ),
        (
            "update gnomon.version set recorded_to = now() - interval '1 second' where id = $1",
            n.open,
            "close an open Version in the past",
        ),
        (
            "update gnomon.version set recorded_to = now() + interval '1 day' where id = $1",
            n.open,
            "close an open Version in the future",
        ),
        ("update gnomon.version set props = '{\"name\": \"forged\"}' where id = $1", n.open, "rewrite the values"),
        ("update gnomon.version set recorded_from = '2000-01-01' where id = $1", n.open, "move the start of Recorded time"),
        ("update gnomon.version set valid_from = '2000-01-01' where id = $1", n.open, "move Valid time"),
        ("update gnomon.version set closed_in = null where id = $1", n.closed, "clear the closing mark"),
        ("update gnomon.contribution set value = '\"forged\"' where node_id = $1", n.n1, "rewrite a Contribution"),
        ("delete from gnomon.contribution where node_id = $1", n.n1, "delete a Contribution"),
        ("update gnomon.citation set place = 'forged' where version_id = $1", n.open, "rewrite a Citation"),
        ("update gnomon.node set kind = 'other' where id = $1", n.n1, "change a Node's Kind"),
        ("delete from gnomon.node where id = $1", n.n1, "delete a Node"),
    ];
    for role in [USER, WORKER] {
        for (sql, target, what) in statements {
            let mut client = n.w.db.login().await;
            let tx = as_caller(&mut client, role, &claims(n.w.t1, &["support"])).await;
            let r = tx.execute_typed(sql, &[(&target, Type::UUID)]).await;
            assert_refused(r, &format!("{role}: {what}"));
        }
    }

    // Closing an open Version when the transaction started is the one change a
    // caller may make, and it may not then rewrite what it just closed.
    let mut client = n.w.db.login().await;
    let tx = as_caller(&mut client, USER, &claims(n.w.t1, &["support"])).await;
    let closed = tx
        .execute_typed(
            "update gnomon.version set recorded_to = transaction_timestamp() where id = $1",
            &[(&n.open, Type::UUID)],
        )
        .await
        .expect("closing an open Version when its transaction started");
    assert_eq!(closed, 1);
    let r = tx
        .execute_typed(
            "update gnomon.version set recorded_to = now() + interval '1 day' where id = $1",
            &[(&n.open, Type::UUID)],
        )
        .await;
    assert_refused(r, "moving the end it just closed");
}

#[tokio::test]
async fn a_caller_cannot_forge_the_mark_that_makes_a_committed_row_look_new() {
    let n = nodes().await;
    let before = version_rows(&n.w.db, n.n1).await;
    let statements = [
        (
            "update gnomon.version set written_in = pg_current_xact_id() where id = $1",
            "mark a committed Version as written now",
        ),
        (
            "update gnomon.citation set written_in = pg_current_xact_id() where version_id = $1",
            "mark a committed Citation as written now",
        ),
        (
            "update gnomon.version set closed_in = pg_current_xact_id() where id = $1",
            "mark a committed Version as closed now",
        ),
    ];
    for role in [USER, WORKER] {
        for (sql, what) in statements {
            let mut client = n.w.db.login().await;
            let tx = as_caller(&mut client, role, &claims(n.w.t1, &["support"])).await;
            let r = tx.execute_typed(sql, &[(&n.open, Type::UUID)]).await;
            assert_refused(r, &format!("{role}: {what}"));
        }
        // Nor can a row be inserted with a mark of the caller's choosing.
        for (sql, what) in [
            (
                "insert into gnomon.version (tenant_id, scope, node_id, props, valid_from,
                   valid_from_precision, valid_to, valid_to_precision, merge_rule, written_in)
                 values ($1, 'support', $2, '{}', '2030-01-01', 'instant', '2030-01-02', 'instant',
                   'recency', pg_current_xact_id())",
                "insert a Version with a mark",
            ),
            (
                "insert into gnomon.version (tenant_id, scope, node_id, props, valid_from,
                   valid_from_precision, valid_to, valid_to_precision, merge_rule, written_in)
                 values ($1, 'support', $2, '{}', '2030-01-01', 'instant', '2030-01-02', 'instant',
                   'recency', '1')",
                "insert a Version with an old mark",
            ),
        ] {
            let mut client = n.w.db.login().await;
            let tx = as_caller(&mut client, role, &claims(n.w.t1, &["support"])).await;
            let r = tx
                .execute_typed(sql, &[(&n.w.t1, Type::UUID), (&n.n1, Type::UUID)])
                .await;
            assert_refused(r, &format!("{role}: {what}"));
        }
    }
    // And a committed Version is still out of reach of a delete.
    let mut client = n.w.db.login().await;
    let tx = as_caller(&mut client, USER, &claims(n.w.t1, &["support"])).await;
    let r = tx
        .execute_typed(
            "delete from gnomon.version where id = $1",
            &[(&n.open, Type::UUID)],
        )
        .await;
    assert_refused(r, "deleting after the forgeries");
    assert_eq!(version_rows(&n.w.db, n.n1).await, before);
}

#[tokio::test]
async fn recorded_time_is_assigned_only_by_gnomon() {
    let n = nodes().await;
    let statements = [
        (
            "insert into gnomon.contribution (tenant_id, scope, node_id, episode_id, property, value,
               valid_from, valid_from_precision, valid_to, valid_to_precision, recorded_at)
             values ($1, 'support', $2, $3, 'name', '\"x\"', '-infinity', 'unknown', 'infinity',
               'unknown', '2000-01-01')",
            "a Contribution's Recorded time",
        ),
        (
            "insert into gnomon.version (tenant_id, scope, node_id, props, valid_from,
               valid_from_precision, valid_to, valid_to_precision, merge_rule, recorded_from)
             values ($1, 'support', $2, '{}', '2030-01-01', 'instant', '2030-01-02', 'instant',
               'recency', '2000-01-01')",
            "a Version's Recorded time",
        ),
        (
            "insert into gnomon.version (tenant_id, scope, node_id, props, valid_from,
               valid_from_precision, valid_to, valid_to_precision, merge_rule, recorded_to)
             values ($1, 'support', $2, '{}', '2030-01-01', 'instant', '2030-01-02', 'instant',
               'recency', '2100-01-01')",
            "a Version's Recorded-time end",
        ),
        (
            "insert into gnomon.citation (tenant_id, scope, version_id, episode_id, property, recorded_at)
             select $1, 'support', $2, $3, 'name', '2000-01-01'",
            "a Citation's Recorded time",
        ),
    ];
    for role in [USER, WORKER] {
        for (sql, what) in statements {
            let mut client = n.w.db.login().await;
            let tx = as_caller(&mut client, role, &claims(n.w.t1, &["support"])).await;
            let second = if what.starts_with("a Citation") {
                n.open
            } else {
                n.n1
            };
            let params: Vec<(&(dyn tokio_postgres::types::ToSql + Sync), Type)> =
                if sql.contains("$3") {
                    vec![
                        (&n.w.t1, Type::UUID),
                        (&second, Type::UUID),
                        (&n.w.t1_support, Type::UUID),
                    ]
                } else {
                    vec![(&n.w.t1, Type::UUID), (&second, Type::UUID)]
                };
            let r = tx.execute_typed(sql, &params).await;
            assert_refused(r, &format!("{role} choosing {what}"));
        }
    }
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
    drop(tx);

    let mut client = n.w.db.login().await;
    let tx = as_caller(&mut client, USER, &claims(n.w.t1, &["support"])).await;
    let err = tx
        .execute_typed(
            "insert into gnomon.contribution (tenant_id, scope, node_id, episode_id, property, value,
               valid_from, valid_from_precision, valid_to, valid_to_precision)
             values ($1, 'support', $2, null, 'name', '\"x\"', '-infinity', 'unknown', 'infinity', 'unknown')",
            &[(&n.w.t1, Type::UUID), (&n.n1, Type::UUID)],
        )
        .await
        .expect_err("a Contribution with no Episode was inserted");
    assert_eq!(err.code(), Some(&SqlState::NOT_NULL_VIOLATION));
}

#[tokio::test]
async fn empty_intervals_are_refused_by_the_tables_as_well_as_the_write() {
    let n = nodes().await;
    let attempts = [
        (
            "insert into gnomon.contribution (tenant_id, scope, node_id, episode_id, property, value,
               valid_from, valid_from_precision, valid_to, valid_to_precision,
               episode_reference_time, episode_recorded_at)
             select $1, 'support', $2, e.id, 'name', '\"x\"', '2030-01-01', 'instant', '2030-01-01',
               'instant', e.reference_time, e.recorded_at
             from gnomon.episode e where e.id = $3",
            "an empty Contribution",
        ),
        (
            "insert into gnomon.version (tenant_id, scope, node_id, props, valid_from,
               valid_from_precision, valid_to, valid_to_precision, merge_rule)
             select $1, 'support', $2, '{}', '2030-01-01', 'instant', '2030-01-01', 'instant', 'recency'
             where $3::uuid is not null",
            "an empty Version",
        ),
    ];
    for (sql, what) in attempts {
        let mut client = n.w.db.login().await;
        let tx = as_caller(&mut client, USER, &claims(n.w.t1, &["support"])).await;
        let err = tx
            .execute_typed(
                sql,
                &[
                    (&n.w.t1, Type::UUID),
                    (&n.n1, Type::UUID),
                    (&n.w.t1_support, Type::UUID),
                ],
            )
            .await
            .expect_err(what);
        assert_eq!(
            err.code(),
            Some(&SqlState::CHECK_VIOLATION),
            "{what}: {err:?}"
        );
    }
    // And a Version may not be closed at its own start, which would leave
    // its Recorded time empty.
    let mut client = n.w.db.login().await;
    let tx = as_caller(&mut client, USER, &claims(n.w.t1, &["support"])).await;
    write_node(
        &tx,
        n.w.t1,
        n.n1,
        Some(n.w.t1_support),
        json!({ "name": "third" }),
    )
    .await
    .unwrap();
    let r = tx
        .execute_typed(
            "update gnomon.version set recorded_to = transaction_timestamp()
             where node_id = $1 and recorded_to = 'infinity'",
            &[(&n.n1, Type::UUID)],
        )
        .await;
    assert_refused(r, "closing a Version at its own start");
}

#[tokio::test]
async fn a_node_cannot_hold_two_versions_at_once_even_when_inserted_directly() {
    let n = nodes().await;
    let mut client = n.w.db.login().await;
    let tx = as_caller(&mut client, USER, &claims(n.w.t1, &["support"])).await;
    let err = tx
        .execute_typed(
            "insert into gnomon.version (tenant_id, scope, node_id, props, valid_from,
               valid_from_precision, valid_to, valid_to_precision, merge_rule)
             values ($1, 'support', $2, '{\"name\": \"overlap\"}', '2030-01-01', 'instant',
               '2030-02-01', 'instant', 'recency')",
            &[(&n.w.t1, Type::UUID), (&n.n1, Type::UUID)],
        )
        .await
        .expect_err("an overlapping Version was inserted");
    assert_eq!(err.code(), Some(&SqlState::EXCLUSION_VIOLATION), "{err:?}");
    drop(tx);

    // The same times on another Node, of this Tenant or another, are fine.
    let mut client = n.w.db.login().await;
    let tx = as_caller(&mut client, USER, &claims(n.w.t1, &["support"])).await;
    let other: Uuid = tx
        .query_typed(
            "select gnomon.create_node($1, 'support', 'node')",
            &[(&n.w.t1, Type::UUID)],
        )
        .await
        .unwrap()[0]
        .get(0);
    write_node(
        &tx,
        n.w.t1,
        other,
        Some(n.w.t1_support),
        json!({ "name": "other" }),
    )
    .await
    .expect("the same Valid and Recorded time on another Node");
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
        .as_worker(TenantId(n.w.t1), Scope::new("support"))
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
async fn a_caller_inserting_directly_cannot_skip_the_kinds_rules() {
    let n = nodes().await;
    n.w.db.grant_kind_registration(USER).await;
    let person: Uuid = {
        let mut client = n.w.db.login().await;
        let tx = as_caller(&mut client, USER, &claims(n.w.t1, &["support"])).await;
        tx.query_typed(
            "select gnomon.register_kind($1, 'person', 'node', null,
               '{\"properties\": {\"age\": {\"type\": \"integer\"}}, \"required\": [\"age\"]}', '{}', null, null)",
            &[(&n.w.t1, Type::UUID)],
        )
        .await
        .unwrap();
        let id = tx
            .query_typed(
                "select gnomon.create_node($1, 'support', 'person')",
                &[(&n.w.t1, Type::UUID)],
            )
            .await
            .unwrap()[0]
            .get(0);
        tx.commit().await.unwrap();
        id
    };
    let contribution =
        "insert into gnomon.contribution (tenant_id, scope, node_id, episode_id, property,
           value, valid_from, valid_from_precision, valid_to, valid_to_precision)
         values ($1, 'support', $2, $3, $4, $5, '-infinity', 'unknown', 'infinity', 'unknown')";
    let version = "insert into gnomon.version (tenant_id, scope, node_id, props, valid_from,
           valid_from_precision, valid_to, valid_to_precision, merge_rule)
         select $1, 'support', $2, $5, '2030-01-01', 'instant', '2030-01-02', 'instant', 'recency'
         where $3::uuid is not null and $4::text is not null";
    let attempts = [
        (contribution, n.n1, "age", json!(1), "Kind \"node\" has no property \"age\""),
        (contribution, n.n1, "name", json!(7), "the value 7 of property \"name\" does not satisfy Kind \"node\""),
        (contribution, person, "age", json!("forty"), "the value \"forty\" of property \"age\" does not satisfy Kind \"person\""),
        (version, n.n1, "-", json!({ "age": 1 }), "Kind \"node\" has no property \"age\""),
        (version, person, "-", json!({ "age": 1.5 }), "the value 1.5 of property \"age\" does not satisfy Kind \"person\""),
        (version, person, "-", json!({ "name": "Ada" }), "the write leaves required property \"age\" of Kind \"person\" without a value over [2030-01-01 00:00:00+00, 2030-01-02 00:00:00+00)"),
    ];
    for role in [USER, WORKER] {
        for (sql, node, property, value, message) in &attempts {
            let mut client = n.w.db.login().await;
            let tx = as_caller(&mut client, role, &claims(n.w.t1, &["support"])).await;
            tx.batch_execute("set local timezone = 'UTC'")
                .await
                .unwrap();
            let err = tx
                .execute_typed(
                    sql,
                    &[
                        (&n.w.t1, Type::UUID),
                        (node, Type::UUID),
                        (&n.w.t1_support, Type::UUID),
                        (property, Type::TEXT),
                        (value, Type::JSONB),
                    ],
                )
                .await
                .expect_err(message);
            assert_eq!(
                err.code().map(|c| c.code()),
                Some(gnomon::INVALID_WRITE),
                "{role}: {err:?}"
            );
            assert_eq!(err.as_db_error().unwrap().message(), *message, "{role}");
        }
    }
}

#[tokio::test]
async fn a_caller_cannot_add_a_citation_to_closed_history() {
    let n = nodes().await;
    let before = version_rows(&n.w.db, n.n1).await;
    let cite = "insert into gnomon.citation (tenant_id, scope, version_id, episode_id, property)
                values ($1, 'support', $2, $3, 'name')";
    for role in [USER, WORKER] {
        let mut client = n.w.db.login().await;
        let tx = as_caller(&mut client, role, &claims(n.w.t1, &["support"])).await;
        let r = tx
            .execute_typed(
                cite,
                &[
                    (&n.w.t1, Type::UUID),
                    (&n.closed, Type::UUID),
                    (&n.w.t1_support, Type::UUID),
                ],
            )
            .await;
        assert_refused(r, &format!("{role} citing a closed Version"));
    }
    assert_eq!(version_rows(&n.w.db, n.n1).await, before);
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
                "select * from gnomon.recompute_node($1, $2, '-infinity', 'infinity')",
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
    let single = store.as_worker(None, Scope::new("support"));
    let episode = single
        .record_episode(&NewEpisode {
            tenant: None,
            scope: Scope::new("support"),
            content: "the default Tenant's".into(),
            reference_time: Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
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
    let episode = single
        .record_episode(&NewEpisode {
            tenant: None,
            scope: Scope::new("support"),
            content: "Ada, 36.".into(),
            reference_time: Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
            authority: None,
        })
        .await
        .unwrap();
    single
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

    let operator = db.operator().await;
    let tables = tenant_tables(&operator).await;
    for expected in [
        "citation",
        "contribution",
        "episode",
        "kind",
        "kind_rule",
        "node",
        "version",
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
