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
- The role that applies the files needs CREATE on the database, the right to create pgvector, and CREATEROLE when the gnomon roles `authenticated`, `gnomon_worker` and `gnomon_definer` do not exist yet. When the callers lack USAGE on pgvector's schema, the first file grants it, so the role must be able to. The last file hands the definer functions to `gnomon_definer`, which needs the role to act as `gnomon_definer` for that step. A role that created `gnomon_definer` holds ADMIN on it and grants itself that for the step. When another role created the gnomon roles, for example for another database on the same cluster, the last file fails with `permission denied to grant role "gnomon_definer"` until a role with ADMIN on `gnomon_definer` runs `grant gnomon_definer to <migrating role> with admin option, inherit false, set false`. `tests/install.rs` shows both.
- Grant `authenticated` and `gnomon_worker` to the role the Store logs in as. The files grant Kind registration (`gnomon.register_kind`, `gnomon.set_merge_rule`) and Erasure (`gnomon.erase_episode`, `gnomon.erase_node`, `gnomon.erase_edge`, `gnomon.erase_scope`) to no role. Grant EXECUTE on them to the roles you choose.
- An Erasure holds one advisory lock for each Node and Edge it touches, in the server's shared lock table of `max_locks_per_transaction * (max_connections + max_prepared_transactions)` entries. In a test on PostgreSQL 17 with `max_connections` 100 and the default `max_locks_per_transaction`, one transaction held 5,000 advisory locks and failed at 20,000. Raise the setting before you erase Episodes cited across many thousands of Nodes.
- A large Erasure can outlast a statement time limit on the login role: a prototype measured 24 to 35 seconds for an Episode across 500 Nodes. Set the limit to allow for it.
- The reference `gnomon.tenant_id()` trusts `request.jwt.claims`, which the Store sets for each call. So the login role, and raw SQL, must never reach end users.
- A replacement `gnomon.tenant_id()` or `gnomon.allowed_scopes()` also runs inside the definer functions, so any `search_path` it sets, or one a function it calls sets, must end in `pg_temp` (ADR-0009).

## Connecting

The Store connects as a login role of your deployment, which may switch to the `authenticated` and `gnomon_worker` roles:

```rust
let store = Store::connect(&url, &ConnectOptions::default()).await?;
```

By default the Store connects with TLS and verifies the server's certificate and host name against the platform's trusted roots. The connection string's `sslmode` cannot turn that off. A managed Postgres often signs its servers with a CA of its own. Trust that CA with `ConnectOptions::default().tls(Tls::trust_pem(ca_pem)?)`. `Tls::unverified()` encrypts without verifying the certificate, and `Tls::plaintext()` turns TLS off. Use either only where you have decided you need it.

The Store works through transaction-mode poolers such as PgBouncer and Supavisor at their default settings. Every call is one transaction that sets its role and its claims locally, and every statement is sent unnamed. Named prepared statements are an opt-in, `ConnectOptions::named_statements(true)`. Turn them on only behind a pooler that supports them: PgBouncer 1.21 or later with `max_prepared_statements` above 0, or Supavisor with named prepared statements enabled. Elsewhere calls fail.

## Development

`docs/testing.md` explains how to run the test suites. They need Docker and start their own Postgres containers.
