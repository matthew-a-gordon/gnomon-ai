//! Seam 1, who recorded an Episode (#81, ADR-0003), through the Rust core's
//! Store API against real Postgres. An Episode and its Retraction record
//! `gnomon.user_id()` in `recorded_by`: the end user's `sub` claim, the user
//! a worker was given, or null. No caller can set it, a deployment's own
//! `gnomon.user_id()` is used, and an Erasure that rebuilds history leaves
//! it as it was on the Episodes that survive.

mod support;

use gnomon::{EpisodeId, NewEpisode, Scope, ValidTime};
use serde_json::json;
use support::erasure::{SCOPE, World, at, values};
use tokio_postgres::error::SqlState;
use tokio_postgres::types::Type;

async fn record(caller: &gnomon::Caller<'_>, w: &World, content: &str) -> EpisodeId {
    caller
        .record_episode(&NewEpisode {
            tenant: Some(w.tenant),
            scope: Scope::new(SCOPE),
            content: content.into(),
            reference_time: at(2024, 1, 1),
            authority: None,
        })
        .await
        .unwrap()
}

fn user(w: &World, sub: Option<&str>) -> serde_json::Map<String, serde_json::Value> {
    let mut claims = values(json!({
        "role": "authenticated",
        "app_metadata": { "tenant_id": w.tenant.0.to_string(), "scopes": [SCOPE] }
    }));
    if let Some(sub) = sub {
        claims.insert("sub".into(), json!(sub));
    }
    claims
}

async fn recorded_by(w: &World, episode: EpisodeId) -> Option<String> {
    w.caller()
        .read_episode(w.tenant, episode)
        .await
        .unwrap()
        .expect("the Episode is there")
        .recorded_by
}

#[tokio::test]
async fn an_episode_records_the_users_sub_the_workers_given_user_or_no_one() {
    let w = World::new().await;
    let by_user = record(&w.store.as_user(user(&w, Some("user-1"))), &w, "a message").await;
    let by_worker = record(
        &w.store
            .as_worker(w.tenant, Scope::new(SCOPE), Some("user-2")),
        &w,
        "an import for user-2",
    )
    .await;
    let by_job = record(&w.caller(), &w, "a scheduled import").await;
    let no_sub = record(&w.store.as_user(user(&w, None)), &w, "no sub").await;
    let empty_sub = record(&w.store.as_user(user(&w, Some(""))), &w, "an empty sub").await;
    assert_eq!(recorded_by(&w, by_user).await.as_deref(), Some("user-1"));
    assert_eq!(recorded_by(&w, by_worker).await.as_deref(), Some("user-2"));
    assert_eq!(recorded_by(&w, by_job).await, None);
    assert_eq!(recorded_by(&w, no_sub).await, None);
    assert_eq!(recorded_by(&w, empty_sub).await, None);
    // A worker's user is a claim like any other: the worker still reads and
    // writes its own Scope alone.
    let episode = w
        .store
        .as_worker(w.tenant, Scope::new(SCOPE), Some("user-2"))
        .read_episode(w.tenant, by_user)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(episode.retracted, None);
}

#[tokio::test]
async fn a_retraction_records_when_and_by_whom() {
    let w = World::new().await;
    let episode = record(&w.store.as_user(user(&w, Some("author"))), &w, "a message").await;
    w.node(
        "company",
        episode,
        ValidTime::UNKNOWN,
        json!({ "name": "Acme" }),
    )
    .await;
    let retraction = w
        .store
        .as_user(user(&w, Some("reviewer")))
        .retract_episode(w.tenant, episode)
        .await
        .unwrap();
    let read = w
        .caller()
        .read_episode(w.tenant, episode)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(read.recorded_by.as_deref(), Some("author"));
    let retracted = read.retracted.expect("the Episode is retracted");
    assert_eq!(retracted.recorded_at, retraction.recorded_at);
    assert_eq!(retracted.recorded_by.as_deref(), Some("reviewer"));
}

#[tokio::test]
async fn no_caller_and_no_write_function_can_set_who_recorded_an_episode() {
    let w = World::new().await;
    let claims = serde_json::Value::Object(user(&w, Some("honest"))).to_string();
    let insert = "insert into gnomon.episode
                    (tenant_id, scope, id, content, reference_time, recorded_by)
                  values ($1, $2, gen_random_uuid(), 'forged', now(), 'someone else')";
    // Not as a caller role, which writes nothing directly, and not as the
    // writer role the write functions run as, which may not insert the
    // column, so `recorded_by` is always its default.
    for role in ["authenticated", "gnomon_worker", "gnomon_writer"] {
        let mut client = w.db.admin().await;
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
        let err = tx
            .execute_typed(insert, &[(&w.tenant.0, Type::UUID), (&SCOPE, Type::TEXT)])
            .await
            .expect_err(role);
        assert_eq!(
            err.code(),
            Some(&SqlState::INSUFFICIENT_PRIVILEGE),
            "{role}: {err:?}"
        );
    }
    for (table, column) in [
        ("episode", "recorded_by"),
        ("episode_retraction", "recorded_by"),
    ] {
        for role in ["authenticated", "gnomon_worker", "gnomon_writer"] {
            for privilege in ["INSERT", "UPDATE"] {
                let may: bool =
                    w.db.admin()
                        .await
                        .query_typed(
                            "select has_column_privilege($1, ('gnomon.' || $2)::regclass, $3, $4)",
                            &[
                                (&role, Type::TEXT),
                                (&table, Type::TEXT),
                                (&column, Type::TEXT),
                                (&privilege, Type::TEXT),
                            ],
                        )
                        .await
                        .unwrap()[0]
                        .get(0);
                assert!(!may, "{role} may {privilege} {table}.{column}");
            }
        }
    }
}

#[tokio::test]
async fn a_deployments_own_user_id_is_used() {
    let w = World::new().await;
    // A deployment that names its users by an email claim, and its workers
    // by the user they were given.
    w.db.operator()
        .await
        .batch_execute(
            "create or replace function gnomon.user_id() returns text
               language sql stable parallel safe security invoker
               set search_path = pg_catalog, pg_temp
               as $$
                 select case
                   when c ->> 'role' = 'gnomon_worker' then 'job for ' || (c ->> 'sub')
                   else c ->> 'email'
                 end
                 from (select nullif(pg_catalog.current_setting('request.jwt.claims', true), '')
                         ::jsonb as c) x
               $$",
        )
        .await
        .unwrap();
    let mut claims = user(&w, Some("user-1"));
    claims.insert("email".into(), json!("ada@example.com"));
    let by_user = record(&w.store.as_user(claims), &w, "a message").await;
    let by_worker = record(
        &w.store
            .as_worker(w.tenant, Scope::new(SCOPE), Some("user-1")),
        &w,
        "an import",
    )
    .await;
    assert_eq!(
        recorded_by(&w, by_user).await.as_deref(),
        Some("ada@example.com")
    );
    assert_eq!(
        recorded_by(&w, by_worker).await.as_deref(),
        Some("job for user-1")
    );
}

#[tokio::test]
async fn an_erasure_that_rebuilds_history_leaves_who_recorded_the_survivors_as_it_was() {
    let w = World::new().await;
    let kept = record(&w.store.as_user(user(&w, Some("author"))), &w, "kept").await;
    let erased = record(&w.store.as_user(user(&w, Some("other"))), &w, "erased").await;
    let node = w
        .node(
            "account",
            kept,
            ValidTime::UNKNOWN,
            json!({ "plan": "gold" }),
        )
        .await;
    w.write(
        node,
        erased,
        ValidTime::UNKNOWN,
        json!({ "plan": "silver" }),
    )
    .await;
    w.store
        .as_user(user(&w, Some("reviewer")))
        .retract_episode(w.tenant, kept)
        .await
        .unwrap();
    let before = w
        .caller()
        .read_episode(w.tenant, kept)
        .await
        .unwrap()
        .unwrap();
    let erasure = w.erase(erased).await;
    assert_eq!(erasure.recomputed_nodes, vec![node]);
    let after = w
        .caller()
        .read_episode(w.tenant, kept)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after, before);
    assert_eq!(after.recorded_by.as_deref(), Some("author"));
    assert_eq!(
        after.retracted.and_then(|r| r.recorded_by).as_deref(),
        Some("reviewer")
    );
    assert!(
        w.caller()
            .read_episode(w.tenant, erased)
            .await
            .unwrap()
            .is_none()
    );
}
