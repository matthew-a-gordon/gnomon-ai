-- PROTOTYPE, wipe me. Erasure targets: one Episode that states a value in many Nodes that already have history.
set role gnomon_owner;
create table if not exists proto.special (label text primary key, episode_id uuid, tenant_id uuid, recorded_at timestamptz);
create table if not exists proto.special_node (label text, node_id uuid);

create or replace function gnomon.proto_gen_special(p_label text, p_tenant uuid, p_n_nodes int, p_rec timestamptz,
                                         p_vf timestamptz, p_vt timestamptz, p_min_eps int) returns jsonb
language plpgsql set search_path = '' as $$
declare v_ep uuid := gnomon.uuid_v7(); x record; n int := 0; cited int;
begin
  create temp table pick on commit drop as
    select n.id node_id, k.ranked_merge rk from gnomon.node n join gnomon.kind k on k.tenant_id = n.tenant_id and k.name = n.kind
     where n.tenant_id = p_tenant
       and not exists (select 1 from proto.cell_node c where c.node_id = n.id)
       and not exists (select 1 from proto.special_node s where s.node_id = n.id)
       and (select count(distinct episode_id) from gnomon.contribution c where c.node_id = n.id) >= p_min_eps
     order by random() limit p_n_nodes;
  insert into gnomon.episode (tenant_id, id, authority, reference_time, recorded_at) values (p_tenant, v_ep, 3, now(), p_rec);
  insert into gnomon.contribution (tenant_id, episode_id, node_id, property, value, valid_from, valid_to, valid_from_prec, valid_to_prec, recorded_at)
    select p_tenant, v_ep, node_id, 'prop0', to_jsonb('vSPECIAL'::text), p_vf, p_vt, 4, 4, p_rec from pick;
  for x in select * from pick loop
    perform gnomon.rebuild_node(p_tenant, x.node_id, p_rec, x.rk); n := n + 1;
  end loop;
  insert into proto.special values (p_label, v_ep, p_tenant, p_rec);
  insert into proto.special_node select p_label, node_id from pick;
  select count(distinct c.version_id) into cited from gnomon.citation c where c.episode_id = v_ep;
  return jsonb_build_object('label', p_label, 'episode', v_ep, 'nodes', n, 'cited_versions', cited);
end $$;
