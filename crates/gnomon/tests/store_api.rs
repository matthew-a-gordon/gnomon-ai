//! Seam 1: the Rust core's Store API against real Postgres.
//!
//! Every test applies the migration files to a fresh database and drives
//! gnomon only through the crate's public methods. The suite runs on
//! `pgvector/pgvector:pg17` and `supabase/postgres` 17.x; see docs/testing.md.

mod support;

use gnomon::{Error, Store};
use support::TestDb;

#[tokio::test]
async fn the_store_accepts_a_freshly_migrated_database() {
    let db = TestDb::migrated().await;
    let store = Store::connect(&db.login_url(), &support::connect_options())
        .await
        .expect("the Store connects");
    store.check().await.expect("the Store accepts the schema");
}

#[tokio::test]
async fn the_store_reports_the_embedding_dimension_the_files_state() {
    let db = TestDb::migrated_with_dimension(768).await;
    let store = Store::connect(&db.login_url(), &support::connect_options())
        .await
        .unwrap();
    assert_eq!(store.embedding_dimension().await.unwrap().get(), 768);
}

#[tokio::test]
async fn connecting_opens_no_connection_until_a_call_needs_one() {
    let db = TestDb::migrated().await;
    let (proxy, log) = wire::record(db.server_addr().await).await;
    let store = Store::connect(&db.login_url_through(proxy), &support::connect_options())
        .await
        .expect("connect opens no connection, so it cannot fail on one");
    // Nothing else holds the proxy's address, so no connection to it means
    // the Store opened none.
    tokio::task::yield_now().await;
    assert_eq!(log.connections(), 0, "Store::connect opened a connection");

    let t = tenant();
    store
        .as_user(user(t, &["support"]))
        .read_episode(t, EpisodeId(uuid::Uuid::new_v4()))
        .await
        .unwrap();
    assert_eq!(log.connections(), 1, "the first call opens one connection");
}

#[tokio::test]
async fn a_store_whose_server_cannot_be_reached_connects_and_its_first_call_is_unavailable() {
    // A port no one listens on: the listener is dropped before the Store
    // connects.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let url = format!("host=127.0.0.1 port={port} user=nobody dbname=nothing");
    let store = Store::connect(&url, &support::connect_options())
        .await
        .expect("connect opens no connection");
    let t = tenant();
    match store
        .as_user(user(t, &["support"]))
        .read_episode(t, EpisodeId(uuid::Uuid::new_v4()))
        .await
    {
        Err(Error::Unavailable {
            reason: gnomon::UnavailableReason::Connect,
            ..
        }) => {}
        other => panic!("expected the call to be unavailable, got {other:?}"),
    }
}

#[tokio::test]
async fn a_failed_schema_check_is_not_remembered_and_the_next_call_checks_again() {
    let db = TestDb::migrated().await;
    let next = gnomon::migrations::SCHEMA_VERSION + 1;
    db.admin()
        .await
        .batch_execute(&format!(
            "insert into gnomon.schema_version (version, name) values ({next}, 'from_a_later_release')"
        ))
        .await
        .unwrap();
    let store = store(&db).await;
    let t = tenant();
    let alice = store.as_user(user(t, &["support"]));
    let id = EpisodeId(uuid::Uuid::new_v4());

    match alice.read_episode(t, id).await {
        Err(Error::SchemaVersion { found, .. }) => assert_eq!(found, Some(next)),
        other => panic!("expected the first call to fail the schema check, got {other:?}"),
    }
    // The database is put back to the version this build expects.
    db.admin()
        .await
        .batch_execute(&format!(
            "delete from gnomon.schema_version where version = {next}"
        ))
        .await
        .unwrap();
    assert_eq!(alice.read_episode(t, id).await.unwrap(), None);
}

#[tokio::test]
async fn the_store_refuses_a_database_with_no_gnomon_schema() {
    let db = TestDb::empty_with_login().await;

    match support::connect_and_check(&db.login_url(), &support::connect_options()).await {
        Err(Error::SchemaVersion { expected, found }) => {
            assert_eq!(expected, gnomon::migrations::SCHEMA_VERSION);
            assert_eq!(found, None);
        }
        Err(other) => panic!("expected a schema version error, got {other:?}"),
        Ok(_) => panic!("the Store ran against a database with no gnomon schema"),
    }
}

#[tokio::test]
async fn the_store_refuses_a_schema_newer_than_it_expects() {
    let db = TestDb::migrated().await;
    let next = gnomon::migrations::SCHEMA_VERSION + 1;
    db.admin()
        .await
        .batch_execute(&format!(
            "insert into gnomon.schema_version (version, name) values ({next}, 'from_a_later_release')"
        ))
        .await
        .unwrap();

    match support::connect_and_check(&db.login_url(), &support::connect_options()).await {
        Err(Error::SchemaVersion { expected, found }) => {
            assert_eq!(expected, gnomon::migrations::SCHEMA_VERSION);
            assert_eq!(found, Some(next));
        }
        Err(other) => panic!("expected a schema version error, got {other:?}"),
        Ok(_) => panic!("the Store ran against a schema it does not expect"),
    }
}

#[tokio::test]
async fn the_store_refuses_a_half_applied_chain() {
    let db = TestDb::migrated().await;
    let last = gnomon::migrations::SCHEMA_VERSION;
    db.admin()
        .await
        .batch_execute(&format!(
            "delete from gnomon.schema_version where version = {last}"
        ))
        .await
        .unwrap();

    match support::connect_and_check(&db.login_url(), &support::connect_options()).await {
        Err(Error::SchemaVersion { found, .. }) => assert_eq!(found, Some(last - 1)),
        Err(other) => panic!("expected a schema version error, got {other:?}"),
        Ok(_) => panic!("the Store ran against a half-applied chain"),
    }
}

#[tokio::test]
async fn the_store_refuses_a_schema_whose_embedding_dimension_pgvector_cannot_store() {
    let db = TestDb::migrated().await;
    db.admin()
        .await
        .batch_execute(
            "create or replace function gnomon.embedding_dimension() returns integer
               language sql immutable set search_path = '' as $$ select 0 $$",
        )
        .await
        .unwrap();

    match support::connect_and_check(&db.login_url(), &support::connect_options()).await {
        Err(Error::InvalidEmbeddingDimension(0)) => {}
        Err(other) => panic!("expected an invalid embedding dimension, got {other:?}"),
        Ok(_) => panic!("the Store accepted embedding dimension 0"),
    }
}

// Recording and reading an Episode, and isolation through the Store API.

use chrono::{TimeZone, Utc};
use gnomon::{EpisodeId, NewEpisode, Scope, TenantId};
use serde_json::Map;
use tokio_postgres::error::SqlState;

use support::{tenant, user};

fn episode(tenant: TenantId, scope: &str, content: &str) -> NewEpisode {
    NewEpisode {
        tenant: Some(tenant),
        scope: Scope::new(scope),
        content: content.to_owned(),
        reference_time: Utc.with_ymd_and_hms(2024, 3, 14, 9, 30, 0).unwrap(),
        authority: Some(3),
    }
}

async fn store(db: &TestDb) -> Store {
    Store::connect(&db.login_url(), &support::connect_options())
        .await
        .unwrap()
}

/// A call that names a Tenant its claims do not bind fails before it
/// reaches the Store function (ADR-0005).
fn assert_mismatch<T: std::fmt::Debug>(result: Result<T, Error>) {
    match result {
        Err(Error::TenantMismatch(_)) => {}
        other => panic!("expected a Tenant mismatch, got {other:?}"),
    }
}

fn assert_denied(result: Result<EpisodeId, Error>) {
    match result {
        Err(e) => {
            assert!(matches!(e, Error::Forbidden(_)), "{e:?}");
            assert_eq!(
                e.sqlstate(),
                Some(&SqlState::INSUFFICIENT_PRIVILEGE),
                "expected an RLS refusal, got {e:?}"
            )
        }
        Ok(id) => panic!("the write was allowed and recorded Episode {id}"),
    }
}

#[tokio::test]
async fn an_end_user_records_an_episode_and_reads_it_back() {
    let db = TestDb::migrated().await;
    let store = store(&db).await;
    let t = tenant();
    let alice = store.as_user(user(t, &["support"]));

    let new = episode(
        t,
        "support",
        "The customer asked to move the renewal to April.",
    );
    let before = Utc::now();
    let id = alice.record_episode(&new).await.unwrap();
    let read = alice
        .read_episode(t, id)
        .await
        .unwrap()
        .expect("the Episode is visible");

    assert_eq!(read.id, id);
    assert_eq!(read.tenant, t);
    assert_eq!(read.scope, Scope::new("support"));
    assert_eq!(
        read.content,
        "The customer asked to move the renewal to April."
    );
    assert_eq!(
        read.reference_time,
        Utc.with_ymd_and_hms(2024, 3, 14, 9, 30, 0).unwrap()
    );
    assert_eq!(read.authority, Some(3));
    let skew = chrono::Duration::seconds(5);
    assert!(
        read.recorded_at > before - skew && read.recorded_at < Utc::now() + skew,
        "recorded_at {} is not the time of the write",
        read.recorded_at
    );
}

#[tokio::test]
async fn an_episode_without_an_authority_reads_back_without_one() {
    let db = TestDb::migrated().await;
    let store = store(&db).await;
    let t = tenant();
    let alice = store.as_user(user(t, &["support"]));

    let new = NewEpisode {
        authority: None,
        ..episode(t, "support", "No rank given.")
    };
    let id = alice.record_episode(&new).await.unwrap();
    assert_eq!(
        alice.read_episode(t, id).await.unwrap().unwrap().authority,
        None
    );
}

#[tokio::test]
async fn a_worker_records_an_episode_and_reads_it_back() {
    let db = TestDb::migrated().await;
    let store = store(&db).await;
    let t = tenant();
    let worker = store.as_worker(t, Scope::new("ingest"), None);

    let id = worker
        .record_episode(&episode(t, "ingest", "A nightly import."))
        .await
        .unwrap();
    let read = worker
        .read_episode(t, id)
        .await
        .unwrap()
        .expect("the Episode is visible");
    assert_eq!(read.content, "A nightly import.");
    assert_eq!(read.scope, Scope::new("ingest"));

    // An end user with that Scope sees the worker's Episode too.
    let alice = store.as_user(user(t, &["ingest"]));
    assert!(alice.read_episode(t, id).await.unwrap().is_some());
}

#[tokio::test]
async fn episode_ids_are_uuid_v7_in_recording_order() {
    let db = TestDb::migrated().await;
    let store = store(&db).await;
    let t = tenant();
    let alice = store.as_user(user(t, &["support"]));

    let mut ids = Vec::new();
    for n in 0..5 {
        ids.push(
            alice
                .record_episode(&episode(t, "support", &format!("message {n}")))
                .await
                .unwrap()
                .0,
        );
        // UUID v7 orders by millisecond; keep the writes in distinct ones.
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    for id in &ids {
        assert_eq!(id.get_version_num(), 7, "{id} is not a UUID v7");
        assert_eq!(
            id.get_variant(),
            uuid::Variant::RFC4122,
            "{id} has the wrong variant"
        );
        let (secs, _) = id
            .get_timestamp()
            .expect("a v7 UUID carries a timestamp")
            .to_unix();
        let age = Utc::now().timestamp() - secs as i64;
        assert!(
            (0..=60).contains(&age),
            "{id} carries a timestamp {age} s from now"
        );
    }
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(
        ids, sorted,
        "UUID v7 IDs sort in the order they were recorded"
    );
}

#[tokio::test]
async fn an_end_user_cannot_read_an_episode_in_another_tenant() {
    let db = TestDb::migrated().await;
    let store = store(&db).await;
    let (t1, t2) = (tenant(), tenant());
    let id = store
        .as_user(user(t1, &["support"]))
        .record_episode(&episode(t1, "support", "Tenant one's secret."))
        .await
        .unwrap();

    let mallory = store.as_user(user(t2, &["support"]));
    assert_mismatch(mallory.read_episode(t1, id).await);
    assert_eq!(mallory.read_episode(t2, id).await.unwrap(), None);
}

#[tokio::test]
async fn an_end_user_cannot_read_an_episode_in_a_scope_they_are_not_allowed() {
    let db = TestDb::migrated().await;
    let store = store(&db).await;
    let t = tenant();
    let id = store
        .as_user(user(t, &["billing"]))
        .record_episode(&episode(t, "billing", "An invoice dispute."))
        .await
        .unwrap();

    let support = store.as_user(user(t, &["support"]));
    assert_eq!(support.read_episode(t, id).await.unwrap(), None);
}

#[tokio::test]
async fn an_end_user_cannot_record_an_episode_into_another_tenant_or_scope() {
    let db = TestDb::migrated().await;
    let store = store(&db).await;
    let (t1, t2) = (tenant(), tenant());
    let alice = store.as_user(user(t1, &["support"]));

    assert_mismatch(
        alice
            .record_episode(&episode(t2, "support", "Into tenant two."))
            .await,
    );
    assert_denied(
        alice
            .record_episode(&episode(t1, "billing", "Into billing."))
            .await,
    );
}

#[tokio::test]
async fn a_worker_is_confined_to_its_tenant_and_scope() {
    let db = TestDb::migrated().await;
    let store = store(&db).await;
    let (t1, t2) = (tenant(), tenant());
    let other_scope = store
        .as_user(user(t1, &["billing"]))
        .record_episode(&episode(t1, "billing", "Billing only."))
        .await
        .unwrap();
    let other_tenant = store
        .as_user(user(t2, &["ingest"]))
        .record_episode(&episode(t2, "ingest", "Tenant two only."))
        .await
        .unwrap();

    let worker = store.as_worker(t1, Scope::new("ingest"), None);
    assert_eq!(worker.read_episode(t1, other_scope).await.unwrap(), None);
    assert_mismatch(worker.read_episode(t2, other_tenant).await);
    assert_denied(
        worker
            .record_episode(&episode(t1, "billing", "Wrong Scope."))
            .await,
    );
    assert_mismatch(
        worker
            .record_episode(&episode(t2, "ingest", "Wrong Tenant."))
            .await,
    );
}

#[tokio::test]
async fn a_caller_without_tenant_claims_sees_nothing_and_writes_nothing() {
    let db = TestDb::migrated().await;
    let store = store(&db).await;
    let t = tenant();
    let id = store
        .as_user(user(t, &["support"]))
        .record_episode(&episode(t, "support", "Visible to support."))
        .await
        .unwrap();

    // With no claims the caller acts in the default Tenant, so naming
    // another Tenant fails, and the default Tenant holds nothing of t's.
    let anonymous = store.as_user(Map::new());
    assert_mismatch(anonymous.read_episode(t, id).await);
    assert_eq!(anonymous.read_episode(None, id).await.unwrap(), None);
    assert_mismatch(
        anonymous
            .record_episode(&episode(t, "support", "No claims."))
            .await,
    );
    // Its Scope set is empty, so it writes nothing in the default Tenant
    // either.
    assert_denied(
        anonymous
            .record_episode(&NewEpisode {
                tenant: None,
                ..episode(t, "support", "No Scope.")
            })
            .await,
    );
}

#[tokio::test]
async fn no_call_carries_its_claims_into_the_next_call_on_the_same_connection() {
    let db = TestDb::migrated().await;
    // A pool of one connection, so every call below shares it.
    let store = Store::connect(
        &db.login_url(),
        &support::connect_options().max_connections(1),
    )
    .await
    .unwrap();
    let (t1, t2) = (tenant(), tenant());

    for round in 0..3 {
        let id = store
            .as_user(user(t1, &["support"]))
            .record_episode(&episode(t1, "support", &format!("round {round}")))
            .await
            .unwrap();
        // Had t1's claims carried over, each of these would succeed.
        assert_mismatch(
            store
                .as_user(user(t2, &["support"]))
                .read_episode(t1, id)
                .await,
        );
        assert_mismatch(store.as_user(Map::new()).read_episode(t1, id).await);
        assert_mismatch(
            store
                .as_worker(t2, Scope::new("support"), None)
                .read_episode(t1, id)
                .await,
        );
        assert_eq!(
            store
                .as_user(user(t2, &["support"]))
                .read_episode(t2, id)
                .await
                .unwrap(),
            None
        );
        assert!(
            store
                .as_worker(t1, Scope::new("support"), None)
                .read_episode(t1, id)
                .await
                .unwrap()
                .is_some()
        );
    }
}

#[tokio::test]
async fn a_refused_call_leaves_the_store_usable() {
    let db = TestDb::migrated().await;
    let store = store(&db).await;
    let t = tenant();
    let alice = store.as_user(user(t, &["support"]));

    assert_denied(
        alice
            .record_episode(&episode(t, "billing", "Refused."))
            .await,
    );
    let id = alice
        .record_episode(&episode(t, "support", "Allowed."))
        .await
        .unwrap();
    assert!(alice.read_episode(t, id).await.unwrap().is_some());
}

// Connection discipline, from the prototype/pooler-prepared-statements
// branch: what reaches the server.

use support::wire::{self, Sent};

/// The statements of one transaction, from the one that opens it, which
/// starts with BEGIN, to COMMIT or ROLLBACK, the opening one included.
fn transactions(sent: &[Sent]) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut current: Option<Vec<String>> = None;
    for m in sent {
        let sql = match m {
            Sent::Query(q) => q.trim().to_owned(),
            Sent::Parse { sql, .. } => sql.trim().to_owned(),
            Sent::Other(_) => continue,
        };
        let upper = sql.to_ascii_uppercase();
        if upper == "BEGIN" || upper.starts_with("BEGIN;") || upper == "START TRANSACTION" {
            assert!(current.is_none(), "BEGIN inside an open transaction");
            current = Some(vec![sql]);
        } else if upper == "COMMIT" || upper == "ROLLBACK" {
            out.push(
                current
                    .take()
                    .expect("COMMIT or ROLLBACK outside a transaction"),
            );
        } else {
            current
                .as_mut()
                .unwrap_or_else(|| panic!("{sql:?} was sent outside an explicit transaction"))
                .push(sql);
        }
    }
    assert!(current.is_none(), "a transaction was left open");
    out
}

#[tokio::test]
async fn every_call_sends_only_unnamed_statements_inside_one_transaction_that_sets_role_and_claims_locally()
 {
    let db = TestDb::migrated().await;
    let (proxy, log) = wire::record(db.server_addr().await).await;
    let options = gnomon::ConnectOptions::default().tls(gnomon::Tls::plaintext());
    let store = Store::connect(&db.login_url_through(proxy), &options)
        .await
        .unwrap();
    let t = tenant();

    let alice = store.as_user(user(t, &["support"]));
    let id = alice
        .record_episode(&episode(t, "support", "Over the wire."))
        .await
        .unwrap();
    alice.read_episode(t, id).await.unwrap();
    assert_denied(
        alice
            .record_episode(&episode(t, "billing", "Refused."))
            .await,
    );
    let worker = store.as_worker(t, Scope::new("support"), None);
    worker.read_episode(t, id).await.unwrap();
    worker
        .record_episode(&episode(t, "support", "From the worker."))
        .await
        .unwrap();

    let sent = log.sent();
    for m in &sent {
        if let Sent::Parse { name, sql } = m {
            assert!(
                name.is_empty(),
                "named statement {name:?} was sent for {sql:?}"
            );
        }
    }

    let txs = transactions(&sent);
    // The schema check of the first connection, then the five calls, all
    // on the one connection the calls took in turn.
    assert_eq!(log.connections(), 1);
    assert_eq!(txs.len(), 6, "{txs:#?}");
    assert_eq!(txs[0][0], "begin; set local role authenticated");
    let expected_roles = [
        "authenticated",
        "authenticated",
        "authenticated",
        "gnomon_worker",
        "gnomon_worker",
    ];
    for (tx, role) in txs[1..].iter().zip(expected_roles) {
        // BEGIN and the role go in one simple query, one round trip.
        assert_eq!(tx[0], format!("begin; set local role {role}"), "{tx:#?}");
        assert_eq!(
            tx[1],
            "with claims as materialized (select set_config('request.jwt.claims', $1, true)) \
             select gnomon.call_tenant($2) from claims",
            "{tx:#?}"
        );
        assert_eq!(tx.len(), 3, "each call is one Store function call: {tx:#?}");
        assert!(tx[2].contains("gnomon."), "{tx:#?}");
    }
}

/// The statements of each Store call of a recorded run, after the schema
/// check of the first connection.
fn calls_after_connect(sent: &[Sent]) -> Vec<Vec<String>> {
    let txs = transactions(sent);
    txs[1..].to_vec()
}

#[tokio::test]
async fn a_batch_as_of_read_of_many_nodes_and_a_traversal_are_each_one_call_whatever_the_number_of_nodes()
 {
    let db = TestDb::migrated().await;
    let t = tenant();
    let scope = "support";
    // The graph is written without the proxy: fifty Nodes, each joined to the
    // first by an Edge.
    let writer = store(&db).await;
    let w = writer.as_worker(t, Scope::new(scope), None);
    let ep = w
        .record_episode(&episode(t, scope, "Fifty Nodes."))
        .await
        .unwrap();
    let mut nodes = Vec::new();
    for i in 0..50 {
        let (node, _) = w
            .create_node(
                &gnomon::NewNode {
                    tenant: Some(t),
                    scope: Scope::new(scope),
                    kind: gnomon::ROOT_NODE_KIND.into(),
                },
                &gnomon::Patch::new(
                    ep,
                    gnomon::ValidTime::UNKNOWN,
                    serde_json::from_value(serde_json::json!({ "name": format!("node {i}") }))
                        .unwrap(),
                ),
            )
            .await
            .unwrap();
        nodes.push(node);
    }
    for node in &nodes[1..] {
        w.create_edge(
            &gnomon::NewEdge {
                tenant: Some(t),
                scope: Scope::new(scope),
                kind: gnomon::ROOT_EDGE_KIND.into(),
                source: nodes[0],
                target: *node,
            },
            &gnomon::Patch::new(ep, gnomon::ValidTime::UNKNOWN, Map::new()),
        )
        .await
        .unwrap();
    }

    let (proxy, log) = wire::record(db.server_addr().await).await;
    let options = gnomon::ConnectOptions::default().tls(gnomon::Tls::plaintext());
    let reader = Store::connect(&db.login_url_through(proxy), &options)
        .await
        .unwrap();
    let r = reader.as_worker(t, Scope::new(scope), None);
    let now = gnomon::AsOf::current(Utc::now());
    let one = r.read_nodes(t, &nodes[..1], now).await.unwrap();
    let all = r.read_nodes(t, &nodes, now).await.unwrap();
    let steps = r
        .traverse(t, nodes[0], now, &gnomon::Traversal::all())
        .await
        .unwrap();
    assert_eq!((one.len(), all.len(), steps.len()), (1, 50, 49));

    let sent = log.sent();
    let calls = calls_after_connect(&sent);
    assert_eq!(calls.len(), 3, "{calls:#?}");
    for (call, function) in calls.iter().zip([
        "gnomon.read_nodes(",
        "gnomon.read_nodes(",
        "gnomon.traverse(",
    ]) {
        assert_eq!(call[0], "begin; set local role gnomon_worker", "{call:#?}");
        assert_eq!(
            call.len(),
            3,
            "the call is one Store function call: {call:#?}"
        );
        assert!(call[2].contains(function), "{call:#?}");
    }
    // Reading fifty Nodes sends exactly what reading one does: the same
    // statements and the same number of messages, so the same round trips.
    assert_eq!(calls[0], calls[1]);
    let per_call: Vec<usize> = {
        let mut counts = Vec::new();
        let mut n = 0;
        let mut open = false;
        for m in &sent {
            match m {
                Sent::Query(q) if q.trim().to_ascii_lowercase().starts_with("begin") => {
                    open = true;
                    n = 0;
                }
                Sent::Query(q) if q.trim().eq_ignore_ascii_case("commit") => {
                    open = false;
                    counts.push(n);
                }
                _ if open => n += 1,
                _ => {}
            }
        }
        counts
    };
    assert_eq!(
        per_call[1], per_call[2],
        "messages sent per call: {per_call:?}"
    );
}
