-- PROTOTYPE, wipe me. Run after gen.py: derived tables, token table, indexes.
\timing on
set maintenance_work_mem = '2GB';
set work_mem = '2GB';
set max_parallel_maintenance_workers = 8;

-- Narrow quantized copies, in shuffled physical order as interleaved inserts would leave them.
insert into gnomon.emb_half select tenant_id, scope, text_hash, embedding::public.halfvec(1536) from gnomon.emb order by random();
insert into gnomon.emb_bit select tenant_id, scope, text_hash, public.binary_quantize(embedding)::bit(1536) from gnomon.emb order by random();
insert into gnomon.name_bit select id, tenant_id, scope, recorded_to, public.binary_quantize(embedding)::bit(1536) from gnomon.name_item order by random();

-- Token table, per-Tenant document frequencies and length totals.
insert into gnomon.tok select v.tenant_id, v.scope, t.lexeme, v.id, pg_catalog.array_length(t.positions, 1), v.doc_len
  from gnomon.ver v cross join lateral pg_catalog.unnest(v.fts) t;
insert into gnomon.tok_df select tenant_id, token, count(*) from gnomon.tok group by 1, 2;
insert into gnomon.tenant_stats select tenant_id, count(*), sum(doc_len) from gnomon.ver group by 1;

alter table gnomon.emb add primary key (tenant_id, scope, text_hash);
alter table gnomon.emb_half add primary key (tenant_id, scope, text_hash);
alter table gnomon.emb_bit add primary key (tenant_id, scope, text_hash);
alter table gnomon.ver add primary key (id);
create index ver_hash on gnomon.ver (tenant_id, scope, text_hash) include (valid_from, valid_to, recorded_from, recorded_to);
create index ver_scan on gnomon.ver (tenant_id, scope, id);
create index ver_src on gnomon.ver (tenant_id, scope, src_id) include (id, valid_from, valid_to, recorded_from, recorded_to) where kind = 'e';
create index ver_dst on gnomon.ver (tenant_id, scope, dst_id) include (id, valid_from, valid_to, recorded_from, recorded_to) where kind = 'e';
create index ver_ent on gnomon.ver (tenant_id, scope, entity_id) include (id, kind, valid_from, valid_to, recorded_from, recorded_to);
alter table gnomon.episode add primary key (tenant_id, scope, id);
create index cit_ent on gnomon.citation (tenant_id, scope, entity_id) include (episode_id);
alter table gnomon.tok add primary key (tenant_id, scope, token, ver_id) include (tf, dl);
alter table gnomon.tok_df add primary key (tenant_id, token);
alter table gnomon.tenant_stats add primary key (tenant_id);
alter table gnomon.name_item add primary key (id);
create index name_item_all on gnomon.name_item (tenant_id, scope);
create index name_item_cur on gnomon.name_item (tenant_id, scope) where recorded_to = 'infinity';
alter table gnomon.name_bit add primary key (id);
create index name_bit_all on gnomon.name_bit (tenant_id, scope);
create index name_bit_cur on gnomon.name_bit (tenant_id, scope) where recorded_to = 'infinity';

vacuum (analyze, freeze) gnomon.emb;
vacuum (analyze, freeze) gnomon.emb_half;
vacuum (analyze, freeze) gnomon.emb_bit;
vacuum (analyze, freeze) gnomon.ver;
vacuum (analyze, freeze) gnomon.episode;
vacuum (analyze, freeze) gnomon.citation;
vacuum (analyze, freeze) gnomon.tok;
vacuum (analyze, freeze) gnomon.tok_df;
vacuum (analyze, freeze) gnomon.tenant_stats;
vacuum (analyze, freeze) gnomon.name_item;
vacuum (analyze, freeze) gnomon.name_bit;
checkpoint;
