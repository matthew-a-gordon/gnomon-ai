-- gnomon migration 0007: ranked merge and Authority.
--
-- A Kind can turn ranked merge on. A Node of a Kind that ranks has its
-- Versions' values chosen by Authority, then Reference time, then Recorded
-- time (ADR-0007). Without ranked merge the order is Reference time, then
-- Recorded time, as before. Authority is an integer the consumer gives an
-- Episode, and an Episode with none ranks as 0.
--
-- Authority decides only between Contributions whose Valid times overlap.
-- The merge cuts a Node's Valid time into segments at every Contribution
-- bound and picks each property's winner in each segment, so a period that
-- only one Contribution covers keeps that Contribution whatever its
-- Authority, and Authority never reaches across a gap.
--
-- Authority ranks statements as it ranks values. A statement of the kinds
-- gnomon holds today is a Contribution whose value is JSON null, which is an
-- End of validity, or a bound statement, which is a Correction or a
-- Refinement. A null from a lower-Authority Episode loses to the values a
-- higher-Authority Episode supplied where their Valid times overlap, and a
-- bound statement moves a Contribution's bound only if its Episode's
-- Authority is at least that of the Contribution's Episode. A losing
-- statement is never deleted: it stays held, and takes effect if the
-- higher-Authority Episode is erased (Erasure, a later migration, deletes
-- that Episode's rows and recomputes).
--
-- The rule is a property of a Kind. A child Kind inherits its ancestors'
-- rule: it ranks if it or any ancestor does, so a child Kind may turn ranked
-- merge on and may not turn it off while an ancestor ranks. Each change of a
-- Kind's own rule is a new row of `gnomon.kind_merge`, the latest of which
-- holds, so no row is updated.
--
-- Each Version records the rule that chose its values in `merge_rule`, and a
-- change to a Kind's rule applies to later writes only: it changes no
-- existing Version. A write recomputes its Valid time widened to the whole of
-- every open Version it overlaps or touches, so a Version outside that window
-- keeps the rule it was written under, and one inside it is chosen again
-- under the Kind's rule at the time of the write. A Version that choice
-- leaves unchanged keeps its recorded rule. Erasure, a later migration, must
-- recompute each Version under its recorded rule.
--
-- Bound statements are read as stored. A statement from an Episode that
-- loses on Authority is recorded as a Refinement or a Correction as the
-- earlier bound decides, as before, even though the merge ignores it, so one
-- `state_bound` call can return `correction` while some of the rows it wrote
-- are held.
--
-- Callers get no new right on Versions. The one new table, `kind_merge`, is
-- written only by `set_merge_rule`, a SECURITY DEFINER function like Kind
-- registration (ADR-0009), which this migration grants to no caller role.

-- The rule that chose a Version's values. `ranked` orders by Authority, then
-- Reference time, then Recorded time. No statement of this migration uses the
-- new value, because a value added in a transaction cannot be used in it.
alter type gnomon.merge_rule add value 'ranked';

-- Each change of a Kind's own merge rule. The latest row of a Kind holds.
create table gnomon.kind_merge (
  tenant_id uuid not null,
  id bigint generated always as identity,
  kind text not null,
  merge_rule text not null check (merge_rule in ('recency', 'ranked')),
  set_at timestamptz not null default transaction_timestamp(),
  primary key (tenant_id, id),
  foreign key (tenant_id, kind) references gnomon.kind (tenant_id, name)
);
create index kind_merge_kind on gnomon.kind_merge (tenant_id, kind, id desc);

alter table gnomon.kind_merge enable row level security;
create policy kind_merge_tenant on gnomon.kind_merge
  for select to authenticated, gnomon_worker, gnomon_definer
  using (tenant_id = gnomon.tenant_id());
create policy kind_merge_tenant_set on gnomon.kind_merge
  for insert to gnomon_definer
  with check (tenant_id = gnomon.tenant_id());

-- The merge rule a Kind has: `ranked` if it or any ancestor ranks, else
-- `recency`. A root Kind, a Kind the Tenant has not registered and a Kind
-- nobody changed use `recency`.
create function gnomon.kind_merge_rule(p_tenant_id uuid, p_kind text)
  returns text
  language sql stable security invoker set search_path = ''
  as $$
    select case when exists (
      select from gnomon.kind k
      cross join lateral unnest(k.ancestors || k.name) a(name)
      where k.tenant_id = p_tenant_id and k.name = p_kind
        and (select m.merge_rule from gnomon.kind_merge m
              where m.tenant_id = k.tenant_id and m.kind = a.name
              order by m.id desc limit 1) = 'ranked'
    ) then 'ranked' else 'recency' end
  $$;

-- Sets a Kind's own merge rule, `recency` or `ranked`. It applies to later
-- writes only. A Kind below a Kind that ranks cannot turn ranked merge off,
-- and a root Kind has no rule of its own to set.
--
-- This is a definer function, owned by gnomon_definer, so that callers need
-- no write grant on `kind_merge`. It refuses any Tenant but the caller's, and
-- RLS checks the Tenant again on the row it writes. Setting the rule a Kind
-- already has changes nothing.
create function gnomon.set_merge_rule(p_tenant_id uuid, p_kind text, p_rule text)
  returns void
  language plpgsql volatile security definer set search_path = ''
  as $$
  declare
    v_ancestors text[];
    v_blocker text;
    v_own text;
  begin
    if p_tenant_id is null or p_tenant_id is distinct from gnomon.tenant_id() then
      raise exception using errcode = 'insufficient_privilege',
        message = 'setting a merge rule may set it only in the caller''s own Tenant';
    end if;
    if p_rule is null or p_rule not in ('recency', 'ranked') then
      raise exception using errcode = 'GNK01',
        message = 'a merge rule is recency or ranked';
    end if;
    if exists (select from gnomon.root_kinds() r where r.name = p_kind) then
      raise exception using errcode = 'GNK01',
        message = format('"%s" is a root Kind, which has no merge rule of its own; register a Kind', p_kind);
    end if;
    select k.ancestors into v_ancestors
      from gnomon.kind k where k.tenant_id = p_tenant_id and k.name = p_kind;
    if not found then
      raise exception using errcode = 'GNK01',
        message = format('Kind "%s" is not registered', p_kind);
    end if;
    if p_rule = 'recency' then
      select a.name into v_blocker
        from unnest(v_ancestors) a(name)
        where (select m.merge_rule from gnomon.kind_merge m
                where m.tenant_id = p_tenant_id and m.kind = a.name
                order by m.id desc limit 1) = 'ranked'
        limit 1;
      if v_blocker is not null then
        raise exception using errcode = 'GNK01',
          message = format(
            'Kind "%s" cannot turn ranked merge off: its ancestor "%s" ranks, and a child Kind '
            'may turn ranked merge on but not off', p_kind, v_blocker);
      end if;
    end if;
    select m.merge_rule into v_own
      from gnomon.kind_merge m
      where m.tenant_id = p_tenant_id and m.kind = p_kind
      order by m.id desc limit 1;
    if coalesce(v_own, 'recency') = p_rule then
      return;
    end if;
    insert into gnomon.kind_merge (tenant_id, kind, merge_rule) values (p_tenant_id, p_kind, p_rule);
  end
  $$;

-- The bound a Contribution has now. Under the recency rule it is the latest
-- statement about it, or the bound it was written with. Under ranked merge
-- the statements of Episodes that rank below the Contribution's Episode are
-- held and ignored, and of the rest the one of the highest Authority, then
-- Reference time, then Recorded time, holds.
create or replace function gnomon.contribution_bound(
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
    join gnomon.episode ce on ce.tenant_id = c.tenant_id and ce.id = c.episode_id
    cross join lateral (
      select gnomon.kind_merge_rule(n.tenant_id, n.kind) = 'ranked' as ranked
      from gnomon.node n where n.tenant_id = c.tenant_id and n.id = c.node_id
    ) r
    left join lateral (
      select x.bound_at, x.bound_precision from gnomon.bound_statement x
      join gnomon.episode se on se.tenant_id = x.tenant_id and se.id = x.episode_id
      where x.tenant_id = c.tenant_id and x.contribution_id = c.id and x.side = p_side
        and (not r.ranked or coalesce(se.authority, 0) >= coalesce(ce.authority, 0))
      order by case when r.ranked then coalesce(se.authority, 0) else 0 end desc,
               case when r.ranked then x.episode_reference_time end desc nulls last,
               case when r.ranked then x.recorded_at end desc nulls last,
               x.id desc
      limit 1
    ) s on true
    where c.tenant_id = p_tenant_id and c.id = p_contribution_id
  $$;

-- A new Version records the rule its Node's Kind has, as every Version the
-- merge opens does, so a caller who inserts directly cannot label a Version
-- with a rule that did not choose it. This is migration 0004's check with
-- that one addition.
create or replace function gnomon.version_check_insert() returns trigger
  language plpgsql security invoker set search_path = ''
  as $$
  declare
    v_kind text;
    v_required jsonb;
    missing text;
    k text;
    v jsonb;
  begin
    if new.recorded_from is distinct from transaction_timestamp()
      or new.recorded_to is distinct from 'infinity'
    then
      raise exception using errcode = 'insufficient_privilege',
        message = 'a new Version''s Recorded time starts when its transaction started, and is open';
    end if;
    if new.written_in is distinct from pg_current_xact_id() or new.closed_in is not null then
      raise exception using errcode = 'insufficient_privilege',
        message = 'a new Version is marked as written by the current transaction';
    end if;
    select n.kind into v_kind
      from gnomon.node n
      where n.tenant_id = new.tenant_id and n.scope = new.scope and n.id = new.node_id;
    if not found then
      return new; -- The foreign key or the policy refuses it.
    end if;
    if new.merge_rule::text is distinct from gnomon.kind_merge_rule(new.tenant_id, v_kind) then
      raise exception using errcode = 'GNW01',
        message = format(
          'a new Version records the merge rule of its Node''s Kind, %s',
          gnomon.kind_merge_rule(new.tenant_id, v_kind));
    end if;
    for k, v in select e.key, e.value from jsonb_each(new.props) e order by e.key loop
      if v = 'null'::jsonb then
        raise exception using errcode = 'GNW01',
          message = format('a Version holds no null; property "%s" is null', k);
      end if;
      perform gnomon.check_node_value(new.tenant_id, v_kind, k, v);
    end loop;
    select x.properties -> 'required' into v_required
      from gnomon.kind x where x.tenant_id = new.tenant_id and x.name = v_kind;
    select r into missing
      from jsonb_array_elements_text(coalesce(v_required, '[]')) r
      where not new.props ? r
      limit 1;
    if missing is not null then
      raise exception using errcode = 'GNW01',
        message = format(
          'the write leaves required property "%s" of Kind "%s" without a value over [%s, %s)',
          missing, v_kind, new.valid_from, new.valid_to);
    end if;
    return new;
  end
  $$;

-- Recomputes a Node's current Versions as migration 0006 does, choosing each
-- property's winner in a segment by the rule the Node's Kind has now: under
-- ranked merge by Authority, then Reference time, then Recorded time, and
-- otherwise by Reference time, then Recorded time. The Versions it opens
-- record that rule.
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
    v_kind text;
    v_rule text;
    v_ranked boolean;
  begin
    select l.node_scope, l.node_kind into v_scope, v_kind
      from gnomon.lock_node_for_write(p_tenant_id, p_node_id) l;
    -- The rule the Node's Kind has now. It chooses this write's values, and
    -- the Versions the write opens record it (ADR-0007).
    v_rule := gnomon.kind_merge_rule(p_tenant_id, v_kind);
    v_ranked := v_rule = 'ranked';

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
      -- written with, unless a bound statement moved them. A bound the window
      -- clips has no precision of its own. With ranked merge on, a statement
      -- moves a bound only if its Episode's Authority is at least that of
      -- the Contribution's Episode, and the statement of the highest
      -- Authority, then Reference time, then Recorded time, holds. A
      -- statement that loses stays held: Erasing the Episode of a winning
      -- statement makes the next one take effect. `auth` is 0 under the
      -- recency rule, so it ranks nothing there.
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
            and (not v_ranked or coalesce(se.authority, 0) >= coalesce(ce.authority, 0))
          order by case when v_ranked then coalesce(se.authority, 0) else 0 end desc,
                   case when v_ranked then s.episode_reference_time end desc nulls last,
                   case when v_ranked then s.recorded_at end desc nulls last,
                   s.id desc
          limit 1
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
      order by s.sf, c.property, c.auth desc, c.ref desc, c.rec desc, c.id desc
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
             v_rule::gnomon.merge_rule
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
      'gnomon.kind_merge_rule(uuid, text)'::regprocedure,
      'gnomon.set_merge_rule(uuid, text, text)'::regprocedure
    ) and a.grantee <> p.proowner
    union
    select format('table %s', c.oid::regclass), a.grantee
    from pg_class c, aclexplode(c.relacl) a
    where c.oid = 'gnomon.kind_merge'::regclass and a.grantee <> c.relowner
  loop
    execute format(
      'revoke all on %s from %s', g.object,
      case when g.grantee = 0 then 'public' else g.grantee::regrole::text end);
  end loop;
  revoke all on function gnomon.kind_merge_rule(uuid, text) from public;
  revoke all on function gnomon.set_merge_rule(uuid, text, text) from public;
end
$$;

-- Hand the rule setter to the definer role, as migration 0003 does for Kind
-- registration: the migration role may revoke its grants only while it owns
-- the function, and owning it needs SET ROLE on the new owner, which a
-- CREATEROLE migration role may grant itself on a role it created.
do $$
declare
  joined boolean := not pg_has_role(current_user, 'gnomon_definer', 'SET');
begin
  if joined then
    execute format('grant gnomon_definer to %I', current_user);
  end if;
  grant create on schema gnomon to gnomon_definer;
  alter function gnomon.set_merge_rule(uuid, text, text) owner to gnomon_definer;
  revoke create on schema gnomon from gnomon_definer;
  if joined then
    execute format('revoke gnomon_definer from %I', current_user);
  end if;
end
$$;

-- Callers read the rule changes and never write them. Only `set_merge_rule`
-- writes them, as gnomon_definer, and no caller role may execute it until a
-- deployment grants that to the roles it chooses.
grant select on gnomon.kind_merge to authenticated, gnomon_worker;
grant select, insert on gnomon.kind_merge to gnomon_definer;
grant execute on function gnomon.kind_merge_rule(uuid, text)
  to authenticated, gnomon_worker;

insert into gnomon.schema_version (version, name) values (7, '0007_ranked_merge');
