# Prototype 5: exact Search over the visible rows

## Question

Can the Store's Search use one plan shape everywhere and stay under 100 ms p95 for candidate retrieval? That plan is an exact vector search over the rows visible under RLS, with quantized vectors and a full-precision re-rank, plus a token-table full-text branch, graph signals and reciprocal rank fusion in SQL. Test it on a visible set (Tenant plus Scope) of up to 200,000 rows, on core Postgres 17 with pgvector and no other extension.

Read `docs/research/search.md` first. It is the background for every decision below. The prototype 1 findings (branch `prototype/vector-recall-rls`, `FINDINGS.md`) give the baseline: exact search with full vectors took 1.7 ms at 100 rows, 49 ms at about 6,000 rows and 1.1 s at 200,000 rows.

## Decisions this tests

These were settled in a design interview. They are not yet ADRs.

- Search honours As-of reads (valid-at and known-at) and filters by Tenant plus Scope under RLS. Store functions are invokers (ADR-0009).
- The Scope policy becomes `scope = any(gnomon.allowed_scopes())`, so a `(tenant_id, scope)` B-tree can serve both policy conditions. This amends ADR-0005's `gnomon.scope_allowed(scope)`.
- Vectors: an exact search over the visible rows, with no partitioning and no extension beyond pgvector. The pgvector floor is 0.7.0. A shared HNSW path is added only if this prototype shows the exact path misses 100 ms p95 at 200,000 visible rows.
- One embedding per Version, shared through a hash of the embedded text by Versions whose text is the same. A separate current-only index (`WHERE recorded_to = 'infinity'`) holds Node names and Aliases for Resolution. Edge Versions are searched through their Sentence. Summaries and Communities are searched, and Episodes are not.
- Full-text search uses a token table, `(tenant_id, scope, token, ...)`. It is maintained by trigger in the write's transaction and looked up by leakproof `texteq` through a B-tree, with BM25 statistics per Tenant. GIN on `tsvector` cannot be used under RLS because `@@` is not leakproof.
- The Store fuses the vector, full-text and neighbourhood candidates with reciprocal rank fusion. Two more ranked lists rank only the candidates already found:
  - Mentions: the visible Episodes citing the item at the As-of instants.
  - Connectedness: the visible Edges at those instants.

  Both are computed at search time, damped by a log, weighted per query, and on by default at a lower weight.
- Neighbourhood expansion: Nodes and Edges within n hops of the seed Nodes at the As-of instants, depth 1 by default and at most 2.
- The embedding dimension is fixed when the migrations are exported.

## Measure

Use visible sets of 1k, 10k, 50k and 200k rows, and 1M if it is cheap. The exact path reads only the visible rows, so the table can be about 2M rows across many Tenants and Scopes, not 10M. Include at least one small Scope inside a large Tenant. Report p50 and p95 for every measurement, under RLS, as a non-owner role, through an invoker Store function. Use 1,536 dimensions and clustered synthetic vectors, as prototype 1 did; uniform random vectors make recall meaningless.

1. Exact vector speed and recall. Compare full `vector`, stored `halfvec`, and stored `bit` from `binary_quantize`. Re-rank the top 100, 200 and 400 at full precision. Report recall@10 against exact full-precision search, and the storage each option needs.
2. The Scope policy. Use `EXPLAIN` to show whether `tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes())` becomes an index condition on the `(tenant_id, scope)` B-tree, under RLS, inside the invoker function. Compare it with the old `gnomon.scope_allowed(scope)` form.
3. Full-text search. Compare the token table with a scan of `tsvector` over the visible rows, at the same sizes. Measure the extra write cost per Version for the trigger, and whether per-Tenant BM25 scoring is practical in SQL.
4. As-of Search. Use per-Version embeddings shared by text hash, with the four As-of conditions, at known-at "now" and at a past known-at. Measure the current-only partial index for names and Aliases. Use HNSW for that partial index only if the exact path for names is too slow.
5. Fusion and graph signals. Run reciprocal rank fusion in SQL across the three branches plus the mentions and connectedness lists. Report the total latency, and the cost of computing the two graph signals at search time on a 200k-row visible set. If they are too slow, say what a per-(Tenant, Scope) rollup for searches at "now" would save.
6. Memory. Repeat the 200k measurement for the best variant in a container capped at 4 GiB, with Postgres settings to match. A 4 GiB managed plan is the smallest realistic host.

## Environment

- `pgvector/pgvector:pg17` on host port 55437.
- Supabase's Postgres image `supabase/postgres` at `17.11.0.018` or the closest 17.x tag, on host port 55438. Report the pgvector version on each server.
- Use no hosted database of any kind.

## Rules for every prototype

- Read `GLOSSARY.md`, every ADR in `docs/adr/` and `docs/research/search.md` first, and use the glossary's terms. The decisions above are settled. This prototype tests them, and reopens them only if the numbers force it.
- Use the `/prototype` skill. Answer exactly the question above with throwaway code. Do not build gnomon itself.
- Other work runs at the same time on this machine (48 cores, 251 GiB RAM). Run Docker containers only on the host ports listed in this brief, name them `gnomon-proto-exact-search-<role>`, and remove them when you finish. Never stop, change or remove any container you did not create. Never use a broad `pkill` or `killall`. Stop only processes you started, by PID.
- Commit only to this worktree's `prototype/exact-search` branch. Never push and never touch `main`.
- The repo is public. Committed files must not name the consuming applications or quote their code, ADRs or issues. Call them "the consuming applications".
- Never print, commit or log a secret.
- When you finish, write `FINDINGS.md` at the worktree root with:
  - the question;
  - the answer in one sentence with its numbers;
  - the method;
  - a results table;
  - what it means for each ADR and for each decision above (confirms, amends or contradicts);
  - a recommendation;
  - caveats, including any other load on the machine.

  Commit it, then tell the user you are done.
