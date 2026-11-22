# Findings: vector recall for a small Tenant under RLS

Prototype 1. Throwaway code is in `prototypes/vector-recall-rls/`. Raw numbers are in `results-pgvector.jsonl` and `results-supabase.jsonl` there. `python3 tables.py` rebuilds every table.

## Question

Does pgvector return good recall for a small Tenant in a large shared embedding table when every query runs under RLS? If not, what does the Store's search need?

## Answer

No: with plain HNSW at the default `hnsw.ef_search` of 40, a 100-row Tenant in a 1,004,200-row table gets recall@10 of 0.13 (1.4 of 10 rows returned), a 1,000-row Tenant gets 0.63, and the large Tenant gets 0.87. The Store's search needs a path chosen per Tenant: an exact scan through the Tenant B-tree for Tenants up to a few thousand rows (recall 1.00, 1.7 ms p50 at 100 rows, 7.9 ms at 1,000, 49 ms at about 6,000), and for larger Tenants HNSW with `hnsw.iterative_scan = relaxed_order` on a table hash-partitioned by Tenant (recall 0.99 to 1.00 for every Tenant up to 6,000 rows, p50 about 5 ms, p95 under 8 ms, both servers).

## Method

Data. One table `gnomon.embedding(id, tenant_id, scope, episode_id, embedding vector(1536))`, so one table, a Tenant on every row and a Citation (the Episode link) on every embedding. It holds 1,004,200 rows across 2,010 Tenants:

- 1 large Tenant with 200,000 rows.
- 2,000 medium Tenants with 800,000 rows in total, from 50 to about 6,000 rows each.
- 9 small Tenants: three each of 100, 300 and 1,000 rows.

Vectors are clustered synthetic, 1,536 dimensions (not a public dataset, not reduced dimensions). 300 topic clusters live in a 48-dimension latent space and are projected to 1,536 dimensions with an orthonormal map. Each Tenant draws 3 to 5 topics (the large Tenant 40), gets its own offset and about 15-row sub-clusters, and every row gets noise plus a little ambient noise before unit normalisation. Heap order is interleaved across Tenants, so a Tenant's rows are scattered over the heap as they would be in production. A first dataset was too flat (top-10 cosine distances 0.060 to 0.074 apart), so recall meant little. It was discarded and its results are kept in `run1-first-pass/` only for the record.

Queries. Two modes, 90 queries per Tenant class per mode, K=10:

- `near`: a point close to a stored document of that Tenant.
- `topical`: a point about one of the Tenant's topics. Other Tenants' rows crowd it, which is the hard case for a post-filtered index.

Classes: S100, S300, S1000 (the small Tenants), M2k (three medium Tenants near 2,000 rows), Mmax (the three largest medium Tenants, about 6,000 rows), L200k. Ground truth is an exact `ORDER BY embedding <=> q` under the same RLS, computed on each server. The first 5 queries of each configuration are warm-up and are discarded.

Every query is one transaction exactly as the ADRs describe: `begin; set local role authenticated; select set_config('request.jwt.claims', ..., true);` then the search, then `commit`. The policy is `tenant_id = gnomon.tenant_id() and gnomon.scope_allowed(scope)`, with both functions stable SQL reading `request.jwt.claims`. `authenticated` is a non-owner role. The policy written as a direct call and as a `(select ...)` initplan gave the same recall and the same latency within noise, so the numbers below use the direct form.

Servers. Both ran one container, shared_buffers 24 GB, maintenance_work_mem 14 GB, JIT off, one connection, warm cache:

| port | image | PostgreSQL | pgvector |
|---|---|---|---|
| 55433 | `pgvector/pgvector:pg17` | 17.11 | 0.8.6 |
| 55434 | `supabase/postgres:17.11.0.018` | 17.11 | 0.8.2 |

HNSW: `m=16`, `ef_construction=64`, `vector_cosine_ops`, built in about 3 to 4 minutes, 7.8 GB. Plus a B-tree on `tenant_id`. Tables are unlogged. The partitioned variant is `gnomon.embedding_p`, hash-partitioned on `tenant_id` into 64 partitions (about 15,700 rows each, about 31 Tenants each), with the same indexes and policy on each partition.

Forcing a plan. The planner may choose the B-tree plan on its own, which would hide the HNSW behaviour. So HNSW rows use `set local enable_sort = off`, and exact rows use `ORDER BY (embedding <=> q) + 0` through the Tenant B-tree. "Planner default" rows force nothing.

## Results

Recall@10, then p50/p95 latency in ms. All rows are the `topical` mode on pgvector 0.8.6 against the single table unless a heading says otherwise. The `near` mode is easier and is in the results files.

| search | S100 | S300 | S1000 | M2k | Mmax (about 6k) | L200k |
|---|---|---|---|---|---|---|
| HNSW, ef_search=40 (default), iterative off | 0.13, 3.2/5.1 | 0.43, 3.5/5.4 | 0.63, 3.8/5.2 | 0.81, 3.7/6.7 | 0.88, 3.0/4.9 | 0.87, 2.2/2.9 |
| HNSW, ef_search=100, iterative off | 0.22, 2.8/4.3 | 0.64, 3.3/4.8 | 0.83, 3.1/4.5 | 0.93, 3.0/4.9 | 0.95, 3.1/4.2 | 0.96, 4.7/6.3 |
| HNSW, ef_search=400, iterative off | 0.45, 5.1/10 | 0.85, 5.4/8.1 | 0.93, 5.1/7.1 | 0.97, 5.3/6.9 | 0.97, 5.4/7.5 | 0.97, 8.8/11.6 |
| HNSW, ef_search=1000, iterative off | 0.64, 7.2/16 | 0.90, 7.4/12 | 0.97, 7.5/12 | 0.98, 7.5/9.4 | 0.97, 7.3/9.7 | 0.99, 11.8/16.9 |
| relaxed_order, ef=40, max_scan_tuples 20k or 2M | 0.84, 6.7/99 | 0.94, 2.6/12 | 0.92, 2.3/5.8 | 0.94, 2.1/4.0 | 0.91, 2.1/2.7 | 0.88, 2.1/2.5 |
| relaxed_order, ef=100, max_scan_tuples=2M | 0.84, 6.2/97 | 0.95, 3.0/12 | 0.95, 2.7/6.1 | 0.97, 2.5/3.7 | 0.97, 2.7/3.7 | 0.96, 2.8/3.3 |
| relaxed_order, ef=40, scan_mem_multiplier=8 | 0.91, 6.6/547 | 0.94, 2.7/12 | 0.92, 2.4/5.9 | 0.94, 2.1/3.3 | 0.91, 2.1/2.5 | 0.88, 2.2/2.4 |
| strict_order, ef=40, max_scan_tuples=2M | 0.66, 7.6/100 | 0.80, 2.8/82 | 0.81, 2.3/12 | 0.86, 2.1/74 | 0.89, 2.0/2.4 | 0.87, 2.1/2.4 |
| strict_order, ef=100, max_scan_tuples=2M | 0.72, 6.2/99 | 0.89, 2.9/19 | 0.91, 2.7/9.1 | 0.94, 2.6/3.6 | 0.95, 2.6/3.1 | 0.96, 2.8/3.0 |
| exact scan via Tenant B-tree | 1.00, 1.7/1.8 | 1.00, 2.8/3.6 | 1.00, 7.9/9.8 | 1.00, 17.6/18.7 | 1.00, 48.6/54.5 | 1.00, 1142/1195 |
| planner default, nothing forced | 1.00, 1.7/2.1 | 1.00, 2.9/3.8 | 1.00, 8.8/9.8 | 1.00, 17.7/19 | 1.00, 50.2/52.9 | 0.87, 3.8/4.9 |
| per-Tenant partial HNSW, ef=100 | 1.00, 1.6/1.7 | 1.00, 2.9/3.5 | 1.00, 1.8/9.9 | 1.00, 1.9/2.4 | 1.00, 2.3/2.7 | 0.96, 3.7/4.4 |
| 64-way hash partitions, HNSW ef=40, iterative off | 0.81, 6.2/7.2 | 0.92, 5.0/6.5 | 1.00, 6.1/8.0 | 0.98, 5.3/6.4 | 0.98, 5.8/7.4 | 0.87, 5.7/6.9 |
| 64-way hash partitions, HNSW ef=100, iterative off | 0.89, 5.1/5.8 | 0.99, 4.9/5.9 | 1.00, 4.9/5.8 | 1.00, 5.4/6.4 | 1.00, 5.8/6.8 | 0.95, 6.5/7.5 |
| 64-way hash partitions, relaxed_order, ef=100 | 0.99, 5.0/7.9 | 1.00, 5.0/5.7 | 1.00, 5.0/5.7 | 1.00, 5.0/5.6 | 1.00, 5.4/6.1 | 0.95, 6.0/6.6 |

Other results:

- The Supabase image (pgvector 0.8.2) matches pgvector 0.8.6 within a few points everywhere. Default HNSW, topical: 0.17, 0.46, 0.62, 0.80, 0.88, 0.88. Relaxed ef=100: 0.84, 0.99, 0.94, 0.97, 0.96, 0.96. Exact: the same latencies as the table. Partitioned relaxed ef=100: 0.99, 1.00, 1.00, 1.00, 1.00, 0.96. Latencies run up to 2x higher on the HNSW rows of the single table, mostly in p95.
- Without RLS and without a Tenant filter, the same HNSW index gives the large-Tenant queries 0.87 at ef=40 and 0.99 at ef=100 (topical). So the 0.96 ceiling for L200k under RLS at ef=100 is partly the filter, not only the index.
- `hnsw.max_scan_tuples` between 20,000 and 2,000,000 changed nothing in any cell. The scan stopped on the memory limit first (`work_mem` times `hnsw.scan_mem_multiplier`). Raising `hnsw.scan_mem_multiplier` from 1 to 8 lifted S100 topical from 0.84 to 0.91 and saturated at 0.93, but p95 rose from about 100 ms to about 530 ms. p50 stayed near 6.5 ms.
- On `strict_order`, S100 returned only 7.8 to 8.3 rows of 10 at ef=40, and its p95 reached 1.5 s at a high scan-memory multiplier.
- The planner default already chose the exact B-tree plan for every Tenant up to about 6,000 rows and HNSW only for the large Tenant. It did so with a poor estimate: every small Tenant outside the most-common-values list is estimated at the table average (323 rows), whether it has 100 or 5,000.
- Exact scan costs about 5.7 microseconds per row of the Tenant. The large Tenant takes 1.1 s.
- Per-Tenant partial HNSW indexes build in 2 to 3 s for a small Tenant and 30 s for the large one. For S1000 the planner preferred the B-tree plan over the partial index (8.9 ms), so the 1.8 ms entry above was the index forced.

## What it means for the ADRs

- ADR-0001 and ADR-0005 (RLS on every row, `gnomon.tenant_id()` from `request.jwt.claims`, non-owner role with `SET LOCAL ROLE` and a transaction-local `set_config`): confirmed as workable. The policy form (direct call or initplan) did not matter for latency or recall. RLS does not guarantee recall. HNSW applies the policy after the index scan, so a small Tenant sees too few rows and the Store has to compensate.
- ADR-0006 (migrations are exported SQL files and gnomon applies no DDL of its own): contradicts one alternative and amends the schema. A per-Tenant partial index gives recall 1.00 at about 1.5 ms p50 for small Tenants, but it needs `CREATE INDEX` at runtime for every Tenant and that cannot ship in an exported migration. Hash partitioning on `tenant_id` is one schema decision made once in the first migration, so it fits. The partition count is then fixed into the migration chain, and changing it later is a table rewrite.
- ADR-0003 (embeddings link to Episodes by Citation): unaffected. The partition key `tenant_id` must be part of the embedding table's primary key, so the key becomes `(tenant_id, id)`.
- PostgreSQL 17 floor (ADR-0006): enough. Everything here ran on 17.11 and needed only pgvector 0.8 (iterative scan arrived in 0.8.0). The Store should require pgvector 0.8 or later.
- ADR-0009 (invoker-mode Store functions): not tested directly. Per-query session settings (`hnsw.iterative_scan`, `hnsw.ef_search`, `hnsw.scan_mem_multiplier`) must be `set local` inside the Store function's transaction. A function-level `SET` clause would also work, but it was not measured.

## Recommendation

1. Make the embedding table hash-partitioned on `tenant_id`, 64 partitions to start, with the HNSW index and the Tenant B-tree created on the parent. Run searches with `set local hnsw.iterative_scan = relaxed_order` and `set local hnsw.ef_search = 100`. This gave recall of 0.99 to 1.00 for every Tenant from 100 to about 6,000 rows, at about 5 ms p50, on both servers. `relaxed_order` returns slightly out-of-order results, so the Store must re-sort the top K outside the index scan.
2. Keep an exact path. For a Tenant under about 5,000 rows, `ORDER BY embedding <=> q` through the Tenant B-tree is exact and takes 2 to 50 ms. Choose it by the Tenant's row count, which the Store should keep or read from a cheap counter, not by the planner's estimate, which cannot tell a 100-row Tenant from a 5,000-row one. Put the Tenant predicate in the query as well as the policy. On the partitioned table it cut the exact path from 4.1 ms to 1.8 ms at 100 rows and from 38 ms to 35 ms at about 6,000 rows, because the planner can then prune partitions.
3. Do not use `strict_order`. It was worse than `relaxed_order` on every small-Tenant cell and returned under 10 rows.
4. Do not tune `hnsw.max_scan_tuples`. Tune `hnsw.scan_mem_multiplier` only if the product needs more than about 0.85 on a 100-row Tenant without partitioning, and accept the p95 cost. Partitioning gives the better recall without that cost.
5. Treat 0.95 to 0.96 as the ceiling for a Tenant with a large share of the table at ef=100 (0.99 at ef=1000, 12 ms). Raise `ef_search` per query for such a Tenant, or accept it.
6. Do not build per-Tenant indexes unless a later ADR allows runtime DDL.

## Caveats

- Other prototypes were running on this machine at the same time (48 cores, 251 GiB), so latencies carry noise from their load. Recall does not. I did not control for it, so read p95 as indicative.
- During the load I used a broad `pkill -f` pattern for Python worker processes owned by the same user. It may have signalled another prototype's Python workers. I found this out afterwards and stopped using it. If another prototype's run died around that time, this is a likely cause.
- The vectors are synthetic. Recall on real embeddings depends on how clustered they are. The unfiltered baseline (0.87 at ef=40, 0.99 at ef=100) matches typical HNSW behaviour, but only a real dataset would show whether Tenant-specific topics crowd each other as hard as they do here.
- The partition effect scales with partition size. At 1M rows, 64 partitions hold about 15,700 rows and 31 Tenants each, and ef=100 covers a large share of a partition. At 10M rows the same 64 partitions would hold 10x more rows and recall would fall toward the single-table numbers. Size the partition count to the expected table size, about 15,000 rows per partition, and test again before settling it.
- 90 queries per class and mode, from only 3 Tenants per small class. A class mean over 3 Tenants carries Tenant-to-Tenant variation. Minimum recall values are in the results files.
- Warm cache with 24 GB of shared buffers, one connection, unlogged tables, simple-protocol queries with no prepared plans. Cold-cache latency, concurrency, writes during search and index bloat were not measured.
- The two servers differ in pgvector version (0.8.6 and 0.8.2), so some of the gap between them could come from that. I did not separate it from image differences.
- Ground truth is exact search under the same RLS, computed on each server. Row ids differ across servers, and ids match between the single and partitioned tables, so the two tables share a ground truth.
- Medium Tenants (50 to about 6,000 rows) were sampled only through M2k and Mmax. A Tenant between 6,000 and 200,000 rows was not tested, and it is where the exact scan stops being cheap (about 50 ms at 6,000 rows, about 1.1 s at 200,000) and the choice between paths matters most.
