---
status: accepted
---

# Store migrations are exported into each consumer's own migration chain

gnomon publishes numbered SQL migration files that never change once released. `gnomon migrations export --target supabase|alembic|plain` writes them into the consumer's own migration chain, and the consumer's tooling applies them. A `gnomon.schema_version` ledger lets gnomon refuse to run against a schema it does not expect. gnomon never applies DDL itself. We chose this because some consumers may only apply migrations through their platform's own pipeline, and because a library that migrates its own schema inside the tenant-isolation boundary is a reason consumers reject graph libraries.

## Considered options

- gnomon runs its own migrator at deploy. Conflicts with platforms that apply migrations only on merge.
- Distribution as a Postgres extension, including a `pg_tle` extension. Managed hosts either lack it or do not document it.

## Consequences

- All objects live in a `gnomon` Postgres schema that PostgREST does not expose. Clients reach the Store through gnomon, not through a REST layer.
- The floor is PostgreSQL 17 with pgvector and btree_gist, set by the hosts gnomon must run on (ADR-0023). The migrations avoid PG 18-only features and ship their own UUID v7 function.
