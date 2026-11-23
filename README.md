# gnomon

> **gno·mon** /ˈnoʊ.mɒn/ (*NOH-mon*) &nbsp; *noun*
>
> 1. The part of a sundial that casts the shadow by which the time is read.
> 2. *Geometry.* The figure left when a parallelogram is removed from one corner of a larger, similar parallelogram.
>
> *Origin:* Greek *gnōmōn*, "indicator, one who knows," from *gignōskein*, "to know."

Gnomon is a bi-temporal knowledge graph for agentic AI systems. It is built on plain Postgres and needs no dedicated graph database.

Every fact in gnomon carries two times: when it was true in the world, and when the system came to know it. An agent can therefore ask what was true at a past instant and what it believed at a past instant as two separate questions. A correction writes a new version instead of overwriting the old one. Every belief traces back to the source it came from. Tenants are isolated by Postgres row-level security.

The name fits on both counts. A gnomon tells the time by the shadow it casts, and the word comes from the Greek for "one who knows."

## Status

Early design. The data model, packaging and client languages are still being decided, and there is nothing to install yet.

## Exporting the migrations

gnomon never changes your schema itself. You export its numbered migrations into your own migration chain, and your tooling applies them like any other migration.

```sh
gnomon migrations export --target supabase --embedding-dimension 1536 --out supabase/migrations
gnomon migrations export --target alembic  --embedding-dimension 1536 --out alembic/versions
gnomon migrations export --target plain    --embedding-dimension 1536 --out migrations/gnomon
```

- `supabase` writes `<timestamp>_gnomon_<name>.sql` files for the Supabase CLI. It creates pgvector in the `extensions` schema, which PostgREST does not expose.
- `alembic` writes `gnomon_<name>.py` revisions. The first follows your chain's head, and each later one follows the one before. A chain with several heads is refused: merge them first.
- `plain` writes `<number>_<name>.sql` files to apply in name order, each in one transaction.

Run the same command again after you upgrade gnomon. It adds only the new migrations, after everything already in your chain, and never rewrites one it exported before. It refuses to run if a released migration in your chain differs from what this release would write, for example because it was exported with another embedding dimension.

Every gnomon object lives in the `gnomon` schema. Keep that schema out of PostgREST's exposed schemas, so clients reach the Store only through gnomon.

## Development

`docs/testing.md` explains how to run the test suites. They need Docker and start their own Postgres containers.
