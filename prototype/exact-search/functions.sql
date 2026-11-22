-- PROTOTYPE, wipe me. Invoker-mode Store functions (search_path = ''), so RLS applies to every read.
-- As-of condition used throughout:
--   valid_from <= v < valid_to  and  recorded_from <= k < recorded_to

-- ---- Test 1: exact vector scans over the visible rows ------------------------------------
drop function if exists gnomon.vsearch_full(uuid, public.vector, int);
create function gnomon.vsearch_full(p_tenant uuid, p_q public.vector, p_k int)
returns table (scope text, text_hash bytea, dist float8) language sql stable set search_path = '' as $$
  select e.scope, e.text_hash, (e.embedding OPERATOR(public.<=>) p_q)::float8 as d
  from gnomon.emb e where e.tenant_id = p_tenant
  order by d limit p_k $$;

drop function if exists gnomon.vsearch_half(uuid, public.vector, int, int);
create function gnomon.vsearch_half(p_tenant uuid, p_q public.vector, p_rerank int, p_k int)
returns table (scope text, text_hash bytea, dist float8) language sql stable set search_path = '' as $$
  with qh as materialized (select p_q::public.halfvec(1536) as q),
  c as materialized (
    select h.scope, h.text_hash from gnomon.emb_half h
    where h.tenant_id = p_tenant
    order by h.embedding OPERATOR(public.<=>) (select q from qh) limit p_rerank)
  select e.scope, e.text_hash, (e.embedding OPERATOR(public.<=>) p_q)::float8 as d
  from c join gnomon.emb e on e.tenant_id = p_tenant and e.scope = c.scope and e.text_hash = c.text_hash
  order by d limit p_k $$;

drop function if exists gnomon.vsearch_bit(uuid, public.vector, int, int);
create function gnomon.vsearch_bit(p_tenant uuid, p_q public.vector, p_rerank int, p_k int)
returns table (scope text, text_hash bytea, dist float8) language sql stable set search_path = '' as $$
  with qb as materialized (select public.binary_quantize(p_q)::bit(1536) as q),
  c as materialized (
    select b.scope, b.text_hash from gnomon.emb_bit b
    where b.tenant_id = p_tenant
    order by b.q OPERATOR(public.<~>) (select q from qb) limit p_rerank)
  select e.scope, e.text_hash, (e.embedding OPERATOR(public.<=>) p_q)::float8 as d
  from c join gnomon.emb e on e.tenant_id = p_tenant and e.scope = c.scope and e.text_hash = c.text_hash
  order by d limit p_k $$;

-- ---- Test 3: full-text, scan of tsvector against the token table ------------------------
drop function if exists gnomon.fts_scan(uuid, text, timestamptz, timestamptz, int);
create function gnomon.fts_scan(p_tenant uuid, p_text text, p_v timestamptz, p_k timestamptz, p_lim int)
returns table (ver_id bigint, score float8) language sql stable set search_path = '' as $$
  with q as materialized (
    select pg_catalog.to_tsquery('simple'::regconfig, string_agg(t.lexeme, ' | ')) as tq
    from pg_catalog.unnest(pg_catalog.to_tsvector('simple'::regconfig, p_text)) t)
  select v.id, pg_catalog.ts_rank(v.fts, q.tq)::float8 as s
  from gnomon.ver v, q
  where v.tenant_id = p_tenant and v.fts @@ q.tq
    and v.valid_from <= p_v and p_v < v.valid_to and v.recorded_from <= p_k and p_k < v.recorded_to
  order by s desc, v.id limit p_lim $$;

-- BM25 over the token table. Statistics are per Tenant (tok_df, tenant_stats).
drop function if exists gnomon.c_fts(uuid, text, timestamptz, timestamptz, int);
create function gnomon.c_fts(p_tenant uuid, p_text text, p_v timestamptz, p_k timestamptz, p_lim int)
returns table (ver_id bigint, scope text, anchor_id bigint, rnk bigint) language sql stable set search_path = '' as $$
  with q as materialized (select distinct t.lexeme from pg_catalog.unnest(pg_catalog.to_tsvector('simple'::regconfig, p_text)) t),
  st as materialized (select s.n_docs::float8 as n, (s.total_len::float8 / s.n_docs) as avgdl from gnomon.tenant_stats s where s.tenant_id = p_tenant),
  idf as materialized (
    select q.lexeme, ln(1 + (st.n - d.df + 0.5) / (d.df + 0.5)) as w
    from q join gnomon.tok_df d on d.tenant_id = p_tenant and d.token = q.lexeme cross join st),
  sc as materialized (
    select t.ver_id, sum(i.w * t.tf * 2.2 / (t.tf + 1.2 * (0.25 + 0.75 * t.dl / st.avgdl))) as s
    from idf i cross join st join gnomon.tok t on t.tenant_id = p_tenant and t.token = i.lexeme
    group by t.ver_id),
  top as (
    select v.id, v.scope, v.anchor_id, sc.s
    from sc join gnomon.ver v on v.id = sc.ver_id and v.tenant_id = p_tenant
    where v.valid_from <= p_v and p_v < v.valid_to and v.recorded_from <= p_k and p_k < v.recorded_to
    order by sc.s desc, v.id limit p_lim)
  select id, scope, anchor_id, row_number() over (order by s desc, id) from top $$;

-- Variant: take the top candidates by score first, check the As-of conditions only on those.
drop function if exists gnomon.c_fts_late(uuid, text, timestamptz, timestamptz, int);
create function gnomon.c_fts_late(p_tenant uuid, p_text text, p_v timestamptz, p_k timestamptz, p_lim int)
returns table (ver_id bigint, scope text, anchor_id bigint, rnk bigint) language sql stable set search_path = '' as $$
  with q as materialized (select distinct t.lexeme from pg_catalog.unnest(pg_catalog.to_tsvector('simple'::regconfig, p_text)) t),
  st as materialized (select s.n_docs::float8 as n, (s.total_len::float8 / s.n_docs) as avgdl from gnomon.tenant_stats s where s.tenant_id = p_tenant),
  idf as materialized (
    select q.lexeme, ln(1 + (st.n - d.df + 0.5) / (d.df + 0.5)) as w
    from q join gnomon.tok_df d on d.tenant_id = p_tenant and d.token = q.lexeme cross join st),
  sc as materialized (
    select t.ver_id, sum(i.w * t.tf * 2.2 / (t.tf + 1.2 * (0.25 + 0.75 * t.dl / st.avgdl))) as s
    from idf i cross join st join gnomon.tok t on t.tenant_id = p_tenant and t.token = i.lexeme
    group by t.ver_id order by s desc limit p_lim * 4),
  top as (
    select v.id, v.scope, v.anchor_id, sc.s
    from sc join gnomon.ver v on v.id = sc.ver_id and v.tenant_id = p_tenant
    where v.valid_from <= p_v and p_v < v.valid_to and v.recorded_from <= p_k and p_k < v.recorded_to
    order by sc.s desc, v.id limit p_lim)
  select id, scope, anchor_id, row_number() over (order by s desc, id) from top $$;

-- ---- Test 4: As-of vector search, embeddings shared by text hash ------------------------
drop function if exists gnomon.c_vec(uuid, public.vector, timestamptz, timestamptz, int, int);
create function gnomon.c_vec(p_tenant uuid, p_q public.vector, p_v timestamptz, p_k timestamptz, p_rerank int, p_lim int)
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
    select e.scope, e.text_hash, (e.embedding OPERATOR(public.<=>) p_q) as d
    from c join gnomon.emb e on e.tenant_id = p_tenant and e.scope = c.scope and e.text_hash = c.text_hash
    order by d limit p_lim)
  select v.id, v.scope, v.anchor_id, row_number() over (order by rr.d, v.id)
  from rr join gnomon.ver v on v.tenant_id = p_tenant and v.scope = rr.scope and v.text_hash = rr.text_hash
   and v.valid_from <= p_v and p_v < v.valid_to and v.recorded_from <= p_k and p_k < v.recorded_to $$;

-- Distance only, no As-of conditions, for the cost of the As-of join.
drop function if exists gnomon.c_vec_noasof(uuid, public.vector, int, int);
create function gnomon.c_vec_noasof(p_tenant uuid, p_q public.vector, p_rerank int, p_lim int)
returns table (scope text, text_hash bytea, dist float8) language sql stable set search_path = '' as $$
  select * from gnomon.vsearch_bit(p_tenant, p_q, p_rerank, p_lim) $$;

-- Node names and Aliases. Current-only partial index when p_current, else all rows at the As-of instants.
drop function if exists gnomon.name_search_cur(uuid, public.vector, int, int);
create function gnomon.name_search_cur(p_tenant uuid, p_q public.vector, p_rerank int, p_k int)
returns table (entity_id bigint, dist float8) language sql stable set search_path = '' as $$
  with qb as materialized (select public.binary_quantize(p_q)::bit(1536) as q),
  c as materialized (
    select b.id from gnomon.name_bit b
    where b.tenant_id = p_tenant and b.recorded_to = 'infinity'::timestamptz
    order by b.q OPERATOR(public.<~>) (select q from qb) limit p_rerank)
  select n.entity_id, (n.embedding OPERATOR(public.<=>) p_q)::float8 as d
  from c join gnomon.name_item n on n.id = c.id and n.tenant_id = p_tenant
  order by d limit p_k $$;

drop function if exists gnomon.name_search_cur_full(uuid, public.vector, int);
create function gnomon.name_search_cur_full(p_tenant uuid, p_q public.vector, p_k int)
returns table (entity_id bigint, dist float8) language sql stable set search_path = '' as $$
  select n.entity_id, (n.embedding OPERATOR(public.<=>) p_q)::float8 as d
  from gnomon.name_item n where n.tenant_id = p_tenant and n.recorded_to = 'infinity'::timestamptz
  order by d limit p_k $$;

drop function if exists gnomon.name_search_asof(uuid, public.vector, timestamptz, int, int);
create function gnomon.name_search_asof(p_tenant uuid, p_q public.vector, p_k timestamptz, p_rerank int, p_lim int)
returns table (entity_id bigint, dist float8) language sql stable set search_path = '' as $$
  with qb as materialized (select public.binary_quantize(p_q)::bit(1536) as q),
  c as materialized (
    select b.id from gnomon.name_item n join gnomon.name_bit b on b.id = n.id and b.tenant_id = p_tenant
    where n.tenant_id = p_tenant and n.recorded_from <= p_k and p_k < n.recorded_to
    order by b.q OPERATOR(public.<~>) (select q from qb) limit p_rerank)
  select n.entity_id, (n.embedding OPERATOR(public.<=>) p_q)::float8 as d
  from c join gnomon.name_item n on n.id = c.id and n.tenant_id = p_tenant
  order by d limit p_lim $$;

-- ---- Test 5: neighbourhood, fusion and the two graph signals -----------------------------
drop function if exists gnomon.c_nbr(uuid, text[], bigint[], int, timestamptz, timestamptz, int);
create function gnomon.c_nbr(p_tenant uuid, p_scopes text[], p_nodes bigint[], p_depth int, p_v timestamptz, p_k timestamptz, p_lim int)
returns table (ver_id bigint, rnk bigint) language sql stable set search_path = '' as $$
  with recursive walk(scope, node, hop, via) as (
    select s.scope, s.node, 0, null::bigint from rows from (pg_catalog.unnest(p_scopes), pg_catalog.unnest(p_nodes)) as s(scope, node)
    union all
    select w.scope, x.nb, w.hop + 1, x.eid
    from walk w cross join lateral (
      (select e.dst_id as nb, e.id as eid from gnomon.ver e
        where e.tenant_id = p_tenant and e.scope = w.scope and e.kind = 'e' and e.src_id = w.node
          and e.valid_from <= p_v and p_v < e.valid_to and e.recorded_from <= p_k and p_k < e.recorded_to limit 50)
      union all
      (select e.src_id, e.id from gnomon.ver e
        where e.tenant_id = p_tenant and e.scope = w.scope and e.kind = 'e' and e.dst_id = w.node
          and e.valid_from <= p_v and p_v < e.valid_to and e.recorded_from <= p_k and p_k < e.recorded_to limit 50)
    ) x
    where w.hop < least(p_depth, 2)
  ),
  cand as (
    select via as vid, min(hop) as h, count(*) as c from walk where via is not null group by via
    union all
    select nv.id, min(w.hop), count(*)
    from walk w join gnomon.ver nv on nv.tenant_id = p_tenant and nv.scope = w.scope and nv.entity_id = w.node and nv.kind = 'n'
      and nv.valid_from <= p_v and p_v < nv.valid_to and nv.recorded_from <= p_k and p_k < nv.recorded_to
    where w.hop > 0 group by nv.id
  )
  select vid, row_number() over (order by h, c desc, vid)
  from (select * from cand order by h, c desc, vid limit p_lim) t $$;

-- The two graph signals, for a set of candidate Versions, computed at search time.
drop function if exists gnomon.signals(uuid, bigint[], timestamptz, timestamptz);
create function gnomon.signals(p_tenant uuid, p_ids bigint[], p_v timestamptz, p_k timestamptz)
returns table (ver_id bigint, mentions bigint, degree bigint) language sql stable set search_path = '' as $$
  with info as materialized (select x.id, x.scope, x.anchor_id from gnomon.ver x where x.id = any(p_ids) and x.tenant_id = p_tenant)
  select i.id,
    (select count(*) from gnomon.citation2 c
      where c.tenant_id = p_tenant and c.scope = i.scope and c.entity_id = i.anchor_id and c.ep_rf <= p_k),
    (select count(*) from gnomon.ver e where e.tenant_id = p_tenant and e.scope = i.scope and e.kind = 'e' and e.src_id = i.anchor_id
        and e.valid_from <= p_v and p_v < e.valid_to and e.recorded_from <= p_k and p_k < e.recorded_to)
    + (select count(*) from gnomon.ver e where e.tenant_id = p_tenant and e.scope = i.scope and e.kind = 'e' and e.dst_id = i.anchor_id
        and e.valid_from <= p_v and p_v < e.valid_to and e.recorded_from <= p_k and p_k < e.recorded_to)
  from info i $$;

drop function if exists gnomon.search(uuid, public.vector, text, timestamptz, timestamptz, int, float8, float8, int);
create function gnomon.search(p_tenant uuid, p_q public.vector, p_text text, p_v timestamptz, p_k timestamptz,
                              p_depth int default 1, p_w_mentions float8 default 0.5, p_w_conn float8 default 0.5, p_limit int default 20)
returns table (ver_id bigint, score float8) language sql stable set search_path = '' as $$
  with v as materialized (select * from gnomon.c_vec2(p_tenant, p_q, p_v, p_k, 200, 100)),
  f as materialized (select * from gnomon.c_fts(p_tenant, p_text, p_v, p_k, 100)),
  seed as materialized (
    select x.ver_id, min(x.scope) as scope, min(x.anchor_id) as anchor, sum(1.0 / (60 + x.rnk)) as s
    from (select ver_id, scope, anchor_id, rnk from v union all select ver_id, scope, anchor_id, rnk from f) x
    group by x.ver_id order by s desc, x.ver_id limit 10),
  nb as materialized (
    select * from gnomon.c_nbr(p_tenant, (select array_agg(scope) from seed), (select array_agg(anchor) from seed), p_depth, p_v, p_k, 100)),
  fused as materialized (
    select u.ver_id, sum(1.0 / (60 + u.rnk)) as s
    from (select ver_id, rnk from v union all select ver_id, rnk from f union all select ver_id, rnk from nb) u
    group by u.ver_id order by s desc, u.ver_id limit 100),
  sig as materialized (select * from gnomon.signals(p_tenant, (select array_agg(ver_id) from fused), p_v, p_k)),
  mr as (select ver_id, rank() over (order by mentions desc) as r from sig where mentions > 0),
  cr as (select ver_id, rank() over (order by degree desc) as r from sig where degree > 0)
  select fu.ver_id, (fu.s + coalesce(p_w_mentions / (60 + mr.r), 0) + coalesce(p_w_conn / (60 + cr.r), 0))::float8 as sc
  from fused fu left join mr using (ver_id) left join cr using (ver_id)
  order by sc desc, fu.ver_id limit p_limit $$;

grant execute on all functions in schema gnomon to gnomon_app;
