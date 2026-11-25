---
status: accepted
---

# Search is exact over the visible rows

The Store's Search returns candidates from three branches, each an invoker function under RLS at the As-of instants: vectors, full text and graph neighbourhood. It fuses them with reciprocal rank fusion, and the Pipeline reranks the fused list with its reranker model.

- The vector branch is exact over the rows the caller can see.
  - Each Version has a `bit` row from `binary_quantize`, carrying the four time columns, in a covering B-tree on `(tenant_id, scope)`.
  - The full vector is stored once per distinct text, keyed by a hash of that text.
  - The top 400 by bit distance are re-ranked at full precision.
- The full-text branch reads a token table maintained by trigger in the write's transaction, through a B-tree on leakproof equality. Its BM25 counts are kept per Scope and summed over the caller's allowed Scopes.

We chose this because, under RLS, only a leakproof condition can be an index condition (ADR-0004). A vector index scan applies Tenant, Scope and As-of only to the rows it returns. Prototype 1 found HNSW recall@10 of 0.13 for a 100-row Tenant in a 1M-row table. GIN on `tsvector` is not used at all, because `@@` is not leakproof. One plan shape gives exact recall for every Tenant and Scope on core Postgres with pgvector alone, which Supabase and DigitalOcean require: neither offers VectorChord or a BM25 extension. Background and sources are in `docs/research/search.md`.

## Considered options

- HNSW with iterative scans, routed by visible-set size to an exact path for small Tenants. That means two plan shapes, and recall that falls as the table grows.
- A table hash-partitioned by Tenant. The partition count is fixed in the migration chain (ADR-0006), recall falls as partitions grow, and a small Scope inside a large Tenant is still post-filtered.
- Extension fast paths: pgvectorscale labels, VectorChord, AlloyDB ScaNN, pg_search. Each needs its own conformance suite, and on Supabase and DigitalOcean they buy little or nothing.
- A `SECURITY DEFINER` full-text function using GIN. RLS would stop being the barrier between Tenants on that call (ADR-0009).
- BM25 statistics per Tenant. Ranking would reveal how often a term appears in Scopes the caller cannot read.
- An embedding per Node with a time filter. A past read would rank a Node by what it says now.
- `halfvec`. Measured no faster than full vectors, at 7.1 GB more storage.

## Consequences

- Measured on 1.8M embeddings, the whole fused Search took 66 ms at p95 for 50,000 visible rows and 167 to 204 ms for 200,000. The budget is 100 ms at p95 up to 50,000 visible rows and 250 ms up to 200,000.
- Larger visible sets need an approximate path: a static partial HNSW index on current Versions with an iterative scan. It is planned, not built. Running the three branches concurrently on separate connections may be enough first, and is to be measured before HNSW is added.
- The pgvector floor is 0.7.0. It becomes 0.8.0 if the HNSW path is added.
- Embeddings arrive after the write commits. The Pipeline adds them, and the token table covers the gap because it is written in the write's own transaction.
- Resolution searches a current-only partial index of Node names and Aliases.
- Edge Versions are searched through their Sentence. Summaries and Communities are searched. Episodes are not searched; they are reached through Citations.
- The embedding dimension is fixed when the migrations are exported. Changing the model means a migration adds a column, the Pipeline back-fills it, and a later migration drops the old column.
- There is no adjacency table. The neighbourhood branch reads `gnomon.edge` and `gnomon.version` under RLS, through the `edge_source` and `edge_target` indexes on `(tenant_id, source_id, scope)` and `(tenant_id, target_id, scope)`, and the fused Search meets the budgets above with it. An adjacency table with one row per Edge Version and direction was planned and not built. Issue #72 tracks the measurement of the branch on hub Nodes.
- Neighbourhood expansion defaults to depth 1, with a maximum of 2. It keeps at most 50 Edges per Node, choosing the most-mentioned at the As-of instants, with Edge ID as the tie-break.
- Mentions and connectedness are two more fused lists. Mentions counts the Episodes citing a candidate. Connectedness counts its Edges.
  - Both are computed at search time over the visible rows. Stored counters would leak across Scopes and be wrong for past reads.
  - They only reorder candidates the other branches found, so a hub cannot enter on popularity alone.
  - Their weights are set per query.
- Under RLS the planner estimates one row for every scan, so join shapes flip on small differences. The re-rank is pinned to primary-key probes. Unpinned, it scanned every full vector in the visible set: 660 ms against 96 ms.
- Summing BM25 counts over the allowed Scopes costs more for a caller who holds many Scopes.
