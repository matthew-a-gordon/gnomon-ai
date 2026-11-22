-- PROTOTYPE, wipe me. Hash-partitioned copy of the embedding table: one schema decision, no per-Tenant DDL.
set maintenance_work_mem = '4GB';
set max_parallel_maintenance_workers = 8;
\timing on
drop table if exists gnomon.embedding_p;
create table gnomon.embedding_p (
  id bigint not null, tenant_id uuid not null, scope text not null default 'default',
  episode_id uuid not null, embedding vector(1536) not null
) partition by hash (tenant_id);
do $$ begin
  for i in 0..63 loop
    execute format('create unlogged table gnomon.embedding_p_%s partition of gnomon.embedding_p for values with (modulus 64, remainder %s)', i, i);
  end loop;
end $$;
insert into gnomon.embedding_p select id, tenant_id, scope, episode_id, embedding from gnomon.embedding;
create index embedding_p_tenant_idx on gnomon.embedding_p (tenant_id);
create index embedding_p_hnsw_idx on gnomon.embedding_p using hnsw (embedding vector_cosine_ops);
alter table gnomon.embedding_p enable row level security;
grant select on gnomon.embedding_p to authenticated;
create policy tenant_isolation on gnomon.embedding_p for select to authenticated
  using (tenant_id = gnomon.tenant_id() and gnomon.scope_allowed(scope));
analyze gnomon.embedding_p;
\timing off
select count(*), pg_size_pretty(sum(pg_relation_size(c.oid)) filter (where c.relname like 'embedding_p_%_embedding_idx' or c.relname like '%hnsw%')) as hnsw_total
  from gnomon.embedding_p, pg_class c where false;
select count(*) from gnomon.embedding_p;
