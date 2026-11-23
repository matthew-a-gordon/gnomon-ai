-- gnomon migration 0006: bound precision, Refinement and the `uncertain`
-- marker.
--
-- Each Valid-time bound carries a precision of `unknown`, `year`, `month`,
-- `day` or `instant`, and is stored as the start of that period in UTC
-- (ADR-0008). A write may name any instant inside the period, and the Store
-- truncates it, so the stored bound never depends on the session's time zone.
-- An unknown start is stored as -infinity and reads as open.
--
-- An As-of read marks a result `uncertain` when its valid-at instant falls
-- inside the period of one of the result's imprecise bounds. The stored end
-- of a half-open Valid time is the start of its period, so a result's valid-at
-- instant always lies before its end bound's period, and only the start bound
-- can mark a result.
--
-- A bound statement is an Episode's statement of one Valid-time bound of one
-- Contribution (ADR-0007). It states a bound that was unknown or coarse, or
-- one that contradicts the earlier bound. It moves no row: a Contribution's
-- effective bound is the latest statement made about it, and every
-- recompute reads the effective bounds, so a later write to the Node cannot
-- undo it, and Erasing the statement's Episode brings the earlier bound
-- back. The trigger on bound statements decides, from the earlier bound,
-- whether a statement is a Refinement or a Correction, so no caller can
-- label one as the other:
--
--   Refinement  the earlier bound was unknown, or the statement narrows its
--               precision inside the same period. The earlier Version was
--               incomplete, not wrong.
--   Correction  any other change. A bound outside the earlier period, or a
--               coarser one, contradicts the earlier Version.
--
-- Either way the earlier Version is closed in Recorded time and stays
-- readable As-of before the statement. Callers get no new UPDATE or DELETE:
-- bound statements are select and insert only, like Contributions.
--
-- Every Version also carries the hash of its text, the property values that
-- Search and embeddings are derived from. Versions with equal text share the
-- hash, so a Refinement, which states no new text, shares the earlier
-- Version's and needs no new embedding. The column is generated, so no
-- caller can set it.

-- The start of the period of `p_at` at a precision, in UTC.
create function gnomon.period_start(p_at timestamptz, p_precision gnomon.time_precision)
  returns timestamptz
  language sql immutable parallel safe security invoker set search_path = ''
  as $$
    select case p_precision
      when 'year' then date_trunc('year', p_at at time zone 'UTC') at time zone 'UTC'
      when 'month' then date_trunc('month', p_at at time zone 'UTC') at time zone 'UTC'
      when 'day' then date_trunc('day', p_at at time zone 'UTC') at time zone 'UTC'
      else p_at
    end
  $$;

-- The end of the period that starts at `p_start`: the start of the next
-- period. An instant has no extent, so its period ends where it starts.
create function gnomon.period_end(p_start timestamptz, p_precision gnomon.time_precision)
  returns timestamptz
  language sql immutable parallel safe security invoker set search_path = ''
  as $$
    select case p_precision
      when 'year' then (p_start at time zone 'UTC' + interval '1 year') at time zone 'UTC'
      when 'month' then (p_start at time zone 'UTC' + interval '1 month') at time zone 'UTC'
      when 'day' then (p_start at time zone 'UTC' + interval '1 day') at time zone 'UTC'
      else p_start
    end
  $$;

-- Whether stating a bound is a Refinement, a Correction, or no change.
create function gnomon.classify_bound(
  p_old_at timestamptz,
  p_old_precision gnomon.time_precision,
  p_new_at timestamptz,
  p_new_precision gnomon.time_precision
) returns text
  language sql immutable parallel safe security invoker set search_path = ''
  as $$
    select case
      when p_old_at = p_new_at and p_old_precision = p_new_precision then 'unchanged'
      when p_old_precision = 'unknown' then 'refinement'
      when p_new_precision > p_old_precision
        and p_new_at >= p_old_at and p_new_at < gnomon.period_end(p_old_at, p_old_precision)
        then 'refinement'
      else 'correction'
    end
  $$;

-- The hash of a Version's text.
alter table gnomon.version add column text_hash text
  generated always as (encode(sha256(jsonb_send(props)), 'hex')) stored;

-- A Contribution is named by its Scope too, so a statement about it can
-- name the Scope with the Tenant and the ID.
alter table gnomon.contribution add constraint contribution_scoped unique (tenant_id, scope, id);

-- A bound statement: an Episode's statement of one Valid-time bound of one
-- Contribution.
create table gnomon.bound_statement (
  tenant_id uuid not null,
  scope text not null,
  -- The order statements were recorded in: the later statement about a bound
  -- is the one that holds.
  id bigint generated always as identity,
  node_id uuid not null,
  episode_id uuid not null,
  contribution_id bigint not null,
  -- The bound the statement is about: `from` or `to`.
  side text not null check (side in ('from', 'to')),
  -- Decided by the insert trigger, never by the caller.
  kind text not null check (kind in ('refinement', 'correction')),
  bound_at timestamptz not null check (bound_at not in ('-infinity', 'infinity')),
  bound_precision gnomon.time_precision not null check (bound_precision <> 'unknown'),
  recorded_at timestamptz not null default transaction_timestamp(),
  episode_reference_time timestamptz not null,
  episode_recorded_at timestamptz not null,
  primary key (tenant_id, id),
  foreign key (tenant_id, scope, node_id) references gnomon.node (tenant_id, scope, id),
  foreign key (tenant_id, scope, contribution_id)
    references gnomon.contribution (tenant_id, scope, id),
  foreign key (tenant_id, scope, episode_id, episode_reference_time, episode_recorded_at)
    references gnomon.episode (tenant_id, scope, id, reference_time, recorded_at)
);
create index bound_statement_contribution
  on gnomon.bound_statement (tenant_id, contribution_id, side, id);
create index bound_statement_episode on gnomon.bound_statement (tenant_id, episode_id);

alter table gnomon.bound_statement enable row level security;
create policy bound_statement_tenant_scope on gnomon.bound_statement
  for all to authenticated, gnomon_worker
  using (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()))
  with check (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()));

-- The bound a Contribution has now: the latest statement about it, or the
-- bound it was written with.
create function gnomon.contribution_bound(
  p_tenant_id uuid,
  p_contribution_id bigint,
  p_side text,
  out bound_at timestamptz,
  out bound_precision gnomon.time_precision
)
  language sql stable security invoker set search_path = ''
  as $$
    select coalesce(s.bound_at, case p_side when 'from' then c.valid_from else c.valid_to end),
           coalesce(s.bound_precision,
                    case p_side when 'from' then c.valid_from_precision else c.valid_to_precision end)
    from gnomon.contribution c
    left join lateral (
      select x.bound_at, x.bound_precision from gnomon.bound_statement x
      where x.tenant_id = c.tenant_id and x.contribution_id = c.id and x.side = p_side
      order by x.id desc limit 1
    ) s on true
    where c.tenant_id = p_tenant_id and c.id = p_contribution_id
  $$;

-- A bound statement is recorded when its transaction started, states the
-- start of its period, and is a Refinement or a Correction as the earlier
-- bound decides. It may not empty the Contribution's Valid time.
create function gnomon.bound_statement_check_insert() returns trigger
  language plpgsql security invoker set search_path = ''
  as $$
  declare
    earlier record;
    other record;
    empty boolean;
  begin
    if new.recorded_at is distinct from transaction_timestamp() then
      raise exception using errcode = 'insufficient_privilege',
        message = 'a bound statement''s Recorded time is the start of its write''s transaction';
    end if;
    if new.bound_at in ('-infinity', 'infinity') or new.bound_precision = 'unknown' then
      return new; -- The check constraints refuse it.
    end if;
    select * into earlier from gnomon.contribution_bound(
      new.tenant_id, new.contribution_id, new.side);
    if earlier.bound_at is null then
      return new; -- A Contribution the caller cannot see fails the foreign key.
    end if;
    new.bound_at := gnomon.period_start(new.bound_at, new.bound_precision);
    new.kind := gnomon.classify_bound(
      earlier.bound_at, earlier.bound_precision, new.bound_at, new.bound_precision);
    if new.kind = 'unchanged' then
      raise exception using errcode = 'GNW01',
        message = 'the statement gives the bound the Contribution already has';
    end if;
    select * into other from gnomon.contribution_bound(
      new.tenant_id, new.contribution_id, case new.side when 'from' then 'to' else 'from' end);
    empty := case new.side
      when 'from' then new.bound_at >= other.bound_at
      else other.bound_at >= new.bound_at
    end;
    if empty then
      raise exception using errcode = 'GNW01',
        message = format('the Valid time [%s, %s) is empty',
          case new.side when 'from' then new.bound_at else other.bound_at end,
          case new.side when 'from' then other.bound_at else new.bound_at end);
    end if;
    return new;
  end
  $$;
create trigger bound_statement_check_insert before insert on gnomon.bound_statement
  for each row execute function gnomon.bound_statement_check_insert();

-- Recomputes a Node's current Versions as migration 0004 does, over each
-- Contribution's effective Valid time, and gives each Version bound the
-- precision its Contributions gave it. A Version whose only change is the
-- precision of a bound is a new Version.
create or replace function gnomon.recompute_node(
  p_tenant_id uuid,
  p_node_id uuid,
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
    v_scope text;
    w0 timestamptz;
    w1 timestamptz;
    candidates jsonb;
    stale uuid[];
  begin
    select l.node_scope into v_scope from gnomon.lock_node_for_write(p_tenant_id, p_node_id) l;

    -- A transaction that started before another wrote this Node, but took
    -- the lock after that one committed, would record a Version earlier
    -- than one already recorded. It fails with a serialization failure, and
    -- the caller retries the transaction. Each statement of a READ
    -- COMMITTED transaction, as the Store runs, sees what committed before
    -- the lock was granted.
    if exists (
      select from gnomon.contribution c
      where c.tenant_id = p_tenant_id and c.scope = v_scope and c.node_id = p_node_id
        and c.recorded_at > t
    ) or exists (
      select from gnomon.version x
      where x.tenant_id = p_tenant_id and x.scope = v_scope and x.node_id = p_node_id
        and x.recorded_from = t and x.written_in <> me
    ) then
      raise exception using errcode = 'serialization_failure',
        message = format('a later transaction wrote Node %s first; retry the write', p_node_id);
    end if;

    -- 1. The window.
    select least(p_from, min(v.valid_from)), greatest(p_to, max(v.valid_to)) into w0, w1
      from gnomon.version v
      where v.tenant_id = p_tenant_id and v.scope = v_scope and v.node_id = p_node_id
        and v.recorded_to = 'infinity'
        and v.valid_from <= p_to and v.valid_to >= p_from;

    -- 2 and 3. The new Versions over the window, each with its Citations.
    with contributions as (
      -- Each Contribution over its effective Valid time: the bounds it was
      -- written with, unless a bound statement of a later Episode moved
      -- them. A bound the window clips has no precision of its own.
      select c.id, c.episode_id, c.property, c.value, c.place,
             greatest(e.vf, w0) as vf, least(e.vt, w1) as vt,
             case when e.vf >= w0 then e.vfp end as vfp,
             case when e.vt <= w1 then e.vtp end as vtp,
             c.episode_reference_time as ref, c.episode_recorded_at as episode_rec,
             c.recorded_at as rec
      from gnomon.contribution c
      cross join lateral (
        select coalesce(f.bound_at, c.valid_from) as vf,
               coalesce(f.bound_precision, c.valid_from_precision) as vfp,
               coalesce(t.bound_at, c.valid_to) as vt,
               coalesce(t.bound_precision, c.valid_to_precision) as vtp
        from (select 1) one
        left join lateral (
          select s.bound_at, s.bound_precision from gnomon.bound_statement s
          where s.tenant_id = c.tenant_id and s.contribution_id = c.id and s.side = 'from'
          order by s.id desc limit 1
        ) f on true
        left join lateral (
          select s.bound_at, s.bound_precision from gnomon.bound_statement s
          where s.tenant_id = c.tenant_id and s.contribution_id = c.id and s.side = 'to'
          order by s.id desc limit 1
        ) t on true
      ) e
      where c.tenant_id = p_tenant_id and c.scope = v_scope and c.node_id = p_node_id
        and e.vf < w1 and e.vt > w0
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
      order by s.sf, c.property, c.ref desc, c.rec desc, c.id desc
    ), supporters as (
      -- Every Contribution in a segment that states its winning value, and
      -- whether this transaction recorded it.
      select w.sf, w.property, c.episode_id, c.place, c.ref, c.episode_rec,
             bool_or(c.rec = t) as fresh
      from winners w
      join contributions c
        on c.property = w.property and c.value = w.value and c.vf <= w.sf and c.vt >= w.st
      where w.value <> 'null'::jsonb
      group by w.sf, w.property, c.episode_id, c.place, c.ref, c.episode_rec
    ), segment_values as (
      select sf, max(st) as st,
             jsonb_object_agg(property, value) filter (where value <> 'null'::jsonb) as props
      from winners group by sf
    ), islands as (
      -- Runs of touching segments with equal values share an island number.
      select sf, st, props,
             sum(case when prev_st = sf and prev_props = props then 0 else 1 end)
               over (order by sf) as island
      from (
        select sf, st, props,
               lag(st) over (order by sf) as prev_st, lag(props) over (order by sf) as prev_props
        from segment_values where props is not null
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
                 where x.tenant_id = p_tenant_id and x.scope = v_scope and x.node_id = p_node_id
                   and x.recorded_to = 'infinity' and x.valid_from = m.vf limit 1),
               gnomon.instant_or_unknown(m.vf)) as vfp,
             coalesce(
               (select max(c.vtp) from contributions c where c.vt = m.vt),
               (select x.valid_to_precision from gnomon.version x
                 where x.tenant_id = p_tenant_id and x.scope = v_scope and x.node_id = p_node_id
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
      where v.tenant_id = p_tenant_id and v.scope = v_scope and v.node_id = p_node_id
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
        where v.tenant_id = p_tenant_id and v.scope = v_scope and v.node_id = p_node_id
          and v.closed_in = me and v.recorded_to = t
          and v.valid_from = cand.vf and v.valid_to = cand.vt and v.props = cand.props
          and v.valid_from_precision::text = cand.vfp and v.valid_to_precision::text = cand.vtp
        returning v.id
    ), inserted as (
      insert into gnomon.version (
        tenant_id, scope, node_id, props, valid_from, valid_from_precision,
        valid_to, valid_to_precision, merge_rule
      )
      select p_tenant_id, v_scope, p_node_id, cand.props,
             cand.vf, cand.vfp::gnomon.time_precision,
             cand.vt, cand.vtp::gnomon.time_precision,
             'recency'
      from jsonb_to_recordset(candidates)
        cand(vf timestamptz, vt timestamptz, vfp text, vtp text, props jsonb)
      where not exists (
        select from gnomon.version v
        where v.tenant_id = p_tenant_id and v.scope = v_scope and v.node_id = p_node_id
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
        on v.tenant_id = p_tenant_id and v.scope = v_scope and v.node_id = p_node_id and v.recorded_to = 'infinity'
       and v.valid_from = cand.vf and v.valid_to = cand.vt
      cross join lateral jsonb_array_elements(cand.cites) cc
      where v.id = any(opened) or (cc ->> 3)::boolean
      on conflict do nothing;
  end
  $$;

-- Writes a patch of property values for a Node, citing one Episode, over one
-- Valid time. This replaces the write of migration 0004 with one that takes
-- a precision for each bound.
--
--   p_valid_from, p_valid_to
--                 a bound is an instant inside its period, or null for an
--                 unknown bound
--   p_valid_from_precision, p_valid_to_precision
--                 `year`, `month`, `day` or `instant`. Null means `instant`
--                 for a stated bound and `unknown` for a null one.
--
-- The stored bound is the start of the period in UTC.
drop function gnomon.write_node(uuid, uuid, uuid, timestamptz, timestamptz, jsonb, jsonb);
create function gnomon.write_node(
  p_tenant_id uuid,
  p_node_id uuid,
  p_episode_id uuid,
  p_valid_from timestamptz,
  p_valid_from_precision text,
  p_valid_to timestamptz,
  p_valid_to_precision text,
  p_values jsonb,
  p_places jsonb
) returns table (status text, recorded_at timestamptz, versions uuid[])
  language plpgsql volatile security invoker set search_path = ''
  as $$
  declare
    vfp gnomon.time_precision;
    vtp gnomon.time_precision;
    vf timestamptz;
    vt timestamptz;
    v_scope text;
    v_reference_time timestamptz;
    v_recorded_at timestamptz;
    step record;
  begin
    if p_episode_id is null then
      raise exception using errcode = 'GNW01',
        message = 'every write cites an Episode';
    end if;
    if p_valid_from in ('-infinity', 'infinity') or p_valid_to in ('-infinity', 'infinity') then
      raise exception using errcode = 'GNW01',
        message = 'a stated Valid-time bound is an instant; leave an unknown bound null';
    end if;
    if (p_valid_from is null and p_valid_from_precision is distinct from 'unknown'
          and p_valid_from_precision is not null)
      or (p_valid_to is null and p_valid_to_precision is distinct from 'unknown'
          and p_valid_to_precision is not null)
      or p_valid_from is not null and p_valid_from_precision = 'unknown'
      or p_valid_to is not null and p_valid_to_precision = 'unknown'
    then
      raise exception using errcode = 'GNW01',
        message = 'a stated bound has a precision of year, month, day or instant, and an unknown bound has none';
    end if;
    if p_valid_from_precision is not null
      and p_valid_from_precision not in ('unknown', 'year', 'month', 'day', 'instant')
      or p_valid_to_precision is not null
      and p_valid_to_precision not in ('unknown', 'year', 'month', 'day', 'instant')
    then
      raise exception using errcode = 'GNW01',
        message = 'a precision is year, month, day or instant';
    end if;
    vfp := coalesce(p_valid_from_precision, case when p_valid_from is null then 'unknown' else 'instant' end)
      ::gnomon.time_precision;
    vtp := coalesce(p_valid_to_precision, case when p_valid_to is null then 'unknown' else 'instant' end)
      ::gnomon.time_precision;
    vf := coalesce(gnomon.period_start(p_valid_from, vfp), '-infinity');
    vt := coalesce(gnomon.period_start(p_valid_to, vtp), 'infinity');
    if vf >= vt then
      raise exception using errcode = 'GNW01',
        message = format('the Valid time [%s, %s) is empty', vf, vt);
    end if;
    if jsonb_typeof(p_values) is distinct from 'object' or p_values = '{}' then
      raise exception using errcode = 'GNW01',
        message = 'a write states a value for at least one property';
    end if;
    if p_places is not null and (
      jsonb_typeof(p_places) <> 'object'
      or exists (
        select from jsonb_each(p_places) p
        where jsonb_typeof(p.value) <> 'string' or not p_values ? p.key
      )
    ) then
      raise exception using errcode = 'GNW01',
        message = 'places are {property: place}, a string for a property the write states';
    end if;

    select l.node_scope into v_scope from gnomon.lock_node_for_write(p_tenant_id, p_node_id) l;
    select e.reference_time, e.recorded_at into v_reference_time, v_recorded_at
      from gnomon.episode e
      where e.tenant_id = p_tenant_id and e.scope = v_scope and e.id = p_episode_id;
    if not found then
      raise exception using errcode = 'GNW01',
        message = format('Episode %s is not visible in the Node''s Scope', p_episode_id);
    end if;

    -- In property order, so the first refused value is the same on every
    -- host.
    insert into gnomon.contribution (
      tenant_id, scope, node_id, episode_id, property, value, place,
      valid_from, valid_from_precision, valid_to, valid_to_precision,
      episode_reference_time, episode_recorded_at
    )
    select p_tenant_id, v_scope, p_node_id, p_episode_id, e.key, e.value, p_places ->> e.key,
           vf, vfp, vt, vtp, v_reference_time, v_recorded_at
    from jsonb_each(p_values) e
    order by e.key;

    select * into step from gnomon.recompute_node(p_tenant_id, p_node_id, vf, vt);
    status := case when step.changed then 'written' else 'unchanged' end;
    recorded_at := transaction_timestamp();
    versions := step.opened;
    return next;
  end
  $$;

-- States a bound of a current Version's Valid time, citing one Episode.
--
--   p_side        `from` or `to`
--   p_bound_at    an instant inside the stated period
--   p_precision   `year`, `month`, `day` or `instant`
--
-- The statement is about every Contribution that gave the Version that
-- bound. Returns `refinement` when the bound was unknown, or the statement
-- narrows its precision inside the same period, `correction` when it falls
-- outside the earlier period or is coarser, and `unchanged` when the Version
-- already has it, with the statement's Recorded time and the IDs of the
-- Versions it opened.
create function gnomon.state_bound(
  p_tenant_id uuid,
  p_node_id uuid,
  p_episode_id uuid,
  p_version_id uuid,
  p_side text,
  p_bound_at timestamptz,
  p_precision text
) returns table (change text, recorded_at timestamptz, versions uuid[])
  language plpgsql volatile security invoker set search_path = ''
  as $$
  declare
    t constant timestamptz := transaction_timestamp();
    v_precision gnomon.time_precision;
    v_scope text;
    v_reference_time timestamptz;
    v_recorded_at timestamptz;
    v gnomon.version;
    v_at timestamptz;
    v_earlier_at timestamptz;
    v_earlier_precision gnomon.time_precision;
    v_contributions bigint[];
    step record;
  begin
    if p_episode_id is null then
      raise exception using errcode = 'GNW01',
        message = 'every statement cites an Episode';
    end if;
    if p_side is null or p_side not in ('from', 'to') then
      raise exception using errcode = 'GNW01', message = 'a bound is the from or the to bound';
    end if;
    if p_bound_at is null or p_bound_at in ('-infinity', 'infinity')
      or p_precision is null or p_precision not in ('year', 'month', 'day', 'instant')
    then
      raise exception using errcode = 'GNW01',
        message = 'a statement gives an instant and a precision of year, month, day or instant';
    end if;
    v_precision := p_precision::gnomon.time_precision;
    v_at := gnomon.period_start(p_bound_at, v_precision);

    select l.node_scope into v_scope from gnomon.lock_node_for_write(p_tenant_id, p_node_id) l;
    select e.reference_time, e.recorded_at into v_reference_time, v_recorded_at
      from gnomon.episode e
      where e.tenant_id = p_tenant_id and e.scope = v_scope and e.id = p_episode_id;
    if not found then
      raise exception using errcode = 'GNW01',
        message = format('Episode %s is not visible in the Node''s Scope', p_episode_id);
    end if;
    select * into v from gnomon.version x
      where x.tenant_id = p_tenant_id and x.scope = v_scope and x.node_id = p_node_id
        and x.id = p_version_id and x.recorded_to = 'infinity';
    if not found then
      raise exception using errcode = 'GNW01',
        message = format('Version %s is not a current Version of Node %s', p_version_id, p_node_id);
    end if;

    v_earlier_at := case p_side when 'from' then v.valid_from else v.valid_to end;
    v_earlier_precision := case p_side
      when 'from' then v.valid_from_precision else v.valid_to_precision end;
    change := gnomon.classify_bound(v_earlier_at, v_earlier_precision, v_at, v_precision);
    recorded_at := t;
    versions := '{}';
    if change = 'unchanged' then
      return next;
      return;
    end if;
    if (p_side = 'from' and v_at >= v.valid_to) or (p_side = 'to' and v.valid_from >= v_at) then
      raise exception using errcode = 'GNW01',
        message = format('the Valid time [%s, %s) is empty',
          case p_side when 'from' then v_at else v.valid_from end,
          case p_side when 'from' then v.valid_to else v_at end);
    end if;

    -- The Contributions that gave the Version this bound.
    select coalesce(array_agg(c.id order by c.id), '{}') into v_contributions
      from gnomon.contribution c
      cross join lateral gnomon.contribution_bound(c.tenant_id, c.id, p_side) b
      cross join lateral gnomon.contribution_bound(
        c.tenant_id, c.id, case p_side when 'from' then 'to' else 'from' end) o
      where c.tenant_id = p_tenant_id and c.scope = v_scope and c.node_id = p_node_id
        and b.bound_at = v_earlier_at
        and case p_side
              when 'from' then o.bound_at > v.valid_from
              else o.bound_at < v.valid_to
            end
        and not (b.bound_at = v_at and b.bound_precision = v_precision);
    if cardinality(v_contributions) = 0 then
      raise exception using errcode = 'GNW01',
        message = format('no Contribution gave Version %s its %s bound', p_version_id, p_side);
    end if;

    insert into gnomon.bound_statement (
      tenant_id, scope, node_id, episode_id, contribution_id, side, bound_at, bound_precision,
      episode_reference_time, episode_recorded_at
    )
    select p_tenant_id, v_scope, p_node_id, p_episode_id, c, p_side, v_at, v_precision,
           v_reference_time, v_recorded_at
    from unnest(v_contributions) c;

    select * into step from gnomon.recompute_node(
      p_tenant_id, p_node_id, least(v.valid_from, v_at), greatest(v.valid_to, v_at));
    versions := step.opened;
    return next;
  end
  $$;

-- Reads a Node As-of a valid-at and a known-at instant, as in migration 0004,
-- and marks the result `uncertain` when the valid-at instant falls inside the
-- period of an imprecise start bound. It also returns the hash of the
-- Version's text.
drop function gnomon.read_node(uuid, uuid, timestamptz, timestamptz);
create function gnomon.read_node(
  p_tenant_id uuid,
  p_node_id uuid,
  p_valid_at timestamptz,
  p_known_at timestamptz
) returns table (
    id uuid,
    node_id uuid,
    scope text,
    kind text,
    props jsonb,
    valid_from timestamptz,
    valid_from_precision gnomon.time_precision,
    valid_to timestamptz,
    valid_to_precision gnomon.time_precision,
    recorded_from timestamptz,
    recorded_to timestamptz,
    merge_rule gnomon.merge_rule,
    uncertain boolean,
    text_hash text
  )
  language sql stable security invoker set search_path = ''
  as $$
    select v.id, v.node_id, v.scope, n.kind, v.props,
           v.valid_from, v.valid_from_precision, v.valid_to, v.valid_to_precision,
           v.recorded_from, v.recorded_to, v.merge_rule,
           v.valid_from_precision in ('year', 'month', 'day')
             and p_valid_at < gnomon.period_end(v.valid_from, v.valid_from_precision),
           v.text_hash
    from gnomon.version v
    join gnomon.node n on n.tenant_id = v.tenant_id and n.id = v.node_id
    where v.tenant_id = p_tenant_id and v.node_id = p_node_id
      and v.valid_from <= p_valid_at and v.valid_to > p_valid_at
      and case
            when p_known_at is null then v.recorded_to = 'infinity'
            else v.recorded_from <= p_known_at and v.recorded_to > p_known_at
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
      'gnomon.period_start(timestamptz, gnomon.time_precision)'::regprocedure,
      'gnomon.period_end(timestamptz, gnomon.time_precision)'::regprocedure,
      'gnomon.classify_bound(timestamptz, gnomon.time_precision, timestamptz, gnomon.time_precision)'::regprocedure,
      'gnomon.contribution_bound(uuid, bigint, text)'::regprocedure,
      'gnomon.bound_statement_check_insert()'::regprocedure,
      'gnomon.write_node(uuid, uuid, uuid, timestamptz, text, timestamptz, text, jsonb, jsonb)'::regprocedure,
      'gnomon.state_bound(uuid, uuid, uuid, uuid, text, timestamptz, text)'::regprocedure,
      'gnomon.read_node(uuid, uuid, timestamptz, timestamptz)'::regprocedure
    ) and a.grantee <> p.proowner
    union
    select format('table %s', c.oid::regclass), a.grantee
    from pg_class c, aclexplode(c.relacl) a
    where c.oid = 'gnomon.bound_statement'::regclass and a.grantee <> c.relowner
  loop
    execute format(
      'revoke all on %s from %s', g.object,
      case when g.grantee = 0 then 'public' else g.grantee::regrole::text end);
  end loop;
  revoke all on function gnomon.period_start(timestamptz, gnomon.time_precision) from public;
  revoke all on function gnomon.period_end(timestamptz, gnomon.time_precision) from public;
  revoke all on function gnomon.classify_bound(
    timestamptz, gnomon.time_precision, timestamptz, gnomon.time_precision) from public;
  revoke all on function gnomon.contribution_bound(uuid, bigint, text) from public;
  revoke all on function gnomon.bound_statement_check_insert() from public;
  revoke all on function gnomon.write_node(
    uuid, uuid, uuid, timestamptz, text, timestamptz, text, jsonb, jsonb) from public;
  revoke all on function gnomon.state_bound(uuid, uuid, uuid, uuid, text, timestamptz, text)
    from public;
  revoke all on function gnomon.read_node(uuid, uuid, timestamptz, timestamptz) from public;
end
$$;

-- Callers read and insert bound statements as they do Contributions, and
-- update and delete nothing. They never insert the kind, which the trigger
-- decides, or a Recorded time.
grant select on gnomon.bound_statement to authenticated, gnomon_worker;
grant insert (
  tenant_id, scope, node_id, episode_id, contribution_id, side, bound_at, bound_precision,
  episode_reference_time, episode_recorded_at
) on gnomon.bound_statement to authenticated, gnomon_worker;

-- The write functions run as the invoker, so the callers need every function
-- they call.
grant execute on function gnomon.period_start(timestamptz, gnomon.time_precision)
  to authenticated, gnomon_worker;
grant execute on function gnomon.period_end(timestamptz, gnomon.time_precision)
  to authenticated, gnomon_worker;
grant execute on function gnomon.classify_bound(
  timestamptz, gnomon.time_precision, timestamptz, gnomon.time_precision)
  to authenticated, gnomon_worker;
grant execute on function gnomon.contribution_bound(uuid, bigint, text)
  to authenticated, gnomon_worker;
grant execute on function gnomon.write_node(
  uuid, uuid, uuid, timestamptz, text, timestamptz, text, jsonb, jsonb)
  to authenticated, gnomon_worker;
grant execute on function gnomon.state_bound(uuid, uuid, uuid, uuid, text, timestamptz, text)
  to authenticated, gnomon_worker;
grant execute on function gnomon.read_node(uuid, uuid, timestamptz, timestamptz)
  to authenticated, gnomon_worker;

insert into gnomon.schema_version (version, name) values (6, '0006_bound_precision');
