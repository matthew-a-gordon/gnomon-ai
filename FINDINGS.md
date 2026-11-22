# Findings: prototype 5, exact Search over the visible rows

Run on 2026-11-21 and 2026-11-22. Code and raw results are in `prototype/exact-search/`. The code is throwaway.

## Question

Can the Store's Search use one plan shape everywhere and stay under 100 ms p95 for candidate retrieval? The plan is an exact vector search over the rows visible under RLS, with quantized vectors and a full-precision re-rank, plus a token-table full-text branch, graph signals and reciprocal rank fusion in SQL. The test covers a visible set (Tenant plus Scope) of up to 200,000 rows on core Postgres 17 with pgvector and no other extension.

## Answer

Yes up to about 50,000 visible rows with a wide margin, and only just at 200,000: the As-of vector branch takes 27 ms p95 at 50k and 91 ms p95 at 200k on the pgvector image (111 ms on Supabase's image), the plain bit scan takes 102 ms p95 at 200k in a 4 GiB container, and the whole fused Search takes 66 ms p95 at 50k and 167 ms p95 at 200k, while 1M visible rows takes 386 ms for the bit scan alone.

Three things made the difference between those numbers and the slow ones I measured first, and the ADR amendments below depend on them:

- A covering index on the quantized column, `(tenant_id, scope, text_hash) include (q)`, halves the bit scan at 200k (80 ms against 160 ms for the same scan through the heap).
- Quantized rows kept per Version, carrying the four As-of columns, so an As-of search needs no join (92 ms at 200k, where the hash-join form took 2.4 s on one of the two servers at 50k, see 4 below).
- The Scope policy written as `scope = any(gnomon.allowed_scopes())`, which becomes an index condition (2.9 ms against 575 ms for an opaque function).

## Method

- Servers: `pgvector/pgvector:pg17` (PostgreSQL 17.11, pgvector 0.8.6) on port 55437, and `supabase/postgres` 17.11.0.018 (PostgreSQL 17.11, pgvector 0.8.2) on port 55438. Both ran as Docker containers, uncapped, with `shared_buffers` 8 GB, `work_mem` 64 MB, `max_parallel_workers_per_gather` 0 (one core per query), `fsync` off and autovacuum off. Tables were vacuumed and frozen after the load, so index-only scans had a full visibility map.
- Data, identical on both servers (seeded): 1,794,190 embeddings of 1,536 dimensions, 2,332,988 Versions, 38 million token rows, 3.6 million Citations, 1.08 million Node names and Aliases. The vectors are two-level clusters (200 topics, 10 subtopics each, noise norm 0.6). Text is drawn from a per-subtopic vocabulary mixed with a Zipf vocabulary.
- Layout: one big Tenant with four Scopes of 1,000, 9,000, 40,000 and 150,000 embeddings. Visible sets are `1k` (Scope 1 alone, a small Scope inside a 200k Tenant), `10k`, `50k` and `200k` (Scopes 1 to 4). A second Tenant holds one Scope of 1,000,000 embeddings. 300 small Tenants fill the rest.
- A visible set of N means N distinct embedded texts. Versions are about 1.3 times that, because 30% of texts have two Versions that share the text (a Refinement or an End of validity). Each entity has one to three texts over its Recorded time. Summaries and Communities are rows of the same table with a kind column and the same time columns, which is my assumption.
- Every timed call runs in its own transaction as the non-owner role `gnomon_app`, with the Tenant and the allowed Scopes set by `set_config(..., true)`, through a `SECURITY INVOKER` function with `search_path = ''`. The timer covers the `select` only. Every query ran once untimed, then once timed. p50 and p95 are over 60 queries (1k, 10k), 40 (50k), 30 (200k) and 12 (1M, where p95 is close to the maximum). The fusion measurements use 20, 13 and 10 queries.
- Recall@10 compares each variant's top 10 with an exact full-precision scan of the same visible set. For As-of searches it compares with an exact scan that applies the same four As-of conditions, over 12 queries.
- As-of instants: "now" is valid-at and known-at 2026-11-21. The past read is valid-at 2025-01-01 and known-at 2025-06-01. At "now", 44% of Versions in the 200k set are visible, and 32% at the past read.
- The narrow tables (`emb_half`, `emb_bit`, `ver`, the token table) are in shuffled physical order, as interleaved inserts would leave them. The full-vector table `emb` is in Tenant order, but at 6 KB a row it is one row per page either way.
- Load on the machine. The first full pass ran while other work held the load average between 5 and 35, and it came out about twice as slow (for example 177 ms for the 200k covering-index bit scan). I discarded it and re-ran measurements 1 to 5 serially with the load average between 1.3 and 3.4. Those are the numbers below. The first pass is kept in `prototype/exact-search/results/loaded-runs/`. The 4 GiB runs (measurement 6) ran with the load average between 3.8 and 8.6, so their absolute numbers carry more noise.

## Results

### 1. Exact vector speed and recall

Milliseconds, pgvector image. "bit, cov" is the bit scan through the covering index and "bit, heap" the same scan with index-only scans turned off. The re-rank takes the top 200 at full precision unless noted. Supabase's image is in the last column for the sizes it ran.

| visible | variant | recall@10 | p50 | p95 | Supabase p50 / p95 |
|---|---|---|---|---|---|
| 1k | full | 1.00 | 1.2 | 1.2 | |
| 1k | halfvec, re-rank 200 | 1.00 | 3.2 | 3.3 | |
| 1k | bit, cov, re-rank 200 | 0.96 | 2.9 | 2.9 | |
| 10k | full | 1.00 | 12.2 | 13.0 | |
| 10k | halfvec, re-rank 200 | 1.00 | 14.2 | 14.9 | |
| 10k | bit, cov, re-rank 200 | 0.86 | 5.7 | 6.2 | |
| 50k | full | 1.00 | 58.3 | 60.9 | 53.7 / 54.2 |
| 50k | halfvec, re-rank 200 | 1.00 | 58.1 | 59.0 | 60.7 / 64.1 |
| 50k | bit, heap, re-rank 200 | 0.86 | 42.3 | 45.7 | 43.4 / 46.7 |
| 50k | bit, cov, re-rank 200 | 0.86 | 23.9 | 25.5 | 23.0 / 24.0 |
| 200k | full | 1.00 | 228 | 230 | 213 / 215 |
| 200k | halfvec, re-rank 200 | 1.00 | 223 | 226 | 234 / 237 |
| 200k | bit, heap, re-rank 200 | 0.83 | 160 | 169 | 173 / 185 |
| 200k | bit, cov, re-rank 100 | 0.64 | 79.6 | 84.5 | 74.0 / 78.9 |
| 200k | bit, cov, re-rank 200 | 0.83 | 78.8 | 80.4 | 76.4 / 77.6 |
| 200k | bit, cov, re-rank 400 | 0.99 | 81.4 | 83.3 | 79.0 / 83.9 |
| 1M | full | 1.00 | 1,154 | 1,159 | 1,092 / 1,098 |
| 1M | bit, cov, re-rank 200 | 0.64 | 382 | 386 | 369 / 407 |
| 1M | bit, cov, re-rank 400 | 0.83 | 387 | 391 | 371 / 372 |

Reading the table:

- Halfvec bought nothing. It is no faster than full vectors at 50k and 200k, 1.2 times slower at 10k and 2.7 times slower at 1k. It also needs 7.1 GB beside the 14.1 GB of full vectors.
- The bit scan is dominated by the Hamming computation and by tuple fetches. Fetching each bit row from a shuffled heap cost about as much as the distance work, and the covering index removes that cost.
- Recall is set by the re-rank depth. At 200k, 0.64 at 100, 0.83 at 200, 0.99 at 400, with almost no change in time. At 1M the same depths give 0.64 and 0.83 at 200 and 400, so the depth has to grow with the visible set. These vectors are a hard case: each subtopic holds hundreds of near-equidistant neighbours. Real embeddings will recall differently, in either direction.
- The re-rank's cost depends on a cached heap. A re-rank of 200 full vectors takes about 1 to 2 ms when they are cached. In my first, uncached run of the 200k function it added about 0.9 s.

Storage per option, in GB (heap plus indexes), 1.79 million rows:

| option | storage |
|---|---|
| full vectors, PLAIN storage (one 6 KB row per 8 KB page) | 14.1 |
| plus halfvec table | 7.1 |
| plus bit table and covering index | 1.1 and 0.5 |
| per-Version bit rows and covering index (see 4) | 1.5 and 0.8 |

### 2. Scope policy

Plans captured with `auto_explain` inside the function. Milliseconds, bit scan, re-rank 200, one run per server.

| policy form | visible | pgvector p50 / p95 | Supabase p50 / p95 | plan |
|---|---|---|---|---|
| `scope = any(gnomon.allowed_scopes())` | 1k in a 200k Tenant | 2.9 / 3.1 | 3.0 / 3.1 | `Index Cond: (tenant_id = $1) AND (scope = ANY (gnomon.allowed_scopes()))` |
| same | 200k | 80 / 83 | 77 / 78 | same |
| `gnomon.scope_allowed(scope)`, plpgsql | 1k in a 200k Tenant | 575 / 581 | 800 / 816 | `Index Cond: (tenant_id = $1)`, `Filter: gnomon.scope_allowed(scope)`, 199,000 rows removed |
| same | 200k | 667 / 671 | 896 / 911 | filter on every row |
| `gnomon.scope_allowed_inl(scope)`, inlinable SQL | 1k in a 200k Tenant | 2.5 / 2.5 | 2.5 / 2.6 | the function body is inlined and becomes an index condition |
| same | 200k | 79 / 82 | 76 / 81 | same |

So the policy becomes an index condition on the `(tenant_id, scope, ...)` B-tree under RLS, inside the invoker function, as the research note inferred. The old function form is a filter whenever the function is opaque to the planner. A small Scope inside a large Tenant is the case that breaks: 575 ms for 1,000 visible rows.

### 3. Full-text search

Milliseconds, pgvector image, As-of "now". Queries are three mid-frequency topic words, so there are no stop words. Overlap is the share of the scan's top 10 found in the token table's top 10. The two methods rank differently (`ts_rank` against BM25), so it is a sanity check and not a recall.

| visible | tsvector scan p50 / p95 | token table, BM25 p50 / p95 | top-10 overlap |
|---|---|---|---|
| 1k | 1.3 / 1.4 | 1.9 / 2.1 | 0.86 |
| 10k | 11.9 / 12.5 | 2.6 / 3.1 | 0.83 |
| 50k | 57.8 / 62.3 | 3.2 / 4.1 | 0.84 |
| 200k | 253 / 268 | 4.7 / 6.0 | 0.80 |
| 1M | 1,409 / 1,433 | 11.6 / 20.5 | 0.59 |

Checking the As-of conditions on only the top four times the limit, after scoring, took 3.5 ms at 200k against 4.7 ms for checking them on every posting, and returned the same top 10 in all 30 queries.

Write cost per Version, in microseconds, measured as `gnomon_app` through RLS with five indexes and the generated `tsvector` column already on the Version table. One session, no concurrency.

| variant | single-row inserts | batch of 1,000 |
|---|---|---|
| no trigger | 205 | 39 |
| row trigger, token table only | 497 | 245 |
| row trigger, token table plus document frequencies plus Tenant totals | 2,163 | 2,389 |
| statement trigger on a transition table, all three | 2,367 | 352 |

Per-Tenant BM25 is practical in SQL. The score reads two small per-Tenant tables (`tok_df`, `tenant_stats`) and the postings, and takes 4.7 ms at 200k. The cost is on the write side: the document-frequency upsert touches a row per distinct token, so a row trigger costs about 1.7 ms more per Version than the token insert alone. Batching with a statement trigger brings the whole thing to 0.31 ms more per Version, but only for batched writes. Contention on hot token rows under concurrent writers is not measured.

### 4. As-of search

Milliseconds, pgvector image, bit scan, re-rank 200. "Hash join" checks As-of visibility by probing the Version table for each candidate text hash. "Per-Version bit rows" keeps a quantized row per Version with the four As-of columns in the covering index, so the scan filters on the index tuples.

| visible | no As-of conditions | hash join, now | per-Version rows, now | per-Version rows, past known-at | recall@10, per-Version rows |
|---|---|---|---|---|---|
| 1k | 3.8 / 4.0 | 5.6 / 5.7 | 4.6 / 4.7 | 4.6 / 4.7 | 1.00 |
| 10k | 6.8 / 7.0 | 45 / 46 | 7.8 / 8.7 | 7.7 / 7.9 | 1.00 |
| 50k | 22.3 / 23.5 | 47 / 50 | 26.4 / 27.0 | 25.8 / 27.3 | 0.99 to 1.00 |
| 200k | 79 / 85 | not run | 92 / 96 | 87 / 90 | 0.98 to 0.99 |

On Supabase's image: 50k per-Version rows 34 / 35 ms, 200k 112 / 113 ms (now) and 105 / 108 ms (past). The hash-join form took 47 ms at 50k on the pgvector image and 2,388 ms on Supabase's image, over identical data. The planner estimates one row for every RLS-filtered scan, and which join it picks then depends on small cost differences. The hash join as written is not safe to ship. At 200k under the 4 GiB container it took 7.7 s.

At 200k the As-of searches cost about the same as the plain scan. Only 44% (now) and 32% (past) of Versions are visible, so fewer distances are computed, and the extra bytes in the index tuple roughly cancel that.

Node names and Aliases, milliseconds, pgvector image. The current-only partial index is `where recorded_to = 'infinity'`. Visible is the Scope set used above, so 200k visible rows hold 120,723 name rows and about 96,000 of them are current. The 1M Scope holds 599,860 name rows.

| visible | exact, full vectors, partial index | bit plus re-rank 200, partial index | recall@10 | past known-at, all rows |
|---|---|---|---|---|
| 1k | 0.8 / 0.8 | 1.8 / 1.9 | 0.97 | 3.9 / 4.0 |
| 10k | 7.3 / 7.6 | 3.9 / 4.0 | 0.98 | 18.8 / 19.2 |
| 50k | 28 / 29 | 16.6 / 17.2 | 0.97 | 93 / 97 |
| 200k | 112 / 115 | 53.6 / 53.9 | 0.95 | 378 / 385 |
| 1M | 536 / 543 | 173 / 180 | 0.78 | 1,787 / 1,811 |

The partial index keeps current names under 100 ms to 200k visible rows. It does not at 1M names, and that is where HNSW on the partial index would earn its place. A past known-at name read has no such index and takes 378 ms at 200k.

### 5. Fusion and graph signals

Milliseconds, pgvector image, depth 1. The total is the `search` function: vector branch (per-Version rows, re-rank 200, 100 candidates), BM25 branch (100), neighbourhood from the top 10 seeds (100 candidates, at most 50 edges per Node), reciprocal rank fusion with k = 60, then the two graph signals on the fused top 100, weighted 0.5 each.

| visible | total p50 / p95 | vector branch | BM25 branch | neighbourhood, depth 1 | signals, 100 candidates | signals from a per-Scope rollup |
|---|---|---|---|---|---|---|
| 1k | 10.0 / 10.3 | 4.6 | 1.8 | 1.7 | 3.9 | 3.9 |
| 10k | 20.8 / 21.4 | 7.7 | 2.5 | 4.7 | 9.4 | 15.4 |
| 50k | 64.6 / 66.4 | 28.2 | 3.3 | 18.2 | 27.5 | 29.9 |
| 200k | 163 / 167 | 89 | 4.2 | 66 | 17.0 | 13.5 |

- At the past known-at the 200k total is 157 / 161 ms. Depth 2 adds under 3 ms at every size because of the fan-out cap. On Supabase's image the 200k total is 199 / 204 ms.
- The branch timings are medians over separate runs and do not add up to the total, because the signals and neighbourhood branch timings use candidates from the vector branch, and because each branch has its own plan.
- The signals cost depends on which Nodes are candidates, not on the visible-set size: a hub Node costs thousands of adjacency entries. The rollup (degree and mention counts at "now") saved nothing measurable. Its cost ranged from 3.9 ms to 30 ms and was slower than live counting at 10k.
- Both graph signals rank only the candidates already found, so their cost is bounded by the candidate count and not by the visible set. At 200k they cost 17 ms of 163 ms.
- The log damping has no effect under rank fusion. A log is monotone, so a rank list built from `ln(1 + n)` equals one built from `n`.
- The neighbourhood branch is the slowest part after the vector branch at 200k (66 ms). Its time goes to looking up current Node Versions for the reached Nodes. I did not tune it further.

### 6. Memory

The pgvector image in a container capped at 4 GiB (`--memory 4g --memory-swap 4g --cpus 2`), with `shared_buffers` 1 GB, `effective_cache_size` 3 GB, `work_mem` 16 MB and `maintenance_work_mem` 512 MB. The container was restarted and its data files' cached pages dropped before the run. Milliseconds, 200k visible rows, 40 queries.

| variant | first 10 queries p50 / max | p50 | p95 |
|---|---|---|---|
| bit, cov, re-rank 200 | 103 / 193 | 96 | 102 |
| full vectors | 664 / 9,753 | 677 | 695 |
| fused search, depth 1 | 228 / 470 | 200 | 207 |

A second run of the 200k vector scans under the same cap gave: bit with covering index 75 / 78 ms at re-rank 100, 77 / 82 ms at 200 and 78 / 81 ms at 400 (p50 / p95), bit through the heap 168 / 189 ms, full 693 / 876 ms and halfvec 681 / 722 ms.

The covering-index path is nearly unaffected by the cap. Its working set is the 506 MB index (plus 800 MB for the per-Version one) and a few hundred full vectors per query. The full-vector scan reads 1.2 GB of heap through a 1 GB buffer pool and is 3 times slower than uncapped, and its first uncached query took 9.8 s. The fused search takes 200 ms against 163 ms uncapped.

## What it means for the ADRs and the decisions

ADR-0001 (one Rust core, claims through `set_config`). Unchanged. Store-side fusion is plain SQL in invoker functions and makes no model call. The pooler questions in the research note are still open and not tested here.

ADR-0003 (Episodes, deterministic Erasure). Extended. The design adds derived rows, and Erasure has to delete every one in the same transaction: the full embedding, the quantized rows (`emb_bit`, and the per-Version rows), the token rows, the Citation's copy of the Episode time, the adjacency rows, and any name rows. The token table also feeds per-Tenant statistics, so Erasure has to decrement the document frequencies and totals, or those statistics keep counting erased text. Index-only scans depend on the visibility map, so after bulk Erasure the covering-index speed returns only after a vacuum.

ADR-0004 (scalar time columns). Confirmed and extended. Every As-of condition ran as an index condition or as an index-tuple filter, and every key comparison that mattered (`uuid_eq`, `texteq`, `byteaeq`, `int8eq`, the `timestamptz` comparisons) is leakproof on PG 17. `@@` is not, and the tsvector scan cost 253 ms at 200k for that reason.

ADR-0005 (every row has a Tenant). Amended, as the brief expected. The policy `scope = any(gnomon.allowed_scopes())` is an index condition on a `(tenant_id, scope, ...)` B-tree. An inlinable SQL function (`gnomon.scope_allowed_inl`) also works, because the planner inlines it, but an opaque function scans the whole Tenant (575 ms for a 1k Scope in a 200k Tenant). The contract each consumer signs is therefore that its Scope rule is a set of allowed keys, or an inlinable expression over one.

ADR-0006 (exported migrations). Compatible. Everything here is tables, B-tree indexes (some partial, some covering), a trigger and functions, with no extension beyond pgvector and no runtime DDL. The vector column types and the `bit(1536)` column fix the embedding dimension when the migrations are exported, as decided. The pgvector floor of 0.7.0 holds (`binary_quantize`, `halfvec`). The prototype ran on 0.8.2 and 0.8.6, and used no HNSW, so the 0.8.3 vacuum fix does not apply to this path.

ADR-0009 (invoker functions). Confirmed. Every measurement ran as a non-owner role through invoker functions under RLS with `search_path = ''`. One finding for the function bodies: the planner estimates one row for every RLS-filtered scan, because the policy calls stable functions. So join shapes flip on small differences. I pinned the re-rank to 200 primary-key probes with `cross join lateral (... limit 1)`. A plain lateral join is pulled up into an ordinary join, and without the pin the 4 GiB run scanned all 200,000 full vectors and took 660 ms instead of 96 ms.

Decisions in the brief:

- Search honours As-of reads and filters under RLS. Confirmed, with one change: keep a quantized row per Version (see 4). The shared hash-join form is fragile: 5.8 times slower than per-Version rows at 10k on the pgvector image, and 70 times slower at 50k on Supabase's image (2,388 against 34 ms).
- Scope policy as `scope = any(...)`. Confirmed (measurement 2). Amends ADR-0005.
- Exact vectors over the visible rows. Confirmed for visible sets up to 200,000 rows with a covering index on the bit column. At 200k the margin is 9 to 20 ms on the pgvector image and negative on Supabase's image for the As-of vector branch (111 ms p95), and the fused search (167 to 204 ms) is over 100 ms. At 1M the bit scan is 386 ms. By the brief's rule (add HNSW only if the exact path misses 100 ms p95 at 200,000 visible rows) the answer is "borderline, and it misses on the Supabase image", so I would plan the HNSW path but not ship it in the first version.
- One embedding per Version, shared by text hash. Amended. Share the full vector by text hash (it is 99% of the bytes). Keep the quantized row per Version, with the As-of columns in its covering index.
- Halfvec column. Contradicted. It was no faster than full vectors and costs 7.1 GB, so drop it.
- Current-only name index. Confirmed to 200k visible rows (54 ms, recall 0.95), not at 1M names (173 ms, recall 0.78). Use HNSW on the partial index only for a Scope with several hundred thousand names. A past known-at name read is 378 ms at 200k and needs its own covering copy if it matters.
- Token table with per-Tenant BM25. Confirmed. 4.7 ms against 253 ms at 200k. The cost is on the write side (see 3), and the Tenant-wide statistics read across Scopes the caller may not see, which is what "per Tenant" means and is a small leak inside a Tenant.
- Reciprocal rank fusion plus two ranked lists. Confirmed. 17 ms for the signals at 200k, so computing them at search time is fine. The rollup did not pay for itself at these sizes. The log damping is moot under rank fusion.
- Neighbourhood expansion, depth 1 by default and at most 2. Confirmed with a fan-out cap of 50 edges per Node, which makes depth 2 nearly free (1 to 2 ms) and also means a hub's neighbourhood is a sample.
- Embedding dimension fixed at export. Confirmed.

## Recommendation

Take the exact-path design forward with these specifics:

1. Policy: `tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes())`. Amend ADR-0005 and state the allowed-keys contract.
2. Tables: full `vector(1536)` per distinct text (PLAIN storage, keyed by Tenant, Scope and text hash). A narrow `bit(1536)` row per Version with a covering index `(tenant_id, scope) include (id, text_hash, q, valid_from, valid_to, recorded_from, recorded_to)`. No halfvec.
3. Vector branch: bit scan with the As-of conditions on the index tuples, re-rank 400 at full precision through a pinned primary-key lateral lookup. Re-rank 200 gave 0.83 recall on this data.
4. Full text: token table `(tenant_id, scope, token, ver_id) include (tf, dl)`, per-Tenant statistics, a statement-level trigger for batches.
5. Graph: an adjacency table with one row per edge Version and direction and a single covering index. Citations carry the Episode's recorded time. Compute both signals at search time.
6. Names: a current-only partial index with bit plus re-rank.
7. Before shipping, decide the visible-set target. At 50k visible rows everything has a wide margin. At 200k the vector branch is within 20 ms of the line and the fused Search is not under it. If 200k visible rows is a real requirement at 100 ms for the whole Search, build the HNSW path now (static partial index on current Versions, iterative scan, as prototype 1 measured) and re-run measurement 1 against it. If 100 ms applies only to candidate retrieval for one branch, the exact path meets it.

## Caveats

- Synthetic data. Vectors are clustered Gaussians with a zero-mean sign structure, which favours binary quantization more than a real embedding model might, while the tight subtopics make top-10 recall harder than usual. Recall figures are for this data only.
- Text uses the `simple` configuration, so there is no stemming or stop-word removal, and queries use topic words only. Full-text timings with common words in the query would be higher.
- One session at a time. No concurrent readers or writers, no autovacuum, `fsync` off. Index-only scans were measured on a freshly vacuumed and frozen table. With churn, heap fetches would erode the covering-index gain.
- Parallel query was off (`max_parallel_workers_per_gather` 0). A host with parallel workers and parallel-safe policy functions could be faster. Marking the claim functions parallel safe was not tested.
- Other load on the machine. See Method. The 4 GiB runs ran at a higher load average than the uncapped ones, and the 1M cases use 12 queries.
- The 4 GiB test ran on the pgvector image only, with `--cpus 2`, and the cold start used `dd iflag=nocache` to drop cached pages for the data files, which I did not verify beyond the first-query times. The Supabase image ran uncapped.
- Hosted database services were not used. Supabase's image has `supautils` and other preloaded libraries, but I ran it as a plain local container.
- Name embeddings are synthetic vectors near the entity's subtopic. Names carry no text.
- The mentions list counts Citations whose Episode was recorded at or before known-at, using a column copied from the Episode. The join form of the same query chose a bad plan inside the function, so I did not measure it.
- Not measured: Erasure, vacuum after Erasure, concurrent writers on token statistics, HNSW at any size, Tenant counts beyond 302, transaction poolers.

## Re-running

`prototype/exact-search/`: `schema.sql`, `gen.py` (with `common.py`), `post.sql`, `policies.sql`, `functions.sql`, then `vbit.sql`, `graph.sql`, `adj.sql`, `roll.sql`, `fix.sql` and `lateral.sql` in that order (these supersede parts of `functions.sql`), and `bench.py PORT USER TAG test`. Results are in `prototype/exact-search/results/`.
