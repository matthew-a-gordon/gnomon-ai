# Research: how search should work in gnomon

Checked on 2026-11-21. Search has no ADR and no glossary term yet. This note uses the glossary's vocabulary. It does not recommend one design. It lays out what exists, what each option costs, and what a design interview has to settle.

## The question

How should the Store find Nodes, Edges, Versions, Summaries and Communities for a query, under the constraints the ADRs already fix?

- RLS on every row, with the policy `tenant_id = gnomon.tenant_id() and gnomon.scope_allowed(scope)`, under a non-owner role set with `SET LOCAL ROLE` (ADR-0005).
- Store functions are `SECURITY INVOKER` with `search_path = ''` (ADR-0009).
- Migrations are exported SQL in each consumer's chain, and gnomon applies no DDL at runtime (ADR-0006).
- gnomon runs on Supabase, DigitalOcean, Neon, RDS, Aurora, Cloud SQL, AlloyDB and Azure Flexible Server, behind transaction poolers.
- The Pipeline makes the model calls, including the embedder and the reranker (ADR-0001).
- Erasure physically deletes everything derived from an Episode, embeddings included (ADR-0003).

Decisions the user has already made, which every option below is weighed against:

1. Search serves two uses equally. The Pipeline assembles model context. A consumer or agent resolves a mention to an existing Node, which needs high recall on small Tenants.
2. The target is many small Tenants plus a long tail of large ones, with about 10M embeddings in the largest deployment.
3. The baseline runs on core Postgres plus pgvector on every host, with a faster path where an extension is present. DigitalOcean and Supabase matter most.
4. Search honours As-of reads: it takes valid-at and known-at and returns only items that held at that point.
5. Embeddings are eventually consistent. A write commits first and its embedding follows from the Pipeline. Full-text search covers the gap.
6. The Store returns candidates for each method (vector, full-text, graph neighbourhood) under RLS and fuses them. The Pipeline reranks with a model and assembles the context.
7. The effective filter is Tenant plus Scope. A Scope can be much smaller than its Tenant.

Prototype 1 (`prototype-vector-recall-rls`, FINDINGS.md) found that pgvector HNSW applies the RLS filter after the index scan, so recall@10 for a 100-row Tenant in a 1M-row table was 0.13 at default settings. It recommended 64-way hash partitioning on `tenant_id`, `hnsw.iterative_scan = relaxed_order` with `ef_search` 100, and an exact B-tree path for small Tenants. This note treats that as one option among several.

## Short answer

Three facts narrow the field more than any benchmark.

- On an RLS table, a query condition becomes an index condition only if its operator is leakproof, and only a superuser can mark one leakproof. Vector distance and trigram distance are ordering operators, so their indexes still work, but the policy, Scope and As-of conditions become post-filters on them. Full-text `@@`, trigram `%` and array `&&` are not leakproof, so their GIN and GiST indexes are not used at all under RLS (section 1, section 2).
- Supabase offers nothing for vector or text search beyond pgvector 0.8.2 and pg_trgm. DigitalOcean adds pgvectorscale on Standard Edition only, and from 2026-11-30 new Standard clusters are capped at 4 GiB of RAM. VectorChord and ParadeDB pg_search are on none of the eight hosts. A BM25 extension exists only on Cloud SQL and AlloyDB, in preview (matrix).
- A filter on a vector index scan is cheap only when the index itself is restricted to the visible rows. Partitioning restricts by Tenant, but nothing restricts by Scope or by As-of instant without runtime DDL, except a label-aware index (pgvectorscale) or a predicate-aware one (AlloyDB ScaNN, VectorChord), and those are on few hosts.

The realistic designs are these four. They share the same lexical branch (a generated tsvector column, scanned over the visible rows), the same name branch for mention resolution (pg_trgm and name embeddings), Store-side RRF in SQL, and a Pipeline reranker.

A. One exact path over the visible rows. Every vector search goes through the B-tree on `(tenant_id, scope)`, computes distance only for rows that pass Tenant, Scope and As-of, and re-ranks. A stored `bit` column from `binary_quantize` or a `halfvec` column makes the scan cheaper, with a full-precision re-rank of the top few hundred. There is one plan shape and recall is exact up to the quantization step, for every Tenant and every Scope. Latency grows linearly with the visible set: prototype 1 measured 1.7 ms at 100 rows, 49 ms at about 6,000 and 1.1 s at 200,000 for full vectors. Whether quantization brings the large Tenants into budget is unmeasured. No partitioning and no extension. It fails if the long tail of large Tenants needs sub-100 ms search over hundreds of thousands of rows.

B. Size-routed: the exact path for small visible sets, and a shared HNSW index with `relaxed_order` iterative scans for large ones. The Store routes by a row count it maintains per (Tenant, Scope), because the planner cannot tell a 100-row Tenant from a 5,000-row one. This is prototype 1's design without partitions. It runs everywhere and keeps one table, at the cost of two plan shapes, and of recall for a mid-size visible set that falls as the table grows. A static partial HNSW index on current Versions (`WHERE recorded_to = 'infinity'`) can ship in a migration and removes closed Versions from the hot path.

C. Partitioned. Design B on a table hash-partitioned by Tenant, as prototype 1 recommended, or a tiered variant where large Tenants are promoted to their own list partitions. It gives the best measured recall at 1M rows (0.99 to 1.00), but the partition count lives in the migration chain, recall falls with rows per partition, promotion is runtime DDL, and Scope is still a post-filter inside a partition.

D. A core baseline (A or B) plus an extension fast path. The vector branch is swapped per host for pgvectorscale label filtering, AlloyDB ScaNN inline filtering or Azure pg_diskann, and the lexical branch for pg_textsearch on the Google hosts. Each fast path needs its own conformance tests and its own migration variant. On the two hosts that matter most it buys little: Supabase has no fast path, and DigitalOcean has pgvectorscale only on Standard Edition, whose new clusters top out at 4 GiB. pgvectorscale labels are `smallint`, so they cannot label every Tenant and Scope pair; they could serve as hash buckets inside one index, which is an untested idea.

Every design has the same lexical cost: under RLS, full-text search scans the visible rows. A large Tenant's lexical query is a scan of that Tenant, unless ADR-0009 allows a `SECURITY DEFINER` candidate function, or a leakproof token table replaces GIN.

## 1. Filtered and multi-tenant vector search in Postgres

### The rule that decides every option: what runs before the RLS policy

Postgres evaluates RLS policy conditions before any condition from the query, unless the query condition is leakproof. The planner enforces this in `restriction_is_securely_promotable`, which lets a condition become an index condition only if its security level is no higher than the lowest one on the table, or if it is leakproof ([restrictinfo.c, REL_17_STABLE](https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/optimizer/util/restrictinfo.c), called from `match_clause_to_index` in [indxpath.c](https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/optimizer/path/indxpath.c)). The documentation says the same: "an index scan cannot be selected for queries on security barrier views (or tables with row-level security policies) if an operator used in the WHERE clause is associated with the operator family of the index, but its underlying function is not marked LEAKPROOF" ([PostgreSQL docs, Rules and Privileges](https://www.postgresql.org/docs/current/rules-privileges.html)). Only a superuser can mark a function leakproof, and ADR-0004 already notes that no managed host grants one.

Three consequences follow, and they apply to every option below.

- A condition written in the RLS policy has the lowest security level, so it can always become an index condition. The Tenant B-tree works under RLS for this reason.
- A condition written in the query can become an index condition only if its operator is leakproof. In PG 17, `uuid_eq`, `texteq`, `text_lt` and the `timestamptz` comparisons are leakproof. `arrayoverlap` (`&&`), `ts_match_vq` (`@@`) and the trigram operators are not ([pg_proc.dat, REL_17_STABLE](https://github.com/postgres/postgres/blob/REL_17_STABLE/src/include/catalog/pg_proc.dat)).
- An `ORDER BY` distance operator is not a restriction, so the rule does not touch it. A vector index scan ordered by `<=>` works under RLS. Every filter on that scan, the policy included, runs as a filter on the rows the index returns, unless the index access method takes the filter as an index key.

Prototype 1 measured the cost of the third point: HNSW at the default `ef_search` returned recall@10 of 0.13 for a 100-row Tenant in a 1M-row table.

The effective filter is Tenant plus Scope. In ADR-0005 the Scope check is `gnomon.scope_allowed(scope)`, a function of the row's column. A B-tree cannot use a function call on a column as an index condition. If the policy instead read `scope = any(gnomon.allowed_scopes())`, a composite B-tree on `(tenant_id, scope)` could serve both conditions as index conditions, because the policy runs at the lowest security level. This is my reading of the planner rules above and was not tested. Whether every consumer's Scope rule can be expressed as a set of allowed keys is an open question.

### pgvector HNSW with iterative scans

How it filters. HNSW post-filters. "With approximate indexes, filtering is applied after the index is scanned. If a condition matches 10% of rows, with HNSW and the default `hnsw.ef_search` of 40, only 4 rows will match on average" ([pgvector README, Filtering](https://github.com/pgvector/pgvector#filtering)). Since 0.8.0, iterative scans keep scanning until enough rows pass the filter, or until `hnsw.max_scan_tuples` (default 20,000) or the memory limit `work_mem` times `hnsw.scan_mem_multiplier` (default 1) is reached ([README, Iterative Index Scans](https://github.com/pgvector/pgvector#iterative-index-scans)). `relaxed_order` gives better recall than `strict_order` but may return rows slightly out of order, so the README wraps the scan in a materialized CTE and re-sorts with `ORDER BY distance + 0` on PG 17.

Filter from RLS. It works the same as a query filter, because both are post-filters on the index scan. Prototype 1 confirmed this under the gnomon policy.

Second filter (Scope). Any number of conditions can be post-filtered. The cost is set by the combined selectivity: a Scope that holds 1% of a Tenant that holds 1% of the table is a 0.01% filter, and the iterative scan must visit about 10,000 index tuples per matching row. With `max_scan_tuples` at 20,000, that returns only a couple of rows. A small Scope inside a large Tenant behaves like a small Tenant.

Evidence. Prototype 1 (pgvector 0.8.6, PG 17.11, 1,004,200 rows, 1,536 dimensions): `relaxed_order` with ef 100 gave recall 0.84 for a 100-row Tenant, 0.95 to 0.97 for 300 to 6,000 rows, and 0.96 for the 200,000-row Tenant, at about 3 ms p50 and up to 97 ms p95 for the smallest Tenant. Raising `hnsw.max_scan_tuples` changed nothing because the memory limit stopped the scan first. AWS reports a category-filtered search where pgvector 0.7.4 returned 10% recall at ef 40 and 0.8.0 iterative scans returned 100% ([AWS database blog, pgvector 0.8.0 on Aurora](https://aws.amazon.com/blogs/database/supercharging-vector-search-performance-and-relevance-with-pgvector-0-8-0-on-amazon-aurora-postgresql/)). LightRAG's maintainers measured the same defect on a shared Postgres table: a workspace holding 0.02% of 212,240 rows returned 0 of 20 rows without iterative scan, 3 with `strict_order`, and all 20 only with raised `max_scan_tuples` and `work_mem`, at 568 ms (section 3). The upstream author planned to make `strict_order` the default in 0.9.0 and then judged it "a bit risky (for current users) with how the Postgres executor handles distance filters" ([pgvector issue #914](https://github.com/pgvector/pgvector/issues/914)). Filter-aware HNSW (a `hnsw-filtering` branch, HQANN and ACORN prototypes) has been explored upstream but not merged ([issue #761](https://github.com/pgvector/pgvector/issues/761), [issue #980](https://github.com/pgvector/pgvector/issues/980)).

Operational cost. No schema change. Settings are per transaction with `SET LOCAL`, which works through transaction poolers. Index build is fastest when the graph fits in `maintenance_work_mem` ([README, Index Build Time](https://github.com/pgvector/pgvector#index-build-time)). HNSW index tuples store the vector itself (`Vector data` in `HnswElementTupleData`, [hnsw.h v0.8.7](https://github.com/pgvector/pgvector/blob/v0.8.7/src/hnsw.h)), so the index is about as large as the vectors. Recall falls as the table grows for a fixed Tenant size, because the filter gets more selective.

Hosts. pgvector is on every host in the list; versions are in the matrix. The current release is 0.8.7, 2026-10-01 ([CHANGELOG](https://github.com/pgvector/pgvector/blob/master/CHANGELOG.md)). The prototype needs 0.8.0 or later.

### pgvector IVFFlat

How it filters. IVFFlat also post-filters. `ivfflat.iterative_scan = relaxed_order` probes more lists until enough rows pass, bounded by `ivfflat.max_probes` ([README, Iterative Scan Options](https://github.com/pgvector/pgvector#iterative-scan-options)). IVFFlat has no `strict_order` mode.

Filter from RLS and Scope. The same as HNSW: post-filter, any number of conditions.

Evidence. The README says IVFFlat "has faster build times and uses less memory than HNSW, but has lower query performance (in terms of speed-recall tradeoff)" ([README, IVFFlat](https://github.com/pgvector/pgvector#ivfflat)). I found no published recall numbers for IVFFlat under a selective filter. For a 100-row Tenant, the iterative scan has to probe lists until about ten of that Tenant's rows appear, which for a Tenant spread over the space means most lists. That is an inference, not a measurement.

Operational cost. IVFFlat needs data before the build ("Create the index after the table has some data") and the list count is tuned to the row count (`rows / 1000` up to 1M, `sqrt(rows)` above). Lists are trained once, so the index drifts as data changes and needs a rebuild. That conflicts with a migration chain that creates the index on an empty table.

Hosts. Part of pgvector, so the same as pgvector.

### halfvec, binary quantization and bit vectors with a re-rank

These change what is scanned, not how the filter works. They matter because they make the exact path cheaper.

- `halfvec` stores 2-byte floats and indexes up to 4,000 dimensions ([README, Half-Precision Vectors](https://github.com/pgvector/pgvector#half-precision-vectors)). An HNSW index can be built on an expression cast to `halfvec`.
- `binary_quantize(embedding)::bit(n)` can be indexed with `bit_hamming_ops`, and the README re-ranks the top 20 by the full vector ([README, Binary Quantization](https://github.com/pgvector/pgvector#binary-quantization)). The README's scaling advice is to use `halfvec` for tables and binary quantization with re-ranking for indexes ([README, Scaling](https://github.com/pgvector/pgvector#scaling)).
- The README recommends `PLAIN` storage for vectors ([README, Storing](https://github.com/pgvector/pgvector#storing)). A `vector(1536)` is about 6 KB and a `halfvec(1536)` about 3 KB, both above the 2 KB TOAST threshold, so without `PLAIN` they are stored out of line. A `bit(1536)` is 192 bytes and stays inline.

Relevance to the exact path. Prototype 1 measured exact scan through the Tenant B-tree at about 5.7 microseconds per row: 1.7 ms at 100 rows, 49 ms at about 6,000 rows, 1.1 s at 200,000. An exact Hamming scan over a stored `bit(1536)` column, followed by a full-precision re-rank of the top few hundred, touches about one thirtieth of the bytes. That might move the point where the exact path stops being cheap from thousands of rows to tens of thousands. This is unmeasured. The recall cost of binary quantization depends on the embedding model. A re-rank over 10 to 20 times K usually recovers most of it, but I found no primary source with numbers for 1,536-dimension text embeddings in Postgres, so treat it as unconfirmed.

Filter from RLS and Scope. On the exact path, Tenant and Scope are index conditions on the B-tree (if the policy is indexable, see above), and the Hamming distance is computed only for rows that pass. Quantization does not change the filter.

Hosts. Part of pgvector since 0.7.0, so the same as pgvector.

### pgvectorscale StreamingDiskANN and label filters

How it filters. Two ways ([pgvectorscale README, Filtered Vector Search](https://github.com/timescale/pgvectorscale#filtered-vector-search)):

- Label filtering, based on Filtered-DiskANN ([Gollapudi et al., WWW 2023](https://dl.acm.org/doi/10.1145/3543507.3583552)). The index is built as `USING diskann (embedding vector_cosine_ops, labels)` on a `smallint[]` column, and queries filter with `labels && ARRAY[...]`. Labels must fit in `smallint` (-32,768 to 32,767). The filter is applied inside the graph walk.
- Any other `WHERE` condition is a post-filter, but a streaming one: the scan keeps returning the next-closest row until the `LIMIT` is met, which Tiger Data says can mean "potentially even traversing the entire graph" ([Tiger Data blog](https://www.tigerdata.com/blog/how-we-made-postgresql-as-fast-as-pinecone-for-vector-data)). Result counts are therefore complete, unlike HNSW without iterative scan.

Filter from RLS. The label operator `&&` for `smallint[]` is defined by the extension as `smallint_array_overlap`, `IMMUTABLE STRICT PARALLEL SAFE`, without `LEAKPROOF` ([vectorscale--0.8.0--0.9.0.sql](https://github.com/timescale/pgvectorscale/blob/main/pgvectorscale/sql/vectorscale--0.8.0--0.9.0.sql)). So a label condition written in the query cannot become an index condition on an RLS table. A label condition written in the policy can, by the rule above. The policy would then have to read something like `labels && array[gnomon.tenant_label()] and tenant_id = gnomon.tenant_id() and ...`. This is inferred from the planner source and the extension's SQL and was not tested.

Second filter (Scope). `&&` means "shares any label", so two labels cannot express "this Tenant and this Scope". A combined (Tenant, Scope) label would need one `smallint` value per pair across the whole deployment, which caps the deployment at about 65,000 pairs. A workable shape might use labels as hash buckets: `label = hash(tenant_id) mod 32768`, with the exact Tenant and Scope conditions post-filtered by the streaming scan. That would emulate hash partitioning inside one index, with the bucket count held in data rather than in the partition DDL. Nobody has published recall for Filtered-DiskANN with tens of thousands of labels of a few hundred rows each, so this is a hypothesis to prototype.

Evidence. Tiger Data's headline benchmark is unfiltered: 50M Cohere vectors with 28x lower p95 latency than Pinecone s1 at 99% recall ([README](https://github.com/timescale/pgvectorscale)). Release 0.7.1 fixed "poor recall with label filters" ([releases](https://github.com/timescale/pgvectorscale/releases)). I found no published recall or latency numbers for label filtering.

Operational cost. The index stores SBQ-compressed vectors by default (`storage_layout = memory_optimized`) and rescores 50 candidates (`diskann.query_rescore`). Label-filtered indexes cannot be built in parallel ([README, index build](https://github.com/timescale/pgvectorscale#parallel-index-build-parameters)). Version 0.9.1 (2026-09-04) fixed type-confusion bugs that could crash a backend or read out of bounds, and indexes built on columns without a `vector(N)` typmod must be dropped and rebuilt ([0.9.1 release notes](https://github.com/timescale/pgvectorscale/releases/tag/0.9.1)). The extension is written in Rust with pgrx and depends on pgvector.

Hosts. See the matrix. pgvectorscale is not on every host, so it can only be a faster path.

### VectorChord (vchordrq) and its prefilter

How it filters. `vchordrq` is an IVF index with RaBitQ quantization and an optional hierarchy of lists ([VectorChord docs, Indexing](https://docs.vectorchord.ai/vectorchord/usage/indexing.html)). The scan does not stop at the probed lists when a filter removes rows: the docs say PostgreSQL "forces the index to continue to search to get 10 results, exhausting the entire search space" ([Similarity Filter](https://docs.vectorchord.ai/vectorchord/usage/similarity-filter.html)), so result counts are complete. Since 0.4.0, `SET vchordrq.prefilter = on` evaluates the filter before computing distances. The docs say "the filter does not need to have a specific form" and recommend it only when the filter is strict and cheap, reporting a 200% QPS gain at 1% selectivity and 5% at 10% on LAION-5m ([Prefilter](https://docs.vectorchord.ai/vectorchord/usage/prefilter.html)). `vchordrq.max_scan_tuples` bounds the scan before filtering.

Filter from RLS. In the source, the prefilter evaluates the index scan node's whole `ps.qual` expression against the heap tuple ([src/index/fetcher.rs](https://github.com/tensorchord/VectorChord/blob/main/src/index/fetcher.rs), `FilterableTuple::filter`). RLS conditions that are not index conditions end up in that list, so the prefilter applies the policy as well as query conditions. This is inferred from the source and was not tested.

Second filter (Scope). The prefilter takes any expression, so Tenant, Scope, and the As-of conditions are all evaluated before distance computation. It does not shrink the set of candidates visited: each candidate still needs a heap fetch to evaluate the filter. For a 100-row Tenant in 10M rows, the scan would visit most of the space, so a small Tenant or Scope would still want the B-tree exact path. That is an inference; I found no published numbers at that selectivity.

Evidence. VectorChord publishes the LAION-5m prefilter numbers above. A customer story reports 761 ms p50 for top-500 searches over 3.2 billion rows with PostGIS filters and partition pruning ([customer story source](https://github.com/tensorchord/pgvecto.rs-docs/blob/main/src/vectorchord/customer-stories/earthgenome.md)). I found no recall numbers for selective filters.

Operational cost. VectorChord must be in `shared_preload_libraries`, which needs a restart ([installation docs source](https://github.com/tensorchord/pgvecto.rs-docs/blob/main/src/vectorchord/getting-started/installation.md)). It is dual-licensed AGPLv3 or ELv2 ([README](https://github.com/tensorchord/VectorChord#license)). The list count is tuned to the row count, and k-means training takes time and memory ([Partitioning Tuning](https://github.com/tensorchord/pgvecto.rs-docs/blob/main/src/vectorchord/usage/partitioning-tuning.md)). Version 1.0.0 (2025-11-12) added a disk-based graph index, `vchordg`, still in preview ([Graph Index](https://github.com/tensorchord/pgvecto.rs-docs/blob/main/src/vectorchord/usage/graph-index.md)). The latest release is 1.1.1, 2026-02-28 ([releases](https://github.com/tensorchord/VectorChord/releases)).

Hosts. See the matrix.

### Lantern and pg_embedding

- pg_embedding is deprecated. Its README says: "As of Sept 29, 2023, Neon is no longer committing to pg_embedding" and recommends migrating to pgvector. The repository is archived ([neondatabase/pg_embedding](https://github.com/neondatabase/pg_embedding)).
- Lantern (`lantern_hnsw`, built on usearch) is not archived, but its last release is v0.5.0 on 2024-11-15 and its last push was 2024-12-12. Its README lists PostgreSQL 11 to 16 only ([lanterndata/lantern](https://github.com/lanterndata/lantern)). It filters the same way as HNSW. I found no formal deprecation notice, so treat it as inactive rather than deprecated. Neither is a candidate for a PG 17 floor.

### Partitioning

Hash partitioning on `tenant_id`.

- Fit with ADR-0006. One `CREATE TABLE ... PARTITION BY HASH` and N `PARTITION OF` statements in the first migration. No runtime DDL.
- The count is not quite frozen. Postgres allows partitions with different moduli as long as each modulus divides the next larger one, so one modulus-8 partition can be detached and replaced by two modulus-16 partitions and repopulated, one at a time ([CREATE TABLE, PARTITION OF](https://www.postgresql.org/docs/17/sql-createtable.html)). Each step is still DDL plus a data move, so under ADR-0006 it would be a published migration that the consumer runs.
- Scope. Hash partitioning by Tenant does nothing for Scope. A small Scope inside a large Tenant is a selective post-filter on that partition's HNSW index. Sub-partitioning by Scope is not possible without knowing the Scopes in advance.
- Prototype 1 shows recall is set by rows per partition, so recall falls as the table grows unless the count grows with it.

List partitioning on `tenant_id`. Each Tenant needs a `CREATE TABLE ... PARTITION OF ... FOR VALUES IN (...)`, which is runtime DDL and takes an `ACCESS EXCLUSIVE` lock on the parent ([CREATE TABLE](https://www.postgresql.org/docs/17/sql-createtable.html)). This conflicts with ADR-0006. pgvector's README recommends list partitioning for tenant isolation ([README, Multitenancy](https://github.com/pgvector/pgvector#multitenancy)), and the Postgres docs suggest hash when the number of customers may grow beyond what list partitioning handles ([5.12.6 Best Practices](https://www.postgresql.org/docs/17/ddl-partitioning.html)).

Tiered partitioning. Small Tenants share a default partition or a hash-partitioned sub-tree. Large Tenants get their own list partition. Promoting a Tenant means creating the partition and moving its rows out of the default. When a default partition exists, adding a partition scans the default to verify it holds no rows for the new partition, under an `ACCESS EXCLUSIVE` lock, unless a `CHECK` constraint on the default proves it ([5.12.2.2 Partition Maintenance](https://www.postgresql.org/docs/17/ddl-partitioning.html)). So the rows have to move first, then the partition is attached. That is runtime DDL plus a data move. Under ADR-0006 it could only be a generated migration that the consumer applies per promotion, or a privileged maintenance function that ADR-0006 currently forbids. Qdrant's tiered multitenancy is the reference design for this shape (section 5).

Indexes on partitions. An index created on the parent is created on every partition, including later ones. `CREATE INDEX CONCURRENTLY` is not allowed on the parent, so a new index on a large partitioned table is built per partition with `CONCURRENTLY` and attached ([5.12.2.2](https://www.postgresql.org/docs/17/ddl-partitioning.html)).

Pruning. Prototype 1 found that adding the Tenant as a literal in the query, as well as in the policy, cut the exact path from 4.1 ms to 1.8 ms at 100 rows, because the planner could prune at plan time. Postgres can also prune at executor start for values known then ([5.12.4 Partition Pruning](https://www.postgresql.org/docs/17/ddl-partitioning.html)).

### Per-Tenant partial indexes

How it filters. A partial HNSW index per Tenant is an index over that Tenant's rows only, so recall is that of an unfiltered search. Prototype 1 measured recall 1.00 at about 1.5 to 2.3 ms p50 for Tenants up to about 6,000 rows. AWS describes a customer that hash-partitions by firm and adds one partial HNSW index per firm ([AWS database blog, CORTO](https://aws.amazon.com/blogs/database/cortos-billion-scale-legal-semantic-search-with-aurora-postgresql-pgvector)).

Filter from RLS. The planner uses a partial index only if the query's conditions prove the index predicate. A policy condition `tenant_id = gnomon.tenant_id()` calls a stable function, which is not folded to a constant at plan time, so it cannot prove `tenant_id = '<literal>'`. The query would have to repeat the Tenant as a literal. This is my reading of how predicate proof works and was not tested here; prototype 1 forced the index.

Second filter (Scope). Only by creating one index per (Tenant, Scope) pair, or by post-filtering Scope inside the Tenant's index.

Operational cost. Every Tenant needs a `CREATE INDEX` at runtime, which ADR-0006 rules out. The Postgres docs warn against "a large set of non-overlapping partial indexes" because "the system does not understand the relationship among the partial indexes, and will laboriously test each one to see if it's applicable to the current query" ([11.8 Partial Indexes](https://www.postgresql.org/docs/17/indexes-partial.html)). With thousands of Tenants, planning cost grows with the index count.

A static partial index is different. `CREATE INDEX ... USING hnsw (...) WHERE recorded_to = 'infinity'` has a constant predicate, ships in a migration, and covers only current beliefs. See section 4.

### Other primary-source options

- AlloyDB ScaNN inline filtering. The AlloyDB planner picks pre-filtering, post-filtering or inline filtering by cost. Inline filtering builds a bitmap from a B-tree, GIN or GiST index and computes distances only for vectors in it. It works only with ScaNN ([AlloyDB filtered vector search](https://cloud.google.com/alloydb/docs/ai/filtered-vector-search-overview)). Adaptive filtering, in preview, switches strategy during execution ([adaptive filtering](https://docs.cloud.google.com/alloydb/docs/ai/adaptive-filtering)). No Google page mentions RLS, so whether a policy condition can feed the bitmap is unconfirmed. AlloyDB only.
- Azure pg_diskann. On Flexible Server, `diskann.iterative_search` defaults to `relaxed_order` and searches the graph in batches until the `LIMIT` is met, which is iterative post-filtering ([Azure docs, pg_diskann](https://learn.microsoft.com/en-us/azure/postgresql/flexible-server/how-to-use-pgdiskann)). Product quantization arrived in 0.6. Predicate pushdown into the graph walk ("advanced filtering") is documented only for Azure HorizonDB, in preview, which is a different product from Flexible Server ([HorizonDB DiskANN](https://learn.microsoft.com/en-us/azure/horizondb/ai/vector-index-diskann)). Azure only.
- ParadeDB vector search. Since 0.25.0, the `pg_search` index can hold pgvector `vector` columns and evaluate filters, text predicates and vector ordering in one index. It is a beta feature, and the docs recommend 0.27.0 or later ([ParadeDB vector overview](https://github.com/paradedb/paradedb/blob/main/docs/concepts/vector/overview.mdx)). The index can be partitioned internally by a column, and the docs name `tenant_id` as the multi-tenant choice, but partition boundaries are set at `CREATE INDEX` or `REINDEX` and are not rebalanced as data is inserted ([partition-by.mdx](https://github.com/paradedb/paradedb/blob/main/docs/reference/indexing/partition-by.mdx)). Whether an RLS policy condition is pushed into the vector scan is unconfirmed.
- Aurora and RDS. AWS's guidance is pgvector with a Tenant B-tree, iterative scans, and partitioning by Tenant; it adds no Aurora-only filter mechanism ([AWS, running pgvector in production on Aurora](https://aws.amazon.com/blogs/database/running-pgvector-in-production-on-amazon-aurora-postgresql/)).

## 2. Hybrid retrieval in Postgres

The user's decisions set the split: the Store returns candidates per method under RLS and fuses them, and the Pipeline reranks with a model. Full-text search also has to cover the gap while an embedding is pending, so it must be maintained in the write's own transaction.

### tsvector full-text search

Same-transaction maintenance. A stored generated column `tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED` is recomputed on every insert and update in the writing transaction. The two-argument form of `to_tsvector` is required, because the one-argument form depends on `default_text_search_config` and is not immutable ([PostgreSQL docs, 12.2.2 Creating Indexes](https://www.postgresql.org/docs/17/textsearch-tables.html)). A trigger works too. Both are core Postgres, so they work on every host, and Supabase documents exactly this column in its hybrid search guide ([Supabase, Hybrid search](https://supabase.com/docs/guides/ai/hybrid-search)). Versions are immutable in gnomon, so the column is computed once per Version.

Interaction with RLS. This is the largest finding in this section. `@@` is `ts_match_vq`, which is not leakproof ([pg_proc.dat](https://github.com/postgres/postgres/blob/REL_17_STABLE/src/include/catalog/pg_proc.dat)). Under RLS a query condition `fts @@ query` therefore cannot be a GIN index condition. Postgres scans the rows that pass the policy, through the Tenant B-tree, and evaluates `@@` on each ([PostgreSQL docs, Rules and Privileges](https://www.postgresql.org/docs/current/rules-privileges.html)). Tom Lane confirmed this for text search on pgsql-bugs: the rules for marking things leakproof "are pretty strict and TS search operators don't qualify" ([pgsql-bugs #18941](https://www.postgresql.org/message-id/flat/18941-90236c38c6101bf0%40postgresql.org)). A composite `btree_gin` index on `(tenant_id, fts)` does not help, because the `fts` part still cannot be used. The GIN index on an RLS table adds write cost and is not used by an invoker-mode read.

The cost is linear in the size of the visible set, not the size of the match set. For a 100-row Scope that is nothing. For a 200,000-row Tenant it is a scan of 200,000 tsvectors per query. This is the same shape as the exact vector path, and it is unmeasured.

Ways around it, none of them free:

- A `SECURITY DEFINER` candidate function that applies the Tenant and Scope conditions itself and returns only ids and ranks, with the caller reading the rows back under RLS. This restores the GIN index. ADR-0009 allows only Erasure and Kind registration to be definers, so this would amend ADR-0009, and the function's own Tenant check becomes a second barrier.
- A token table maintained by trigger: `(tenant_id, scope, lexeme, item_id)` with a B-tree, queried with `lexeme = any($terms)`. `texteq` is leakproof, so the condition is an index condition under RLS. This is a hand-built inverted index with no ranking of its own. It is my design, not a published one.
- An ordered BM25 scan (below), on the hosts that have one.

Ranking. `ts_rank` and `ts_rank_cd` use only the document and the query. They have no corpus-wide statistics, so they cannot leak information across Tenants, and they do not give BM25-quality ranking ([PostgreSQL docs, 12.3.3 Ranking](https://www.postgresql.org/docs/17/textsearch-controls.html)).

### BM25: ParadeDB pg_search and Tiger Data pg_textsearch

Availability. Neither is on Supabase or DigitalOcean. pg_search is on none of the eight hosts, and Neon retired it from existing projects on 2026-09-21 ([Neon, pg_search](https://neon.com/docs/extensions/pg_search)). ParadeDB itself recommends running it as a logical-replication subscriber beside a managed primary ([ParadeDB logical replication](https://www.paradedb.com/docs/operate/deploy/logical-replication/getting-started)). pg_textsearch is in public preview on Cloud SQL and AlloyDB ([Cloud SQL pg_textsearch](https://docs.cloud.google.com/sql/docs/postgres/pg-textsearch)). Both need `shared_preload_libraries` ([ParadeDB install](https://github.com/paradedb/paradedb/blob/main/docs/start/install.mdx), [pg_textsearch README](https://github.com/timescale/pg_textsearch)).

Same-transaction maintenance. Both are index access methods, so the index changes in the writing transaction. ParadeDB states that writes to indexed columns "are atomic, consistent, and respect Postgres' isolation levels" ([ParadeDB guarantees](https://github.com/paradedb/paradedb/blob/main/docs/concepts/guarantees.mdx)). pg_textsearch writes to a WAL-logged memtable ([README, Memtable Architecture](https://github.com/timescale/pg_textsearch)). ParadeDB Community indexes are not physically replicated, and "after a failover, the promoted server needs a REINDEX" ([ParadeDB guarantees](https://github.com/paradedb/paradedb/blob/main/docs/concepts/guarantees.mdx)).

Interaction with RLS.

- pg_textsearch ranks through an ordering operator, `ORDER BY content <@> 'query'`. An ordering operator is not a restriction, so the leakproof rule does not block the index scan. Other conditions, the policy included, are post-filters, and "post-filtered scans automatically grow their internal scoring batch until the LIMIT is filled or matches are exhausted" ([README, Pre-filtering and Post-filtering](https://github.com/timescale/pg_textsearch)). That it behaves this way under RLS is inferred from the planner rules and was not tested.
- Both extensions compute BM25 statistics over every row in the index, including rows hidden by RLS. pg_textsearch says "a user who already knows a term can infer frequency information affected by inaccessible rows" ([README, Row-Level Security](https://github.com/timescale/pg_textsearch)). ParadeDB says "a score returned to a restricted role can depend on rows that role cannot read" and advises a separate table and index if that matters ([score.mdx](https://github.com/paradedb/paradedb/blob/main/docs/reference/full-text/score.mdx)). For gnomon, a large Tenant's vocabulary would shift a small Tenant's scores, and a Tenant could probe term frequencies in other Tenants. The scores would not be stable either, because they change when another Tenant writes.
- ParadeDB 0.26.6 and 0.26.7 changed how RLS policy conditions interact with its join scan and scores ([0.26.7 changelog](https://github.com/paradedb/paradedb/blob/main/docs/project/changelog/0.26.7.mdx)). Its aggregate API rejects callers when RLS applies ([0.26.5 changelog](https://github.com/paradedb/paradedb/blob/main/docs/project/changelog/0.26.5.mdx)).

### Trigram (pg_trgm)

pg_trgm is on every host (matrix). It suits mention resolution: matching a name such as "Acme Corp." against a Node called "ACME Corporation".

- The similarity operators `%`, `<%` and `%>` and `LIKE` are not leakproof, so under RLS they are post-filters, and a trigram GIN or GiST index is not used for them ([PostgreSQL docs, Rules and Privileges](https://www.postgresql.org/docs/current/rules-privileges.html), [pg_proc.dat](https://github.com/postgres/postgres/blob/REL_17_STABLE/src/include/catalog/pg_proc.dat)).
- The distance operator `<->` supports nearest-neighbour ordering through a GiST index (`ORDER BY name <-> 'query' LIMIT k`) ([PostgreSQL docs, pg_trgm](https://www.postgresql.org/docs/17/pgtrgm.html)). As an ordering operator, it is not blocked by the leakproof rule, so it behaves like a vector index under RLS: the policy post-filters the ordered stream. For a small Scope that is the same recall problem as HNSW, but without an iterative-scan budget, because GiST keeps returning rows until the `LIMIT` is met. This is inferred and was not tested.
- For small visible sets, a scan through the Tenant and Scope B-tree with `similarity()` computed per row is exact and cheap.

### Reciprocal rank fusion in SQL

RRF scores each candidate as the sum over methods of `1 / (k + rank)`, with `k = 60` in the original paper ([Cormack, Clarke and Buettcher, SIGIR 2009](https://plg.uwaterloo.ca/~gvcormac/cormacksigir09-rrf.pdf)). Three published SQL shapes:

- pgvector-python: one CTE per method with `RANK() OVER (ORDER BY ...)` and `LIMIT 20`, joined with `FULL OUTER JOIN` and scored with `COALESCE(1.0 / (k + rank), 0.0)` per side ([rrf.py](https://github.com/pgvector/pgvector-python/blob/master/examples/hybrid_search/rrf.py)).
- Supabase: the same shape inside a SQL function, with per-method weights, `rrf_k = 50` by default, and each branch limited to twice the requested count ([Supabase, Hybrid search](https://supabase.com/docs/guides/ai/hybrid-search)).
- ParadeDB: one CTE per branch, then `UNION ALL` of `weight / (60 + rank)` and `GROUP BY id` with `sum()`, with `id` as a tie-breaker in every `ORDER BY` so the result is deterministic ([ParadeDB, RRF](https://github.com/paradedb/paradedb/blob/main/docs/reference/hybrid/rrf.mdx)).

The `UNION ALL` and `GROUP BY` shape extends to a third branch (graph neighbourhood) without nested full outer joins. Each branch is its own query under RLS, so each one gets the same Tenant and Scope conditions and the same As-of conditions. The fusion step itself reads nothing new. Under `relaxed_order`, the vector branch must re-sort inside a materialized CTE before `RANK()` is taken ([pgvector README, Iterative Index Scans](https://github.com/pgvector/pgvector#iterative-index-scans)).

### Reranker

The reranker is a model call, so it belongs to the Pipeline (ADR-0001). It sees only rows that the Store already returned under RLS, so it adds no isolation risk inside the database. It does send Version text to the configured reranker endpoint, which ADR-0001 already leaves to the consumer's endpoint configuration. pgvector-python has a cross-encoder example of the same split ([cross_encoder.py](https://github.com/pgvector/pgvector-python/blob/master/examples/hybrid_search/cross_encoder.py)).

## 3. How graph-memory systems search a knowledge graph

Source read at these commits on 2026-11-21: graphiti `9b8a463` (v0.31.2), graphrag `34b2b67` (v3.3.0), LightRAG `46984a4` (v1.5.8rc2), cognee `1961133` (v1.6.10), and mem0 `fff8a64`, with the graph code quoted from tag v1.0.11, the last release that had graph memory.

### Graphiti and Zep

#### What it searches

Graphiti runs four searches side by side, one per object type: entity edges (facts), entity nodes, episodes and communities ([search.py, `search()`](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/search/search.py#L98)). Edges and nodes can be searched by cosine, BM25 and breadth-first search. Communities can be searched by cosine and BM25. Episodes are searched by BM25 only ([search_config.py](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/search/search_config.py)). The Zep paper describes the same three search functions, with BM25 on the fact for edges, the name for nodes and the name for communities (section 3.1, [arXiv 2501.13956](https://arxiv.org/html/2501.13956v1)).

#### What it embeds

- Each `EntityEdge` stores a `fact_embedding` of its `fact` sentence ([edges.py](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/edges.py#L228)).
- Each `EntityNode` stores a `name_embedding` of its name only, not its summary ([nodes.py](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/nodes.py#L427)). Each `CommunityNode` also stores a `name_embedding` ([nodes.py](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/nodes.py#L715)).
- A node's summary is reached only through the full-text index, which covers name and summary ([graph_queries.py](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/graph_queries.py#L101)).

#### How it combines results

- Each method fetches `2 * limit` candidates, and one reranker merges them per object type ([search.py, `edge_search`](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/search/search.py#L259)).
- The rerankers are `rrf`, `mmr`, `node_distance`, `episode_mentions` and `cross_encoder`. Graphiti's `rrf()` scores each item as the sum of `1 / (rank + 1)`, so its constant is 1, not 60 ([search_utils.py](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/search/search_utils.py#L1551)).
- `node_distance` ranks results one hop from a given centre node first. `episode_mentions` counts the `MENTIONS` edges pointing at each node.
- The cross-encoder takes the top `2 * limit` by RRF and ranks the fact strings or names. The default client scores with OpenAI logprobs, and BGE and Gemini clients exist ([cross_encoder/](https://github.com/getzep/graphiti/tree/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/cross_encoder)).
- `Graphiti.search()` defaults to edges only with BM25 and cosine fused by RRF, or node distance when the caller names a centre node ([graphiti.py](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/graphiti.py#L2284)). `search_()` defaults to all four object types with BFS and a cross-encoder ([search_config_recipes.py](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/search/search_config_recipes.py#L81)).

#### Time

An edge has `valid_at`, `invalid_at`, `expired_at` and `reference_time` ([edges.py](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/edges.py#L228)). The source describes `expired_at` as transaction time and `valid_at` and `invalid_at` as world time. The paper calls the two timelines T′ (ingestion order) and T (event order), and says a contradicted edge is invalidated by setting its t_invalid to the new edge's t_valid (sections 2.1 and 2.2.3, [arXiv 2501.13956](https://arxiv.org/html/2501.13956v1)).

Search does not filter on time by default. `SearchFilters()` is empty unless the caller passes date filters on `valid_at`, `invalid_at`, `created_at` or `expired_at`, as OR-of-AND lists ([search_filters.py](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/search/search_filters.py#L55)). When given, they become `WHERE` conditions in the same query as the similarity score. Invalidated facts are returned with their date ranges written into the context string, so the model reasons about time ([search_helpers.py](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/search/search_helpers.py#L22)).

#### Tenant scoping and backing stores

Graphiti supports Neo4j, FalkorDB and Amazon Neptune, with no Postgres backend in the core ([driver.py](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/driver/driver.py#L59)). The tenant key is `group_id`.

- Graphiti creates no vector index. Cosine search is `MATCH ... WHERE e.group_id IN $group_ids`, then `vector.similarity.cosine(...)` per matching row, then `ORDER BY score LIMIT` ([search_utils.py, `edge_similarity_search`](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/search/search_utils.py#L289)). The tenant is a filter before ranking, and the search is exact and linear in the tenant's size. On Neptune the embeddings are fetched and compared in Python.
- For BM25, `group_id` is a field of each Lucene index and `fulltext_query()` adds `group_id:"..."` to the query, so the tenant filter runs inside the index ([search_utils.py](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/search/search_utils.py#L85)).
- On FalkorDB each `group_id` is a separate graph, and a search over several groups runs once per group ([decorators.py](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/decorators.py#L29), [falkordb_driver.py](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/driver/falkordb_driver.py#L342)).
- Zep's hosted service runs on its own graph database ([README](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/README.md)). How it indexes is not public and is unconfirmed.

#### What it suggests for gnomon

- The Edge is the main search unit, reached through a fact sentence. gnomon's Edge Versions hold properties, not a sentence, so either the Pipeline writes searchable text per Edge Version or the Store builds it from the Kind, the endpoint names and the properties.
- Embed Node names, and put Summaries in the lexical index.
- Search each object type separately and fuse in the Store. The cross-encoder belongs to the Pipeline's reranker role.
- Graphiti's exact, tenant-first cosine scan is design A in this note. Its date filters run in the same query as the ranking, which gnomon can do on its four scalar columns.

### Microsoft GraphRAG: local, global and DRIFT search

#### What it embeds

Three embeddings by default: `entity_description`, `community_full_content` (the community report) and `text_unit_text` (chunks) ([config/embeddings.py](https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag/graphrag/config/embeddings.py)). Relationships are not embedded.

#### Local search

1. `map_query_to_entities` runs a vector search over entity descriptions, oversampling by 2x ([entity_extraction.py](https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag/graphrag/query/context_builder/entity_extraction.py#L41)).
2. `LocalSearchMixedContext.build_context` collects the communities, relationships, text units and covariates linked to those entities.
3. It fills the context window by fixed token shares: `community_prop=0.25`, `text_unit_prop=0.5`, and the rest for entities and relationships ([mixed_context.py](https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag/graphrag/query/structured_search/local_search/mixed_context.py#L91)).

There is no score fusion. Graph links choose the candidates and the token budget decides what fits ([local search docs](https://microsoft.github.io/graphrag/query/local_search/)).

#### Global search

Global search is LLM map-reduce over community reports ([global_search/search.py](https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag/graphrag/query/structured_search/global_search/search.py)). The map step asks for points with a 0 to 100 importance score ([map prompt](https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag/graphrag/prompts/query/global_search_map_system_prompt.py#L21)), and the reduce step combines them. Optional dynamic community selection has an LLM rate each report and keeps those above a threshold ([dynamic_community_selection.py](https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag/graphrag/query/context_builder/dynamic_community_selection.py#L73)). The paper describes the same pattern ([arXiv 2404.16130](https://arxiv.org/abs/2404.16130)). No vector index is involved.

#### DRIFT search

1. The primer expands the query with HyDE and embeds the expansion ([primer.py](https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag/graphrag/query/structured_search/drift_search/primer.py#L114)).
2. It ranks every community report by cosine, in pandas over all reports, and keeps the top `drift_k_followups` ([drift_context.py](https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag/graphrag/query/structured_search/drift_search/drift_context.py#L216)).
3. It answers from those reports and generates follow-up questions, each run as a local search.

Microsoft Research reports DRIFT beating local search on comprehensiveness 78% of the time and on diversity 81%, on 50 questions over more than 5,000 AP articles, judged by an LLM, with two iterations ([Microsoft Research blog](https://www.microsoft.com/en-us/research/blog/introducing-drift-search-combining-global-and-local-search-methods-to-improve-quality-and-efficiency/)).

#### Time, tenancy and stores

I found no temporal model and no tenant concept in the query code. One index is one dataset. The vector stores are LanceDB, Azure AI Search and Cosmos DB, with no Postgres store ([graphrag_vectors/](https://github.com/microsoft/graphrag/tree/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag-vectors/graphrag_vectors)). A generic filter expression exists, and LanceDB compiles it with `prefilter=True` ([lancedb.py](https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag-vectors/graphrag_vectors/lancedb.py#L212)).

#### What it suggests for gnomon

- A Node's Summary is what GraphRAG embeds to find entry points, and a Community's Summary is what it searches for global questions.
- Global questions are not a vector-index problem. They are a scan of a Tenant's Communities, bounded by how many Communities the Tenant has.
- Local search has the common shape: vector search for seed Nodes, then graph expansion, which in gnomon is Store traversal under RLS.

### LightRAG

#### What it searches

An LLM extracts `low_level_keywords` (specific entities) and `high_level_keywords` (themes) from the query ([prompt.py](https://github.com/HKUDS/LightRAG/blob/46984a451b513e4684a12cf3dafde52df04a1ae2/lightrag/prompt.py#L489)). Low-level keywords are matched against an entities vector store (`local` mode), high-level keywords against a relationships vector store (`global` mode), `hybrid` does both, and `mix` adds chunk search ([operate.py, `_perform_kg_search`](https://github.com/HKUDS/LightRAG/blob/46984a451b513e4684a12cf3dafde52df04a1ae2/lightrag/operate.py#L5954)). The paper calls this dual-level retrieval with one-hop neighbours (section 3.2, [arXiv 2410.05779](https://arxiv.org/html/2410.05779v3)).

Entities are embedded as `"{name}\n{description}"` and relations as `"{keywords}\t{src}\n{tgt}\n{description}"` ([operate.py](https://github.com/HKUDS/LightRAG/blob/46984a451b513e4684a12cf3dafde52df04a1ae2/lightrag/operate.py#L2158), [operate.py](https://github.com/HKUDS/LightRAG/blob/46984a451b513e4684a12cf3dafde52df04a1ae2/lightrag/operate.py#L2688)).

#### How it combines results

Local and global results are interleaved round-robin and deduplicated. Related edges are sorted by degree, then weight ([operate.py](https://github.com/HKUDS/LightRAG/blob/46984a451b513e4684a12cf3dafde52df04a1ae2/lightrag/operate.py#L7159)). Chunks from all sources are merged round-robin ([`_merge_all_chunks`](https://github.com/HKUDS/LightRAG/blob/46984a451b513e4684a12cf3dafde52df04a1ae2/lightrag/operate.py#L6554)), and an optional reranker model reorders them ([utils.py, `process_chunks_unified`](https://github.com/HKUDS/LightRAG/blob/46984a451b513e4684a12cf3dafde52df04a1ae2/lightrag/utils.py#L7635)). There is no RRF and no temporal filter.

#### Tenancy and Postgres

A `workspace` column scopes every row of one shared table. Vector queries are `WHERE workspace = $1 AND content_vector <=> $q < $t ORDER BY content_vector <=> $q LIMIT $k`, over HNSW, HNSW on `halfvec`, IVFFlat or VectorChord `vchordrq` ([postgres_impl.py](https://github.com/HKUDS/LightRAG/blob/46984a451b513e4684a12cf3dafde52df04a1ae2/lightrag/kg/postgres_impl.py#L10913)). The graph can live in plain Postgres tables ([pgtable_impl.py](https://github.com/HKUDS/LightRAG/blob/46984a451b513e4684a12cf3dafde52df04a1ae2/lightrag/kg/pgtable_impl.py)) or in Apache AGE.

LightRAG's design note [PgVectorFilteredSearch.md](https://github.com/HKUDS/LightRAG/blob/46984a451b513e4684a12cf3dafde52df04a1ae2/docs/design/PgVectorFilteredSearch.md) describes the defect prototype 1 found. It sets `SET LOCAL hnsw.iterative_scan = strict_order` inside each query's own transaction, citing transaction poolers, raises `hnsw.ef_search` to `top_k` with `set_config(..., true)` capped at 1000, and uses `relaxed_order` with a re-sort for IVFFlat ([postgres_impl.py](https://github.com/HKUDS/LightRAG/blob/46984a451b513e4684a12cf3dafde52df04a1ae2/lightrag/kg/postgres_impl.py#L345)). Its numbers, on 212,240 rows of 16-dimension random vectors with `LIMIT 20`:

| Workspace share | Rows returned, iterative scan off | Rows returned, `strict_order` |
|---|---|---|
| 0.09% (200 rows) | 0 | 16, at 31 ms |
| 0.02% (40 rows) | 0 | 3 |

At 0.02%, all 20 rows also needed a raised `max_scan_tuples` and `work_mem`, at 568 ms. The note calls the fix "a mitigation, not a guarantee", defers partitioning and per-workspace partial indexes because they need DDL per workspace, and points at Qdrant's tenant index for many or uneven workspaces.

#### What it suggests for gnomon

- Separate indexes for Node text and Edge text, chosen from the query. The keyword step would be a Pipeline model call.
- Embed a Node's name with its Summary.
- LightRAG is the closest Postgres comparator. Its maintainers reached prototype 1's diagnosis, chose `strict_order` where prototype 1 chose `relaxed_order`, and left the structural fix open.

### Cognee

#### What it searches

The default search type is `HYBRID_COMPLETION`, which combines passages with the neighbourhoods of matching entities ([SearchType.py](https://github.com/topoteretes/cognee/blob/1961133d0c68b450fe8e71941a756371e94f7c04/cognee/modules/search/types/SearchType.py)). `GRAPH_COMPLETION` runs `brute_force_triplet_search` ([brute_force_triplet_search.py](https://github.com/topoteretes/cognee/blob/1961133d0c68b450fe8e71941a756371e94f7c04/cognee/modules/retrieval/utils/brute_force_triplet_search.py#L232)):

1. It vector-searches the collections `Entity_name`, `TextSummary_text`, `EntityType_name`, `DocumentChunk_text` and `EdgeType_relationship_name`, 100 candidates each.
2. It loads the matching subgraph into memory.
3. It scores each triplet as the sum of the two node distances and the edge distance, weighted by importance and feedback, with a penalty distance of 6.5 for an unmatched element, and returns the k smallest ([CogneeGraph.py](https://github.com/topoteretes/cognee/blob/1961133d0c68b450fe8e71941a756371e94f7c04/cognee/modules/graph/cognee_graph/CogneeGraph.py#L480)).

The hybrid type ranks chunk and summary pairs with RRF, with k between 30 and 60 ([hybrid/ranking.py](https://github.com/topoteretes/cognee/blob/1961133d0c68b450fe8e71941a756371e94f7c04/cognee/modules/retrieval/hybrid/ranking.py)). BM25 runs in the process over loaded chunks ([bm25_retriever.py](https://github.com/topoteretes/cognee/blob/1961133d0c68b450fe8e71941a756371e94f7c04/cognee/modules/retrieval/bm25_retriever.py)).

#### Time

`TEMPORAL` has an LLM extract the query's time window, finds `Timestamp` nodes in it, and moves the chunks attached to them to the front. Time is a rerank, not a filter ([temporal_hybrid_retriever.py](https://github.com/topoteretes/cognee/blob/1961133d0c68b450fe8e71941a756371e94f7c04/cognee/modules/retrieval/temporal_hybrid_retriever.py)).

#### Tenancy and Postgres

With access control on, Cognee isolates datasets physically. The default pgvector handler runs `CREATE DATABASE` per dataset ([PGVectorDatasetDatabaseHandler.py](https://github.com/topoteretes/cognee/blob/1961133d0c68b450fe8e71941a756371e94f7c04/cognee/infrastructure/databases/vector/pgvector/PGVectorDatasetDatabaseHandler.py)). An alternative creates one schema per dataset and pins `search_path` to it ([PGVectorSharedDatasetDatabaseHandler.py](https://github.com/topoteretes/cognee/blob/1961133d0c68b450fe8e71941a756371e94f7c04/cognee/infrastructure/databases/vector/pgvector/PGVectorSharedDatasetDatabaseHandler.py)). Collections can be hash-partitioned by `id`, not by tenant. HNSW uses `m=8` with `ef_search` of at least 100, and a source comment reports recall@10 of 0.971 there ([PGVectorAdapter.py](https://github.com/topoteretes/cognee/blob/1961133d0c68b450fe8e71941a756371e94f7c04/cognee/infrastructure/databases/vector/pgvector/PGVectorAdapter.py#L80)). Every query is local to its tenant, at the cost of runtime DDL per dataset.

#### What it suggests for gnomon

- Scoring a whole triplet ranks an Edge by its endpoints too. It needs Node and Edge vectors from the same query.
- A database or schema per dataset conflicts with ADR-0006.
- Cognee's temporal mode only reorders results, which is weaker than an As-of read.

### mem0 graph memory

#### Status

The open-source graph memory was removed: "Graph memory is removed from OSS" and moved to the hosted Platform ([oss-v2-to-v3.mdx](https://github.com/mem0ai/mem0/blob/fff8a6472512d1bc1a4bccab8c645b65a1df7fe5/docs/migration/oss-v2-to-v3.mdx#L19)). v1.0.11 still has it.

#### The last open-source graph search, v1.0.11

1. An LLM extracts entities from the query.
2. Each entity is embedded and matched against node embeddings in Neo4j with `vector.similarity.cosine`, scoped by `user_id`, `agent_id` and `run_id`, with a 0.7 threshold.
3. It returns relations where `r.valid IS NULL OR r.valid = true`.
4. It reranks the triples with in-process BM25 and keeps the top 5 ([graph_memory.py](https://github.com/mem0ai/mem0/blob/v1.0.11/mem0/memory/graph_memory.py#L96)).

The scoping is a filter before an exact scan. The Apache AGE backend fetches every node embedding for the user and computes cosine in Python ([apache_age_memory.py](https://github.com/mem0ai/mem0/blob/v1.0.11/mem0/memory/apache_age_memory.py#L153)). The paper's Mem0g marks conflicting relationships invalid rather than deleting them, and reports LOCOMO J scores of 68.44 for Mem0g, 66.88 for Mem0 and 65.99 for Zep, with search p50 of 0.476 s, 0.148 s and 0.513 s (section 2.2 and Table 2, [arXiv 2504.19413](https://arxiv.org/html/2504.19413v1)).

#### Current open-source retrieval

`_search_vector_store` over-fetches `max(limit*4, 60)` semantic results with scope filters, adds BM25 normalized by a sigmoid and an entity boost of weight 0.5, and sums the three scores ([main.py](https://github.com/mem0ai/mem0/blob/fff8a6472512d1bc1a4bccab8c645b65a1df7fe5/mem0/memory/main.py#L1642), [scoring.py](https://github.com/mem0ai/mem0/blob/fff8a6472512d1bc1a4bccab8c645b65a1df7fe5/mem0/utils/scoring.py#L60)). It drops expired memories in Python after the vector search, so an ANN result set can be thinned after the fact. The pgvector store filters on JSONB keys such as `payload->>'user_id'` over HNSW or pgvectorscale `diskann`, with a GIN `to_tsvector` index, and sets no iterative scan ([pgvector.py](https://github.com/mem0ai/mem0/blob/fff8a6472512d1bc1a4bccab8c645b65a1df7fe5/mem0/vector_stores/pgvector.py#L285)). The recall loss that implies is inferred from the code and not measured. The Platform's graph memory links memories through shared entities and boosts ranking ([graph-memory.mdx](https://github.com/mem0ai/mem0/blob/fff8a6472512d1bc1a4bccab8c645b65a1df7fe5/docs/platform/features/graph-memory.mdx)); its implementation is not public.

#### What it suggests for gnomon

- Marking relations invalid instead of deleting them matches Retraction and End of validity.
- Filtering by time after the vector search, in application code, is the pattern to avoid.
- mem0 moved from a graph store to entity mentions as a ranking boost. That hints that Edges and Citations may serve better as rerank signals than as the main search unit, though it is one data point.

### Patterns across the five systems

| System | Objects searched | Lexical | Vector | Graph | Fusion | Time at search | Tenant scope |
|---|---|---|---|---|---|---|---|
| Graphiti | edges (fact), nodes (name), communities (name), episodes | Lucene BM25 with a group_id term | exact cosine after a group_id filter, no ANN index | BFS, node distance | RRF (k=1), MMR, mentions, cross-encoder | optional WHERE on four dates, off by default | group_id condition, or one graph per group |
| GraphRAG | entity descriptions, community reports, chunks | none | LanceDB with prefilter, and others | links from entities | token budget shares, LLM map-reduce | none | one index per dataset |
| LightRAG | entities (name and description), relations, chunks | none | pgvector HNSW with strict_order | one hop, degree sort | round-robin, optional reranker | none | workspace column in a shared table |
| Cognee | entity names, summaries, types, chunks, edge types | in-process BM25 | pgvector, one database or schema per dataset | in-memory triplet scoring | summed distances, RRF for hybrid | Timestamp-node rerank | database or schema per dataset |
| mem0 v1.0.11 | entity nodes, then triples | BM25 rerank of triples | exact cosine on user-scoped nodes | one hop | BM25 top 5 | `valid` flag | user, agent and run properties |

### What this suggests for Nodes, Edges, Versions, Summaries and Communities

- Edges. All five systems search relationships, through a fact or relation text (Graphiti, LightRAG, Mem0g) or through expansion from seed entities (GraphRAG, Cognee). gnomon needs searchable text per Edge Version, and who writes it is a design question.
- Nodes. The embedding input is the name (Graphiti, Cognee, mem0) or the name with a description (GraphRAG, LightRAG). In gnomon the description is the Summary, which Erasure can remove until the Pipeline regenerates it, so a name embedding is the more stable choice for mention resolution.
- Summaries and Communities. GraphRAG and Graphiti index them separately. GraphRAG's global and DRIFT modes scan every community report per query with no ANN index.
- Versions. No system embeds history per version. Graphiti keeps invalidated edges in the same index and filters dates only when asked. Mem0g excludes invalid relations with a flag. I found no system that filters on valid time and recorded time together with a vector search.
- Fusion. RRF per object type is the common cheap step, with a cross-encoder or LLM after it. Graph signals such as node distance, mention counts and degree need Store traversal under RLS.
- Tenancy. The Postgres-backed systems use an exact scan after a tenant condition (Graphiti's pattern, mem0 v1), a shared ANN index with iterative scan (LightRAG), or a database or schema per tenant (Cognee). These are designs A, B and a variant that ADR-0006 rules out.

## 4. Search and bi-temporality

### What an As-of search has to filter

An As-of search at valid-at `v` and known-at `k` keeps a Version when `valid_from <= v < valid_to` and `recorded_from <= k < recorded_to`, on the four scalar columns of ADR-0004. Those are query conditions, not policy conditions. The `timestamptz` comparison functions are leakproof ([pg_proc.dat](https://github.com/postgres/postgres/blob/REL_17_STABLE/src/include/catalog/pg_proc.dat)), which is why ADR-0004 could use B-tree indexes on them under RLS.

With a vector index ordered by distance, the As-of conditions are post-filters like the policy. They multiply the selectivity of Tenant and Scope. A search at "now" on the current belief removes every closed Version. A search at a past known-at removes every Version recorded later. In both cases the iterative scan budget is shared by all four filters. On the exact path, the B-tree on Tenant and Scope selects the rows and the As-of conditions are evaluated per row, so the cost stays linear in the visible set.

### Embedding per Version against embedding per Node

Per Version.

- Correctness. Each Version carries the text it had when it held, so an As-of search ranks the state that held at `(v, k)`. This is the only option that gives correct results for past reads.
- Row count. Embedding rows grow with Versions, not Nodes. Many Versions differ only in time bounds: a Refinement or an End of validity changes no property text. A content hash on the text that is embedded lets those Versions share one embedding row, through a Version-to-embedding link. That keeps the vector count near the number of distinct texts. The cost is a join between the vector scan and the Version table for the As-of conditions. A nested loop under a `LIMIT` keeps pulling from the ordered index scan, so the iterative scan still applies, but the join conditions then count against `hnsw.max_scan_tuples`.
- Duplicates. Versions of one Node usually have near-identical vectors and crowd the top of the ranking. The Store would take `DISTINCT ON` the Node after the scan, and each duplicate uses up scan budget.
- Current-only index. A static partial index, `CREATE INDEX ... USING hnsw (embedding vector_cosine_ops) WHERE recorded_to = 'infinity'`, covers only current beliefs. The predicate is a constant, so it ships in the migration chain and fits ADR-0006. A search at known-at "now" that repeats `recorded_to = 'infinity'` in the query can use it. The `=` on `timestamptz` is leakproof, and a literal predicate proves the index predicate. Past known-at searches fall back to the full index or the exact path. Closing a Version updates `recorded_to`, which in Postgres is a new heap tuple, so the old one leaves the partial index on the next vacuum. This is inferred, not tested.

Per Node, with a temporal filter.

- The embedding describes the Node's current text, and the temporal filter only decides whether the Node existed at `(v, k)`. A search at a past instant would rank the Node by what it says now. For a bi-temporal graph that is a wrong answer, not an approximation.
- It is smaller and needs no re-embedding on Refinement, but every Correction or property change rewrites the Node's embedding, so the Node's vector changes under past reads.
- Graphiti embeds facts (its edges) and entity names separately and filters on time (section 3), which is closer to per-Version embedding for facts and per-Node embedding for names.

Per-Node name embeddings for mention resolution are a separate case. Resolving "Acme" to an existing Node needs high recall on small Tenants, and the name changes rarely. An index of names, aliases and their embeddings per Node, maintained as current-only, fits that use better than per-Version text.

### Eventual consistency of embeddings

The write commits first and the Pipeline adds the embedding afterwards. During that gap the Version has a tsvector, maintained in the write's transaction, but no vector. A search therefore needs the lexical branch for recent Versions. RRF already handles a candidate that appears in only one branch: it scores 0 for the missing branch ([rrf.py](https://github.com/pgvector/pgvector-python/blob/master/examples/hybrid_search/rrf.py)). pgvector does not index `NULL` vectors ([README, troubleshooting](https://github.com/pgvector/pgvector#why-are-there-less-results-for-a-query-after-adding-an-hnsw-index)), so a nullable embedding column, or a separate embedding table filled later, both work. A separate table keeps the hot Version table narrow and lets the embedding row carry its own Citations.

### Erasure

ADR-0003 and ADR-0007 make Erasure delete every Version cited only by the erased Episode and recompute every Version it touched. For search that means:

- An embedding of a deleted Version is deleted with it. A foreign key with `ON DELETE CASCADE`, or the Erasure function, can do this in the same transaction.
- An embedding of a surviving but recomputed Version encodes text that may contain the erased value. It must be deleted in the Erasure transaction, not refreshed later, because the vector itself is derived data. The Pipeline re-embeds the recomputed text afterwards, and full-text search covers the gap, as for a new write. The tsvector is a generated column, so the recompute regenerates it.
- With content-hash sharing, an embedding row is deleted when any of the Episodes behind its text is erased. This keeps the rule "every derived item that cites it" from ADR-0003 simple.
- Index pages. HNSW index tuples hold a copy of the vector ([hnsw.h](https://github.com/pgvector/pgvector/blob/v0.8.7/src/hnsw.h)). A GIN index holds the lexemes. After the heap row is deleted, those bytes stay in index pages until vacuum removes the dead entries. Heap tuples behave the same way, so this is not new to search, but it is a second place the erased content lives. `VACUUM` cannot run inside a transaction block or a function ([PostgreSQL docs, VACUUM](https://www.postgresql.org/docs/17/sql-vacuum.html)), so the Erasure transaction cannot finish this step itself. Whether pgvector's HNSW vacuum overwrites a deleted element's vector bytes or only marks it deleted is unconfirmed.
- BM25 extensions keep corpus statistics that include deleted rows until their segments are compacted. That is another place where an erased Episode can still influence scores. Unconfirmed for both extensions.
- Recall after many deletes. Dead tuples reduce HNSW results until vacuum ([README, troubleshooting](https://github.com/pgvector/pgvector#why-are-there-less-results-for-a-query-after-adding-an-hnsw-index)). Erasure is rare, but a Tenant Erasure that deletes most of a large Tenant would leave many dead elements in a shared index. On a partitioned table, a Tenant Erasure on a dedicated partition would be a `DROP` or `TRUNCATE` of that partition.

## 5. Production guidance on multi-tenant vector search

All eight vendors give the same two pieces of advice. Below some size, search a tenant exactly rather than through an ANN index. Above it, give large tenants their own physical unit of index. The thresholds are close: Weaviate switches to a flat scan below 40,000 matching objects and its `dynamic` index converts from flat to HNSW at 10,000, Qdrant suggests a dedicated shard at about 20,000 points, and AWS suggests no index at all for 10,000 to 50,000 vectors. Only Qdrant builds per-tenant structure inside one shared index without a schema change per tenant.

### Supabase

- RLS is the documented access control for RAG, and semantic search "will continue to respect these RLS policies" ([Supabase, RAG with permissions](https://supabase.com/docs/guides/ai/rag-with-permissions)).
- HNSW filtering is a post-filter, "applied as the index returns candidates", and Supabase recommends `hnsw.iterative_scan`, bounded by `hnsw.max_scan_tuples` and `hnsw.scan_mem_multiplier` ([Supabase, HNSW indexes](https://supabase.com/docs/guides/ai/vector-indexes/hnsw-indexes), [pgvector](https://supabase.com/docs/guides/database/extensions/pgvector)).
- A filter chained after `rpc()` runs after the function has ranked and limited, so the filter must go inside the SQL function ([Supabase, Semantic search](https://supabase.com/docs/guides/ai/semantic-search)).
- For very large collections it recommends separate projects ([Engineering for scale](https://supabase.com/docs/guides/ai/engineering-for-scale)).
- I found no Supabase page on recall for a selective tenant filter under RLS, or on partitioning by tenant.

### Neon

- Neon recommends a database or project per tenant, for isolation, per-tenant point-in-time recovery, placement and no noisy neighbours ([Neon, database per tenant](https://neon.com/use-cases/database-per-tenant)), and builds multi-tenant RAG that way ([Neon blog, multi-tenant RAG](https://neon.com/blog/multi-tenant-rag)).
- In Neon's own test, a sequential scan took about 36 ms at 10,000 rows of GIST-960 on 4 CU, and Neon calls it costly from 50,000 rows ([Neon, vector search optimization](https://neon.com/docs/ai/ai-vector-search-optimization)).
- Neon has no guidance on RLS or tenant filters for vectors ([Neon, pgvector](https://neon.com/docs/extensions/pgvector)).

### Tiger Data and pgvectorscale

- Label filtering runs inside the graph walk, on a `smallint[]` column built into the index, following Filtered-DiskANN ([pgvectorscale README](https://github.com/timescale/pgvectorscale), [Gollapudi et al.](https://dl.acm.org/doi/10.1145/3543507.3583552)). It shipped in 0.7.0, and 0.7.1 fixed "poor recall with label filters" ([releases](https://github.com/timescale/pgvectorscale/releases)).
- Other conditions are streaming post-filters with no `ef_search` cutoff, which can mean "potentially even traversing the entire graph" ([Tiger Data blog](https://www.tigerdata.com/blog/how-we-made-postgresql-as-fast-as-pinecone-for-vector-data)). A tenant filter on a plain column always returns full results, at a cost that grows as the tenant shrinks.
- No published recall or latency for label filtering was found.

### AWS (RDS and Aurora)

- AWS's self-managed multi-tenant post uses one table with an RLS policy `tenantid = current_setting(...)::bigint`, set per transaction with `set_config(..., true)`, which is the ADR-0005 pattern. For highly selective queries it advises "a b-tree index or iterative index scans" ([AWS, self-managed multi-tenant vector search](https://aws.amazon.com/blogs/database/self-managed-multi-tenant-vector-search-with-amazon-aurora-postgresql)). The Bedrock Knowledge Bases version uses metadata filters instead ([AWS, Bedrock Knowledge Bases](https://aws.amazon.com/blogs/database/multi-tenant-vector-search-with-amazon-aurora-postgresql-and-amazon-bedrock-knowledge-bases)).
- AWS reports recall on a category-filtered search: 10% with pgvector 0.7.4 at `ef_search` 40, 0% at 200, and 100% with 0.8.0 iterative scans in either order ([AWS, pgvector 0.8.0 on Aurora](https://aws.amazon.com/blogs/database/supercharging-vector-search-performance-and-relevance-with-pgvector-0-8-0-on-amazon-aurora-postgresql/)).
- The June 2026 production guide recommends a B-tree on `tenant_id`, HNSW at `m = 16` and `ef_construction = 128`, `SET LOCAL hnsw.iterative_scan = relaxed_order` and `hnsw.ef_search = 100`, and "partition by tenant for multi-tenant workloads". It says to use no vector index for tables of 10,000 to 50,000 vectors, and for partitioned data that needs complete recall, citing a customer that keeps embeddings in per-user partitions of about 1 GB each and scans them in parallel ([AWS, running pgvector in production](https://aws.amazon.com/blogs/database/running-pgvector-in-production-on-amazon-aurora-postgresql/)).
- Another customer hash-partitions by firm and adds one partial HNSW index per firm on a `halfvec` cast, which means a `CREATE INDEX` per new tenant ([AWS, CORTO](https://aws.amazon.com/blogs/database/cortos-billion-scale-legal-semantic-search-with-aurora-postgresql-pgvector)).

### Google (AlloyDB and Cloud SQL)

- The AlloyDB planner chooses pre-filtering, post-filtering or inline filtering by cost. Inline filtering works only with ScaNN: it builds a bitmap from a B-tree, GIN or GiST index and computes distances only for vectors in it ([AlloyDB filtered vector search](https://cloud.google.com/alloydb/docs/ai/filtered-vector-search-overview), [vector search overview](https://docs.cloud.google.com/alloydb/docs/ai/vector-search-overview)). Google positions it for filters matching about 0.5% to 10% of rows ([Google Cloud blog](https://cloud.google.com/blog/products/databases/alloydb-ais-scann-index-improves-search-on-all-kinds-of-data)).
- Adaptive filtering, in preview, switches between inline filtering and pre-filtering during execution ([adaptive filtering](https://docs.cloud.google.com/alloydb/docs/ai/adaptive-filtering)).
- No Google page mentions RLS. Whether a policy condition can feed the inline-filter bitmap is unconfirmed. The leakproof rule from section 1 suggests that a query condition with a non-leakproof operator cannot, and a policy condition might.
- Cloud SQL's vector page has no guidance on filtered or multi-tenant search ([Cloud SQL, vectors](https://cloud.google.com/sql/docs/postgres/work-with-vectors)).

### Qdrant: the closest design reference

- Filterable HNSW. Qdrant runs the HNSW build inside each indexed payload value and merges those edges into the main graph, which at most doubles the edge count "regardless of the number of categories" ([Qdrant, filterable HNSW](https://qdrant.tech/articles/filterable-hnsw/)). The extra edges exist only for payload indexes created before the build, and adding one later means rebuilding the graph ([Qdrant, indexing](https://qdrant.tech/documentation/concepts/indexing/)).
- Tenant-only graphs. `payload_m: 16` with `m: 0` turns off the global graph and builds only per-tenant sub-graphs, which indexes faster and makes cross-tenant queries slower ([Qdrant, multitenancy](https://qdrant.tech/documentation/manage-data/multitenancy/)).
- `is_tenant`. Since v1.11, this flag on a `keyword` or `uuid` payload index tells storage to "co-locate vectors of the same tenant together", so one tenant is read in a sequential pass.
- The planner works per segment from the payload index's cardinality estimate: full scan for a small segment, payload-index retrieval with exact rescoring below `full_scan_threshold`, filterable HNSW above it, and ACORN for combinations of strict filters ([Qdrant, search](https://qdrant.tech/documentation/search/search/)). The threshold defaults to 10,000 KB, where "1Kb = 1 vector of size 256". By my arithmetic that is about 1,700 vectors of 1,536 float32 dimensions. ACORN arrived in v1.16, is off by default, applies below `max_selectivity` 0.4, and is about 2 to 10 times slower.
- Tiered multitenancy, since v1.16. Small tenants share a fallback shard. Large tenants get dedicated shards, and promotion copies a tenant's points with `replicate_points` and a tenant filter while reads and writes continue. Qdrant suggests promoting at about 20,000 points and keeping to about 1,000 dedicated shards per cluster ([Qdrant, multitenancy guide](https://qdrant.tech/documentation/guides/multitenancy/)).
- BM25 IDF statistics are shared across a shard by default, and an `idf.corpus` filter limits them to one tenant. This is the same cross-tenant statistics issue as in section 2.

### Weaviate

- Each tenant gets its own shard, for isolation, fast queries, and fast deletes that do not affect other tenants. Deleting a tenant deletes its shard ([Weaviate, multi-tenancy](https://docs.weaviate.io/weaviate/concepts/data#multi-tenancy)). A node can hold 50,000 or more active shards, and a 9-node test cluster held about 170,000 active tenants.
- Tenant states are `ACTIVE`, `INACTIVE` (on disk, not queryable) and `OFFLOADED` (in S3). `autoTenantActivation` reactivates a tenant on access ([Weaviate, tenant states](https://docs.weaviate.io/weaviate/manage-collections/tenant-states)). Shards and segments load lazily ([Weaviate blog](https://weaviate.io/blog/weaviate-multi-tenancy-architecture-explained)).
- Within a shard, filtering is a pre-filter: a Roaring Bitmap allow-list is passed into the HNSW search, and below `flatSearchCutoff` (40,000) Weaviate brute-forces the allow-list ([Weaviate, filtering](https://docs.weaviate.io/weaviate/concepts/filtering), [vector index config](https://docs.weaviate.io/weaviate/config-refs/indexing/vector-index)). ACORN is the default filter strategy since v1.34 ([1.34 release](https://weaviate.io/blog/weaviate-1-34-release)), and PathSeer, an adaptive strategy, arrived in v1.40 ([PathSeer](https://weaviate.io/blog/pathseer-filtered-vector-search)).

### Pinecone

- Pinecone recommends one namespace per tenant on a serverless index. Namespaces are stored separately, so tenants are physically isolated and are not noisy neighbours. Query cost scales with namespace size at 1 RU per GB, so filtering one shared namespace of 100 one-GB tenants costs 100 RU instead of 1. Deleting a namespace is "almost instant" ([Pinecone, implement multitenancy](https://docs.pinecone.io/guides/index-data/implement-multitenancy)).
- A metadata filter over a shared namespace is the fallback for cross-tenant queries, and Pinecone warns against filtering by large lists of user ids ([Pinecone, design for multitenancy](https://docs.pinecone.io/guides/index-data/design-for-multitenancy)). Whether the serverless filter runs before or during the ANN search is unconfirmed.

### What the references imply for a Postgres design under RLS and ADR-0006

1. An exact path chosen by an accurate count is standard practice. Qdrant and Weaviate switch on their own cardinality estimates. Prototype 1 showed the Postgres planner estimates every Tenant outside the most-common-values list at the table average, so gnomon would keep its own count per Tenant and Scope and choose the path in its own query.
2. Postgres has nothing like Qdrant's per-tenant sub-graphs that ships as one fixed migration. The direct equivalents, a partial index or a partition per Tenant, need runtime DDL. pgvectorscale labels come closest within one index, but `smallint` caps the label space.
3. Qdrant's `is_tenant` co-location corresponds to partitioning by Tenant, which keeps a Tenant's rows together. `CLUSTER` does not: it is a one-off rewrite under an exclusive lock, and later inserts are not kept in order ([PostgreSQL docs, CLUSTER](https://www.postgresql.org/docs/17/sql-cluster.html)).
4. Tiered multitenancy maps onto a default or hash partition for small Tenants plus list partitions for large ones. Promotion is DDL, so under ADR-0006 it is a published migration the consumer runs. Qdrant's numbers give a starting point: promote at about 20,000 rows and keep dedicated units to about 1,000 or fewer.
5. Physical units per Tenant make Tenant Erasure a partition drop, like a Weaviate tenant delete or a Pinecone namespace delete. Episode Erasure still deletes rows, and the dead entries stay in the HNSW graph until vacuum.
6. AlloyDB inline and adaptive filtering is the only managed feature that chooses pre-filter or in-index filtering per query as Qdrant's planner does. It is on one host, and its behaviour under RLS is unconfirmed.
7. A database or project per Tenant isolates completely, as Neon and Supabase suggest for scale. It conflicts with ADR-0005, and each Tenant database becomes another migration target.

## Host availability matrix

PostgreSQL 17, checked 2026-11-21 against each host's official extension page. "no" means the page does not list the extension. "listed" means the page lists it without a version.

| Extension | Supabase | DO Standard | DO Advanced | Neon | RDS | Aurora | Cloud SQL | AlloyDB | Azure Flexible |
|---|---|---|---|---|---|---|---|---|---|
| PG 17 minor | 17.12 | unconfirmed | unconfirmed | 17.12 | 17.11 | 17.11 | 17.11 | 17.10 | 17.11 |
| vector (pgvector) | 0.8.2 | listed | pre-installed, no version | 0.8.7 | 0.8.2 on 17.10 and 17.11 | 0.8.2 on 17.10 and 17.11 | 0.8.5 | 0.8.5.google-1 | 0.8.7 |
| vectorscale | no | listed | no | no | no | no | no | no | no |
| vchord, vchord_bm25 | no | no | no | no | no | no | no | no | no |
| lantern | no | no | no | no | no | no | no | no | no |
| pg_embedding | no | no | no | removed | no | no | no | no | no |
| pg_search (ParadeDB) | no | no | no | retired 2026-09-21 | no | no | no | no | no |
| pg_textsearch | no | no | no | no | no | no | listed, preview | 1.3, preview | no |
| lakebase_vector, lakebase_text | no | no | no | 1.1.2, 0.1.3 | no | no | no | no | no |
| pg_trgm | 1.6 | listed | available | 1.6 | 1.6 | 1.6 | 1.6 | 1.6 | 1.6 |
| alloydb_scann | no | no | no | no | no | no | no | 0.1.3 | no |
| pg_diskann | no | no | no | no | no | no | no | no | 0.6.6 |
| Transaction pooler | Supavisor (6543); dedicated PgBouncer on Pro and above | PgBouncer, transaction default | platform pooler, mode unconfirmed | PgBouncer, transaction | RDS Proxy | RDS Proxy | Managed Connection Pooling, Enterprise Plus | managed pooling, transaction default | PgBouncer 1.26.1, not on Burstable |

Upstream on the same date: pgvector 0.8.7 (2026-10-01), pgvectorscale 0.9.1 (2026-09-04), VectorChord 1.1.1 (2026-02-28), ParadeDB 0.27.0 (2026-11-19), pg_textsearch 1.5.1 (2026-10-02), Lantern 0.5.0 (2024-11-15, inactive).

Notes that change how the matrix reads:

- Supabase, RDS and Aurora ship pgvector 0.8.2. Release 0.8.3 (2026-06-17) "fixed possible index corruption with HNSW vacuuming", and 0.8.4 fixed a "hnsw graph not repaired" error ([pgvector CHANGELOG](https://github.com/pgvector/pgvector/blob/master/CHANGELOG.md)). Erasure makes HNSW vacuuming routine, so the pgvector floor gnomon states matters.
- RDS Proxy does not pin a connection for `SET LOCAL` inside a transaction, but it does pin for `set_config(name, value, true)` ([RDS Proxy pinning](https://docs.aws.amazon.com/AmazonRDS/latest/UserGuide/rds-proxy-pinning.html)). ADR-0001's `as_user` sets claims with `set_config`.
- Cloud SQL Managed Connection Pooling lists `SET/RESET` as unsupported in transaction mode ([Cloud SQL managed connection pooling](https://docs.cloud.google.com/sql/docs/postgres/managed-connection-pooling)). Whether `SET LOCAL hnsw.ef_search` and `set_config(..., true)` work through it is unconfirmed.
- Neon's Lakebase Search (lakebase_vector, IVF with RaBitQ; lakebase_text, BM25) replaced pg_search ([Neon, Lakebase Search](https://neon.com/docs/ai/lakebase-search)). How its filters interact with RLS is unconfirmed.
- Aurora Optimized Reads adds an NVMe cache and names pgvector as a use case ([Aurora Optimized Reads](https://docs.aws.amazon.com/AmazonRDS/latest/AuroraUserGuide/AuroraPostgreSQL.optimized.reads.html)). It does not change filtering.

Sources per host: [Supabase versions.json](https://raw.githubusercontent.com/supabase/postgres/develop/nix/ext/versions.json), [DigitalOcean supported extensions](https://docs.digitalocean.com/products/databases/postgresql/details/supported-extensions/), [Neon extensions](https://neon.com/docs/extensions/pg-extensions), [RDS extensions](https://docs.aws.amazon.com/AmazonRDS/latest/PostgreSQLReleaseNotes/postgresql-extensions.html), [Aurora extensions](https://docs.aws.amazon.com/AmazonRDS/latest/AuroraPostgreSQLReleaseNotes/AuroraPostgreSQL.Extensions.html), [Cloud SQL extensions](https://docs.cloud.google.com/sql/docs/postgres/extensions), [AlloyDB extensions](https://docs.cloud.google.com/alloydb/docs/reference/extensions), [Azure extensions](https://learn.microsoft.com/en-us/azure/postgresql/extensions/concepts-extensions-versions). Poolers: [Supabase](https://supabase.com/docs/guides/database/connecting-to-postgres), [DigitalOcean](https://docs.digitalocean.com/products/databases/postgresql/how-to/manage-connection-pools/), [Neon](https://neon.com/docs/connect/connection-pooling), [AlloyDB](https://docs.cloud.google.com/alloydb/docs/configure-managed-connection-pooling), [Azure](https://learn.microsoft.com/en-us/azure/postgresql/connectivity/concepts-pgbouncer).

## Supabase and DigitalOcean in detail

### Supabase

Extensions on PG 17. From `nix/ext/versions.json` on the develop branch of supabase/postgres, last committed 2026-11-20 ([versions.json](https://raw.githubusercontent.com/supabase/postgres/develop/nix/ext/versions.json)); the release file names image `17.12.0.002` ([postgres_release.yml](https://raw.githubusercontent.com/supabase/postgres/develop/ansible/postgres_release.yml)). Which image a hosted project runs by default is unconfirmed.

- Search-relevant: vector 0.8.2, pg_trgm 1.6, fuzzystrmatch and unaccent (contrib), pgroonga 3.2.5, rum 1.3.
- Planning and maintenance: hypopg 1.4.1, index_advisor 0.2.1, pg_partman 5.3.1, pg_cron 1.6.4.
- Absent: vectorscale, vchord, vchord_bm25, pg_search, pg_textsearch.
- pgroonga and rum are worth a look for lexical search. Whether their operators are leakproof, and so whether their indexes work under RLS, is unconfirmed.

Installing and upgrading. Users enable extensions with `CREATE EXTENSION` or the dashboard. New versions arrive with platform images: "If a new version of an extension becomes available on Supabase, you need to initiate a software upgrade in the General Settings" ([Supabase, Extensions](https://supabase.com/docs/guides/database/extensions)). Then `ALTER EXTENSION ... UPDATE`. A consumer cannot move pgvector past the version in its image.

Deprecations. On PG 17, plcoffee, plls, plv8, timescaledb and pgjwt are deprecated and must be disabled before upgrading from 15 ([Supabase, Upgrading](https://supabase.com/docs/guides/platform/upgrading)). No announcement about pg_search, VectorChord or pgvectorscale was found.

Limits that affect index builds.

- Compute runs from Nano (0.5 GB) and Micro (1 GB) through Large (8 GB, 2 vCPU), XL (16 GB), 2XL (32 GB) and 4XL (64 GB) to 16XL (256 GB, 64 vCPU) ([Supabase, Compute and disk](https://supabase.com/docs/guides/platform/compute-and-disk)). At 1,536 dimensions, 1M full-precision vectors are about 6 GB before index overhead, so the HNSW graph for a large Tenant does not fit in `maintenance_work_mem` on the small sizes.
- `maintenance_work_mem`, `work_mem`, `max_parallel_maintenance_workers` and `statement_timeout` can be set in SQL or with `supabase postgres-config update`, without a restart ([Supabase, Custom Postgres config](https://supabase.com/docs/guides/database/custom-postgres-config)). `shared_preload_libraries` is not in the configurable list; whether a user can change it is unconfirmed.
- Supabase's HNSW guide says to raise `maintenance_work_mem` and `max_parallel_maintenance_workers` per session and set `statement_timeout = '0'`, and to use psql because the dashboard times out after about 2 minutes ([Supabase troubleshooting, HNSW index](https://supabase.com/docs/guides/troubleshooting/increase-vector-lookup-speeds-by-applying-an-hsnw-index-ohLHUM)).
- Default statement timeouts are 3 s for `anon` and 8 s for `authenticated`. A session `SET statement_timeout` does not hold through Supavisor transaction mode on port 6543 ([Supabase, Timeouts](https://supabase.com/docs/guides/database/postgres/timeouts)). A migration that builds an HNSW index must run over a direct or session-mode connection. A search that runs as `authenticated` must finish within 8 s unless the Store's function sets its own timeout.

Supabase's own vector guidance is RLS plus pgvector iterative scans for selective filters ([Supabase, pgvector](https://supabase.com/docs/guides/database/extensions/pgvector), [HNSW indexes](https://supabase.com/docs/guides/ai/vector-indexes/hnsw-indexes)), and separate projects for very large vector collections ([Engineering for scale](https://supabase.com/docs/guides/ai/engineering-for-scale)).

### DigitalOcean

DigitalOcean now has two editions, and the split matters ([DigitalOcean, supported extensions](https://docs.digitalocean.com/products/databases/postgresql/details/supported-extensions/), updated 2026-10-06; no versions given for either).

- Standard Edition, PG 17: vector, vectorscale, pg_trgm, fuzzystrmatch, unaccent, rum, btree_gin, btree_gist, pg_partman, pg_cron, timescaledb, pg_repack, intarray, dict_int. No hypopg, index_advisor, pgroonga, pg_search, vchord or pg_textsearch.
- Advanced Edition, PG 16 to 18: vector, pg_repack, pg_stat_statements, pgaudit and plpgsql pre-installed; pg_trgm, fuzzystrmatch, unaccent, btree_gin, btree_gist and about fifteen others available. No vectorscale, pg_partman, pg_cron or rum.

Installing and upgrading. The docs say users can run `CREATE EXTENSION` and `ALTER EXTENSION ... UPDATE`. On Advanced Edition only listed extensions can be installed, and anything else needs a support request ([DigitalOcean, limits](https://docs.digitalocean.com/products/databases/postgresql/details/limits/)). Whether new versions arrive only with maintenance updates is unconfirmed.

Announcements ([DigitalOcean PostgreSQL release notes](https://docs.digitalocean.com/release-notes/postgresql/)):

- 2026-04-28: Vector Databases became generally available with vector and vectorscale. Advanced Edition entered public preview.
- 2026-09-17: Advanced Edition became generally available. 2026-09-30: `CREATE EXTENSION` was blocked on Advanced Edition. 2026-10-06: Advanced Edition gained 20 extensions.
- From 2026-11-30, new Standard Edition clusters are limited to plans of 4 GiB of RAM or less, with no standby or read-only nodes. Existing clusters are not affected ([upcoming plan changes](https://docs.digitalocean.com/release-notes/upcoming/dbaas-plan-changes)). Larger plans and HA need Advanced Edition, from $130 a month for 8 GiB and 2 vCPU ([pricing](https://docs.digitalocean.com/products/databases/postgresql/details/pricing)).
- DigitalOcean's own limits page calls HNSW memory-bound and points to pgvectorscale StreamingDiskANN for data larger than RAM ([limits](https://docs.digitalocean.com/products/databases/postgresql/details/limits/)). Advanced Edition, the only edition for new clusters above 4 GiB, does not offer pgvectorscale.

Limits that affect index builds.

- Standard Edition's configuration API exposes `work_mem` (default "1MB + 0.075% of total RAM (up to 32MB)"), `max_parallel_workers`, `max_worker_processes`, `shared_buffers_percentage` (20 to 60), `temp_file_limit` and `idle_in_transaction_session_timeout`. It does not expose `maintenance_work_mem`, `max_parallel_maintenance_workers`, `statement_timeout` or `shared_preload_libraries` ([Terraform, database_postgresql_config](https://docs.digitalocean.com/reference/terraform/reference/resources/database_postgresql_config/)).
- Advanced Edition's API exposes `work_mem` (default 4096 kB), `max_parallel_maintenance_workers` (default 2), `statement_timeout` (default 0) and `shared_buffers`, but not `maintenance_work_mem` or `shared_preload_libraries` ([Terraform, database_advanced_postgresql_config](https://docs.digitalocean.com/reference/terraform/reference/resources/database_advanced_postgresql_config/)).
- `maintenance_work_mem` is a user-settable parameter in Postgres, so `SET maintenance_work_mem` in the migration session should work on both editions. DigitalOcean does not document this, so it is unconfirmed.
- Backend connections are 25 per GiB minus 3, capped at 997, so 97 at 4 GiB. Superuser is not available ([limits](https://docs.digitalocean.com/products/databases/postgresql/details/limits/)).
- On a 4 GiB Standard cluster, `work_mem` defaults to about 4 MB. pgvector's iterative scan stops at `work_mem` times `hnsw.scan_mem_multiplier`, and prototype 1 found that memory limit, not `max_scan_tuples`, ended the scan. The Store would need to raise `work_mem` or the multiplier per transaction with `SET LOCAL`.

## What the findings mean for the existing ADRs

- ADR-0001 (one Rust core; Pipeline makes model calls; `as_user` sets claims with `set_config` and `SET LOCAL ROLE`; transaction poolers). Store-side RRF makes no model call, so it fits the Store. The reranker stays in the Pipeline. Two pooler findings: RDS Proxy pins a connection for `set_config(name, value, true)`, which would pin every gnomon transaction, and Cloud SQL's pooler lists `SET/RESET` as unsupported in transaction mode. Both need a test, because the search path also depends on `SET LOCAL` for `hnsw.ef_search`, `hnsw.iterative_scan` and `work_mem`.
- ADR-0002 (Kind property schemas). Unaffected directly. What text a Kind contributes to its embedding and its tsvector is a new per-Kind decision (open questions).
- ADR-0003 (Episodes and deterministic Erasure). Confirmed and extended. An embedding is a derived item that cites Episodes, and so is a tsvector, which is generated from the Version and regenerated on recompute. Erasure must delete the embedding of every recomputed Version, not only of deleted ones. Index pages keep copies of vectors and lexemes until vacuum, which cannot run inside the Erasure transaction. A BM25 extension's corpus statistics would be a further place where an erased Episode lingers.
- ADR-0004 (scalar time columns, chosen because range operators are not leakproof). Confirmed and generalized. The same leakproof rule removes GIN full-text, trigram similarity and array-overlap indexes from every invoker-mode read on an RLS table. The As-of conditions on scalar `timestamptz` columns are leakproof, so they remain index conditions on the exact path and cheap post-filters on a vector scan.
- ADR-0005 (every row has a Tenant; deployment-supplied `gnomon.tenant_id()` and `gnomon.scope_allowed(scope)`). Amended in shape. `scope_allowed(scope)` is a function of the column, so no index can serve it. A policy written as `scope = any(gnomon.allowed_scopes())` would let a `(tenant_id, scope)` B-tree serve both conditions. In single-tenant mode, Scope is the only selective filter, so the Scope problem is the whole problem there.
- ADR-0006 (exported migrations, no runtime DDL). It rules out per-Tenant partial indexes, per-Tenant list partitions and tiered promotion, unless promotion becomes a generated migration the consumer applies. Hash partitioning and static partial indexes, such as current-only HNSW, fit. Extension fast paths do not fit a single chain: an index `USING diskann` fails on a host without vectorscale, and `shared_preload_libraries` cannot be set by a migration. They would need optional migrations per `--target`, with the ledger recording which are applied. Index builds need session-mode or direct connections and raised `maintenance_work_mem` and `statement_timeout`, which the exported migration has to set.
- ADR-0007 (Versions computed from Contributions). Any recompute changes a Version's text, so it invalidates its embedding. Versions whose text did not change, such as Refinements, can keep a shared embedding if embeddings are keyed by content hash.
- ADR-0008 (bi-temporal, with precision). Search results inherit the `uncertain` mark when valid-at falls inside an imprecise bound. An unknown start reads as open, so it never excludes a Version from an As-of search.
- ADR-0009 (invoker-mode functions; only Erasure and Kind registration are definers). The search path runs as invoker and is subject to every finding above. Restoring GIN full-text search for large Tenants needs a definer candidate function, which would be a third definer. Search settings must be `SET LOCAL` inside the function's transaction, or a function-level `SET` clause; the function-level form was not measured.
- PostgreSQL 17 floor. Sufficient for every core option. The pgvector floor is a separate decision: 0.8.0 for iterative scans, 0.8.3 for the HNSW vacuum fix that Supabase, RDS and Aurora do not yet ship.

## Open questions for a design interview

1. What latency budget does each use need? Mention resolution may be interactive. Context assembly may tolerate more. The answer decides whether design A's linear scan is acceptable for the large Tenants.
2. How large is the largest visible set (Tenant times Scope) that must be searched at full recall, and how many such Tenants are there?
3. Can every consumer's Scope rule be expressed as a set of allowed keys, so the policy reads `scope = any(gnomon.allowed_scopes())`? Are Scopes ever hierarchical?
4. Must semantic search at a past known-at instant have the same quality as at "now", or may it use the exact path only?
5. What text does each Kind contribute to its embedding and its tsvector: name, properties, Summary? Do Edges get embeddings of their own, as Graphiti's facts do?
6. Are embeddings per Version, or per distinct text with a content hash shared across Versions?
7. Are Summaries and Communities searched in their own branch, or used only to expand from Nodes?
8. Is a `SECURITY DEFINER` candidate function for full-text search acceptable under ADR-0009, or must lexical search stay a scan of the visible rows?
9. Is runtime DDL ever acceptable, for example a generated migration per promoted Tenant?
10. Is an extension fast path worth a second conformance suite, given that Supabase has none and DigitalOcean offers pgvectorscale only on small Standard clusters?
11. Is BM25 with corpus statistics shared across Tenants acceptable, given the ranking drift and the information leak?
12. Must Erasure wait for a vacuum of the affected indexes before it reports completion?
13. What is the minimum pgvector version, given that three hosts ship 0.8.2?
14. Is the embedding model and dimension fixed per deployment? A change means re-embedding everything, and the dimension sits in the column type in the migration.
15. How does the Store keep a per-(Tenant, Scope) row count for routing without a hot counter row on every write?

## Sources

Every URL cited above, grouped by site. All were read on or before 2026-11-21. Code links are pinned to a commit or tag where the section says so.

### arxiv.org

- https://arxiv.org/abs/2404.16130
- https://arxiv.org/html/2410.05779v3
- https://arxiv.org/html/2501.13956v1
- https://arxiv.org/html/2504.19413v1

### aws.amazon.com

- https://aws.amazon.com/blogs/database/cortos-billion-scale-legal-semantic-search-with-aurora-postgresql-pgvector
- https://aws.amazon.com/blogs/database/multi-tenant-vector-search-with-amazon-aurora-postgresql-and-amazon-bedrock-knowledge-bases
- https://aws.amazon.com/blogs/database/running-pgvector-in-production-on-amazon-aurora-postgresql/
- https://aws.amazon.com/blogs/database/self-managed-multi-tenant-vector-search-with-amazon-aurora-postgresql
- https://aws.amazon.com/blogs/database/supercharging-vector-search-performance-and-relevance-with-pgvector-0-8-0-on-amazon-aurora-postgresql/

### cloud.google.com

- https://cloud.google.com/alloydb/docs/ai/filtered-vector-search-overview
- https://cloud.google.com/blog/products/databases/alloydb-ais-scann-index-improves-search-on-all-kinds-of-data
- https://cloud.google.com/sql/docs/postgres/work-with-vectors

### dl.acm.org

- https://dl.acm.org/doi/10.1145/3543507.3583552

### docs.aws.amazon.com

- https://docs.aws.amazon.com/AmazonRDS/latest/AuroraPostgreSQLReleaseNotes/AuroraPostgreSQL.Extensions.html
- https://docs.aws.amazon.com/AmazonRDS/latest/AuroraUserGuide/AuroraPostgreSQL.optimized.reads.html
- https://docs.aws.amazon.com/AmazonRDS/latest/PostgreSQLReleaseNotes/postgresql-extensions.html
- https://docs.aws.amazon.com/AmazonRDS/latest/UserGuide/rds-proxy-pinning.html

### docs.cloud.google.com

- https://docs.cloud.google.com/alloydb/docs/ai/adaptive-filtering
- https://docs.cloud.google.com/alloydb/docs/ai/vector-search-overview
- https://docs.cloud.google.com/alloydb/docs/configure-managed-connection-pooling
- https://docs.cloud.google.com/alloydb/docs/reference/extensions
- https://docs.cloud.google.com/sql/docs/postgres/extensions
- https://docs.cloud.google.com/sql/docs/postgres/managed-connection-pooling
- https://docs.cloud.google.com/sql/docs/postgres/pg-textsearch

### docs.digitalocean.com

- https://docs.digitalocean.com/products/databases/postgresql/details/limits/
- https://docs.digitalocean.com/products/databases/postgresql/details/pricing
- https://docs.digitalocean.com/products/databases/postgresql/details/supported-extensions/
- https://docs.digitalocean.com/products/databases/postgresql/how-to/manage-connection-pools/
- https://docs.digitalocean.com/reference/terraform/reference/resources/database_advanced_postgresql_config/
- https://docs.digitalocean.com/reference/terraform/reference/resources/database_postgresql_config/
- https://docs.digitalocean.com/release-notes/postgresql/
- https://docs.digitalocean.com/release-notes/upcoming/dbaas-plan-changes

### docs.pinecone.io

- https://docs.pinecone.io/guides/index-data/design-for-multitenancy
- https://docs.pinecone.io/guides/index-data/implement-multitenancy

### docs.vectorchord.ai

- https://docs.vectorchord.ai/vectorchord/usage/indexing.html
- https://docs.vectorchord.ai/vectorchord/usage/prefilter.html
- https://docs.vectorchord.ai/vectorchord/usage/similarity-filter.html

### docs.weaviate.io

- https://docs.weaviate.io/weaviate/concepts/data#multi-tenancy
- https://docs.weaviate.io/weaviate/concepts/filtering
- https://docs.weaviate.io/weaviate/config-refs/indexing/vector-index
- https://docs.weaviate.io/weaviate/manage-collections/tenant-states

### github.com/getzep/graphiti

- https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/README.md
- https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/decorators.py#L29
- https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/driver/driver.py#L59
- https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/driver/falkordb_driver.py#L342
- https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/edges.py#L228
- https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/graph_queries.py#L101
- https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/graphiti.py#L2284
- https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/nodes.py#L427
- https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/nodes.py#L715
- https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/search/search.py#L259
- https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/search/search.py#L98
- https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/search/search_config.py
- https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/search/search_config_recipes.py#L81
- https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/search/search_filters.py#L55
- https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/search/search_helpers.py#L22
- https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/search/search_utils.py#L1551
- https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/search/search_utils.py#L289
- https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/search/search_utils.py#L85
- https://github.com/getzep/graphiti/tree/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/cross_encoder

### github.com/HKUDS/LightRAG

- https://github.com/HKUDS/LightRAG/blob/46984a451b513e4684a12cf3dafde52df04a1ae2/docs/design/PgVectorFilteredSearch.md
- https://github.com/HKUDS/LightRAG/blob/46984a451b513e4684a12cf3dafde52df04a1ae2/lightrag/kg/pgtable_impl.py
- https://github.com/HKUDS/LightRAG/blob/46984a451b513e4684a12cf3dafde52df04a1ae2/lightrag/kg/postgres_impl.py#L10913
- https://github.com/HKUDS/LightRAG/blob/46984a451b513e4684a12cf3dafde52df04a1ae2/lightrag/kg/postgres_impl.py#L345
- https://github.com/HKUDS/LightRAG/blob/46984a451b513e4684a12cf3dafde52df04a1ae2/lightrag/operate.py#L2158
- https://github.com/HKUDS/LightRAG/blob/46984a451b513e4684a12cf3dafde52df04a1ae2/lightrag/operate.py#L2688
- https://github.com/HKUDS/LightRAG/blob/46984a451b513e4684a12cf3dafde52df04a1ae2/lightrag/operate.py#L5954
- https://github.com/HKUDS/LightRAG/blob/46984a451b513e4684a12cf3dafde52df04a1ae2/lightrag/operate.py#L6554
- https://github.com/HKUDS/LightRAG/blob/46984a451b513e4684a12cf3dafde52df04a1ae2/lightrag/operate.py#L7159
- https://github.com/HKUDS/LightRAG/blob/46984a451b513e4684a12cf3dafde52df04a1ae2/lightrag/prompt.py#L489
- https://github.com/HKUDS/LightRAG/blob/46984a451b513e4684a12cf3dafde52df04a1ae2/lightrag/utils.py#L7635

### github.com/lanterndata/lantern

- https://github.com/lanterndata/lantern

### github.com/mem0ai/mem0

- https://github.com/mem0ai/mem0/blob/fff8a6472512d1bc1a4bccab8c645b65a1df7fe5/docs/migration/oss-v2-to-v3.mdx#L19
- https://github.com/mem0ai/mem0/blob/fff8a6472512d1bc1a4bccab8c645b65a1df7fe5/docs/platform/features/graph-memory.mdx
- https://github.com/mem0ai/mem0/blob/fff8a6472512d1bc1a4bccab8c645b65a1df7fe5/mem0/memory/main.py#L1642
- https://github.com/mem0ai/mem0/blob/fff8a6472512d1bc1a4bccab8c645b65a1df7fe5/mem0/utils/scoring.py#L60
- https://github.com/mem0ai/mem0/blob/fff8a6472512d1bc1a4bccab8c645b65a1df7fe5/mem0/vector_stores/pgvector.py#L285
- https://github.com/mem0ai/mem0/blob/v1.0.11/mem0/memory/apache_age_memory.py#L153
- https://github.com/mem0ai/mem0/blob/v1.0.11/mem0/memory/graph_memory.py#L96

### github.com/microsoft/graphrag

- https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag-vectors/graphrag_vectors/lancedb.py#L212
- https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag/graphrag/config/embeddings.py
- https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag/graphrag/prompts/query/global_search_map_system_prompt.py#L21
- https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag/graphrag/query/context_builder/dynamic_community_selection.py#L73
- https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag/graphrag/query/context_builder/entity_extraction.py#L41
- https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag/graphrag/query/structured_search/drift_search/drift_context.py#L216
- https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag/graphrag/query/structured_search/drift_search/primer.py#L114
- https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag/graphrag/query/structured_search/global_search/search.py
- https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag/graphrag/query/structured_search/local_search/mixed_context.py#L91
- https://github.com/microsoft/graphrag/tree/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag-vectors/graphrag_vectors

### github.com/neondatabase/pg_embedding

- https://github.com/neondatabase/pg_embedding

### github.com/paradedb/paradedb

- https://github.com/paradedb/paradedb/blob/main/docs/concepts/guarantees.mdx
- https://github.com/paradedb/paradedb/blob/main/docs/concepts/vector/overview.mdx
- https://github.com/paradedb/paradedb/blob/main/docs/project/changelog/0.26.5.mdx
- https://github.com/paradedb/paradedb/blob/main/docs/project/changelog/0.26.7.mdx
- https://github.com/paradedb/paradedb/blob/main/docs/reference/full-text/score.mdx
- https://github.com/paradedb/paradedb/blob/main/docs/reference/hybrid/rrf.mdx
- https://github.com/paradedb/paradedb/blob/main/docs/reference/indexing/partition-by.mdx
- https://github.com/paradedb/paradedb/blob/main/docs/start/install.mdx

### github.com/pgvector/pgvector

- https://github.com/pgvector/pgvector#binary-quantization
- https://github.com/pgvector/pgvector#filtering
- https://github.com/pgvector/pgvector#half-precision-vectors
- https://github.com/pgvector/pgvector#index-build-time
- https://github.com/pgvector/pgvector#iterative-index-scans
- https://github.com/pgvector/pgvector#iterative-scan-options
- https://github.com/pgvector/pgvector#ivfflat
- https://github.com/pgvector/pgvector#multitenancy
- https://github.com/pgvector/pgvector#scaling
- https://github.com/pgvector/pgvector#storing
- https://github.com/pgvector/pgvector#why-are-there-less-results-for-a-query-after-adding-an-hnsw-index
- https://github.com/pgvector/pgvector/blob/master/CHANGELOG.md
- https://github.com/pgvector/pgvector/blob/v0.8.7/src/hnsw.h
- https://github.com/pgvector/pgvector/issues/761
- https://github.com/pgvector/pgvector/issues/914
- https://github.com/pgvector/pgvector/issues/980

### github.com/pgvector/pgvector-python

- https://github.com/pgvector/pgvector-python/blob/master/examples/hybrid_search/cross_encoder.py
- https://github.com/pgvector/pgvector-python/blob/master/examples/hybrid_search/rrf.py

### github.com/postgres/postgres

- https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/optimizer/path/indxpath.c
- https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/optimizer/util/restrictinfo.c
- https://github.com/postgres/postgres/blob/REL_17_STABLE/src/include/catalog/pg_proc.dat

### github.com/supabase/postgres

- https://raw.githubusercontent.com/supabase/postgres/develop/ansible/postgres_release.yml
- https://raw.githubusercontent.com/supabase/postgres/develop/nix/ext/versions.json

### github.com/tensorchord/pgvecto.rs-docs

- https://github.com/tensorchord/pgvecto.rs-docs/blob/main/src/vectorchord/customer-stories/earthgenome.md
- https://github.com/tensorchord/pgvecto.rs-docs/blob/main/src/vectorchord/getting-started/installation.md
- https://github.com/tensorchord/pgvecto.rs-docs/blob/main/src/vectorchord/usage/graph-index.md
- https://github.com/tensorchord/pgvecto.rs-docs/blob/main/src/vectorchord/usage/partitioning-tuning.md

### github.com/tensorchord/VectorChord

- https://github.com/tensorchord/VectorChord#license
- https://github.com/tensorchord/VectorChord/blob/main/src/index/fetcher.rs
- https://github.com/tensorchord/VectorChord/releases

### github.com/timescale/pg_textsearch

- https://github.com/timescale/pg_textsearch

### github.com/timescale/pgvectorscale

- https://github.com/timescale/pgvectorscale
- https://github.com/timescale/pgvectorscale#filtered-vector-search
- https://github.com/timescale/pgvectorscale#parallel-index-build-parameters
- https://github.com/timescale/pgvectorscale/blob/main/pgvectorscale/sql/vectorscale--0.8.0--0.9.0.sql
- https://github.com/timescale/pgvectorscale/releases
- https://github.com/timescale/pgvectorscale/releases/tag/0.9.1

### github.com/topoteretes/cognee

- https://github.com/topoteretes/cognee/blob/1961133d0c68b450fe8e71941a756371e94f7c04/cognee/infrastructure/databases/vector/pgvector/PGVectorAdapter.py#L80
- https://github.com/topoteretes/cognee/blob/1961133d0c68b450fe8e71941a756371e94f7c04/cognee/infrastructure/databases/vector/pgvector/PGVectorDatasetDatabaseHandler.py
- https://github.com/topoteretes/cognee/blob/1961133d0c68b450fe8e71941a756371e94f7c04/cognee/infrastructure/databases/vector/pgvector/PGVectorSharedDatasetDatabaseHandler.py
- https://github.com/topoteretes/cognee/blob/1961133d0c68b450fe8e71941a756371e94f7c04/cognee/modules/graph/cognee_graph/CogneeGraph.py#L480
- https://github.com/topoteretes/cognee/blob/1961133d0c68b450fe8e71941a756371e94f7c04/cognee/modules/retrieval/bm25_retriever.py
- https://github.com/topoteretes/cognee/blob/1961133d0c68b450fe8e71941a756371e94f7c04/cognee/modules/retrieval/hybrid/ranking.py
- https://github.com/topoteretes/cognee/blob/1961133d0c68b450fe8e71941a756371e94f7c04/cognee/modules/retrieval/temporal_hybrid_retriever.py
- https://github.com/topoteretes/cognee/blob/1961133d0c68b450fe8e71941a756371e94f7c04/cognee/modules/retrieval/utils/brute_force_triplet_search.py#L232
- https://github.com/topoteretes/cognee/blob/1961133d0c68b450fe8e71941a756371e94f7c04/cognee/modules/search/types/SearchType.py

### learn.microsoft.com

- https://learn.microsoft.com/en-us/azure/horizondb/ai/vector-index-diskann
- https://learn.microsoft.com/en-us/azure/postgresql/connectivity/concepts-pgbouncer
- https://learn.microsoft.com/en-us/azure/postgresql/extensions/concepts-extensions-versions
- https://learn.microsoft.com/en-us/azure/postgresql/flexible-server/how-to-use-pgdiskann

### microsoft.com

- https://www.microsoft.com/en-us/research/blog/introducing-drift-search-combining-global-and-local-search-methods-to-improve-quality-and-efficiency/

### microsoft.github.io

- https://microsoft.github.io/graphrag/query/local_search/

### neon.com

- https://neon.com/blog/multi-tenant-rag
- https://neon.com/docs/ai/ai-vector-search-optimization
- https://neon.com/docs/ai/lakebase-search
- https://neon.com/docs/connect/connection-pooling
- https://neon.com/docs/extensions/pg-extensions
- https://neon.com/docs/extensions/pg_search
- https://neon.com/docs/extensions/pgvector
- https://neon.com/use-cases/database-per-tenant

### paradedb.com

- https://www.paradedb.com/docs/operate/deploy/logical-replication/getting-started

### plg.uwaterloo.ca

- https://plg.uwaterloo.ca/~gvcormac/cormacksigir09-rrf.pdf

### postgresql.org

- https://www.postgresql.org/docs/17/ddl-partitioning.html
- https://www.postgresql.org/docs/17/indexes-partial.html
- https://www.postgresql.org/docs/17/pgtrgm.html
- https://www.postgresql.org/docs/17/sql-cluster.html
- https://www.postgresql.org/docs/17/sql-createtable.html
- https://www.postgresql.org/docs/17/sql-vacuum.html
- https://www.postgresql.org/docs/17/textsearch-controls.html
- https://www.postgresql.org/docs/17/textsearch-tables.html
- https://www.postgresql.org/docs/current/rules-privileges.html
- https://www.postgresql.org/message-id/flat/18941-90236c38c6101bf0%40postgresql.org

### qdrant.tech

- https://qdrant.tech/articles/filterable-hnsw/
- https://qdrant.tech/documentation/concepts/indexing/
- https://qdrant.tech/documentation/guides/multitenancy/
- https://qdrant.tech/documentation/manage-data/multitenancy/
- https://qdrant.tech/documentation/search/search/

### supabase.com

- https://supabase.com/docs/guides/ai/engineering-for-scale
- https://supabase.com/docs/guides/ai/hybrid-search
- https://supabase.com/docs/guides/ai/rag-with-permissions
- https://supabase.com/docs/guides/ai/semantic-search
- https://supabase.com/docs/guides/ai/vector-indexes/hnsw-indexes
- https://supabase.com/docs/guides/database/connecting-to-postgres
- https://supabase.com/docs/guides/database/custom-postgres-config
- https://supabase.com/docs/guides/database/extensions
- https://supabase.com/docs/guides/database/extensions/pgvector
- https://supabase.com/docs/guides/database/postgres/timeouts
- https://supabase.com/docs/guides/platform/compute-and-disk
- https://supabase.com/docs/guides/platform/upgrading
- https://supabase.com/docs/guides/troubleshooting/increase-vector-lookup-speeds-by-applying-an-hsnw-index-ohLHUM

### tigerdata.com

- https://www.tigerdata.com/blog/how-we-made-postgresql-as-fast-as-pinecone-for-vector-data

### weaviate.io

- https://weaviate.io/blog/pathseer-filtered-vector-search
- https://weaviate.io/blog/weaviate-1-34-release
- https://weaviate.io/blog/weaviate-multi-tenancy-architecture-explained
