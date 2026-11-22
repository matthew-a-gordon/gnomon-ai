-- PROTOTYPE, wipe me. Default HNSW parameters (m=16, ef_construction=64), cosine.
set maintenance_work_mem = '14GB';
set max_parallel_maintenance_workers = 8;
\timing on
create index embedding_hnsw_idx on gnomon.embedding using hnsw (embedding vector_cosine_ops);
\timing off
select pg_size_pretty(pg_relation_size('gnomon.embedding_hnsw_idx')) as hnsw_size,
       pg_size_pretty(pg_relation_size('gnomon.embedding_tenant_idx')) as btree_size;
analyze gnomon.embedding;
