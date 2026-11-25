-- gnomon migration 0010: Retract Episode.
--
-- Retract Episode records a Retraction for every current Version cited only
-- by the Episode, and keeps history readable (ADR-0003). It is not Erasure:
-- it deletes no Contribution, no Citation, no statement and no Episode, so an
-- As-of read at a known-at instant before it returns what it returned then,
-- and Erasure (ticket #13) is still the only route that deletes.
--
-- A Retraction is the statement #11 made (`gnomon.statement`, kind
-- 'retraction'), held like a Contribution, so Retract Episode adds no way to
-- end a Version. It is written by the retracted Episode, at its Authority and
-- Reference time, and for an Edge it is a null for the built-in `holds`. A
-- Version that another Episode also cites is not touched, and ranked merge
-- applies to the Retractions as it does to any statement: a Retraction from
-- a lower-Authority Episode does not end values a higher-Authority Episode
-- supplied where their Valid times overlap (ADR-0007).
--
-- The Episode's own statements are held too (a Correction, an End of
-- validity or a Retraction of the Episode that lost, or that took effect).
-- Retract Episode retracts the Episode, so none of them counts any more, and
-- none takes effect if a higher-Authority Episode is erased later.
-- `gnomon.episode_retraction` records that the Episode is retracted, and
-- `gnomon.recompute_versions`, which this migration replaces, leaves out the
-- Contributions of an Episode's statements and its bound statements once the
-- row exists, apart from the Retractions Retract Episode itself made
-- (`statement.retracts_episode`). The statements stay in their tables, so
-- history stays readable; only the merge stops reading them. Contributions of
-- values, not of statements, still count: they are what the Versions cite, and
-- the Retractions above are what ends the Versions that only the Episode
-- cited.
--
-- Erasure (ticket #13) of a retracted Episode deletes its
-- `episode_retraction` row with the rest: the row cascades from the Episode.
--
-- Callers get no UPDATE or DELETE right and no new SECURITY DEFINER function.
-- `retract_episode` runs as the invoker (ADR-0009).

create table gnomon.episode_retraction (
  tenant_id uuid not null,
  scope text not null,
  episode_id uuid not null,
  recorded_at timestamptz not null default transaction_timestamp(),
  primary key (tenant_id, episode_id),
  foreign key (tenant_id, episode_id) references gnomon.episode (tenant_id, id)
    on delete cascade
);

alter table gnomon.episode_retraction enable row level security;
create policy episode_retraction_tenant_scope on gnomon.episode_retraction
  for all to authenticated, gnomon_worker
  using (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()))
  with check (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()));

-- A Retraction made by Retract Episode, which still counts after its Episode
-- is retracted.
alter table gnomon.statement add column retracts_episode boolean not null default false;
alter table gnomon.statement add constraint statement_retracts_episode_is_a_retraction
  check (not retracts_episode or kind = 'retraction');

-- The marker's Scope is its Episode's, and the caller names only the Episode.
-- The Episode must be visible to the caller.
create function gnomon.episode_retraction_check_insert() returns trigger
  language plpgsql security invoker set search_path = ''
  as $$
  begin
    if new.recorded_at is distinct from transaction_timestamp() then
      raise exception using errcode = 'insufficient_privilege',
        message = 'an Episode''s Retraction is recorded when its transaction started';
    end if;
    select e.scope into new.scope
      from gnomon.episode e where e.tenant_id = new.tenant_id and e.id = new.episode_id;
    if not found then
      raise exception using errcode = 'GNW01',
        message = format('Episode %s is not visible to the caller', new.episode_id);
    end if;
    return new;
  end
  $$;
create trigger episode_retraction_check_insert before insert on gnomon.episode_retraction
  for each row execute function gnomon.episode_retraction_check_insert();

-- At commit, a retracted Episode has no current Version that only it cites,
-- and a statement that retracts an Episode belongs to an Episode retracted in
-- the same transaction. A row inserted directly, without Retract Episode's
-- Retractions, fails here.
create function gnomon.episode_retraction_check_complete() returns trigger
  language plpgsql security invoker set search_path = ''
  as $$
  begin
    if tg_table_name = 'episode_retraction' then
      if exists (
        select from gnomon.version x
        where x.tenant_id = new.tenant_id and x.recorded_to = 'infinity'
          and exists (select from gnomon.citation c
                      where c.tenant_id = x.tenant_id and c.version_id = x.id
                        and c.episode_id = new.episode_id)
          and not exists (select from gnomon.citation c
                          where c.tenant_id = x.tenant_id and c.version_id = x.id
                            and c.episode_id <> new.episode_id)
      ) then
        raise exception using errcode = 'GNW01',
          message = format('Episode %s still has a current Version that only it cites; '
            'retract it with gnomon.retract_episode', new.episode_id);
      end if;
    elsif new.retracts_episode and not exists (
      select from gnomon.episode_retraction r
      where r.tenant_id = new.tenant_id and r.episode_id = new.episode_id
        and r.recorded_at = new.recorded_at
    ) then
      raise exception using errcode = 'GNW01',
        message = 'only Retract Episode makes a Retraction that retracts its Episode';
    end if;
    return null;
  end
  $$;
create constraint trigger episode_retraction_complete
  after insert on gnomon.episode_retraction
  deferrable initially deferred
  for each row execute function gnomon.episode_retraction_check_complete();
create constraint trigger statement_retracts_episode
  after insert on gnomon.statement
  deferrable initially deferred
  for each row when (new.retracts_episode)
  execute function gnomon.episode_retraction_check_complete();

-- Migration 0009's `write_ending`, with the flag that marks a Retraction made
-- by Retract Episode. `write_ending` is now this function with no flag.
create function gnomon.write_ending_as(
  p_tenant_id uuid,
  p_id uuid,
  p_is_edge boolean,
  p_episode_id uuid,
  p_kind text,
  p_version_id uuid,
  p_at timestamptz,
  p_precision text,
  p_retracts_episode boolean
) returns table (status text, recorded_at timestamptz, versions uuid[])
  language plpgsql volatile security invoker set search_path = ''
  as $$
  declare
    v_of constant text := case when p_is_edge then 'Edge' else 'Node' end;
    v_precision gnomon.time_precision;
    v_at timestamptz;
    v_scope text;
    v_reference_time timestamptz;
    v_recorded_at timestamptz;
    v gnomon.version;
    sf timestamptz;
    sfp gnomon.time_precision;
    v_statement bigint;
    step record;
  begin
    if p_episode_id is null then
      raise exception using errcode = 'GNW01',
        message = 'every statement cites an Episode';
    end if;
    if p_kind = 'end_of_validity' then
      if p_at is null or p_at in ('-infinity', 'infinity')
        or p_precision is null or p_precision not in ('year', 'month', 'day', 'instant')
      then
        raise exception using errcode = 'GNW01',
          message = 'an End of validity gives an instant and a precision of year, month, day or instant';
      end if;
      v_precision := p_precision::gnomon.time_precision;
      v_at := gnomon.period_start(p_at, v_precision);
    elsif p_version_id is null then
      raise exception using errcode = 'GNW01',
        message = 'a Retraction names the Version it retracts';
    end if;

    select l.scope into v_scope from gnomon.lock_for_write(p_tenant_id, p_id, p_is_edge) l;
    select e.reference_time, e.recorded_at into v_reference_time, v_recorded_at
      from gnomon.episode e
      where e.tenant_id = p_tenant_id and e.scope = v_scope and e.id = p_episode_id;
    if not found then
      raise exception using errcode = 'GNW01',
        message = format('Episode %s is not visible in the %s''s Scope', p_episode_id, v_of);
    end if;

    if p_kind = 'retraction' then
      select * into v from gnomon.version x
        where x.tenant_id = p_tenant_id and x.scope = v_scope and x.versioned_id = p_id
          and x.id = p_version_id and x.recorded_to = 'infinity';
      if not found then
        raise exception using errcode = 'GNW01',
          message = format('Version %s is not a current Version of %s %s', p_version_id, v_of, p_id);
      end if;
      sf := v.valid_from;
      sfp := v.valid_from_precision;
    else
      select * into v from gnomon.version x
        where x.tenant_id = p_tenant_id and x.scope = v_scope and x.versioned_id = p_id
          and x.recorded_to = 'infinity' and x.valid_from < v_at and x.valid_to > v_at;
      if not found then
        raise exception using errcode = 'GNW01',
          message = format('%s %s has no current Version that holds across %s; '
            'a Retraction states that a Version never held', v_of, p_id, v_at);
      end if;
      sf := v_at;
      sfp := v_precision;
    end if;

    insert into gnomon.statement (
      tenant_id, scope, node_id, edge_id, episode_id, kind, version_id,
      valid_from, valid_from_precision, valid_to, valid_to_precision,
      episode_reference_time, episode_recorded_at, retracts_episode
    ) values (
      p_tenant_id, v_scope, case when not p_is_edge then p_id end,
      case when p_is_edge then p_id end, p_episode_id, p_kind, v.id,
      sf, sfp, v.valid_to, v.valid_to_precision, v_reference_time, v_recorded_at,
      coalesce(p_retracts_episode, false)
    ) returning id into v_statement;

    -- In property order, as a write's Contributions are.
    insert into gnomon.contribution (
      tenant_id, scope, node_id, edge_id, episode_id, property, value,
      valid_from, valid_from_precision, valid_to, valid_to_precision,
      episode_reference_time, episode_recorded_at, statement_id
    )
    select p_tenant_id, v_scope,
           case when not p_is_edge then p_id end, case when p_is_edge then p_id end,
           p_episode_id, x.property, 'null'::jsonb,
           sf, sfp, v.valid_to, v.valid_to_precision, v_reference_time, v_recorded_at, v_statement
    from (
      select k as property from jsonb_object_keys(v.props) k
        where not p_is_edge or p_kind = 'retraction'
      union
      select b.property from gnomon.edge_builtin_rules() b where p_is_edge
    ) x
    order by x.property;

    select * into step from gnomon.recompute_versions(
      p_tenant_id, p_id, p_is_edge, sf, v.valid_to);
    status := case when step.changed then 'written' else 'unchanged' end;
    recorded_at := transaction_timestamp();
    versions := step.opened;
    return next;
  end
  $$;

create or replace function gnomon.write_ending(
  p_tenant_id uuid,
  p_id uuid,
  p_is_edge boolean,
  p_episode_id uuid,
  p_kind text,
  p_version_id uuid,
  p_at timestamptz,
  p_precision text
) returns table (status text, recorded_at timestamptz, versions uuid[])
  language sql volatile security invoker set search_path = ''
  as $$
    select w.status, w.recorded_at, w.versions
    from gnomon.write_ending_as(
      p_tenant_id, p_id, p_is_edge, p_episode_id, p_kind, p_version_id, p_at, p_precision, false) w
  $$;

-- The merge of migration 0008, which no longer reads the statements of a
-- retracted Episode (see the header).
create or replace function gnomon.recompute_versions(
  p_tenant_id uuid,
  p_id uuid,
  p_is_edge boolean,
  p_from timestamptz,
  p_to timestamptz,
  out changed boolean,
  out opened uuid[]
)
  language plpgsql volatile security invoker set search_path = ''
  as $$
  declare
    t constant timestamptz := transaction_timestamp();
    me constant xid8 := pg_current_xact_id();
    v_of constant text := case when p_is_edge then 'Edge' else 'Node' end;
    v_scope text;
    w0 timestamptz;
    w1 timestamptz;
    candidates jsonb;
    stale uuid[];
    v_kind text;
    v_rule text;
    v_ranked boolean;
    -- The statements and bound statements of this Node or Edge that belong to
    -- a retracted Episode, which the merge leaves out. Found once, not for
    -- each Contribution.
    ignored_statements bigint[];
    ignored_bounds bigint[];
  begin
    select l.scope, l.kind into v_scope, v_kind
      from gnomon.lock_for_write(p_tenant_id, p_id, p_is_edge) l;
    select coalesce(array_agg(st.id), '{}') into ignored_statements
      from gnomon.statement st
      where st.tenant_id = p_tenant_id and st.scope = v_scope and st.versioned_id = p_id
        and not st.retracts_episode
        and exists (select from gnomon.episode_retraction r
                    where r.tenant_id = st.tenant_id and r.episode_id = st.episode_id);
    select coalesce(array_agg(b.id), '{}') into ignored_bounds
      from gnomon.bound_statement b
      where b.tenant_id = p_tenant_id and b.scope = v_scope and b.versioned_id = p_id
        and exists (select from gnomon.episode_retraction r
                    where r.tenant_id = b.tenant_id and r.episode_id = b.episode_id);
    -- The rule the Kind has now. It chooses this write's values, and the
    -- Versions the write opens record it (ADR-0007).
    v_rule := gnomon.kind_merge_rule(p_tenant_id, v_kind);
    v_ranked := v_rule = 'ranked';

    -- A transaction that started before another wrote this Node or Edge,
    -- but took the lock after that one committed, would record a Version
    -- earlier than one already recorded. It fails with a serialization
    -- failure, and the caller retries the transaction.
    if exists (
      select from gnomon.contribution c
      where c.tenant_id = p_tenant_id and c.scope = v_scope and c.versioned_id = p_id
        and c.recorded_at > t
    ) or exists (
      select from gnomon.version x
      where x.tenant_id = p_tenant_id and x.scope = v_scope and x.versioned_id = p_id
        and x.recorded_from = t and x.written_in <> me
    ) then
      raise exception using errcode = 'serialization_failure',
        message = format('a later transaction wrote %s %s first; retry the write', v_of, p_id);
    end if;

    -- 1. The window.
    select least(p_from, min(v.valid_from)), greatest(p_to, max(v.valid_to)) into w0, w1
      from gnomon.version v
      where v.tenant_id = p_tenant_id and v.scope = v_scope and v.versioned_id = p_id
        and v.recorded_to = 'infinity'
        and v.valid_from <= p_to and v.valid_to >= p_from;

    -- 2 and 3. The new Versions over the window, each with its Citations.
    with contributions as (
      select c.id, c.episode_id, c.property, c.value, c.place,
             greatest(e.vf, w0) as vf, least(e.vt, w1) as vt,
             case when e.vf >= w0 then e.vfp end as vfp,
             case when e.vt <= w1 then e.vtp end as vtp,
             case when v_ranked then coalesce(ce.authority, 0) else 0 end as auth,
             c.episode_reference_time as ref, c.episode_recorded_at as episode_rec,
             c.recorded_at as rec
      from gnomon.contribution c
      join gnomon.episode ce on ce.tenant_id = c.tenant_id and ce.id = c.episode_id
      cross join lateral (
        select coalesce(f.bound_at, c.valid_from) as vf,
               coalesce(f.bound_precision, c.valid_from_precision) as vfp,
               coalesce(t.bound_at, c.valid_to) as vt,
               coalesce(t.bound_precision, c.valid_to_precision) as vtp
        from (select 1) one
        left join lateral (
          select s.bound_at, s.bound_precision from gnomon.bound_statement s
          join gnomon.episode se on se.tenant_id = s.tenant_id and se.id = s.episode_id
          where s.tenant_id = c.tenant_id and s.contribution_id = c.id and s.side = 'from'
            and s.id <> all (ignored_bounds)
            and (not v_ranked or coalesce(se.authority, 0) >= coalesce(ce.authority, 0))
          order by case when v_ranked then coalesce(se.authority, 0) else 0 end desc,
                   case when v_ranked then s.episode_reference_time end desc nulls last,
                   case when v_ranked then s.recorded_at end desc nulls last,
                   s.id desc
          limit 1
        ) f on true
        left join lateral (
          select s.bound_at, s.bound_precision from gnomon.bound_statement s
          join gnomon.episode se on se.tenant_id = s.tenant_id and se.id = s.episode_id
          where s.tenant_id = c.tenant_id and s.contribution_id = c.id and s.side = 'to'
            and s.id <> all (ignored_bounds)
            and (not v_ranked or coalesce(se.authority, 0) >= coalesce(ce.authority, 0))
          order by case when v_ranked then coalesce(se.authority, 0) else 0 end desc,
                   case when v_ranked then s.episode_reference_time end desc nulls last,
                   case when v_ranked then s.recorded_at end desc nulls last,
                   s.id desc
          limit 1
        ) t on true
      ) e
      where c.tenant_id = p_tenant_id and c.scope = v_scope and c.versioned_id = p_id
        and e.vf < w1 and e.vt > w0
        -- The statements of a retracted Episode no longer count, except the
        -- Retractions Retract Episode made.
        and (c.statement_id is null or c.statement_id <> all (ignored_statements))
    ), bounds as (
      select vf as at from contributions
      union select vt from contributions
      union select w0
      union select w1
    ), segments as (
      select at as sf, lead(at) over (order by at) as st from bounds
    ), winners as (
      select distinct on (s.sf, c.property) s.sf, s.st, c.property, c.value
      from segments s join contributions c on c.vf <= s.sf and c.vt >= s.st
      where s.st is not null
      order by s.sf, c.property, c.auth desc, c.ref desc, c.rec desc, c.id desc
    ), supporters as (
      -- Every Contribution in a segment that states its winning value, and
      -- whether this transaction recorded it. For an Edge, the Contributions
      -- of `holds` are among them, so its Versions cite every Episode that
      -- said it holds.
      select w.sf, w.property, c.episode_id, c.place, c.ref, c.episode_rec,
             bool_or(c.rec = t) as fresh
      from winners w
      join contributions c
        on c.property = w.property and c.value = w.value and c.vf <= w.sf and c.vt >= w.st
      where w.value <> 'null'::jsonb
      group by w.sf, w.property, c.episode_id, c.place, c.ref, c.episode_rec
    ), segment_values as (
      -- A segment holds a Version where an Edge's `holds` wins with a value,
      -- or, for a Node, where any property does.
      select sf, max(st) as st,
             coalesce(
               jsonb_object_agg(property, value)
                 filter (where value <> 'null'::jsonb and not (p_is_edge and property = 'holds')),
               '{}'::jsonb) as props,
             case when p_is_edge then coalesce(bool_or(property = 'holds' and value <> 'null'::jsonb), false)
                  else bool_or(value <> 'null'::jsonb) end as held
      from winners group by sf
    ), islands as (
      -- Runs of touching segments with equal values share an island number.
      select sf, st, props,
             sum(case when prev_st = sf and prev_props = props then 0 else 1 end)
               over (order by sf) as island
      from (
        select sf, st, props,
               lag(st) over (order by sf) as prev_st, lag(props) over (order by sf) as prev_props
        from segment_values where held
      ) x
    ), merged as (
      select island, min(sf) as vf, max(st) as vt, (array_agg(props))[1] as props
      from islands group by island
    ), merged_precision as (
      -- A Version's bound has the finest precision any Contribution gave
      -- that instant, so restating a bound more coarsely loses nothing. A
      -- bound the window clipped keeps the precision of the open Version
      -- that has it.
      select m.island, m.vf, m.vt, m.props,
             coalesce(
               (select max(c.vfp) from contributions c where c.vf = m.vf),
               (select x.valid_from_precision from gnomon.version x
                 where x.tenant_id = p_tenant_id and x.scope = v_scope and x.versioned_id = p_id
                   and x.recorded_to = 'infinity' and x.valid_from = m.vf limit 1),
               gnomon.instant_or_unknown(m.vf)) as vfp,
             coalesce(
               (select max(c.vtp) from contributions c where c.vt = m.vt),
               (select x.valid_to_precision from gnomon.version x
                 where x.tenant_id = p_tenant_id and x.scope = v_scope and x.versioned_id = p_id
                   and x.recorded_to = 'infinity' and x.valid_to = m.vt limit 1),
               gnomon.instant_or_unknown(m.vt)) as vtp
      from merged m
    ), island_citations as (
      select i.island,
             jsonb_agg(distinct jsonb_build_array(
               s.property, s.episode_id, s.place, s.fresh, s.ref, s.episode_rec)) as cites
      from islands i join supporters s on s.sf = i.sf
      group by i.island
    )
    select coalesce(jsonb_agg(jsonb_build_object(
             'vf', m.vf, 'vt', m.vt, 'vfp', m.vfp, 'vtp', m.vtp, 'props', m.props,
             'cites', coalesce(ic.cites, '[]'::jsonb))),
           '[]'::jsonb)
      into candidates
      from merged_precision m left join island_citations ic using (island);

    -- 4. The stale Versions.
    select coalesce(array_agg(v.id), '{}') into stale
      from gnomon.version v
      where v.tenant_id = p_tenant_id and v.scope = v_scope and v.versioned_id = p_id
        and v.recorded_to = 'infinity'
        and v.valid_from >= w0 and v.valid_to <= w1
        and not exists (
          select from jsonb_to_recordset(candidates)
            cand(vf timestamptz, vt timestamptz, vfp text, vtp text, props jsonb)
          where cand.vf = v.valid_from and cand.vt = v.valid_to and cand.props = v.props
            and cand.vfp = v.valid_from_precision::text and cand.vtp = v.valid_to_precision::text
        );
    changed := cardinality(stale) > 0;
    delete from gnomon.citation x
      where x.tenant_id = p_tenant_id and x.version_id = any(stale) and x.written_in = me;
    delete from gnomon.version v
      where v.tenant_id = p_tenant_id and v.id = any(stale) and v.written_in = me;
    update gnomon.version v set recorded_to = t
      where v.tenant_id = p_tenant_id and v.id = any(stale);

    -- 5. Reopen what this transaction closed and now restores, and insert
    -- the rest.
    with reopened as (
      update gnomon.version v set recorded_to = 'infinity'
        from jsonb_to_recordset(candidates)
          cand(vf timestamptz, vt timestamptz, vfp text, vtp text, props jsonb)
        where v.tenant_id = p_tenant_id and v.scope = v_scope and v.versioned_id = p_id
          and v.closed_in = me and v.recorded_to = t
          and v.valid_from = cand.vf and v.valid_to = cand.vt and v.props = cand.props
          and v.valid_from_precision::text = cand.vfp and v.valid_to_precision::text = cand.vtp
        returning v.id
    ), inserted as (
      insert into gnomon.version (
        tenant_id, scope, node_id, edge_id, props, valid_from, valid_from_precision,
        valid_to, valid_to_precision, merge_rule
      )
      select p_tenant_id, v_scope,
             case when not p_is_edge then p_id end, case when p_is_edge then p_id end,
             cand.props,
             cand.vf, cand.vfp::gnomon.time_precision,
             cand.vt, cand.vtp::gnomon.time_precision,
             v_rule::gnomon.merge_rule
      from jsonb_to_recordset(candidates)
        cand(vf timestamptz, vt timestamptz, vfp text, vtp text, props jsonb)
      where not exists (
        select from gnomon.version v
        where v.tenant_id = p_tenant_id and v.scope = v_scope and v.versioned_id = p_id
          and v.valid_from = cand.vf and v.valid_to = cand.vt and v.props = cand.props
          and v.valid_from_precision::text = cand.vfp and v.valid_to_precision::text = cand.vtp
          and (v.recorded_to = 'infinity' or (v.closed_in = me and v.recorded_to = t))
      )
      returning id
    )
    select coalesce(array_agg(id), '{}') into opened
      from (select id from reopened union all select id from inserted) o;
    changed := changed or cardinality(opened) > 0;

    -- 6. The Citations.
    insert into gnomon.citation (
      tenant_id, scope, version_id, episode_id, property, place,
      episode_reference_time, episode_recorded_at
    )
      select p_tenant_id, v_scope, v.id, (cc ->> 1)::uuid, cc ->> 0, cc ->> 2,
             (cc ->> 4)::timestamptz, (cc ->> 5)::timestamptz
      from jsonb_to_recordset(candidates) cand(vf timestamptz, vt timestamptz, cites jsonb)
      join gnomon.version v
        on v.tenant_id = p_tenant_id and v.scope = v_scope and v.versioned_id = p_id
       and v.recorded_to = 'infinity'
       and v.valid_from = cand.vf and v.valid_to = cand.vt
      cross join lateral jsonb_array_elements(cand.cites) cc
      where v.id = any(opened) or (cc ->> 3)::boolean
      on conflict do nothing;
  end
  $$;

-- Retract Episode: records a Retraction for each current Version cited only
-- by the Episode, and stops the Episode's own statements counting. It
-- returns the Versions it retracted. A second call for the same Episode
-- retracts nothing.
--
-- It takes the lock of every Node and Edge it touches up front, in a fixed
-- order, so two Retract Episodes cannot deadlock and a write waits for it
-- (ADR-0007).
create function gnomon.retract_episode(p_tenant_id uuid, p_episode_id uuid)
  returns table (recorded_at timestamptz, versions uuid[])
  language plpgsql volatile security invoker set search_path = ''
  as $$
  declare
    v_scope text;
    t record;
    v record;
    v_done uuid[] := '{}';
  begin
    if p_tenant_id is null or p_tenant_id is distinct from gnomon.tenant_id() then
      raise exception using errcode = 'insufficient_privilege',
        message = 'Retract Episode may act only in the caller''s own Tenant';
    end if;
    select e.scope into v_scope
      from gnomon.episode e where e.tenant_id = p_tenant_id and e.id = p_episode_id;
    if not found then
      raise exception using errcode = 'GNW01',
        message = format('Episode %s is not visible to the caller', p_episode_id);
    end if;
    perform pg_catalog.pg_advisory_xact_lock(gnomon.node_lock_key(p_tenant_id, p_episode_id));
    recorded_at := transaction_timestamp();
    if exists (select from gnomon.episode_retraction r
               where r.tenant_id = p_tenant_id and r.episode_id = p_episode_id) then
      versions := '{}';
      return next;
      return;
    end if;

    -- Every Node and Edge the Episode touched, locked in id order.
    perform pg_catalog.pg_advisory_xact_lock(gnomon.node_lock_key(p_tenant_id, x.id))
      from (
        select c.versioned_id as id from gnomon.contribution c
          where c.tenant_id = p_tenant_id and c.episode_id = p_episode_id
        union
        select s.versioned_id from gnomon.statement s
          where s.tenant_id = p_tenant_id and s.episode_id = p_episode_id
        union
        select b.versioned_id from gnomon.bound_statement b
          where b.tenant_id = p_tenant_id and b.episode_id = p_episode_id
      ) x
      order by x.id;

    -- The Retractions: each current Version that only this Episode cites.
    for v in
      select x.id, x.versioned_id, x.edge_id is not null as is_edge
      from gnomon.version x
      where x.tenant_id = p_tenant_id and x.scope = v_scope and x.recorded_to = 'infinity'
        and exists (select from gnomon.citation c
                    where c.tenant_id = x.tenant_id and c.version_id = x.id
                      and c.episode_id = p_episode_id)
        and not exists (select from gnomon.citation c
                        where c.tenant_id = x.tenant_id and c.version_id = x.id
                          and c.episode_id <> p_episode_id)
      order by x.versioned_id, x.valid_from, x.id
    loop
      -- An earlier Retraction may have recomputed a Version that touches this
      -- one, but a recompute keeps a Version whose values and Valid time have
      -- not changed. One that changed is no longer cited by this Episode alone.
      if exists (select from gnomon.version x
                 where x.tenant_id = p_tenant_id and x.id = v.id and x.recorded_to = 'infinity') then
        perform gnomon.write_ending_as(
          p_tenant_id, v.versioned_id, v.is_edge, p_episode_id, 'retraction', v.id, null, null,
          true);
        -- Under ranked merge a Retraction can lose, and then the Version stays
        -- current: it is not retracted and is not reported.
        if not exists (select from gnomon.version x
                       where x.tenant_id = p_tenant_id and x.id = v.id
                         and x.recorded_to = 'infinity') then
          v_done := v_done || v.id;
        end if;
      end if;
    end loop;

    -- From here the Episode's other statements stop counting.
    insert into gnomon.episode_retraction (tenant_id, episode_id)
      values (p_tenant_id, p_episode_id);

    -- Recompute what those statements shaped, held or in effect.
    for t in
      select x.id, x.is_edge, min(x.vf) as vf, max(x.vt) as vt
      from (
        select s.versioned_id as id, s.edge_id is not null as is_edge,
               s.valid_from as vf, s.valid_to as vt
          from gnomon.statement s
          where s.tenant_id = p_tenant_id and s.episode_id = p_episode_id
            and not s.retracts_episode
        union all
        -- A bound statement can move a bound of the Contribution it names
        -- anywhere, so the whole history is recomputed.
        select b.versioned_id, b.edge_id is not null, '-infinity'::timestamptz, 'infinity'::timestamptz
          from gnomon.bound_statement b
          where b.tenant_id = p_tenant_id and b.episode_id = p_episode_id
      ) x
      group by x.id, x.is_edge
      order by x.id
    loop
      perform gnomon.recompute_versions(p_tenant_id, t.id, t.is_edge, t.vf, t.vt);
    end loop;

    versions := v_done;
    return next;
  end
  $$;

-- Grants. Every grant a default privilege made on this migration's new
-- objects is revoked first, so each holds only the grants below.
do $$
declare
  g record;
begin
  for g in
    select format('function %s', p.oid::regprocedure) as object, a.grantee
    from pg_proc p, aclexplode(p.proacl) a
    where p.oid in (
      'gnomon.episode_retraction_check_insert()'::regprocedure,
      'gnomon.episode_retraction_check_complete()'::regprocedure,
      'gnomon.write_ending_as(uuid, uuid, boolean, uuid, text, uuid, timestamptz, text, boolean)'::regprocedure,
      'gnomon.retract_episode(uuid, uuid)'::regprocedure
    ) and a.grantee <> p.proowner
    union
    select format('table %s', c.oid::regclass), a.grantee
    from pg_class c, aclexplode(c.relacl) a
    where c.oid = 'gnomon.episode_retraction'::regclass and a.grantee <> c.relowner
  loop
    execute format(
      'revoke all on %s from %s', g.object,
      case when g.grantee = 0 then 'public' else g.grantee::regrole::text end);
  end loop;
  revoke all on function gnomon.episode_retraction_check_insert() from public;
  revoke all on function gnomon.episode_retraction_check_complete() from public;
  revoke all on function gnomon.write_ending_as(
    uuid, uuid, boolean, uuid, text, uuid, timestamptz, text, boolean) from public;
  revoke all on function gnomon.retract_episode(uuid, uuid) from public;
end
$$;

-- Callers read and insert the marker and update and delete nothing. They
-- name only the Episode: its Scope is the Episode's and its Recorded time is
-- the transaction's.
grant select on gnomon.episode_retraction to authenticated, gnomon_worker;
grant insert (tenant_id, episode_id) on gnomon.episode_retraction
  to authenticated, gnomon_worker;
grant insert (retracts_episode) on gnomon.statement to authenticated, gnomon_worker;

grant execute on function gnomon.write_ending_as(
  uuid, uuid, boolean, uuid, text, uuid, timestamptz, text, boolean)
  to authenticated, gnomon_worker;
grant execute on function gnomon.retract_episode(uuid, uuid) to authenticated, gnomon_worker;

insert into gnomon.schema_version (version, name) values (10, '0010_retract_episode');
