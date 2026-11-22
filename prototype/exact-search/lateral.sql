-- PROTOTYPE, wipe me. Re-rank reads each full vector by primary key through a lateral join, so the plan cannot flip to a hash join.
create or replace function gnomon.vsearch_half(p_tenant uuid, p_q public.vector, p_rerank int, p_k int)
returns table (scope text, text_hash bytea, dist float8) language sql stable set search_path = '' as $$
  with qh as materialized (select p_q::public.halfvec(1536) as q),
  c as materialized (
    select h.scope, h.text_hash from gnomon.emb_half h
    where h.tenant_id = p_tenant
    order by h.embedding OPERATOR(public.<=>) (select q from qh) limit p_rerank)
  select c.scope, c.text_hash, (e.embedding OPERATOR(public.<=>) p_q)::float8 as d
  from c cross join lateral (select x.embedding from gnomon.emb x where x.tenant_id = p_tenant and x.scope = c.scope and x.text_hash = c.text_hash limit 1) e
  order by d limit p_k $$;

create or replace function gnomon.vsearch_bit(p_tenant uuid, p_q public.vector, p_rerank int, p_k int)
returns table (scope text, text_hash bytea, dist float8) language sql stable set search_path = '' as $$
  with qb as materialized (select public.binary_quantize(p_q)::bit(1536) as q),
  c as materialized (
    select b.scope, b.text_hash from gnomon.emb_bit b
    where b.tenant_id = p_tenant
    order by b.q OPERATOR(public.<~>) (select q from qb) limit p_rerank)
  select c.scope, c.text_hash, (e.embedding OPERATOR(public.<=>) p_q)::float8 as d
  from c cross join lateral (select x.embedding from gnomon.emb x where x.tenant_id = p_tenant and x.scope = c.scope and x.text_hash = c.text_hash limit 1) e
  order by d limit p_k $$;

create or replace function gnomon.c_vec(p_tenant uuid, p_q public.vector, p_v timestamptz, p_k timestamptz, p_rerank int, p_lim int)
returns table (ver_id bigint, scope text, anchor_id bigint, rnk bigint) language sql stable set search_path = '' as $$
  with qb as materialized (select public.binary_quantize(p_q)::bit(1536) as q),
  c as materialized (
    select b.scope, b.text_hash from gnomon.emb_bit b
    where b.tenant_id = p_tenant
      and exists (select 1 from gnomon.ver v
                  where v.tenant_id = b.tenant_id and v.scope = b.scope and v.text_hash = b.text_hash
                    and v.valid_from <= p_v and p_v < v.valid_to and v.recorded_from <= p_k and p_k < v.recorded_to)
    order by b.q OPERATOR(public.<~>) (select q from qb) limit p_rerank),
  rr as materialized (
    select c.scope, c.text_hash, (e.embedding OPERATOR(public.<=>) p_q) as d
    from c cross join lateral (select x.embedding from gnomon.emb x where x.tenant_id = p_tenant and x.scope = c.scope and x.text_hash = c.text_hash limit 1) e
    order by d limit p_lim)
  select v.id, v.scope, v.anchor_id, row_number() over (order by rr.d, v.id)
  from rr cross join lateral (select y.id, y.scope, y.anchor_id from gnomon.ver y
      where y.tenant_id = p_tenant and y.scope = rr.scope and y.text_hash = rr.text_hash
        and y.valid_from <= p_v and p_v < y.valid_to and y.recorded_from <= p_k and p_k < y.recorded_to limit 1) v $$;

create or replace function gnomon.c_vec2(p_tenant uuid, p_q public.vector, p_v timestamptz, p_k timestamptz, p_rerank int, p_lim int)
returns table (ver_id bigint, scope text, anchor_id bigint, rnk bigint) language sql stable set search_path = '' as $$
  with qb as materialized (select public.binary_quantize(p_q)::bit(1536) as q),
  c as materialized (
    select b.id, b.scope, b.text_hash from gnomon.vbit b
    where b.tenant_id = p_tenant
      and b.valid_from <= p_v and p_v < b.valid_to and b.recorded_from <= p_k and p_k < b.recorded_to
    order by b.q OPERATOR(public.<~>) (select q from qb) limit p_rerank),
  rr as materialized (
    select c.id, c.scope, (e.embedding OPERATOR(public.<=>) p_q) as d
    from c cross join lateral (select x.embedding from gnomon.emb x where x.tenant_id = p_tenant and x.scope = c.scope and x.text_hash = c.text_hash limit 1) e
    order by d limit p_lim)
  select rr.id, rr.scope, v.anchor_id, row_number() over (order by rr.d, rr.id)
  from rr cross join lateral (select y.anchor_id from gnomon.ver y where y.id = rr.id and y.tenant_id = p_tenant limit 1) v $$;
grant execute on all functions in schema gnomon to gnomon_app;
