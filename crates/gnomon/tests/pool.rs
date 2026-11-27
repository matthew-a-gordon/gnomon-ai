//! Seam 1 for the Store's connection pool and its public transaction (#82,
//! ADR-0016): calls that run at once, a dead connection replaced, the wait
//! for a free connection, a transaction that commits only when every call
//! in it succeeded, cancellation at any point leaving nothing on a
//! connection that a later call takes, and the typed errors.
//!
//! The tests that count connections or cut them run through the recording
//! proxy of `support::wire`, so they see what each connection carried.

mod support;

use std::future::Future;
use std::time::Duration;

use chrono::{TimeZone, Utc};
use gnomon::{Error, NewEpisode, Patch, Scope, Store, TenantId, UnavailableReason, ValidTime};
use serde_json::json;
use support::erasure::{World, at, values};
use support::wire::{self, Sent};
use support::{LOGIN_ROLE, TestDb, tenant, user};
use tokio_postgres::types::Type;

fn episode(tenant: TenantId, content: &str) -> NewEpisode {
    NewEpisode {
        tenant: Some(tenant),
        scope: Scope::new("support"),
        content: content.to_owned(),
        reference_time: Utc.with_ymd_and_hms(2024, 3, 14, 9, 30, 0).unwrap(),
        authority: None,
    }
}

/// How many Episodes hold `content`, read past RLS.
async fn episodes_saying(db: &TestDb, content: &str) -> i64 {
    db.admin()
        .await
        .query_one(
            "select count(*) from gnomon.episode where content = $1",
            &[&content],
        )
        .await
        .unwrap()
        .get(0)
}

/// Drives `call` until Postgres shows a backend of the login role in this
/// database waiting on a lock, and fails if `call` finishes first.
async fn until_waiting_on_a_lock<F: Future + Unpin>(db: &TestDb, call: &mut F)
where
    F::Output: std::fmt::Debug,
{
    let admin = db.admin().await;
    loop {
        tokio::select! {
            done = &mut *call => panic!("the call finished while the lock was held: {done:?}"),
            _ = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
        let waiting: i64 = admin
            .query_typed(
                "select count(*) from pg_stat_activity
                 where datname = current_database() and usename = $1 and wait_event_type = 'Lock'",
                &[(&LOGIN_ROLE, Type::TEXT)],
            )
            .await
            .unwrap()[0]
            .get(0);
        if waiting > 0 {
            return;
        }
    }
}

#[tokio::test]
async fn calls_run_at_once_and_a_call_waiting_on_a_lock_does_not_hold_up_another() {
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    let locked = w
        .node("account", e, ValidTime::UNKNOWN, json!({ "plan": "basic" }))
        .await;
    let free = w
        .node("account", e, ValidTime::UNKNOWN, json!({ "plan": "basic" }))
        .await;
    let patch = |plan: &str| Patch::new(e, ValidTime::UNKNOWN, values(json!({ "plan": plan })));
    let caller = w.caller();

    // A transaction writes the Node, and holds its lock until it commits.
    let mut holder = caller.transaction(w.tenant).await.unwrap();
    holder.write_node(locked, &patch("pro")).await.unwrap();

    // A single call that writes the same Node waits for that lock, on a
    // connection of its own.
    let pro = patch("enterprise");
    let waiting = caller.write_node(w.tenant, locked, &pro);
    tokio::pin!(waiting);
    until_waiting_on_a_lock(&w.db, &mut waiting).await;

    // Meanwhile calls on other connections run and finish: a write to
    // another Node and a read. With one connection behind a lock they would
    // wait for the holder, so a timeout stands in for a hang.
    let others = async {
        caller
            .write_node(w.tenant, free, &patch("pro"))
            .await
            .unwrap();
        caller
            .read_node(w.tenant, locked, gnomon::AsOf::current(at(2024, 1, 1)))
            .await
            .unwrap()
    };
    let read = tokio::time::timeout(Duration::from_secs(60), others)
        .await
        .expect("the other calls were held up by the call waiting on the lock")
        .expect("the Node is visible");
    // The holder has not committed, so the read sees the Node as it was.
    assert_eq!(
        serde_json::Value::Object(read.values),
        json!({ "plan": "basic" })
    );
    assert_eq!(
        w.current_values(free, at(2024, 1, 1)).await,
        Some(json!({ "plan": "pro" }))
    );

    holder.commit().await.unwrap();
    waiting.await.unwrap();
    assert_eq!(
        w.current_values(locked, at(2024, 1, 1)).await,
        Some(json!({ "plan": "enterprise" }))
    );
}

#[tokio::test]
async fn a_connection_the_server_closed_between_two_calls_is_replaced_and_the_second_call_succeeds()
{
    let db = TestDb::migrated().await;
    let options = support::connect_options().max_connections(1);
    let store = Store::connect(&db.login_url(), &options).await.unwrap();
    let t = tenant();
    let alice = store.as_user(user(t, &["support"]));
    let first = alice
        .record_episode(&episode(t, "Before the connection closed."))
        .await
        .unwrap();

    // The server ends every session of the login role in this database, the
    // Store's one connection among them.
    let ended: i64 = db
        .admin()
        .await
        .query_typed(
            "select count(*) filter (where pg_terminate_backend(pid)) from pg_stat_activity
             where datname = current_database() and usename = $1",
            &[(&LOGIN_ROLE, Type::TEXT)],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert!(ended >= 1, "no session of the Store's was ended");

    let read = alice.read_episode(t, first).await.unwrap();
    assert!(
        read.is_some(),
        "the second call reads the first call's Episode"
    );
}

#[tokio::test]
async fn a_connection_found_dead_when_its_transaction_starts_is_replaced_once_and_a_healthy_call_sends_nothing_extra()
 {
    let db = TestDb::migrated().await;
    // Straight to the server, so the proxy's connections are the Store's.
    let (proxy, log) = wire::record(db.direct_server_addr().await).await;
    let options = support::connect_options().max_connections(1);
    let store = Store::connect(&db.direct_login_url_through(proxy), &options)
        .await
        .unwrap();
    let t = tenant();
    let alice = store.as_user(user(t, &["support"]));
    let first = alice
        .record_episode(&episode(t, "On the first connection."))
        .await
        .unwrap();

    // The connection dies unseen: the client learns of it only when it
    // sends the next call's BEGIN.
    log.cut_open_connections();
    let read = alice.read_episode(t, first).await.unwrap();
    assert!(read.is_some());

    let connections = log.by_connection();
    assert_eq!(
        connections.len(),
        2,
        "one dead connection and its replacement"
    );
    let queries = |c: &[Sent]| -> Vec<String> {
        c.iter()
            .filter_map(|m| match m {
                Sent::Query(q) => Some(q.clone()),
                Sent::Parse { sql, .. } => Some(sql.trim().to_owned()),
                Sent::Other(_) => None,
            })
            .collect()
    };
    let dead = queries(&connections[0]);
    // The schema check, the first call, and the second call's BEGIN, which
    // found the connection dead and went no further on it.
    assert_eq!(
        dead.last().map(String::as_str),
        Some("begin; set local role authenticated"),
        "{dead:#?}"
    );
    assert_eq!(dead.len(), 3 + 4 + 1, "{dead:#?}");
    // On the new connection the call runs as any call does: no query to
    // test the connection, and no second schema check.
    let replacement = queries(&connections[1]);
    assert_eq!(replacement.len(), 4, "{replacement:#?}");
    assert_eq!(replacement[0], "begin; set local role authenticated");
    assert!(replacement[2].contains("gnomon.read_episode("));
    assert_eq!(replacement[3], "commit");
}

#[tokio::test]
async fn a_call_that_finds_no_free_connection_fails_unavailable_after_the_wait_timeout() {
    let db = TestDb::migrated().await;
    let options = support::connect_options()
        .max_connections(1)
        .wait_timeout(Duration::from_millis(200));
    let store = Store::connect(&db.login_url(), &options).await.unwrap();
    let t = tenant();
    let alice = store.as_user(user(t, &["support"]));

    // The transaction holds the pool's one connection.
    let mut tx = alice.transaction(t).await.unwrap();
    let id = tx
        .record_episode(&episode(t, "Inside the transaction."))
        .await
        .unwrap();
    match alice.read_episode(t, id).await {
        Err(
            e @ Error::Unavailable {
                reason: UnavailableReason::PoolTimeout,
                ..
            },
        ) => {
            assert!(e.can_run_again(), "nothing was sent, so it can run again");
            assert!(
                support::chain(&e).contains("Timeout"),
                "the pool's timeout is the cause: {}",
                support::chain(&e)
            );
        }
        other => panic!("expected no free connection, got {other:?}"),
    }

    tx.commit().await.unwrap();
    assert!(alice.read_episode(t, id).await.unwrap().is_some());
}

#[tokio::test]
async fn a_transaction_saves_nothing_until_it_commits_and_commit_fails_after_a_failed_call() {
    let db = TestDb::migrated().await;
    let store = Store::connect(&db.login_url(), &support::connect_options())
        .await
        .unwrap();
    let t = tenant();
    let alice = store.as_user(user(t, &["support"]));

    // Dropped without a commit: nothing is saved.
    let mut tx = alice.transaction(t).await.unwrap();
    tx.record_episode(&episode(t, "Dropped.")).await.unwrap();
    drop(tx);
    assert_eq!(episodes_saying(&db, "Dropped.").await, 0);

    // Rolled back: nothing is saved.
    let mut tx = alice.transaction(t).await.unwrap();
    tx.record_episode(&episode(t, "Rolled back."))
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(episodes_saying(&db, "Rolled back.").await, 0);

    // A call fails after one that succeeded: COMMIT would roll back without
    // an error, so commit reports it, and the first call's Episode is not
    // saved.
    let mut tx = alice.transaction(t).await.unwrap();
    tx.record_episode(&episode(t, "Before the failure."))
        .await
        .unwrap();
    let refused = tx
        .record_episode(&NewEpisode {
            scope: Scope::new("billing"),
            ..episode(t, "Into a Scope the caller may not write.")
        })
        .await;
    assert!(matches!(refused, Err(Error::Forbidden(_))), "{refused:?}");
    let commit = tx.commit().await;
    assert!(matches!(commit, Err(Error::RolledBack)), "{commit:?}");
    assert!(!commit.unwrap_err().can_run_again());
    assert_eq!(episodes_saying(&db, "Before the failure.").await, 0);

    // A call that names another Tenant than the transaction's fails, and
    // fails the transaction too.
    let mut tx = alice.transaction(t).await.unwrap();
    let other = tx
        .record_episode(&episode(tenant(), "Another Tenant."))
        .await;
    assert!(matches!(other, Err(Error::TenantMismatch(_))), "{other:?}");
    assert!(matches!(tx.commit().await, Err(Error::RolledBack)));

    // Committed: everything in it is saved at once.
    let mut tx = alice.transaction(t).await.unwrap();
    for n in 0..3 {
        tx.record_episode(&episode(t, &format!("Committed {n}.")))
            .await
            .unwrap();
    }
    assert_eq!(episodes_saying(&db, "Committed 0.").await, 0);
    tx.commit().await.unwrap();
    for n in 0..3 {
        assert_eq!(episodes_saying(&db, &format!("Committed {n}.")).await, 1);
    }
}

#[tokio::test]
async fn a_deadlock_in_a_transaction_is_a_conflict_its_holder_can_run_again() {
    let w = World::new().await;
    let e = w.episode(at(2024, 1, 1), None).await;
    let a = w
        .node("account", e, ValidTime::UNKNOWN, json!({ "plan": "basic" }))
        .await;
    let b = w
        .node("account", e, ValidTime::UNKNOWN, json!({ "plan": "basic" }))
        .await;
    let patch = Patch::new(e, ValidTime::UNKNOWN, values(json!({ "plan": "pro" })));
    let caller = w.caller();

    // Each transaction locks one Node, then writes the other's.
    let mut first = caller.transaction(w.tenant).await.unwrap();
    let mut second = caller.transaction(w.tenant).await.unwrap();
    first.write_node(a, &patch).await.unwrap();
    second.write_node(b, &patch).await.unwrap();
    let (one, two) = tokio::join!(first.write_node(b, &patch), second.write_node(a, &patch));

    // Postgres breaks the deadlock by failing one of them.
    let (winner, loser, lost) = match (one, two) {
        (Ok(_), Err(e)) => (first, second, e),
        (Err(e), Ok(_)) => (second, first, e),
        other => panic!("expected exactly one deadlock, got {other:?}"),
    };
    assert!(matches!(lost, Error::Conflict(_)), "{lost:?}");
    assert!(lost.can_run_again());
    assert_eq!(
        lost.sqlstate(),
        Some(&tokio_postgres::error::SqlState::T_R_DEADLOCK_DETECTED)
    );
    assert!(matches!(loser.commit().await, Err(Error::RolledBack)));
    winner.commit().await.unwrap();
}

#[tokio::test]
async fn a_connection_lost_during_commit_says_the_outcome_is_unknown() {
    let db = TestDb::migrated().await;
    let (proxy, log) = wire::record(db.direct_server_addr().await).await;
    let store = Store::connect(
        &db.direct_login_url_through(proxy),
        &support::connect_options(),
    )
    .await
    .unwrap();
    let t = tenant();
    let alice = store.as_user(user(t, &["support"]));
    let mut tx = alice.transaction(t).await.unwrap();
    tx.record_episode(&episode(t, "Committed or not."))
        .await
        .unwrap();

    // The connection dies when COMMIT is sent, before the server answers.
    log.cut_open_connections();
    match tx.commit().await {
        Err(
            e @ Error::Unavailable {
                reason: UnavailableReason::LostDuringCommit,
                ..
            },
        ) => {
            assert!(!e.can_run_again(), "the first run may have committed");
            assert!(e.to_string().contains("unknown"), "{e}");
        }
        other => panic!("expected an unknown outcome, got {other:?}"),
    }
    let last = log.by_connection()[0].iter().rev().find_map(|m| match m {
        Sent::Query(q) => Some(q.clone()),
        _ => None,
    });
    assert_eq!(last.as_deref(), Some("commit"));
}

/// Where in a statement's text a transaction starts or ends.
fn transaction_edge(m: &Sent) -> Option<bool> {
    match m {
        Sent::Query(q) => {
            let q = q.trim().to_ascii_lowercase();
            if q.starts_with("begin") {
                Some(true)
            } else if q == "commit" || q == "rollback" {
                Some(false)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Runs `call` until it finishes or the proxy has seen `n` more messages
/// after `seen`, whichever comes first, and drops it there. Returns its
/// output, or `None` when it was cancelled.
async fn cancel_after<F: Future>(
    log: &wire::WireLog,
    seen: usize,
    n: usize,
    call: F,
) -> Option<F::Output> {
    tokio::select! {
        biased;
        () = log.until_seen(seen + n) => None,
        done = call => Some(done),
    }
}

#[tokio::test]
async fn a_call_cancelled_at_any_point_leaves_no_open_transaction_on_a_connection_a_later_call_takes()
 {
    let db = TestDb::migrated().await;
    let (proxy, log) = wire::record(db.direct_server_addr().await).await;
    let options = support::connect_options().max_connections(1);
    let (t, stranger) = (tenant(), tenant());
    let mut cancelled = [0; 3];
    // Each kind of call is cancelled once the client has sent its first
    // message, its second, and so on, past the last one a call sends: the
    // schema check, BEGIN, the claims, the call and COMMIT each in turn.
    for kind in 0..3 {
        for n in 1..=24 {
            // A fresh Store each time, so the first call's schema check is
            // cut short too.
            let store = Store::connect(&db.direct_login_url_through(proxy), &options)
                .await
                .unwrap();
            let alice = store.as_user(user(t, &["support"]));
            let content = format!("Cancelled call {kind} after {n} messages.");
            match kind {
                // A single call.
                0 => {
                    let new = episode(t, &content);
                    let call = alice.record_episode(&new);
                    if cancel_after(&log, log.len(), n, call).await.is_none() {
                        cancelled[0] += 1;
                    }
                }
                // A transaction, while it starts.
                1 => match cancel_after(&log, log.len(), n, alice.transaction(t)).await {
                    None => cancelled[1] += 1,
                    Some(tx) => tx.unwrap().commit().await.unwrap(),
                },
                // A call inside a transaction, which then cannot commit it.
                _ => {
                    let mut tx = alice.transaction(t).await.unwrap();
                    let new = episode(t, &content);
                    let call = tx.record_episode(&new);
                    match cancel_after(&log, log.len(), n, call).await {
                        None => {
                            cancelled[2] += 1;
                            assert!(matches!(tx.commit().await, Err(Error::RolledBack)));
                            assert_eq!(episodes_saying(&db, &content).await, 0);
                        }
                        Some(done) => {
                            done.unwrap();
                            tx.commit().await.unwrap();
                        }
                    }
                }
            }
            // A later call of another caller takes the pool's one
            // connection, or a new one if the cancelled call's was closed.
            let other = store.as_user(user(stranger, &["support"]));
            assert_eq!(
                other
                    .read_episode(stranger, gnomon::EpisodeId(uuid::Uuid::new_v4()))
                    .await
                    .unwrap(),
                None
            );
        }
    }
    assert!(
        cancelled.iter().all(|&n| n >= 4),
        "every kind of call was cancelled at several points: {cancelled:?}"
    );

    // On every connection each transaction ended before the next began. A
    // connection whose transaction a cancellation left open carries
    // nothing more.
    for (n, sent) in log.by_connection().iter().enumerate() {
        let mut open = false;
        for m in sent {
            match transaction_edge(m) {
                Some(true) => {
                    assert!(
                        !open,
                        "connection {n} began a transaction inside another: {sent:#?}"
                    );
                    open = true;
                }
                Some(false) => open = false,
                None => {}
            }
        }
    }
}
