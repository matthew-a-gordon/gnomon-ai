---
status: accepted
---

# Store migrations are plain SQL files that each consumer copies into its own migration chain

gnomon publishes its schema as plain SQL files in `crates/gnomon/migrations`, `NNNN_name.sql`, which never change once released. Each defines its objects once, in their final form, and inserts its own row into a `gnomon.schema_version` ledger, which lets gnomon refuse to run against a schema it does not expect. A consumer copies the files into its own migration chain, and its tooling applies them in order, each in one transaction: psql, an Alembic revision that runs them, or the Supabase CLI under timestamped names. An upgrade copies only the new files. gnomon never applies DDL itself. We chose this because some consumers may only apply migrations through their platform's own pipeline, and because a library that migrates its own schema inside the tenant-isolation boundary is a reason consumers reject graph libraries.

## Considered options

- gnomon runs its own migrator at deploy. Conflicts with platforms that apply migrations only on merge.
- Distribution as a Postgres extension, including a `pg_tle` extension. Managed hosts either lack it or do not document it.
- A `gnomon migrations export` command with plain, Supabase and Alembic targets, which rendered the files into each chain's format with the embedding dimension and pgvector's schema filled in. It was built and removed in #80. It needed SQL held in string literals so that pgvector's schema could be written in at apply time, a placeholder for the dimension in 17 places, and copies of the output checked by tests, and it carried every earlier migration's history: 25 functions redefined 36 times across 20 files. Plain files are what every tool already applies.

## Consequences

- All objects live in a `gnomon` Postgres schema that PostgREST does not expose. Clients reach the Store through gnomon, not through a REST layer.
- The floor is PostgreSQL 17 with pgvector and btree_gist, set by the hosts gnomon must run on (ADR-0023). The files avoid PG 18-only features and ship their own UUID v7 function. The first file creates both extensions when they are missing, in `extensions` when that schema exists, as on Supabase.
- The SQL is static. The functions that use pgvector name its types and operators without a schema, under a `search_path` the file sets and the functions keep (ADR-0009), so the files work whether pgvector is in `public` or in `extensions`.
- The embedding dimension is one line, `gnomon.embedding_dimension()` in the first file, which a consumer edits before applying the files (ADR-0010).
- The files are numbered by subject: the schema, Kinds, reads and writes, Search, Erasure, and grants. The grants come last, in one file with one revoke block, so a chain applied up to an earlier file grants callers nothing.
- The files become unchangeable at the first tagged release. Until then they may be rewritten, because no database that must be kept has applied them.
- The Rust crate embeds the files in `gnomon::migrations::MIGRATIONS`, with `SCHEMA_VERSION`, for its tests and for later bindings.
