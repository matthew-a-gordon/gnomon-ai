-- PROTOTYPE, wipe me. Per-Version quantized rows carrying the As-of columns; full vectors stay shared by text hash.
set maintenance_work_mem = '2GB'; set work_mem = '1GB';
drop table if exists gnomon.vbit;
create table gnomon.vbit as
  select v.tenant_id, v.scope, v.id, v.text_hash, b.q, v.valid_from, v.valid_to, v.recorded_from, v.recorded_to
  from gnomon.ver v join gnomon.emb_bit b on b.tenant_id = v.tenant_id and b.scope = v.scope and b.text_hash = v.text_hash
  order by random();
create index vbit_cov on gnomon.vbit (tenant_id, scope) include (id, text_hash, q, valid_from, valid_to, recorded_from, recorded_to);
drop index if exists gnomon.ver_hash;
create index ver_hash on gnomon.ver (tenant_id, scope, text_hash) include (id, anchor_id, valid_from, valid_to, recorded_from, recorded_to);
vacuum (analyze, freeze) gnomon.vbit;
vacuum analyze gnomon.ver;
alter table gnomon.vbit enable row level security;
create policy p_all on gnomon.vbit for all to gnomon_app using (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()));
grant select on gnomon.vbit to gnomon_app;
-- As-of search without a join: the As-of conditions are evaluated on the covering index tuples.
drop function if exists gnomon.c_vec2(uuid, public.vector, timestamptz, timestamptz, int, int);
create function gnomon.c_vec2(p_tenant uuid, p_q public.vector, p_v timestamptz, p_k timestamptz, p_rerank int, p_lim int)
returns table (ver_id bigint, scope text, anchor_id bigint, rnk bigint) language sql stable set search_path = '' as $$
  with qb as materialized (select public.binary_quantize(p_q)::bit(1536) as q),
  c as materialized (
    select b.id, b.scope, b.text_hash from gnomon.vbit b
    where b.tenant_id = p_tenant
      and b.valid_from <= p_v and p_v < b.valid_to and b.recorded_from <= p_k and p_k < b.recorded_to
    order by b.q OPERATOR(public.<~>) (select q from qb) limit p_rerank),
  rr as materialized (
    select c.id, c.scope, (e.embedding OPERATOR(public.<=>) p_q) as d
    from c join gnomon.emb e on e.tenant_id = p_tenant and e.scope = c.scope and e.text_hash = c.text_hash
    order by d limit p_lim)
  select rr.id, rr.scope, v.anchor_id, row_number() over (order by rr.d, rr.id)
  from rr join gnomon.ver v on v.id = rr.id $$;
grant execute on all functions in schema gnomon to gnomon_app;
