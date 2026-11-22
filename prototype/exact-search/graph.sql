-- PROTOTYPE, wipe me. Graph-signal support: Citations carry the Episode's recorded time; edge indexes cover the other endpoint.
set maintenance_work_mem = '2GB'; set work_mem = '1GB';
drop table if exists gnomon.citation2;
create table gnomon.citation2 as
  select c.tenant_id, c.scope, c.entity_id, c.episode_id, e.recorded_from as ep_rf
  from gnomon.citation c join gnomon.episode e on e.tenant_id = c.tenant_id and e.scope = c.scope and e.id = c.episode_id;
create index cit2_ent on gnomon.citation2 (tenant_id, scope, entity_id) include (ep_rf);
drop index gnomon.ver_src; drop index gnomon.ver_dst;
create index ver_src on gnomon.ver (tenant_id, scope, src_id) include (id, dst_id, valid_from, valid_to, recorded_from, recorded_to) where kind = 'e';
create index ver_dst on gnomon.ver (tenant_id, scope, dst_id) include (id, src_id, valid_from, valid_to, recorded_from, recorded_to) where kind = 'e';
vacuum (analyze, freeze) gnomon.citation2;
vacuum analyze gnomon.ver;
alter table gnomon.citation2 enable row level security;
create policy p_all on gnomon.citation2 for all to gnomon_app using (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()));
grant select on gnomon.citation2 to gnomon_app;
