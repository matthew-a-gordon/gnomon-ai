-- PROTOTYPE, wipe me. Adjacency rows: one row per edge Version and direction, one covering index.
set maintenance_work_mem = '2GB'; set work_mem = '1GB';
drop table if exists gnomon.adj;
create table gnomon.adj as
  select tenant_id, scope, src_id as node, dst_id as other, id, valid_from, valid_to, recorded_from, recorded_to from gnomon.ver where kind = 'e'
  union all
  select tenant_id, scope, dst_id, src_id, id, valid_from, valid_to, recorded_from, recorded_to from gnomon.ver where kind = 'e';
create index adj_node on gnomon.adj (tenant_id, scope, node) include (id, other, valid_from, valid_to, recorded_from, recorded_to);
vacuum (analyze, freeze) gnomon.adj;
drop index if exists gnomon.ver_src; drop index if exists gnomon.ver_dst;
alter table gnomon.adj enable row level security;
create policy p_all on gnomon.adj for all to gnomon_app using (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()));
grant select on gnomon.adj to gnomon_app;

drop function if exists gnomon.c_nbr(uuid, text[], bigint[], int, timestamptz, timestamptz, int);
create function gnomon.c_nbr(p_tenant uuid, p_scopes text[], p_nodes bigint[], p_depth int, p_v timestamptz, p_k timestamptz, p_lim int)
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
    from walk w join gnomon.ver nv on nv.tenant_id = p_tenant and nv.scope = w.scope and nv.entity_id = w.node and nv.kind = 'n'
      and nv.valid_from <= p_v and p_v < nv.valid_to and nv.recorded_from <= p_k and p_k < nv.recorded_to
    where w.hop > 0 group by nv.id
  )
  select vid, row_number() over (order by h, c desc, vid)
  from (select * from cand order by h, c desc, vid limit p_lim) t $$;

drop function if exists gnomon.signals(uuid, bigint[], timestamptz, timestamptz);
create function gnomon.signals(p_tenant uuid, p_ids bigint[], p_v timestamptz, p_k timestamptz)
returns table (ver_id bigint, mentions bigint, degree bigint) language sql stable set search_path = '' as $$
  with info as materialized (select x.id, x.scope, x.anchor_id from gnomon.ver x where x.id = any(p_ids) and x.tenant_id = p_tenant)
  select i.id,
    (select count(*) from gnomon.citation2 c where c.tenant_id = p_tenant and c.scope = i.scope and c.entity_id = i.anchor_id and c.ep_rf <= p_k),
    (select count(*) from gnomon.adj a where a.tenant_id = p_tenant and a.scope = i.scope and a.node = i.anchor_id
        and a.valid_from <= p_v and p_v < a.valid_to and a.recorded_from <= p_k and p_k < a.recorded_to)
  from info i $$;
grant execute on all functions in schema gnomon to gnomon_app;
