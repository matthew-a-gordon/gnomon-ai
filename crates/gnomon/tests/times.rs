//! Seam 1, the instants of the public API: how a `jiff::Timestamp` crosses to
//! `timestamptz` and back, through the Rust core's Store API against real
//! Postgres. SQL appears only to write a time the Rust API cannot express.

mod support;

use gnomon::{
    AsOf, EpisodeId, Error, NewEpisode, NewNode, Patch, ROOT_NODE_KIND, Scope, Store, TenantId,
    ValidTime,
};
use jiff::Timestamp;
use serde_json::{Map, Value, json};
use support::TestDb;
use tokio_postgres::Row;
use tokio_postgres::types::Type;

const SCOPE: &str = "support";

fn name(name: &str) -> Map<String, Value> {
    Map::from_iter([("name".to_owned(), json!(name))])
}

fn instant(rfc_3339: &str) -> Timestamp {
    rfc_3339.parse().unwrap()
}

struct World {
    db: TestDb,
    store: Store,
    tenant: TenantId,
}

impl World {
    async fn new() -> World {
        let db = TestDb::migrated().await;
        let store = db.store().await;
        World {
            db,
            store,
            tenant: support::tenant(),
        }
    }

    fn caller(&self) -> gnomon::Caller<'_> {
        self.store.as_worker(self.tenant, Scope::new(SCOPE), None)
    }

    async fn episode(&self, reference_time: Timestamp) -> EpisodeId {
        self.caller()
            .record_episode(&NewEpisode {
                tenant: Some(self.tenant),
                scope: Scope::new(SCOPE),
                content: "an Episode".into(),
                reference_time,
                authority: None,
            })
            .await
            .unwrap()
    }

    /// Runs one statement from SQL, as an end user who sees the Scope, with
    /// the Tenant as `$1`.
    async fn sql(&self, statement: &str) -> Vec<Row> {
        let claims = serde_json::Value::Object(support::user(self.tenant, &[SCOPE])).to_string();
        let mut client = self.db.login().await;
        let tx = client.transaction().await.unwrap();
        tx.batch_execute("set local role authenticated")
            .await
            .unwrap();
        tx.query_typed(
            "select set_config('request.jwt.claims', $1, true)",
            &[(&claims, Type::TEXT)],
        )
        .await
        .unwrap();
        let rows = tx
            .query_typed(statement, &[(&self.tenant.0, Type::UUID)])
            .await
            .unwrap();
        tx.commit().await.unwrap();
        rows
    }
}

/// The read failed because a stored time does not fit a `jiff::Timestamp`,
/// and for no other reason.
fn assert_time_out_of_range<T: std::fmt::Debug>(read: Result<T, Error>) {
    match read {
        Err(e @ Error::Database(_)) => {
            let reason = support::chain(&e);
            assert!(reason.contains("value too large to decode"), "{reason}");
        }
        other => panic!("expected the read to refuse the time, got {other:?}"),
    }
}

#[tokio::test]
async fn a_time_finer_than_a_microsecond_is_stored_truncated_toward_the_year_2000() {
    let w = World::new().await;
    // Postgres counts from 2000-01-01, and the driver drops the finer digits
    // of that count. So a later time moves down and an earlier one up.
    for (given, stored) in [
        (
            "2024-03-14T09:30:00.123456789Z",
            "2024-03-14T09:30:00.123456Z",
        ),
        (
            "1969-07-20T20:17:40.123456789Z",
            "1969-07-20T20:17:40.123457Z",
        ),
    ] {
        let id = w.episode(instant(given)).await;
        let read = w.caller().read_episode(w.tenant, id).await.unwrap();
        assert_eq!(read.unwrap().reference_time, instant(stored), "{given}");
    }
}

#[tokio::test]
async fn a_reference_time_after_the_year_9999_fails_the_read_with_an_error() {
    let w = World::new().await;
    let rows = w
        .sql(
            "select gnomon.record_episode($1, 'support', 'far ahead', '10000-01-01T00:00:00Z',
                                          null)",
        )
        .await;
    let id = EpisodeId(rows[0].get(0));

    assert_time_out_of_range(w.caller().read_episode(w.tenant, id).await);
}

#[tokio::test]
async fn a_valid_time_that_ends_after_the_year_9999_fails_each_read_of_it_with_an_error() {
    let w = World::new().await;
    let episode = w.episode(instant("2024-01-01T00:00:00Z")).await;
    let (node, _) = w
        .caller()
        .create_node(
            &NewNode {
                tenant: Some(w.tenant),
                scope: Scope::new(SCOPE),
                kind: ROOT_NODE_KIND.into(),
            },
            &Patch::new(episode, ValidTime::UNKNOWN, name("Ada")),
        )
        .await
        .unwrap();
    w.sql(&format!(
        "select gnomon.write_node($1, '{}', '{}', '2025-01-01T00:00:00Z', 'instant',
                                  '10000-01-01T00:00:00Z', 'instant', '{{\"name\": \"Grace\"}}',
                                  null)",
        node.0, episode.0
    ))
    .await;
    let as_of = AsOf::current(instant("2026-01-01T00:00:00Z"));

    assert_time_out_of_range(w.caller().read_node(w.tenant, node, as_of).await);
    assert_time_out_of_range(w.caller().read_nodes(w.tenant, &[node], as_of).await);

    // A failed read leaves its transaction as any other failed call does.
    let mut tx = w.caller().transaction(w.tenant).await.unwrap();
    assert_time_out_of_range(tx.read_node(node, as_of).await);
    assert!(matches!(tx.commit().await, Err(Error::RolledBack)));
}
