# gnomon

> **gno·mon** /ˈnoʊ.mɒn/ (*NOH-mon*) &nbsp; *noun*
>
> 1. The part of a sundial that casts the shadow by which the time is read.
> 2. *Geometry.* The figure left when a parallelogram is removed from one corner of a larger, similar parallelogram.
>
> *Origin:* Greek *gnōmōn*, "indicator, one who knows," from *gignōskein*, "to know."

Gnomon is a bi-temporal knowledge graph for agentic AI systems. It is built on plain Postgres and needs no dedicated graph database.

Every Version in gnomon carries two times: when it was true in the world, and when the system came to know it. An agent can therefore ask what was true at a past instant and what it believed at a past instant as two separate questions. A correction writes a new version instead of overwriting the old one. Every belief traces back to the source it came from. Tenants are isolated by Postgres row-level security.

The name fits on both counts. A gnomon tells the time by the shadow it casts, and the word comes from the Greek for "one who knows."

## Status

Early design. The data model, packaging and client languages are still being decided, and nothing is released yet.

## Installing the schema

gnomon never changes your schema itself. Its schema is six plain SQL files in `crates/gnomon/migrations`, `0001_schema.sql` to `0006_grants.sql`. Copy them into your own migration chain, and your tooling applies them like any other migration, in order, each in one transaction. Every gnomon object lives in the `gnomon` schema. Keep that schema out of PostgREST's exposed schemas, so clients reach the Store only through gnomon.

The files state the embedding dimension once, in `gnomon.embedding_dimension()` in `0001_schema.sql`: `as $$ select 1536 $$`. If your embedding model has another width, edit that number before you apply the files. Change nothing else.

### With psql

```sh
for f in migrations/gnomon/*.sql; do
  psql -v ON_ERROR_STOP=1 --single-transaction -f "$f" "$DATABASE_URL"
done
```

### With Alembic

Copy the files into a `gnomon/` directory beside your `versions/` directory, and add one revision that runs them. `no_parameters` sends the SQL to the driver as written, so neither Alembic nor the driver reads a `%` or a `:` in it as a placeholder.

```python
"""gnomon's migration files."""

from pathlib import Path

from alembic import op

revision = "gnomon_0006"
down_revision = None  # your chain's head
branch_labels = None
depends_on = None

GNOMON = Path(__file__).resolve().parent.parent / "gnomon"
FILES = [
    "0001_schema.sql",
    "0002_kinds.sql",
    "0003_reads_and_writes.sql",
    "0004_search.sql",
    "0005_erasure.sql",
    "0006_grants.sql",
]


def upgrade() -> None:
    for name in FILES:
        sql = (GNOMON / name).read_text(encoding="utf-8")
        op.get_bind().exec_driver_sql(sql, execution_options={"no_parameters": True})


def downgrade() -> None:
    raise NotImplementedError("gnomon's migrations have no downgrade")
```

### With the Supabase CLI

Copy each file into `supabase/migrations` as `<timestamp>_gnomon_<name>.sql`, with timestamps after your own latest migration and in the files' order, such as `20250101000001_gnomon_0001_schema.sql`. Supabase has an `extensions` schema, which PostgREST does not expose, and the first file creates pgvector and btree_gist there.

### Upgrading

A file never changes once it is released. A later release adds files with higher numbers. Copy only the new ones: with psql, apply them in order; with Alembic, add a revision that runs them after your head; with the Supabase CLI, give them timestamps after your latest migration. The Store refuses to run against a schema whose `gnomon.schema_version` is not the one it expects.

### What the database needs

- The floor is PostgreSQL 17, pgvector 0.7.0 or later, and btree_gist (ADR-0023). The first file creates each extension when it is missing, in the `extensions` schema when that schema exists and in the migrating role's default schema otherwise. btree_gist is a trusted extension, so a role with CREATE on the database may create it. On Azure Flexible Server, add `vector` and `btree_gist` to `azure.extensions` first.
- The functions that use pgvector keep a search path of `pg_catalog`, pgvector's schema and `pg_temp`, set when the files run. So no untrusted role may have CREATE on pgvector's schema, and moving the extension to another schema later breaks those functions.
- The role that applies the files needs CREATE on the database, the right to create pgvector, and CREATEROLE when the gnomon roles `authenticated`, `gnomon_worker`, `gnomon_definer` and `gnomon_writer` do not exist yet. `gnomon_definer` and `gnomon_writer` are NOLOGIN and NOINHERIT, cannot bypass row-level security and are members of no role, and no caller role may be a member of either. The first file refuses a role of either name that holds more, and neither may own a table, so that row-level security holds it to its policies. When the callers or the definer roles lack USAGE on pgvector's schema, the first file grants it, so the role must be able to. The last file hands Erasure, Kind registration, the merge-rule setter and the search index trigger to `gnomon_definer`, and every write function to `gnomon_writer`, which needs the role to act as each for that step. A role that created them holds ADMIN on each and grants itself that for the step. When another role created the gnomon roles, for example for another database on the same cluster, the last file fails with `permission denied to grant role "gnomon_definer"` (or `"gnomon_writer"`) until a role with ADMIN on both runs `grant gnomon_definer, gnomon_writer to <migrating role> with admin option, inherit false, set false`. `tests/install.rs` shows both.
- Grant `authenticated` and `gnomon_worker` to the role the Store logs in as. The files grant Kind registration (`gnomon.register_kind`, `gnomon.set_merge_rule`) and Erasure (`gnomon.erase_episode`, `gnomon.erase_node`, `gnomon.erase_edge`, `gnomon.erase_scope`) to no role. Grant EXECUTE on them to the roles you choose.
- An Erasure holds one advisory lock for each Node and Edge it touches, in the server's shared lock table of `max_locks_per_transaction * (max_connections + max_prepared_transactions)` entries. In a test on PostgreSQL 17 with `max_connections` 100 and the default `max_locks_per_transaction`, one transaction held 5,000 advisory locks and failed at 20,000. Raise the setting before you erase Episodes cited across many thousands of Nodes.
- A large Erasure can outlast a statement time limit on the login role: a prototype measured 24 to 35 seconds for an Episode across 500 Nodes. Set the limit to allow for it.
- To switch a deployment to multi-tenant mode as a role other than the one that applied the files, grant it EXECUTE on `gnomon.switch_to_multi_tenant()`, SELECT on `gnomon.tenancy` and UPDATE (mode, switched_at) on it.

### Claims, Scopes and who recorded an Episode

The database trusts the claims in `request.jwt.claims`, which the Store sets for each call from what the application passes it. The Store does not verify them: the application verifies the token first and passes `Store::as_user` the claims it verified. So the login role, and raw SQL, must never reach end users.

Callers hold no write privilege on any gnomon table. Every write goes through a Store function owned by `gnomon_writer`, which runs under its own row-level security policies (ADR-0009, ADR-0024). Four functions decide what a call may reach. The reference versions read these claims:

| Function | Returns | Reads |
|---|---|---|
| `gnomon.tenant_id()` | the call's Tenant | `app_metadata.tenant_id`, or the default Tenant in single-tenant mode |
| `gnomon.allowed_scopes()` | the Scopes the caller may read | `app_metadata.scopes` |
| `gnomon.writable_scopes()` | the Scopes the caller may write | the same Scopes as `gnomon.allowed_scopes()` |
| `gnomon.user_id()` | who records an Episode or its Retraction | `sub`, or null when it is missing or empty |

A token without `app_metadata.tenant_id` and `app_metadata.scopes` therefore reaches nothing until you add those claims or replace the functions. A write needs its Scope in both lists, so a caller may read a Scope it may not write: every write to it is refused with `insufficient_privilege` and the message "Scope ... is read-only for the caller". A row in a writable Scope may still refer to one there, such as an Edge to a Node of a read-only Scope or a Summary of it. Erasure needs the named Episode's, Node's or Edge's Scope, or the named Scope, to be writable, and still removes what depends on it in every Scope.

A background job acts as a worker for one Tenant and Scope. `Store::as_worker(tenant, scope, user)` sets the claims `{"role": "gnomon_worker", "sub": <user>, "app_metadata": {"tenant_id": <tenant>, "scopes": [<scope>]}}`, with no `sub` when `user` is `None`. Pass the user a job works for, such as the person whose upload it imports, and the Episodes it records name them in `recorded_by`. Without one, `recorded_by` is null, and gnomon still records the Episode.

`Caller::read_episode` returns `recorded_by`, and for a retracted Episode when it was retracted and by whom. Whoever can read the Episode can read both, and erasing it removes both. Each edit by a person is an Episode of its own, so a Version, Summary or Community leads to who stated it through its Citations, and a write that cites an older Episode shows who recorded that one. gnomon records nothing else about who made a write: not who ran an Erasure, registered a Kind or set a merge rule. Your application logs what it needs (ADR-0003).

To resolve any of the four your own way, replace it with `create or replace function` after the files run, keeping its signature. These rules apply to all four, because each runs as the caller during a read, as `gnomon_writer` during a write and as `gnomon_definer` during an Erasure, Kind registration, the merge-rule setter and the search index trigger:

- Read only the claims, or reach other data through a `SECURITY DEFINER` function of your own.
- Do not depend on `current_user`.
- Grant whatever outside gnomon it calls to `authenticated`, `gnomon_worker`, `gnomon_writer` and `gnomon_definer`.
- Answer for worker claims too. Tell a worker call by its `role` claim, because a worker call may carry `sub`.
- End every `search_path` it sets, or that a function it calls sets, in `pg_temp` (ADR-0009).

For example, a deployment that keeps each user's Scopes in a membership table of its own:

```sql
create schema membership;
create table membership.member (
  user_id text not null,
  scope text not null,
  writable boolean not null,
  primary key (user_id, scope)
);

create function membership.scopes(p_writable boolean) returns text[]
  language sql stable security definer set search_path = pg_catalog, pg_temp
  as $$
    select coalesce(pg_catalog.array_agg(m.scope order by m.scope), '{}')
    from membership.member m
    where m.user_id = nullif(pg_catalog.current_setting('request.jwt.claims', true), '')::jsonb
                        ->> 'sub'
      and (m.writable or not p_writable)
  $$;
revoke all on function membership.scopes(boolean) from public;
grant usage on schema membership to authenticated, gnomon_worker, gnomon_writer, gnomon_definer;
grant execute on function membership.scopes(boolean)
  to authenticated, gnomon_worker, gnomon_writer, gnomon_definer;

create or replace function gnomon.allowed_scopes() returns text[]
  language sql stable parallel safe security invoker set search_path = pg_catalog, pg_temp
  as $$
    select case
      when c ->> 'role' = 'gnomon_worker' then
        array(select pg_catalog.jsonb_array_elements_text(c -> 'app_metadata' -> 'scopes'))
      else membership.scopes(false)
    end
    from (select nullif(pg_catalog.current_setting('request.jwt.claims', true), '')::jsonb as c) x
  $$;

create or replace function gnomon.writable_scopes() returns text[]
  language sql stable parallel safe security invoker set search_path = pg_catalog, pg_temp
  as $$
    select case
      when c ->> 'role' = 'gnomon_worker' then gnomon.allowed_scopes()
      else membership.scopes(true)
    end
    from (select nullif(pg_catalog.current_setting('request.jwt.claims', true), '')::jsonb as c) x
  $$;
```

`tests/writable_scopes.rs` runs this example for reads, writes and Erasure. The Scope rule must stay a set of keys, so the policies keep their index conditions (ADR-0005).


## Connecting

The Store connects as a login role of your deployment, which may switch to the `authenticated` and `gnomon_worker` roles:

```rust
let store = Store::connect(&url, &ConnectOptions::default()).await?;
store.check().await?; // optional: fail at startup if the schema is not the one this build expects
```

`Store::connect` opens no connection. The Store keeps a small pool: it opens a connection when a call needs one and none is free, keeps at most 10 by default (`ConnectOptions::max_connections`), and runs no background task. A call waits at most 30 seconds for a free connection (`wait_timeout`) and then fails with `Error::Unavailable`. `idle_timeout` closes a connection that sat idle longer than it, when a call would take it next. The first connection checks the schema version and the embedding dimension, and `Store::check` runs that check on its own.

By default the Store connects with TLS and verifies the server's certificate and host name against the platform's trusted roots. The connection string's `sslmode` cannot turn that off, and `sslmode=verify-full` asks for what the Store does already. A managed Postgres often signs its servers with a CA of its own. Trust that CA with `ConnectOptions::default().tls(Tls::trust_pem(ca_pem)?)`. `Tls::unverified()` encrypts without verifying the certificate, and `Tls::plaintext()` turns TLS off. Use either only where you have decided you need it.

The Store works through transaction-mode poolers such as PgBouncer and Supavisor at their default settings. Every call is one transaction that sets its role and its claims locally, and every statement is sent unnamed, so the Store needs no pooler setting for prepared statements.

Each call on a `Caller` is a transaction of its own. To make several calls in one, start a transaction:

```rust
let worker = store.as_worker(tenant, Scope::new("ingest"), None);
let mut tx = worker.transaction(tenant).await?;
tx.store_sentence(version, &sentence).await?;
tx.write_stages(&[finish]).await?;
tx.commit().await?;
```

Nothing in it is saved until `commit` succeeds. Dropping it saves nothing, and `commit` fails once a call in it has failed. It holds one pooled connection until it ends, so make model calls before you start it.

## Development

`docs/testing.md` explains how to run the test suites. They need Docker and start their own Postgres containers.
