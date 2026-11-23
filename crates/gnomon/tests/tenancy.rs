//! Seam 1, tenancy modes: single-tenant and multi-tenant deployments through
//! the Rust core's Store API (ADR-0005).
//!
//! The migrations install single-tenant mode with a default Tenant. A
//! deployment switches to multi-tenant mode with
//! `gnomon.switch_to_multi_tenant()`, run by an operator role, and cannot
//! switch back. The adversarial cases live in sql_boundary.rs.

mod support;

use chrono::{TimeZone, Utc};
use gnomon::{
    AsOf, EpisodeId, Error, KindOf, NewEpisode, NewKind, NewNode, NodeId, Patch, Scope, Store,
    TenantId, ValidTime, VersionId,
};
use serde_json::{json, Map, Value};
use support::TestDb;

async fn store(db: &TestDb) -> Store {
    Store::connect(&db.login_url(), &support::connect_options())
        .await
        .unwrap()
}

/// An end user's claims that bind no Tenant, as a single-tenant deployment
/// issues them.
fn user_without_tenant(scopes: &[&str]) -> Map<String, Value> {
    match json!({
        "sub": uuid::Uuid::new_v4().to_string(),
        "role": "authenticated",
        "app_metadata": { "scopes": scopes },
    }) {
        Value::Object(m) => m,
        _ => unreachable!(),
    }
}

fn episode(tenant: Option<TenantId>, scope: &str, content: &str) -> NewEpisode {
    NewEpisode {
        tenant,
        scope: Scope::new(scope),
        content: content.to_owned(),
        reference_time: Utc.with_ymd_and_hms(2024, 3, 14, 9, 30, 0).unwrap(),
        authority: None,
    }
}

#[tokio::test]
async fn in_single_tenant_mode_an_end_user_who_names_no_tenant_uses_the_default_tenant() {
    let db = TestDb::migrated().await;
    let store = store(&db).await;
    let default = db.default_tenant().await;
    let alice = store.as_user(user_without_tenant(&["support"]));

    let id = alice
        .record_episode(&episode(None, "support", "No Tenant named."))
        .await
        .unwrap();
    let read = alice
        .read_episode(None, id)
        .await
        .unwrap()
        .expect("the Episode is visible");
    assert_eq!(read.tenant, default);
    assert_eq!(read.content, "No Tenant named.");

    // A caller need not name the default Tenant, but naming it reaches the
    // same Episode.
    assert_eq!(
        alice.read_episode(default, id).await.unwrap().map(|e| e.id),
        Some(id)
    );
}

#[tokio::test]
async fn in_single_tenant_mode_a_worker_that_names_no_tenant_uses_the_default_tenant() {
    let db = TestDb::migrated().await;
    let store = store(&db).await;
    let default = db.default_tenant().await;
    let worker = store.as_worker(None, Scope::new("ingest"));

    let id = worker
        .record_episode(&episode(None, "ingest", "A nightly import."))
        .await
        .unwrap();
    let read = worker
        .read_episode(None, id)
        .await
        .unwrap()
        .expect("the Episode is visible");
    assert_eq!(read.tenant, default);

    // An end user of the same deployment sees it in the same Tenant.
    let alice = store.as_user(user_without_tenant(&["ingest"]));
    assert!(alice.read_episode(None, id).await.unwrap().is_some());
    // So does a worker that names the default Tenant.
    let named = store.as_worker(default, Scope::new("ingest"));
    assert!(named.read_episode(default, id).await.unwrap().is_some());
}

/// The message of a `TenantRequired` refusal; panics on anything else.
fn tenant_required<T: std::fmt::Debug>(result: Result<T, Error>) -> String {
    match result {
        Err(Error::TenantRequired(m)) => m,
        other => panic!("expected a TenantRequired refusal, got {other:?}"),
    }
}

/// The message of a `TenantMismatch` refusal; panics on anything else.
fn tenant_mismatch<T: std::fmt::Debug>(result: Result<T, Error>) -> String {
    match result {
        Err(Error::TenantMismatch(m)) => m,
        other => panic!("expected a TenantMismatch refusal, got {other:?}"),
    }
}

const TENANT_REQUIRED_MESSAGE: &str =
    "this deployment is multi-tenant, so every call names its Tenant";

#[tokio::test]
async fn in_multi_tenant_mode_every_call_that_names_no_tenant_fails_and_never_uses_the_default() {
    let db = TestDb::migrated().await;
    db.grant_kind_registration("authenticated").await;
    let store = store(&db).await;
    let first = db.default_tenant().await;
    db.switch_to_multi_tenant().await;

    // A caller whose claims bind the former default Tenant, and one whose
    // claims bind none: neither reaches the default by naming no Tenant.
    let bound = store.as_user(support::user(first, &["support"]));
    let unbound = store.as_user(user_without_tenant(&["support"]));
    let worker = store.as_worker(None, Scope::new("support"));
    let (episode_id, node, version) = (
        EpisodeId(uuid::Uuid::new_v4()),
        NodeId(uuid::Uuid::new_v4()),
        VersionId(uuid::Uuid::new_v4()),
    );
    let patch = Patch::new(episode_id, ValidTime::UNKNOWN, Default::default());
    let new_node = NewNode {
        tenant: None,
        scope: Scope::new("support"),
        kind: gnomon::ROOT_NODE_KIND.into(),
    };
    let new_kind = NewKind {
        tenant: None,
        name: "person".into(),
        of: KindOf::Node,
        parent: None,
        properties: json!({}),
        search: vec![],
    };
    let now = AsOf::current(Utc::now());
    for caller in [&bound, &unbound, &worker] {
        let new = episode(None, "support", "No Tenant named.");
        assert_eq!(
            tenant_required(caller.record_episode(&new).await),
            TENANT_REQUIRED_MESSAGE
        );
        assert_eq!(
            tenant_required(caller.read_episode(None, episode_id).await),
            TENANT_REQUIRED_MESSAGE
        );
        assert_eq!(
            tenant_required(caller.register_kind(&new_kind).await),
            TENANT_REQUIRED_MESSAGE
        );
        assert_eq!(
            tenant_required(caller.read_kind(None, "node").await),
            TENANT_REQUIRED_MESSAGE
        );
        assert_eq!(
            tenant_required(caller.create_node(&new_node, &patch).await),
            TENANT_REQUIRED_MESSAGE
        );
        assert_eq!(
            tenant_required(caller.write_node(None, node, &patch).await),
            TENANT_REQUIRED_MESSAGE
        );
        assert_eq!(
            tenant_required(caller.write_nodes(None, &[(node, patch.clone())]).await),
            TENANT_REQUIRED_MESSAGE
        );
        assert_eq!(
            tenant_required(caller.read_node(None, node, now).await),
            TENANT_REQUIRED_MESSAGE
        );
        assert_eq!(
            tenant_required(caller.list_citations(None, version).await),
            TENANT_REQUIRED_MESSAGE
        );
    }

    // Claims that bind no Tenant no longer reach the default Tenant either.
    let m = tenant_mismatch(unbound.read_episode(first, episode_id).await);
    assert_eq!(
        m,
        format!("the call names Tenant {first}, but the caller's claims bind no Tenant")
    );
    // Nothing was written into the former default Tenant.
    let rows = db
        .operator()
        .await
        .query_typed(
            "select (select count(*) from gnomon.episode) + (select count(*) from gnomon.kind)
                    + (select count(*) from gnomon.node)",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows[0].get::<_, i64>(0), 0);
}

#[tokio::test]
async fn a_call_whose_tenant_differs_from_the_tenant_in_the_claims_fails_in_either_mode() {
    let db = TestDb::migrated().await;
    let store = store(&db).await;
    let default = db.default_tenant().await;
    let (a, b) = (support::tenant(), support::tenant());
    let theirs = store
        .as_user(support::user(b, &["support"]))
        .record_episode(&episode(Some(b), "support", "Tenant b's."))
        .await
        .unwrap();

    for multi in [false, true] {
        if multi {
            db.switch_to_multi_tenant().await;
        }
        let alice = store.as_user(support::user(a, &["support"]));
        let worker = store.as_worker(a, Scope::new("support"));
        let expected =
            format!("the call names Tenant {b}, but the caller's claims bind Tenant {a}");
        for caller in [&alice, &worker] {
            assert_eq!(
                tenant_mismatch(caller.read_episode(b, theirs).await),
                expected
            );
            assert_eq!(
                tenant_mismatch(
                    caller
                        .record_episode(&episode(Some(b), "support", "Into b."))
                        .await
                ),
                expected
            );
            assert_eq!(tenant_mismatch(caller.read_kind(b, "node").await), expected);
        }
    }

    // In single-tenant mode, leaving the Tenant out means the default
    // Tenant, so claims that bind another Tenant do not match it.
    let db = TestDb::migrated().await;
    let store = self::store(&db).await;
    let alice = store.as_user(support::user(a, &["support"]));
    assert_eq!(
        tenant_mismatch(alice.read_episode(None, theirs).await),
        format!("the call names Tenant {default}, but the caller's claims bind Tenant {a}")
    );
}

fn values(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        other => panic!("{other} is not an object"),
    }
}

fn day(d: u32) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2024, 1, d, 0, 0, 0).unwrap()
}

#[tokio::test]
async fn switching_to_multi_tenant_mode_keeps_every_row_readable_under_the_former_default_tenant() {
    let db = TestDb::migrated().await;
    db.grant_kind_registration("authenticated").await;
    let store = store(&db).await;
    let first = db.default_tenant().await;

    // A single-tenant deployment writes a row into every table, naming no
    // Tenant: a Kind with a compiled rule, Episodes, a Node, its
    // Contributions, its Version and the Version's Citations.
    let single = store.as_user(user_without_tenant(&["support"]));
    single
        .register_kind(&NewKind {
            tenant: None,
            name: "person".into(),
            of: KindOf::Node,
            parent: None,
            properties: json!({ "properties": { "age": { "type": "integer" } } }),
            search: vec![],
        })
        .await
        .unwrap();
    let record = |at| {
        let single = &single;
        async move {
            let mut new = episode(None, "support", "Ada, 36.");
            new.reference_time = at;
            single.record_episode(&new).await.unwrap()
        }
    };
    let later = record(day(5)).await;
    let (node, written) = single
        .create_node(
            &NewNode {
                tenant: None,
                scope: Scope::new("support"),
                kind: "person".into(),
            },
            &Patch::new(
                later,
                ValidTime::UNKNOWN,
                values(json!({ "name": "Ada", "age": 36 })),
            ),
        )
        .await
        .unwrap();
    let version = written.opened[0];
    let citations = single.list_citations(None, version).await.unwrap();
    assert_eq!(citations.len(), 2);

    db.switch_to_multi_tenant().await;

    // Every call now names the former default Tenant, with claims that
    // bind it.
    let multi = store.as_user(support::user(first, &["support"]));
    let read = multi
        .read_episode(first, later)
        .await
        .unwrap()
        .expect("the Episode is readable");
    assert_eq!(read.tenant, first);
    let kind = multi
        .read_kind(first, "person")
        .await
        .unwrap()
        .expect("the Kind is readable");
    assert!(kind.rules.contains_key("age"), "{kind:?}");
    let now = multi
        .read_node(first, node, AsOf::current(day(10)))
        .await
        .unwrap()
        .expect("the Version is readable");
    assert_eq!(now.id, version);
    assert_eq!(now.values, values(json!({ "name": "Ada", "age": 36 })));
    assert_eq!(
        multi.list_citations(first, version).await.unwrap(),
        citations
    );

    // The Kind's rule still applies to the Tenant's Node.
    let earlier = {
        let mut new = episode(Some(first), "support", "Ada, 35.");
        new.reference_time = day(1);
        multi.record_episode(&new).await.unwrap()
    };
    match multi
        .write_node(
            first,
            node,
            &Patch::new(earlier, ValidTime::UNKNOWN, values(json!({ "age": "old" }))),
        )
        .await
    {
        Err(Error::InvalidWrite(_)) => {}
        other => panic!("the Kind's rule did not apply: {other:?}"),
    }
    // The single-tenant Contributions still take part in the merge: an
    // Episode with an earlier Reference time loses to them.
    let out = multi
        .write_node(
            first,
            node,
            &Patch::new(earlier, ValidTime::UNKNOWN, values(json!({ "age": 35 }))),
        )
        .await
        .unwrap();
    assert_eq!(out.status, gnomon::WriteStatus::Unchanged);
    assert_eq!(
        multi
            .read_node(first, node, AsOf::current(day(10)))
            .await
            .unwrap()
            .unwrap()
            .values,
        values(json!({ "name": "Ada", "age": 36 }))
    );
}

#[tokio::test]
async fn switching_back_to_single_tenant_mode_is_refused_even_to_the_operator() {
    let db = TestDb::migrated().await;
    let first = db.default_tenant().await;
    db.switch_to_multi_tenant().await;
    let operator = db.operator().await;

    for sql in [
        "update gnomon.tenancy set mode = 'single', switched_at = null",
        "update gnomon.tenancy set default_tenant_id = gen_random_uuid()",
        "update gnomon.tenancy set switched_at = switched_at - interval '1 day'",
        "delete from gnomon.tenancy",
        "truncate gnomon.tenancy",
    ] {
        match operator.batch_execute(sql).await {
            Err(e) => assert_eq!(
                e.code().map(|c| c.code()),
                Some(gnomon::TENANCY_ONE_WAY),
                "{sql}: {e:?}"
            ),
            Ok(()) => panic!("{sql} was allowed"),
        }
    }
    match operator
        .batch_execute("insert into gnomon.tenancy (mode, default_tenant_id) values ('single', gen_random_uuid())")
        .await
    {
        Err(e) => assert_eq!(e.code(), Some(&tokio_postgres::error::SqlState::UNIQUE_VIOLATION), "{e:?}"),
        Ok(()) => panic!("a second tenancy row was inserted"),
    }

    // Switching again changes nothing, and the deployment is still
    // multi-tenant with the same first Tenant.
    let again: uuid::Uuid = operator
        .query_typed("select gnomon.switch_to_multi_tenant()", &[])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(TenantId(again), first);
    let store = store(&db).await;
    assert_eq!(
        tenant_required(
            store
                .as_user(user_without_tenant(&["support"]))
                .read_episode(None, EpisodeId(uuid::Uuid::new_v4()))
                .await
        ),
        TENANT_REQUIRED_MESSAGE
    );
}

#[tokio::test]
async fn in_single_tenant_mode_the_operator_cannot_change_the_default_tenant_or_remove_the_row() {
    let db = TestDb::migrated().await;
    let operator = db.operator().await;
    for sql in [
        "update gnomon.tenancy set default_tenant_id = gen_random_uuid()",
        "delete from gnomon.tenancy",
        "truncate gnomon.tenancy",
    ] {
        match operator.batch_execute(sql).await {
            Err(e) => assert_eq!(
                e.code().map(|c| c.code()),
                Some(gnomon::TENANCY_ONE_WAY),
                "{sql}: {e:?}"
            ),
            Ok(()) => panic!("{sql} was allowed"),
        }
    }
}

#[tokio::test]
async fn a_deployment_that_replaced_tenant_id_before_this_migration_keeps_its_own() {
    // A chain at migration 0004, whose deployment reads the Tenant from
    // its own claim.
    let db = TestDb::empty_with_login().await;
    let operator = db.operator().await;
    let files = support::exported(support::DEFAULT_DIMENSION);
    let (before, after): (Vec<_>, Vec<_>) =
        files.iter().partition(|(name, _)| name.as_str() < "0005");
    assert!(!after.is_empty(), "the export has a migration 0005");
    for (_, sql) in &before {
        operator.batch_execute(sql).await.unwrap();
    }
    operator
        .batch_execute(
            "create or replace function gnomon.tenant_id() returns uuid
               language sql stable parallel safe security invoker set search_path = ''
               as $$ select (nullif(pg_catalog.current_setting('request.jwt.claims', true), '')::jsonb
                             ->> 'org')::uuid $$",
        )
        .await
        .unwrap();
    for (_, sql) in &after {
        operator.batch_execute(sql).await.unwrap();
    }

    let org = support::tenant();
    let mut client = db.login().await;
    let tx = client.transaction().await.unwrap();
    tx.batch_execute("set local role authenticated")
        .await
        .unwrap();
    let tenant_of = |claims: Value| {
        let tx = &tx;
        async move {
            tx.query_typed(
                "select set_config('request.jwt.claims', $1, true)",
                &[(&claims.to_string(), tokio_postgres::types::Type::TEXT)],
            )
            .await
            .unwrap();
            tx.query_typed("select gnomon.tenant_id()", &[])
                .await
                .unwrap()[0]
                .get::<_, Option<uuid::Uuid>>(0)
        }
    };
    assert_eq!(
        tenant_of(json!({ "org": org.to_string() })).await,
        Some(org.0)
    );
    // The deployment's own function has no fallback, so claims without its
    // claim reach no Tenant, not even the default one.
    assert_eq!(tenant_of(json!({ "app_metadata": {} })).await, None);
}

async fn tenancy_mode(db: &TestDb) -> String {
    db.operator()
        .await
        .query_typed("select mode from gnomon.tenancy", &[])
        .await
        .unwrap()[0]
        .get(0)
}

#[tokio::test]
async fn a_role_the_deployment_grants_the_switch_can_switch_and_one_without_update_cannot() {
    let db = TestDb::migrated().await;
    let first = db.default_tenant().await;
    // Granted in this test's database only, the way a deployment would
    // grant its own operations role.
    db.admin()
        .await
        .batch_execute("grant execute on function gnomon.switch_to_multi_tenant() to gnomon_worker")
        .await
        .unwrap();

    // EXECUTE without UPDATE: a clear refusal, and the mode stays single.
    let mut client = db.login().await;
    let tx = client.transaction().await.unwrap();
    tx.batch_execute("set local role gnomon_worker")
        .await
        .unwrap();
    match tx
        .batch_execute("select gnomon.switch_to_multi_tenant()")
        .await
    {
        Err(e) => assert_eq!(
            e.code(),
            Some(&tokio_postgres::error::SqlState::INSUFFICIENT_PRIVILEGE),
            "{e:?}"
        ),
        Ok(()) => panic!("switched without UPDATE on gnomon.tenancy"),
    }
    drop(tx);
    assert_eq!(tenancy_mode(&db).await, "single");

    db.admin()
        .await
        .batch_execute("grant update (mode) on gnomon.tenancy to gnomon_worker")
        .await
        .unwrap();
    let tx = client.transaction().await.unwrap();
    tx.batch_execute("set local role gnomon_worker")
        .await
        .unwrap();
    let switched: uuid::Uuid = tx
        .query_typed("select gnomon.switch_to_multi_tenant()", &[])
        .await
        .unwrap()[0]
        .get(0);
    // The grant reaches the switch and nothing else: the update policy
    // admits no multi-tenant row, so switching back updates nothing.
    tx.batch_execute("update gnomon.tenancy set mode = 'single'")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(TenantId(switched), first);
    assert_eq!(tenancy_mode(&db).await, "multi");
}
