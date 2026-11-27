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
- The full-text branch bounds the work it spends on common words. This is planned, not built (#87).
  - It prunes by max-score first. A word's BM25 contribution is at most its idf times (k1 + 1), so a text that holds only common words is skipped when that bound is below the k-th score so far. The top k is identical to unpruned BM25.
  - Before scoring, it sums the document frequencies of the query words over the caller's allowed Scopes. If more than M token rows would be read, it falls back to a cutoff: it drops each word that is in more than X% of the caller's visible texts. If every query word is above X, it keeps the rarest one.
  - X and M are constants in the migrations, set by measurement. They are not parameters of Search.
  - A Search result does not say whether the cutoff applied.

We chose this because, under RLS, only a leakproof condition can be an index condition (ADR-0004). A vector index scan applies Tenant, Scope and As-of only to the rows it returns. Prototype 1 found HNSW recall@10 of 0.13 for a 100-row Tenant in a 1M-row table. GIN on `tsvector` is not used at all, because `@@` is not leakproof. One plan shape gives exact recall for every Tenant and Scope on core Postgres with pgvector alone, which Supabase and DigitalOcean require: neither offers VectorChord or a BM25 extension. Background and sources are in `docs/research/search.md`.

## Considered options

- HNSW with iterative scans, routed by visible-set size to an exact path for small Tenants. That means two plan shapes, and recall that falls as the table grows.
- A table hash-partitioned by Tenant. The partition count is fixed in the migration chain (ADR-0006), recall falls as partitions grow, and a small Scope inside a large Tenant is still post-filtered.
- Extension fast paths: pgvectorscale labels, VectorChord, AlloyDB ScaNN, pg_search. Each needs its own conformance suite, and on Supabase and DigitalOcean they buy little or nothing.
- A `SECURITY DEFINER` full-text function using GIN. A definer owned by the tables' owner or by a `BYPASSRLS` role escapes row-level security, so its own Tenant check would be the only barrier between Tenants on that call. A definer that stays under row-level security, as gnomon's do (ADR-0009), gains nothing: `@@` is not leakproof, so it still cannot be an index condition.
- BM25 statistics per Tenant. Ranking would reveal how often a term appears in Scopes the caller cannot read.
- An embedding per Node with a time filter. A past read would rank a Node by what it says now.
- `halfvec`. Measured no faster than full vectors, at 7.1 GB more storage.
- A document-frequency cutoff alone, with no max-score pruning. It is simple and bounded, but it changes ranking for every query that holds a common word, including questions whose rare words would have decided the top k.
- Scoring only the rarest N query words. It is bounded, and it changes ranking for long queries.
- A stop-word list. It depends on the language, and the token table uses the `simple` configuration so that every language splits the same way.
- A looser budget for natural-language questions. `context` sends them on every call, so the budget would no longer measure the normal case.

## Consequences

- Measured on 1.8M embeddings, the whole fused Search took 66 ms at p95 for 50,000 visible rows and 167 to 204 ms for 200,000. The budget is 100 ms at p95 up to 50,000 visible rows and 250 ms up to 200,000.
- That measurement used a query word found in one text. With realistic word frequencies, a natural-language question took 526 ms at p95 for 50,000 visible rows and 1,820 ms for 200,000 (#87). The budgets apply to every query class of the performance suite, natural-language questions included.
  - The max-score path is tested against an unpruned BM25 function kept for tests only: the top k must match in IDs, scores and order.
  - The cutoff path changes ranking on purpose. Its recall@10 against unpruned BM25, over the full-text branch alone, must be at least 0.9. That floor is lowered only after a measurement is reported.
  - If the budgets are still missed with both paths, the next option is a decision of its own, not a wider cutoff or a looser budget. The `SECURITY DEFINER` function using GIN, judged against the threat model of #81, is the option to revisit.
  - Resolution's full-text half keeps exact BM25 with no cutoff, because a missed candidate can create a duplicate Node. If it misses a budget, max-score pruning is the only allowed change.
  - The values of X and M, and the timings with them, are to be added by #87.
- Larger visible sets need an approximate path: a static partial HNSW index on current Versions with an iterative scan. It is planned, not built. Running the three branches concurrently on separate connections may be enough first, and is to be measured before HNSW is added.
- The pgvector floor is 0.7.0. It becomes 0.8.0 if the HNSW path is added.
- Embeddings arrive after the write commits. The Pipeline adds them, and the token table covers the gap because it is written in the write's own transaction.
- Resolution searches a current-only partial index of Node names and Aliases.
- Edge Versions are searched through their Sentence. Summaries and Communities are searched. Episodes are not searched; they are reached through Citations.
- The embedding dimension is stated once, in `gnomon.embedding_dimension()`, which a consumer edits before applying the files. See the amendment below.
- There is no adjacency table. The neighbourhood branch reads `gnomon.edge` and `gnomon.version` under RLS, through the `edge_source` and `edge_target` indexes on `(tenant_id, source_id, scope)` and `(tenant_id, target_id, scope)`, and the fused Search meets the budgets above with it. An adjacency table with one row per Edge Version and direction was planned and not built. Issue #72 tracks the measurement of the branch on hub Nodes.
- Neighbourhood expansion defaults to depth 1, with a maximum of 2. It keeps at most 50 Edges per Node, choosing the most-mentioned at the As-of instants, with Edge ID as the tie-break.
- Mentions and connectedness are two more fused lists. Mentions counts the Episodes citing a candidate. Connectedness counts its Edges.
  - Both are computed at search time over the visible rows. Stored counters would leak across Scopes and be wrong for past reads.
  - They only reorder candidates the other branches found, so a hub cannot enter on popularity alone.
  - Their weights are set per query.
- Under RLS the planner estimates one row for every scan, so join shapes flip on small differences. The re-rank is pinned to primary-key probes. Unpinned, it scanned every full vector in the visible set: 660 ms against 96 ms.
- Summing BM25 counts over the allowed Scopes costs more for a caller who holds many Scopes.

## Amendment: the vector columns and a change of model (#80)

The vector columns are of type `vector` and the quantized columns `bit varying`, with no width in the type. A check on each holds it to `gnomon.embedding_dimension()`: `vector_dims(...)` for a vector, `length(...)` for its bits. That function is the one place that states the dimension, 1536 by default, and a consumer whose model has another width edits that line before applying the files. Bit variables in the functions are `bit varying`, and the functions cast to no width, because a bare `::bit` means `bit(1)`. A future HNSW index needs a typed width, so it will be an expression index with a cast.

A change of embedding model is a migration the consumer writes by hand: add a column, back-fill it with the new model's embeddings, then drop the old column. gnomon ships no files for it. A consumer copies gnomon's files into its own chain, which holds the data, so a model change belongs in that chain. When the dimension changes, the migration needs more than those three steps:

- The new column needs its own dimension check. The checks on the existing columns all call `gnomon.embedding_dimension()`, so they cannot hold two widths at once.
- The functions that read and write the vectors, such as `gnomon.embedding_vector`, `gnomon.store_embedding` and `gnomon.search_vector`, need replacements that use the new column and its width.

These two points come from reading the schema. Nobody has tried them yet. #86 moves the vectors and rewrites this part of the ADR.

