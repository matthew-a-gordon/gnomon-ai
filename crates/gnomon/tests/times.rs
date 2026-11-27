//! Seam 1, the instants of the public API: how a `jiff::Timestamp` crosses to
//! `timestamptz` and back, through the Rust core's Store API against real
//! Postgres. SQL appears only to write a time the Rust API cannot express.

mod support;

use std::time::Duration;

use gnomon::{
    AsOf, EpisodeId, Error, NewEpisode, NewNode, Patch, ROOT_NODE_KIND, Scope, StageError,
    StageOutcome, StageStatus, StageWrite, Store, TenantId, ValidTime,
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
async fn a_time_is_written_and_read_as_the_microsecond_the_server_counts() {
    let w = World::new().await;
    // Each direction is checked against the server's own count from 1970, so
    // an error that a write and a read would cancel out cannot pass.
    for (time, micros) in [
        ("0001-01-01T00:00:00Z", -62_135_596_800_000_000_i64),
        ("1969-07-20T20:17:40.123456Z", -14_182_939_876_544),
        ("2000-01-01T00:00:00Z", 946_684_800_000_000),
        ("2024-03-14T09:30:00.123456Z", 1_710_408_600_123_456),
        // The last microsecond a `Timestamp` holds.
        ("9999-12-30T22:00:00.999999Z", 253_402_207_200_999_999),
    ] {
        let written = w.episode(instant(time)).await;
        let rows = w
            .sql(&format!(
                "select (extract(epoch from reference_time) * 1000000)::int8
                 from gnomon.read_episode($1, '{}')",
                written.0
            ))
            .await;
        assert_eq!(rows[0].get::<_, i64>(0), micros, "write of {time}");

        let rows = w
            .sql(&format!(
                "select gnomon.record_episode($1, '{SCOPE}', 'from SQL', '{time}', null)"
            ))
            .await;
        let from_sql = EpisodeId(rows[0].get(0));
        let read = w.caller().read_episode(w.tenant, from_sql).await.unwrap();
        assert_eq!(
            read.unwrap().reference_time.as_microsecond(),
            micros,
            "read of {time}"
        );
    }
}

#[tokio::test]
async fn a_stored_time_outside_the_range_of_a_timestamp_fails_the_read_with_an_error() {
    let w = World::new().await;
    // A `Timestamp` ends at 9999-12-30T22:00 UTC, so the last day of 9999,
    // a common stand-in for "no end", is already past it.
    for time in [
        "9999-12-31T00:00:00Z",
        "10000-01-01T00:00:00Z",
        "infinity",
        "-infinity",
    ] {
        let rows = w
            .sql(&format!(
                "select gnomon.record_episode($1, '{SCOPE}', 'out of range', '{time}', null)"
            ))
            .await;
        let id = EpisodeId(rows[0].get(0));

        assert_time_out_of_range(w.caller().read_episode(w.tenant, id).await);
    }
}

#[tokio::test]
async fn a_retry_delay_that_ends_past_the_range_of_a_timestamp_is_refused() {
    let w = World::new().await;
    let episode = w.episode(instant("2024-01-01T00:00:00Z")).await;
    let caller = w.caller();
    caller
        .write_stages(w.tenant, &[StageWrite::pending(episode, "graph")])
        .await
        .unwrap();
    let claim = caller
        .claim_stage(w.tenant, Some(episode), Duration::from_secs(3600))
        .await
        .unwrap()
        .expect("the stage is due");

    // Ten thousand years, as a caller might write "never".
    let never = Duration::from_secs(10_000 * 365 * 24 * 3600);
    let refused = caller
        .write_stages(
            w.tenant,
            &[StageWrite::Finish {
                episode,
                stage: "graph".into(),
                attempt: claim.attempts,
                outcome: StageOutcome::Retry {
                    error: StageError {
                        code: "model_unavailable".into(),
                        status: None,
                    },
                    after: never,
                },
                usage: vec![],
            }],
        )
        .await;

    assert!(
        matches!(refused, Err(Error::InvalidWrite(_))),
        "{refused:?}"
    );
    // Nothing was saved, so the stage records still read.
    let stages = caller.stage_records(w.tenant, episode).await.unwrap();
    assert_eq!(stages[0].status, StageStatus::Running);
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
