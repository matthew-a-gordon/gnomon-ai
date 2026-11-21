-- PROTOTYPE, wipe me. Functions for the recompute-cost prototype. Run as superuser; objects are owned by gnomon_owner.
set role gnomon_owner;

-- One recorded-time step for one Node: recompute the current state over a valid-time window from the visible
-- Contributions, then close the open Versions that no longer match and open the new ones. Used by the write path
-- (p_visible = infinity, p_at = transaction time) and by Erasure replay (p_visible = p_at = a past step time).
create function gnomon.apply_step(p_tenant uuid, p_node uuid, p_at timestamptz, p_wfrom timestamptz, p_wto timestamptz,
                                  p_visible timestamptz, p_ranked boolean, out closed int, out inserted int, out cands int)
language plpgsql security invoker set search_path = '' as $$
declare w0 timestamptz; w1 timestamptz; v_new jsonb;
begin
  -- extend the window to whole open Versions that overlap or touch it, so adjacent equal Versions merge as in a full recompute
  select least(p_wfrom, min(valid_from)), greatest(p_wto, max(valid_to)) into w0, w1
    from gnomon.version
   where tenant_id = p_tenant and node_id = p_node and recorded_to = 'infinity'
     and valid_from <= p_wto and valid_to >= p_wfrom;

  with c as (
    select k.id, k.episode_id, k.property, k.value,
           greatest(k.valid_from, w0) vf, least(k.valid_to, w1) vt,
           case when p_ranked then e.authority else 0 end auth, e.reference_time ref, k.recorded_at rec
      from gnomon.contribution k join gnomon.episode e on e.id = k.episode_id
     where k.tenant_id = p_tenant and k.node_id = p_node and k.recorded_at <= p_visible
       and k.valid_from < w1 and k.valid_to > w0
  ), pts as (
    select vf p from c union select vt from c union select w0 union select w1
  ), segs as (
    select p sf, lead(p) over (order by p) st from pts
  ), win as (   -- the winning Contribution per segment and property: Authority (if ranked), Reference time, Recorded time
    select distinct on (s.sf, c.property) s.sf, s.st, c.property, c.value
      from segs s join c on c.vf <= s.sf and c.vt >= s.st
     where s.st is not null
     order by s.sf, c.property, c.auth desc, c.ref desc, c.rec desc, c.id desc
  ), cit as (   -- Citations: every overlapping Contribution that states the winning value
    select w.sf, w.property, c.episode_id
      from win w join c on c.property = w.property and c.value = w.value and c.vf <= w.sf and c.vt >= w.st
     where w.value <> 'null'::jsonb
     group by w.sf, w.property, c.episode_id
  ), segp as (
    select sf, max(st) st, jsonb_object_agg(property, value) filter (where value <> 'null'::jsonb) props from win group by sf
  ), isl as (
    select sf, st, props,
           sum(case when lag_st = sf and lag_props = props then 0 else 1 end) over (order by sf) grp
      from (select sf, st, props, lag(st) over (order by sf) lag_st, lag(props) over (order by sf) lag_props
              from segp where props is not null) x
  ), vers as (
    select grp, min(sf) vf, max(st) vt, (array_agg(props))[1] props from isl group by grp
  ), vc as (
    select i.grp, jsonb_agg(distinct jsonb_build_array(cit.property, cit.episode_id)) cites
      from isl i join cit on cit.sf = i.sf group by i.grp
  )
  select coalesce(jsonb_agg(jsonb_build_object('vf', v.vf, 'vt', v.vt, 'props', v.props, 'cites', coalesce(vc.cites, '[]'::jsonb))), '[]'::jsonb)
    into v_new from vers v left join vc using (grp);
  cands := jsonb_array_length(v_new);

  update gnomon.version v set recorded_to = p_at
   where v.tenant_id = p_tenant and v.node_id = p_node and v.recorded_to = 'infinity'
     and v.valid_from >= w0 and v.valid_to <= w1
     and not exists (select 1 from jsonb_to_recordset(v_new) n(vf timestamptz, vt timestamptz, props jsonb)
                      where n.vf = v.valid_from and n.vt = v.valid_to and n.props = v.props);
  get diagnostics closed = row_count;

  insert into gnomon.version (id, tenant_id, node_id, props, valid_from, valid_to, valid_from_prec, valid_to_prec, recorded_from, rule)
  select gnomon.uuid_v7(), p_tenant, p_node, n.props, n.vf, n.vt,
         case when n.vf = '-infinity' then 0 else 4 end, case when n.vt = 'infinity' then 0 else 4 end,
         p_at, case when p_ranked then 'ranked' else 'recency' end
    from jsonb_to_recordset(v_new) n(vf timestamptz, vt timestamptz, props jsonb)
   where not exists (select 1 from gnomon.version v
                      where v.tenant_id = p_tenant and v.node_id = p_node and v.recorded_to = 'infinity'
                        and v.valid_from = n.vf and v.valid_to = n.vt and v.props = n.props);
  get diagnostics inserted = row_count;

  insert into gnomon.citation (tenant_id, version_id, episode_id, property)
  select p_tenant, v.id, (cc ->> 1)::uuid, cc ->> 0
    from jsonb_to_recordset(v_new) n(vf timestamptz, vt timestamptz, cites jsonb)
    join gnomon.version v on v.tenant_id = p_tenant and v.node_id = p_node and v.recorded_to = 'infinity'
                         and v.valid_from = n.vf and v.valid_to = n.vt
    cross join lateral jsonb_array_elements(n.cites) cc
  on conflict do nothing;
end $$;

-- The patch write: one Episode, one Valid-time interval, several properties. Runs as the invoker under RLS.
create function gnomon.write_patch(p_tenant uuid, p_node uuid, p_authority int, p_reference timestamptz,
                                   p_vf timestamptz, p_vt timestamptz, p_patch jsonb) returns jsonb
language plpgsql security invoker set search_path = '' as $$
declare v_ep uuid := gnomon.uuid_v7(); v_ranked boolean; r record; v_now timestamptz := transaction_timestamp();
begin
  if p_tenant is distinct from gnomon.tenant_id() then raise exception 'tenant does not match the claims'; end if;
  perform pg_advisory_xact_lock(hashtextextended(p_node::text, 0));   -- serialises writes and Erasure per Node
  select k.ranked_merge into v_ranked from gnomon.node n join gnomon.kind k on k.tenant_id = n.tenant_id and k.name = n.kind
   where n.tenant_id = p_tenant and n.id = p_node;
  if not found then raise exception 'no such node'; end if;
  insert into gnomon.episode (tenant_id, id, authority, reference_time, recorded_at) values (p_tenant, v_ep, p_authority, p_reference, v_now);
  insert into gnomon.contribution (tenant_id, episode_id, node_id, property, value, valid_from, valid_to, valid_from_prec, valid_to_prec, recorded_at)
  select p_tenant, v_ep, p_node, e.key, e.value, p_vf, p_vt, 4, 4, v_now from jsonb_each(p_patch) e;
  select * into r from gnomon.apply_step(p_tenant, p_node, v_now, p_vf, p_vt, 'infinity', v_ranked);
  return jsonb_build_object('closed', r.closed, 'inserted', r.inserted, 'cands', r.cands);
end $$;

-- Rebuild a Node's history from its remaining Contributions, from recorded time p_from onward (-infinity = everything).
-- Truncate: drop the Versions that began at or after p_from, reopen the ones alive just before it, then replay each step.
create function gnomon.rebuild_node(p_tenant uuid, p_node uuid, p_from timestamptz, p_ranked boolean) returns jsonb
language plpgsql security invoker set search_path = '' as $$
declare s record; r record; n_del int; n_reopen int; steps int := 0; closed int := 0; ins int := 0;
begin
  delete from gnomon.version where tenant_id = p_tenant and node_id = p_node and recorded_from >= p_from;
  get diagnostics n_del = row_count;
  update gnomon.version set recorded_to = 'infinity'
   where tenant_id = p_tenant and node_id = p_node and recorded_to >= p_from and recorded_to <> 'infinity';
  get diagnostics n_reopen = row_count;
  for s in select recorded_at t, min(valid_from) wf, max(valid_to) wt from gnomon.contribution
            where tenant_id = p_tenant and node_id = p_node and recorded_at >= p_from
            group by recorded_at order by recorded_at loop
    select * into r from gnomon.apply_step(p_tenant, p_node, s.t, s.wf, s.wt, s.t, p_ranked);
    steps := steps + 1; closed := closed + r.closed; ins := ins + r.inserted;
  end loop;
  return jsonb_build_object('deleted', n_del, 'reopened', n_reopen, 'steps', steps, 'closed', closed, 'inserted', ins);
end $$;

-- Erasure (definer). p_strategy: 'full' replays every Node's whole history; 'from_earliest' replays from the earliest
-- recorded time the erased Episode touched in that Node.
create function gnomon.erase_episode(p_tenant uuid, p_episode uuid, p_strategy text default 'full') returns jsonb
language plpgsql security definer set search_path = '' as $$
declare
  t0 timestamptz := clock_timestamp(); t1 timestamptz; t2 timestamptz; t3 timestamptz;
  v_nodes jsonb; x jsonb; r jsonb; n_cit int; n_con int;
  nodes int := 0; steps int := 0; deleted int := 0; reopened int := 0; closed int := 0; ins int := 0;
begin
  if p_tenant is distinct from gnomon.tenant_id() then raise exception 'tenant does not match the claims'; end if;
  perform 1 from gnomon.episode where tenant_id = p_tenant and id = p_episode;
  if not found then raise exception 'no such episode'; end if;
  select jsonb_agg(jsonb_build_object('node', c.node_id, 'te', c.te, 'ranked', k.ranked_merge, 'lk', hashtextextended(c.node_id::text, 0)) order by hashtextextended(c.node_id::text, 0))
    into v_nodes
    from (select node_id, min(recorded_at) te from gnomon.contribution where tenant_id = p_tenant and episode_id = p_episode group by node_id) c
    join gnomon.node n on n.id = c.node_id join gnomon.kind k on k.tenant_id = n.tenant_id and k.name = n.kind;
  for x in select * from jsonb_array_elements(coalesce(v_nodes, '[]')) loop
    perform pg_advisory_xact_lock((x ->> 'lk')::bigint);
  end loop;
  t1 := clock_timestamp();
  delete from gnomon.citation where tenant_id = p_tenant and episode_id = p_episode;   get diagnostics n_cit = row_count;
  delete from gnomon.contribution where tenant_id = p_tenant and episode_id = p_episode; get diagnostics n_con = row_count;
  delete from gnomon.episode where tenant_id = p_tenant and id = p_episode;
  t2 := clock_timestamp();
  for x in select * from jsonb_array_elements(coalesce(v_nodes, '[]')) loop
    r := gnomon.rebuild_node(p_tenant, (x ->> 'node')::uuid,
           case when p_strategy = 'full' then '-infinity'::timestamptz else (x ->> 'te')::timestamptz end,
           (x ->> 'ranked')::boolean);
    nodes := nodes + 1; steps := steps + (r ->> 'steps')::int; deleted := deleted + (r ->> 'deleted')::int;
    reopened := reopened + (r ->> 'reopened')::int; closed := closed + (r ->> 'closed')::int; ins := ins + (r ->> 'inserted')::int;
  end loop;
  t3 := clock_timestamp();
  return jsonb_build_object('nodes', nodes, 'steps', steps, 'contributions_deleted', n_con, 'citations_deleted_direct', n_cit,
    'versions_deleted_by_truncate', deleted, 'versions_reopened', reopened, 'versions_closed_by_replay', closed, 'versions_inserted_by_replay', ins,
    'ms_lock', extract(milliseconds from t1 - t0), 'ms_delete', extract(milliseconds from t2 - t1), 'ms_rebuild', extract(milliseconds from t3 - t2));
end $$;
alter function gnomon.erase_episode(uuid, uuid, text) owner to gnomon_owner;
revoke all on function gnomon.erase_episode(uuid, uuid, text) from public;
grant execute on function gnomon.erase_episode(uuid, uuid, text) to authenticated;
grant execute on function gnomon.write_patch(uuid, uuid, int, timestamptz, timestamptz, timestamptz, jsonb) to authenticated;

-- Data generator (superuser load only; not part of the measured paths).
-- Writes p_episodes Episodes with p_fixed_props (or 1..3 when null) properties each, spread over 700 days of Recorded time,
-- over five years of Valid time, then builds the Node's history by a full replay.
create function gnomon.proto_gen_node(p_tenant uuid, p_node uuid, p_kind text, p_episodes int, p_fixed_props int default null) returns void
language plpgsql set search_path = '' as $$
declare v_ranked boolean; v_base timestamptz := date_trunc('second', now()) - interval '720 days';
begin
  insert into gnomon.node (tenant_id, id, kind) values (p_tenant, p_node, p_kind);
  select ranked_merge into v_ranked from gnomon.kind where tenant_id = p_tenant and name = p_kind;
  with eps as materialized (
    select i, gnomon.uuid_v7() id,
           v_base + ((i - 1) + random()) * (interval '700 days' / p_episodes) rec,
           floor(random() * 4)::int auth, random() rr, floor(random() * 6)::int p0,
           coalesce(p_fixed_props, 1 + floor(random() * 3)::int) np,
           timestamptz '2021-01-01' + random() * interval '1826 days' v0,
           interval '1 day' * least(400, 7 + (-ln(1 - random()) * 55)) len, random() shape, random() isend
      from generate_series(1, p_episodes) i
  ), ins_e as (
    insert into gnomon.episode (tenant_id, id, authority, reference_time, recorded_at)
    select p_tenant, id, auth, rec - interval '1 day' * rr * 400, rec from eps returning 1
  )
  insert into gnomon.contribution (tenant_id, episode_id, node_id, property, value, valid_from, valid_to, valid_from_prec, valid_to_prec, recorded_at)
  select p_tenant, e.id, p_node, 'prop' || ((e.p0 + j) % 6),
         case when e.isend < 0.05 and j = 0 and p_fixed_props is null then 'null'::jsonb else to_jsonb('v' || floor(random() * 4)::int) end,
         case when e.shape >= 0.04 and e.shape < 0.06 then '-infinity'::timestamptz else e.v0 end,
         case when e.shape < 0.04 then 'infinity'::timestamptz else e.v0 + e.len end,
         case when e.shape >= 0.04 and e.shape < 0.06 then 0 else 4 end, case when e.shape < 0.04 then 0 else 4 end, e.rec
    from eps e cross join lateral generate_series(0, e.np - 1) j;
  perform gnomon.rebuild_node(p_tenant, p_node, '-infinity', v_ranked);
end $$;

-- Bulk loader: p_count Nodes for one Tenant with a mixed history size (60% small, 30% medium, 10% large), half ranked.
create function gnomon.proto_gen_bulk(p_tenant uuid, p_count int) returns void language plpgsql set search_path = '' as $$
declare r float; e int;
begin
  for i in 1..p_count loop
    r := random();
    e := case when r < 0.6 then 3 + floor(random() * 6)::int when r < 0.9 then 10 + floor(random() * 21)::int else 40 + floor(random() * 81)::int end;
    perform gnomon.proto_gen_node(p_tenant, gnomon.uuid_v7(), case when random() < 0.5 then 'ranked' else 'plain' end, e);
  end loop;
end $$;

-- Benchmark cell Nodes: exactly p_contribs Contributions (2 properties per Episode), recorded in table proto.cell_node.
create function gnomon.proto_gen_cell(p_cell text, p_tenant uuid, p_count int, p_contribs int, p_kind text, p_first_idx int) returns void
language plpgsql set search_path = '' as $$
declare v_node uuid;
begin
  for i in 0..p_count - 1 loop
    v_node := gnomon.uuid_v7();
    perform gnomon.proto_gen_node(p_tenant, v_node, p_kind, p_contribs / 2, 2);
    insert into proto.cell_node (cell, idx, tenant_id, node_id) values (p_cell, p_first_idx + i, p_tenant, v_node);
  end loop;
end $$;
