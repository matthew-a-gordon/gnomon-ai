//! Seam 1 under concurrency: no caller's context reaches another caller's
//! call.
//!
//! Many Stores, each one connection, run users and workers on different
//! Tenants and Scopes at once. Raw connections as the login role run beside
//! them and check that nothing the Store set is left on the session. Lane 4
//! runs this through PgBouncer and Supavisor in transaction mode, where the
//! callers and the bystanders share a small pool of server connections, so
//! a role or claims left on a server connection would reach another
//! client's transaction (ADR-0001, ADR-0020).

mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use chrono::{TimeZone, Utc};
use gnomon::{Caller, EpisodeId, Error, NewEpisode, Scope, Store, TenantId};
use support::{TestDb, LOGIN_ROLE};
use tokio::sync::Barrier;

/// More clients than the poolers' default pool of 20 server connections,
/// so their transactions share server connections.
const STORES: usize = 24;
const BYSTANDERS: usize = 6;
const ROUNDS: usize = 20;

#[derive(Clone, Copy)]
enum Role {
    User,
    Worker,
}

/// Who one caller is: its Tenant, shared with one sibling in another Scope,
/// its Scope, and whether it is an end user or a worker.
#[derive(Clone)]
struct Identity {
    tenant: TenantId,
    scope: String,
    role: Role,
}

impl Identity {
    fn caller<'s>(&self, store: &'s Store) -> Caller<'s> {
        match self.role {
            Role::Worker => store.as_worker(self.tenant, Scope::new(&self.scope)),
            Role::User => store.as_user(support::user(self.tenant, &[&self.scope])),
        }
    }
}

/// The latest Episode each caller recorded, for the others to try to read.
type Latest = Arc<Mutex<Vec<Option<EpisodeId>>>>;

async fn run_caller(
    i: usize,
    who: Vec<Identity>,
    store: Store,
    latest: Latest,
    start: Arc<Barrier>,
) -> Result<usize, String> {
    let me = &who[i];
    // The sibling shares this caller's Tenant in another Scope. The
    // stranger is in another Tenant.
    let sibling = i ^ 1;
    let stranger = (i + 2) % who.len();
    let caller = me.caller(&store);
    start.wait().await;
    let mut checks = 0;
    for round in 0..ROUNDS {
        let content = format!("caller {i}, round {round}");
        let id = caller
            .record_episode(&NewEpisode {
                tenant: Some(me.tenant),
                scope: Scope::new(&me.scope),
                content: content.clone(),
                reference_time: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
                authority: None,
            })
            .await
            .map_err(|e| format!("caller {i} round {round}: record: {}", support::chain(&e)))?;
        latest.lock().unwrap()[i] = Some(id);

        let read = caller
            .read_episode(me.tenant, id)
            .await
            .map_err(|e| {
                format!(
                    "caller {i} round {round}: read its own Episode: {}",
                    support::chain(&e)
                )
            })?
            .ok_or_else(|| format!("caller {i} round {round}: its own Episode is not visible"))?;
        if read.tenant != me.tenant || read.scope.as_str() != me.scope || read.content != content {
            return Err(format!(
                "caller {i} round {round}: read {read:?} for its own Episode"
            ));
        }
        checks += 1;

        let theirs = latest.lock().unwrap()[sibling];
        if let Some(theirs) = theirs {
            match caller.read_episode(me.tenant, theirs).await {
                Ok(None) => checks += 1,
                Ok(Some(e)) => {
                    return Err(format!(
                        "caller {i} round {round}: read caller {sibling}'s Episode in Scope {}",
                        e.scope
                    ))
                }
                Err(e) => return Err(format!("caller {i} round {round}: {}", support::chain(&e))),
            }
        }

        let theirs = latest.lock().unwrap()[stranger];
        if let Some(theirs) = theirs {
            match caller.read_episode(who[stranger].tenant, theirs).await {
                Err(Error::TenantMismatch(_)) => checks += 1,
                Ok(found) => {
                    return Err(format!(
                        "caller {i} round {round}: a call in caller {stranger}'s Tenant \
                         ran with this caller's claims and read {found:?}"
                    ))
                }
                Err(e) => return Err(format!("caller {i} round {round}: {}", support::chain(&e))),
            }
        }
    }
    Ok(checks)
}

/// Checks, until `done`, that a transaction as the login role sees no
/// claims and no role but the login role's own.
async fn run_bystander(
    mut client: tokio_postgres::Client,
    done: Arc<AtomicBool>,
    start: Arc<Barrier>,
) -> Result<usize, String> {
    start.wait().await;
    let mut checks = 0;
    // At least a few checks, however fast the callers finish.
    while !done.load(Ordering::Relaxed) || checks < 20 {
        let tx = client.transaction().await.map_err(|e| support::chain(&e))?;
        let row = tx
            .query_typed(
                "select current_user::text, \
                        coalesce(current_setting('request.jwt.claims', true), ''), \
                        current_setting('role')",
                &[],
            )
            .await
            .map_err(|e| support::chain(&e))?
            .remove(0);
        tx.commit().await.map_err(|e| support::chain(&e))?;
        let (user, claims, role): (String, String, String) = (row.get(0), row.get(1), row.get(2));
        if user != LOGIN_ROLE || !claims.is_empty() || role != "none" {
            return Err(format!(
                "a bystander's transaction ran as {user} with role {role} and claims {claims:?}"
            ));
        }
        checks += 1;
    }
    Ok(checks)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_callers_never_see_another_callers_tenant_or_claims() {
    let db = TestDb::migrated().await;
    let tenants: Vec<TenantId> = (0..STORES / 2).map(|_| support::tenant()).collect();
    let who: Vec<Identity> = (0..STORES)
        .map(|i| Identity {
            tenant: tenants[i / 2],
            scope: format!("scope-{i}"),
            role: if i % 3 == 0 { Role::Worker } else { Role::User },
        })
        .collect();

    let mut stores = Vec::new();
    for _ in 0..STORES {
        stores.push(db.store().await);
    }
    let latest: Latest = Arc::new(Mutex::new(vec![None; STORES]));
    let start = Arc::new(Barrier::new(STORES + BYSTANDERS));
    let done = Arc::new(AtomicBool::new(false));

    let callers: Vec<_> = stores
        .into_iter()
        .enumerate()
        .map(|(i, store)| {
            tokio::spawn(run_caller(
                i,
                who.clone(),
                store,
                latest.clone(),
                start.clone(),
            ))
        })
        .collect();
    let mut bystanders = Vec::new();
    for _ in 0..BYSTANDERS {
        let client = db.login().await;
        bystanders.push(tokio::spawn(run_bystander(
            client,
            done.clone(),
            start.clone(),
        )));
    }

    let mut caller_results = Vec::new();
    for c in callers {
        caller_results.push(c.await.expect("a caller task panicked"));
    }
    done.store(true, Ordering::Relaxed);
    let mut bystander_results = Vec::new();
    for b in bystanders {
        bystander_results.push(b.await.expect("a bystander task panicked"));
    }

    let mut failures = Vec::new();
    let (mut caller_checks, mut bystander_checks) = (0, 0);
    for r in caller_results {
        match r {
            Ok(n) => caller_checks += n,
            Err(e) => failures.push(e),
        }
    }
    for r in bystander_results {
        match r {
            Ok(n) => bystander_checks += n,
            Err(e) => failures.push(e),
        }
    }
    println!(
        "{STORES} callers on {} Tenants, {ROUNDS} rounds: {caller_checks} caller checks, \
         {bystander_checks} bystander checks",
        STORES / 2
    );
    assert!(failures.is_empty(), "{failures:#?}");
    assert!(
        caller_checks >= STORES * ROUNDS,
        "only {caller_checks} caller checks ran"
    );
}
