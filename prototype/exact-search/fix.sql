-- PROTOTYPE, wipe me. Plan fixes: drive graph functions from PK / index lookups instead of scans.
alter function gnomon.c_nbr(uuid, text[], bigint[], int, timestamptz, timestamptz, int) reset enable_hashjoin;
alter function gnomon.c_nbr(uuid, text[], bigint[], int, timestamptz, timestamptz, int) reset enable_mergejoin;
alter function gnomon.signals(uuid, bigint[], timestamptz, timestamptz) reset enable_hashjoin;
alter function gnomon.signals(uuid, bigint[], timestamptz, timestamptz) reset enable_mergejoin;

create or replace function gnomon.c_nbr(p_tenant uuid, p_scopes text[], p_nodes bigint[], p_depth int, p_v timestamptz, p_k timestamptz, p_lim int)
returns table (ver_id bigint, rnk bigint) language sql stable set search_path = '' as $$
  with recursive walk(scope, node, hop, via) as (
    select s.scope, s.node, 0, null::bigint from rows from (pg_catalog.unnest(p_scopes), pg_catalog.unnest(p_nodes)) as s(scope, node)
    union all
    select w.scope, x.other, w.hop + 1, x.id
    from walk w cross join lateral (
      select a.other, a.id from gnomon.adj a
      where a.tenant_id = p_tenant and a.scope = w.scope and a.node = w.node
        and a.valid_from <= p_v and p_v < a.valid_to and a.recorded_from <= p_k and p_k < a.recorded_to limit 50
    ) x
    where w.hop < least(p_depth, 2)
  ),
  cand as (
    select via as vid, min(hop) as h, count(*) as c from walk where via is not null group by via
    union all
    select nv.id, min(w.hop), count(*)
    from walk w cross join lateral (
      select v.id from gnomon.ver v
      where v.tenant_id = p_tenant and v.scope = w.scope and v.entity_id = w.node and v.kind = 'n'
        and v.valid_from <= p_v and p_v < v.valid_to and v.recorded_from <= p_k and p_k < v.recorded_to) nv
    where w.hop > 0 group by nv.id
  )
  select vid, row_number() over (order by h, c desc, vid)
  from (select * from cand order by h, c desc, vid limit p_lim) t $$;

create or replace function gnomon.signals(p_tenant uuid, p_ids bigint[], p_v timestamptz, p_k timestamptz)
returns table (ver_id bigint, mentions bigint, degree bigint) language sql stable set search_path = '' as $$
  with info as materialized (
    select x.id, x.scope, x.anchor_id from pg_catalog.unnest(p_ids) as u(id) join gnomon.ver x on x.id = u.id where x.tenant_id = p_tenant)
  select i.id,
    (select count(*) from gnomon.citation2 c where c.tenant_id = p_tenant and c.scope = i.scope and c.entity_id = i.anchor_id and c.ep_rf <= p_k),
    (select count(*) from gnomon.adj a where a.tenant_id = p_tenant and a.scope = i.scope and a.node = i.anchor_id
        and a.valid_from <= p_v and p_v < a.valid_to and a.recorded_from <= p_k and p_k < a.recorded_to)
  from info i $$;

create or replace function gnomon.signals_roll(p_tenant uuid, p_ids bigint[])
returns table (ver_id bigint, mentions bigint, degree bigint) language sql stable set search_path = '' as $$
  with info as materialized (
    select x.id, x.scope, x.anchor_id from pg_catalog.unnest(p_ids) as u(id) join gnomon.ver x on x.id = u.id where x.tenant_id = p_tenant)
  select i.id,
    coalesce((select m.mentions from gnomon.roll_ment m where m.tenant_id = p_tenant and m.scope = i.scope and m.node = i.anchor_id), 0),
    coalesce((select d.degree from gnomon.roll_now d where d.tenant_id = p_tenant and d.scope = i.scope and d.node = i.anchor_id), 0)
  from info i $$;
grant execute on all functions in schema gnomon to gnomon_app;
