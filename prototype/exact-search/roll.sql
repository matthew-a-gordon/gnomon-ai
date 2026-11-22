-- PROTOTYPE, wipe me. Per-(Tenant, Scope) rollup for searches at "now", and nested-loop-only graph functions.
set maintenance_work_mem = '2GB'; set work_mem = '1GB';
drop table if exists gnomon.roll_now;
create table gnomon.roll_now as
  select a.tenant_id, a.scope, a.node, count(*)::int as degree
  from gnomon.adj a where a.recorded_to = 'infinity' and a.valid_from <= '2026-11-21' and '2026-11-21' < a.valid_to
  group by 1, 2, 3;
create table gnomon.roll_ment as
  select tenant_id, scope, entity_id as node, count(*)::int as mentions from gnomon.citation2 group by 1, 2, 3;
alter table gnomon.roll_now add primary key (tenant_id, scope, node);
alter table gnomon.roll_ment add primary key (tenant_id, scope, node);
vacuum (analyze, freeze) gnomon.roll_now; vacuum (analyze, freeze) gnomon.roll_ment;
alter table gnomon.roll_now enable row level security; alter table gnomon.roll_ment enable row level security;
create policy p_all on gnomon.roll_now for all to gnomon_app using (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()));
create policy p_all on gnomon.roll_ment for all to gnomon_app using (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()));
grant select on gnomon.roll_now, gnomon.roll_ment to gnomon_app;
drop function if exists gnomon.signals_roll(uuid, bigint[]);
create function gnomon.signals_roll(p_tenant uuid, p_ids bigint[])
returns table (ver_id bigint, mentions bigint, degree bigint) language sql stable set search_path = '' as $$
  with info as materialized (select x.id, x.scope, x.anchor_id from gnomon.ver x where x.id = any(p_ids) and x.tenant_id = p_tenant)
  select i.id,
    coalesce((select m.mentions from gnomon.roll_ment m where m.tenant_id = p_tenant and m.scope = i.scope and m.node = i.anchor_id), 0),
    coalesce((select d.degree from gnomon.roll_now d where d.tenant_id = p_tenant and d.scope = i.scope and d.node = i.anchor_id), 0)
  from info i $$;
alter function gnomon.c_nbr(uuid, text[], bigint[], int, timestamptz, timestamptz, int) set enable_hashjoin = off;
alter function gnomon.c_nbr(uuid, text[], bigint[], int, timestamptz, timestamptz, int) set enable_mergejoin = off;
alter function gnomon.signals(uuid, bigint[], timestamptz, timestamptz) set enable_hashjoin = off;
alter function gnomon.signals(uuid, bigint[], timestamptz, timestamptz) set enable_mergejoin = off;
grant execute on all functions in schema gnomon to gnomon_app;
