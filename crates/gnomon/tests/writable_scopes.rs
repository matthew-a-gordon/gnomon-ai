//! Seam 1, writable Scopes (#81, ADR-0005, ADR-0009), through the Rust
//! core's Store API against real Postgres. A caller writes a Scope only when
//! `gnomon.writable_scopes()` names it as well as `gnomon.allowed_scopes()`.
//! Every write function refuses a Scope the caller may only read, while a
//! row in a writable Scope may refer to one there. Erasure needs the named
//! item's Scope to be writable, and still removes what depends on it in a
//! Scope the caller cannot write. A deployment's functions that read a
//! membership table through a definer function of their own work for reads,
//! writes and Erasure.

mod support;

use gnomon::{
    AsOf, Bound, BoundSide, BoundStatement, DerivedCitation, EdgeId, EndOfValidity, EpisodeId,
    Error, NewCommunity, NewEdge, NewEpisode, NewNode, NewSummary, NodeId, Patch, Retraction,
    Scope, StageOutcome, StageUsage, StageWrite, ValidTime, VersionId,
};
use serde_json::json;
use support::erasure::{SCOPE, World, at, values};
use tokio_postgres::error::SqlState;
use tokio_postgres::types::Type;

/// The Scope the restricted caller may read and not write.
const READ_ONLY: &str = "billing";

/// A deployment's `gnomon.writable_scopes()`: the claim
/// `app_metadata.writable_scopes` when the token carries one, and every
/// Scope the caller may read when it does not, as a worker's does not.
const WRITABLE_FROM_CLAIMS: &str = "
create or replace function gnomon.writable_scopes() returns text[]
  language sql stable parallel safe security invoker set search_path = pg_catalog, pg_temp
  as $$
    select case
      when c -> 'app_metadata' ? 'writable_scopes' then
        array(select pg_catalog.jsonb_array_elements_text(c -> 'app_metadata' -> 'writable_scopes'))
      else gnomon.allowed_scopes()
    end
    from (select nullif(pg_catalog.current_setting('request.jwt.claims', true), '')::jsonb as c) x
  $$";

fn assert_read_only<T: std::fmt::Debug>(result: Result<T, Error>, what: &str) {
    match result {
        Err(Error::Forbidden(e)) => {
            assert_eq!(
                e.code(),
                Some(&SqlState::INSUFFICIENT_PRIVILEGE),
                "{what}: {e:?}"
            );
            assert_eq!(
                e.as_db_error().unwrap().message(),
                format!("Scope {READ_ONLY} is read-only for the caller"),
                "{what}"
            );
        }
        other => panic!("{what}: expected a refusal of a read-only Scope, got {other:?}"),
    }
}

/// What `READ_ONLY` holds, written by its own worker, which may write it.
struct ReadOnly {
    w: World,
    episode: EpisodeId,
    account: NodeId,
    account_version: VersionId,
    account_hash: String,
    company: NodeId,
    edge: EdgeId,
    edge_version: VersionId,
    summary: gnomon::SummaryId,
    community: gnomon::CommunityId,
    /// An Episode and a company of `SCOPE`, which the restricted caller may
    /// write.
    writable_episode: EpisodeId,
    writable_company: NodeId,
}

async fn read_only() -> ReadOnly {
    let w = World::new().await;
    w.db.operator()
        .await
        .batch_execute(WRITABLE_FROM_CLAIMS)
        .await
        .unwrap();
    w.db.grant_erasure("authenticated").await;
    let episode = w
        .episode_in(READ_ONLY, at(2024, 1, 1), "billing's Episode")
        .await;
    let account = w
        .node_in(READ_ONLY, "account", episode, json!({ "plan": "gold" }))
        .await;
    let company = w
        .node_in(READ_ONLY, "company", episode, json!({ "name": "Acme" }))
        .await;
    let edge = w
        .edge_in(READ_ONLY, account, company, episode, json!({ "share": 1 }))
        .await;
    let both = w.seeing(&[SCOPE, READ_ONLY]);
    let now = AsOf::current(at(2025, 1, 1));
    let account_read = both
        .read_node(w.tenant, account, now)
        .await
        .unwrap()
        .unwrap();
    let edge_version = both
        .read_edge(w.tenant, edge, now)
        .await
        .unwrap()
        .unwrap()
        .id;
    let worker = w.worker_in(READ_ONLY);
    let summary = worker
        .write_summary(
            w.tenant,
            &NewSummary {
                scope: Scope::new(READ_ONLY),
                node: account,
                content: "An account on the gold plan.".into(),
                as_of_valid: at(2024, 6, 1),
                citations: vec![DerivedCitation::of(episode)],
            },
        )
        .await
        .unwrap();
    let community = worker
        .write_community(
            w.tenant,
            &NewCommunity {
                scope: Scope::new(READ_ONLY),
                replaces: vec![],
                members: vec![account, company],
                content: "Acme and its account.".into(),
                as_of_valid: at(2024, 6, 1),
                citations: vec![DerivedCitation::of(episode)],
            },
        )
        .await
        .unwrap();
    worker
        .write_stages(w.tenant, &[StageWrite::pending(episode, "graph")])
        .await
        .unwrap();
    let writable_episode = w
        .episode_in(SCOPE, at(2024, 2, 1), "support's Episode")
        .await;
    let writable_company = w
        .node_in(
            SCOPE,
            "company",
            writable_episode,
            json!({ "name": "Initech" }),
        )
        .await;
    ReadOnly {
        episode,
        account,
        account_version: account_read.id,
        account_hash: account_read.text_hash,
        company,
        edge,
        edge_version,
        summary,
        community,
        writable_episode,
        writable_company,
        w,
    }
}

/// A user who may read `SCOPE` and `READ_ONLY` and write `SCOPE` alone.
fn restricted(w: &World) -> gnomon::Caller<'_> {
    w.store.as_user(values(json!({
        "sub": "restricted-user",
        "role": "authenticated",
        "app_metadata": {
            "tenant_id": w.tenant.0.to_string(),
            "scopes": [SCOPE, READ_ONLY],
            "writable_scopes": [SCOPE],
        }
    })))
}

/// Every row of every gnomon table that has a Scope, in `READ_ONLY`, past
/// RLS.
async fn read_only_rows(w: &World) -> Vec<String> {
    let admin = w.db.admin().await;
    let tables: Vec<String> = admin
        .query_typed(
            "select c.relname::text from pg_class c
             where c.relnamespace = 'gnomon'::regnamespace and c.relkind = 'r'
               and exists (select from pg_attribute a
                           where a.attrelid = c.oid and a.attname = 'scope')
             order by 1",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    let mut rows = Vec::new();
    for table in tables {
        rows.extend(
            admin
                .query_typed(
                    &format!("select t::text from gnomon.{table} t where scope = $1 order by 1"),
                    &[(&READ_ONLY, Type::TEXT)],
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
async fn every_write_function_refuses_a_scope_the_caller_may_only_read() {
    let r = read_only().await;
    let w = &r.w;
    let t = w.tenant;
    // The record its own worker claimed, which the restricted caller may
    // not finish.
    let claim = w
        .worker_in(READ_ONLY)
        .claim_stage(t, Some(r.episode), std::time::Duration::from_secs(3600))
        .await
        .unwrap()
        .expect("the graph stage is due");
    let before = read_only_rows(w).await;
    let caller = restricted(w);
    let patch = |v: serde_json::Value| Patch::new(r.episode, ValidTime::UNKNOWN, values(v));
    let mut rng = support::vectors::Rng::new(81);

    assert_read_only(
        caller
            .record_episode(&NewEpisode {
                tenant: Some(t),
                scope: Scope::new(READ_ONLY),
                content: "a new Episode".into(),
                reference_time: at(2024, 3, 1),
                authority: None,
            })
            .await,
        "recording an Episode",
    );
    assert_read_only(
        caller
            .create_node(
                &NewNode {
                    tenant: Some(t),
                    scope: Scope::new(READ_ONLY),
                    kind: "company".into(),
                },
                &patch(json!({ "name": "Globex" })),
            )
            .await,
        "creating a Node",
    );
    assert_read_only(
        caller
            .write_node(t, r.account, &patch(json!({ "plan": "silver" })))
            .await,
        "writing a Node",
    );
    assert_read_only(
        caller
            .correct_node(t, r.account, &patch(json!({ "plan": "silver" })))
            .await,
        "correcting a Node",
    );
    assert_read_only(
        caller
            .state_bound(
                t,
                r.account,
                &BoundStatement {
                    episode: r.episode,
                    version: r.account_version,
                    side: BoundSide::Start,
                    bound: Bound::At(at(2023, 1, 1)),
                },
            )
            .await,
        "stating a Node's bound",
    );
    assert_read_only(
        caller
            .end_node_validity(
                t,
                r.account,
                &EndOfValidity {
                    episode: r.episode,
                    at: Bound::At(at(2024, 5, 1)),
                },
            )
            .await,
        "ending a Node's validity",
    );
    assert_read_only(
        caller
            .retract_node_version(
                t,
                r.account,
                &Retraction {
                    episode: r.episode,
                    version: r.account_version,
                },
            )
            .await,
        "retracting a Node's Version",
    );
    assert_read_only(
        caller
            .create_edge(
                &NewEdge {
                    tenant: Some(t),
                    scope: Scope::new(READ_ONLY),
                    kind: "owns".into(),
                    source: r.account,
                    target: r.company,
                },
                &patch(json!({ "share": 2 })),
            )
            .await,
        "creating an Edge",
    );
    assert_read_only(
        caller
            .write_edge(t, r.edge, &patch(json!({ "share": 2 })))
            .await,
        "writing an Edge",
    );
    assert_read_only(
        caller
            .correct_edge(t, r.edge, &patch(json!({ "share": 2 })))
            .await,
        "correcting an Edge",
    );
    assert_read_only(
        caller
            .state_edge_bound(
                t,
                r.edge,
                &BoundStatement {
                    episode: r.episode,
                    version: r.edge_version,
                    side: BoundSide::Start,
                    bound: Bound::At(at(2023, 1, 1)),
                },
            )
            .await,
        "stating an Edge's bound",
    );
    assert_read_only(
        caller
            .end_edge_validity(
                t,
                r.edge,
                &EndOfValidity {
                    episode: r.episode,
                    at: Bound::At(at(2024, 5, 1)),
                },
            )
            .await,
        "ending an Edge's validity",
    );
    assert_read_only(
        caller
            .retract_edge_version(
                t,
                r.edge,
                &Retraction {
                    episode: r.episode,
                    version: r.edge_version,
                },
            )
            .await,
        "retracting an Edge's Version",
    );
    assert_read_only(
        caller.retract_episode(t, r.episode).await,
        "Retract Episode",
    );
    assert_read_only(
        caller
            .write_stages(t, &[StageWrite::pending(r.episode, "sentences")])
            .await,
        "writing a pending stage record",
    );
    assert_read_only(
        caller
            .write_stages(
                t,
                &[StageWrite::finish(
                    &claim,
                    StageOutcome::Succeeded,
                    vec![StageUsage {
                        model: "a-model".into(),
                        calls: 1,
                        input_tokens: 10,
                        output_tokens: 1,
                        cost_usd: None,
                    }],
                )],
            )
            .await,
        "finishing a stage record",
    );
    assert_read_only(
        caller
            .claim_stage(t, Some(r.episode), std::time::Duration::from_secs(0))
            .await,
        "claiming a stage record of the Episode",
    );
    assert_read_only(
        caller
            .write_summary(
                t,
                &NewSummary {
                    scope: Scope::new(READ_ONLY),
                    node: r.account,
                    content: "A regenerated Summary.".into(),
                    as_of_valid: at(2024, 6, 1),
                    citations: vec![DerivedCitation::of(r.episode)],
                },
            )
            .await,
        "writing a Summary",
    );
    assert_read_only(
        caller
            .write_community(
                t,
                &NewCommunity {
                    scope: Scope::new(READ_ONLY),
                    replaces: vec![r.community],
                    members: vec![r.account],
                    content: "A regenerated Community.".into(),
                    as_of_valid: at(2024, 6, 1),
                    citations: vec![DerivedCitation::of(r.episode)],
                },
            )
            .await,
        "writing a Community",
    );
    assert_read_only(
        caller
            .store_embedding(t, r.account_version, &r.account_hash, &rng.unit())
            .await,
        "storing a Version's embedding",
    );
    assert_read_only(
        caller
            .store_name_embedding(t, r.company, "Acme", &rng.unit())
            .await,
        "storing a name's embedding",
    );
    assert_read_only(
        caller
            .store_summary_embedding(t, r.summary, &rng.unit())
            .await,
        "storing a Summary's embedding",
    );
    assert_read_only(
        caller
            .store_community_embedding(t, r.community, &rng.unit())
            .await,
        "storing a Community's embedding",
    );
    assert_read_only(
        caller
            .store_sentence(t, r.edge_version, "The account owns a share of Acme.")
            .await,
        "storing a Sentence",
    );
    // A claim that names no Episode passes over the read-only Scope's due
    // records rather than fail: there is nothing in the caller's writable
    // Scopes to claim.
    assert_eq!(
        caller
            .claim_stage(t, None, std::time::Duration::from_secs(0))
            .await
            .unwrap(),
        None
    );
    // Erasure of a named item in the read-only Scope, or of the Scope.
    assert_read_only(caller.erase_episode(t, r.episode).await, "Erase Episode");
    assert_read_only(caller.erase_node(t, r.account).await, "erasing a Node");
    assert_read_only(caller.erase_edge(t, r.edge).await, "erasing an Edge");
    assert_read_only(
        caller.erase_scope(t, &Scope::new(READ_ONLY)).await,
        "erasing the Scope",
    );

    assert_eq!(
        read_only_rows(w).await,
        before,
        "the read-only Scope changed"
    );
    // The caller still reads it.
    assert!(
        caller
            .read_node(t, r.account, AsOf::current(at(2025, 1, 1)))
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn an_edge_and_a_summary_in_a_writable_scope_may_refer_to_a_node_in_a_read_only_one() {
    let r = read_only().await;
    let w = &r.w;
    let t = w.tenant;
    let before = read_only_rows(w).await;
    let caller = restricted(w);
    let (edge, written) = caller
        .create_edge(
            &NewEdge {
                tenant: Some(t),
                scope: Scope::new(SCOPE),
                kind: "owns".into(),
                source: r.account,
                target: r.writable_company,
            },
            &Patch::new(
                r.writable_episode,
                ValidTime::UNKNOWN,
                values(json!({ "share": 5 })),
            ),
        )
        .await
        .unwrap();
    assert_eq!(written.opened.len(), 1);
    caller
        .write_edge(
            t,
            edge,
            &Patch::new(
                r.writable_episode,
                ValidTime::UNKNOWN,
                values(json!({ "share": 6 })),
            ),
        )
        .await
        .unwrap();
    let summary = caller
        .write_summary(
            t,
            &NewSummary {
                scope: Scope::new(SCOPE),
                node: r.account,
                content: "Support's view of the gold account.".into(),
                as_of_valid: at(2024, 6, 1),
                citations: vec![DerivedCitation::of(r.writable_episode)],
            },
        )
        .await
        .unwrap();
    let summaries = caller
        .read_summaries(t, r.account, AsOf::current(at(2025, 1, 1)))
        .await
        .unwrap();
    assert!(summaries.iter().any(|s| s.id == summary), "{summaries:?}");
    assert_eq!(
        read_only_rows(w).await,
        before,
        "the read-only Scope changed"
    );
}

#[tokio::test]
async fn an_erasure_of_a_writable_node_removes_what_depends_on_it_in_a_scope_the_caller_cannot_write()
 {
    let r = read_only().await;
    let w = &r.w;
    let t = w.tenant;
    // An Edge of the read-only Scope to the writable company, and a Summary
    // of the company there, written by a user who may write both.
    let joining = w
        .edge_in(
            READ_ONLY,
            r.account,
            r.writable_company,
            r.episode,
            json!({ "share": 3 }),
        )
        .await;
    let summary = w
        .seeing(&[SCOPE, READ_ONLY])
        .write_summary(
            t,
            &NewSummary {
                scope: Scope::new(READ_ONLY),
                node: r.writable_company,
                content: "Billing's view of Initech.".into(),
                as_of_valid: at(2024, 6, 1),
                citations: vec![DerivedCitation::of(r.episode)],
            },
        )
        .await
        .unwrap();
    let erased = restricted(w)
        .erase_node(t, r.writable_company)
        .await
        .unwrap();
    assert_eq!(erased.erased_nodes, vec![r.writable_company]);
    assert_eq!(erased.erased_edges, vec![joining]);
    assert!(!w.exists("edge", joining.0).await, "the joining Edge stays");
    assert!(!w.exists("summary", summary.0).await, "the Summary stays");
    // What does not depend on the company stays.
    assert!(w.exists("edge", r.edge.0).await);
    assert!(w.exists("node", r.account.0).await);
}

/// A deployment whose Scopes are rows of a membership table of its own:
/// each user's rows name the Scopes they may read, and whether they may
/// write each. A definer function of the deployment's reads the table, so
/// no gnomon role needs a right on it, and it answers for the worker's
/// claims, which it tells by their `role`.
const MEMBERSHIP: &str = "
create schema membership;
create table membership.member (
  user_id text not null,
  scope text not null,
  writable boolean not null,
  primary key (user_id, scope)
);
create function membership.scopes(p_writable boolean) returns text[]
  language plpgsql stable security definer set search_path = pg_catalog, pg_temp
  as $$
  begin
    return (
      select coalesce(pg_catalog.array_agg(m.scope order by m.scope), '{}')
      from membership.member m
      where m.user_id = nullif(pg_catalog.current_setting('request.jwt.claims', true), '')::jsonb
                          ->> 'sub'
        and (m.writable or not p_writable));
  end
  $$;
revoke all on function membership.scopes(boolean) from public;
grant usage on schema membership
  to authenticated, gnomon_worker, gnomon_writer, gnomon_definer, gnomon_eraser;
grant execute on function membership.scopes(boolean)
  to authenticated, gnomon_worker, gnomon_writer, gnomon_definer, gnomon_eraser;
create or replace function gnomon.allowed_scopes() returns text[]
  language plpgsql stable parallel safe security invoker set search_path = pg_catalog, pg_temp
  as $$
  declare
    c jsonb := nullif(pg_catalog.current_setting('request.jwt.claims', true), '')::jsonb;
  begin
    if c ->> 'role' = 'gnomon_worker' then
      return array(select pg_catalog.jsonb_array_elements_text(c -> 'app_metadata' -> 'scopes'));
    end if;
    return membership.scopes(false);
  end
  $$;
create or replace function gnomon.writable_scopes() returns text[]
  language plpgsql stable parallel safe security invoker set search_path = pg_catalog, pg_temp
  as $$
  declare
    c jsonb := nullif(pg_catalog.current_setting('request.jwt.claims', true), '')::jsonb;
  begin
    if c ->> 'role' = 'gnomon_worker' then
      return gnomon.allowed_scopes();
    end if;
    return membership.scopes(true);
  end
  $$;
insert into membership.member values
  ('member', 'support', true),
  ('member', 'billing', false);
";

#[tokio::test]
async fn a_deployments_functions_that_read_a_membership_table_work_for_reads_writes_and_erasure() {
    let w = World::new().await;
    w.db.operator()
        .await
        .batch_execute(MEMBERSHIP)
        .await
        .unwrap();
    w.db.grant_erasure("authenticated").await;
    let billing = w
        .episode_in(READ_ONLY, at(2024, 1, 1), "billing's Episode")
        .await;
    let support = w
        .episode_in(SCOPE, at(2024, 1, 2), "support's Episode")
        .await;
    let member = w.store.as_user(values(json!({
        "sub": "member",
        "role": "authenticated",
        "app_metadata": { "tenant_id": w.tenant.0.to_string() }
    })));
    let t = w.tenant;

    // Reads follow the table: both Scopes.
    for episode in [billing, support] {
        assert!(
            member.read_episode(t, episode).await.unwrap().is_some(),
            "the member reads {episode:?}"
        );
    }
    // Writes follow it too: support alone.
    let written = member
        .record_episode(&NewEpisode {
            tenant: Some(t),
            scope: Scope::new(SCOPE),
            content: "the member's Episode".into(),
            reference_time: at(2024, 3, 1),
            authority: None,
        })
        .await
        .unwrap();
    let node = member
        .create_node(
            &NewNode {
                tenant: Some(t),
                scope: Scope::new(SCOPE),
                kind: "company".into(),
            },
            &Patch::new(
                written,
                ValidTime::UNKNOWN,
                values(json!({ "name": "Hooli" })),
            ),
        )
        .await
        .unwrap()
        .0;
    assert_read_only(
        member
            .record_episode(&NewEpisode {
                tenant: Some(t),
                scope: Scope::new(READ_ONLY),
                content: "the member's billing Episode".into(),
                reference_time: at(2024, 3, 1),
                authority: None,
            })
            .await,
        "the member recording in billing",
    );
    // And so does Erasure.
    assert_read_only(
        member.erase_episode(t, billing).await,
        "the member erasing billing's Episode",
    );
    let erased = member.erase_node(t, node).await.unwrap();
    assert_eq!(erased.erased_nodes, vec![node]);
    member.erase_episode(t, support).await.unwrap();
    assert!(member.read_episode(t, support).await.unwrap().is_none());

    // A user with no row reads and writes nothing, and a worker still reads
    // and writes its own Scope from its claims.
    let stranger = w.store.as_user(values(json!({
        "sub": "stranger",
        "role": "authenticated",
        "app_metadata": { "tenant_id": w.tenant.0.to_string() }
    })));
    assert!(stranger.read_episode(t, billing).await.unwrap().is_none());
    assert!(
        w.worker_in(READ_ONLY)
            .read_episode(t, billing)
            .await
            .unwrap()
            .is_some()
    );
    w.episode_in(READ_ONLY, at(2024, 4, 1), "the worker's Episode")
        .await;
}
