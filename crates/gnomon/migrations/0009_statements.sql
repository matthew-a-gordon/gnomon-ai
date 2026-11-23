-- gnomon migration 0009: Correction, End of validity and Retraction.
--
-- A Correction, an End of validity and a Retraction are statements of the
-- Episode that makes them, held like Contributions (ADR-0007). Each has a row
-- in `gnomon.statement` that records its kind, its Episode, its Valid time
-- and, for an End of validity or a Retraction, the Version it ended or
-- retracted. The merge reads no statement row: a statement takes effect
-- through Contributions that name it in `contribution.statement_id`, so
-- `gnomon.recompute_versions`, ranked merge, Citations and the guard triggers
-- cover statements with no second copy of their rules, and Erasure
-- (ticket #13) recomputes through the same function.
--
--   Correction        Contributions of the values it states, over the Valid
--                     time it states them for. The values replace the earlier
--                     Version's where the merge lets them win: the earlier
--                     Version is closed in Recorded time and a new one opens,
--                     so the earlier Version stays readable As-of before the
--                     Correction.
--   End of validity   Contributions of JSON null, from the stated instant to
--                     the end of the Version that held across it, for each
--                     property of that Version. For an Edge it is one
--                     Contribution of the built-in `holds`, so the Edge holds
--                     no Version from that instant whatever property values
--                     remain.
--   Retraction        Contributions of JSON null over the whole Valid time of
--                     a current Version, for each property of that Version
--                     and, for an Edge, `holds`. The Version is closed in
--                     Recorded time and stays readable As-of before the
--                     Retraction.
--
-- Because a statement is a set of Contributions, ranked merge applies to it
-- as it does to values: a statement from a lower-Authority Episode loses to
-- the values a higher-Authority Episode supplied where their Valid times
-- overlap, stays held, and takes effect when that Episode is erased
-- (ADR-0007). Without ranked merge a statement wins only when its Episode's
-- Reference time is not before the values it ends, as any Contribution does.
--
-- `bound_statement` (migration 0006) is the other statement table. It names
-- one Contribution and one bound, and the merge reads it. `statement` names a
-- Node or an Edge, and the merge reads the Contributions that carry its
-- `statement_id`. Retract Episode (ticket #12) and Erasure (ticket #13) must
-- delete, for an Episode: its `contribution` rows, which include those of its
-- statements, then its `statement` rows, then its `bound_statement` rows. A
-- statement's Contributions are deleted with it, by the foreign key.
--
-- Callers get no new UPDATE or DELETE right, and no new SECURITY DEFINER
-- function: the three functions below run as the invoker (ADR-0009).

-- A statement of an Episode about a Node or an Edge.
create table gnomon.statement (
  tenant_id uuid not null,
  scope text not null,
  id bigint generated always as identity,
  node_id uuid,
  edge_id uuid,
  versioned_id uuid generated always as (coalesce(node_id, edge_id)) stored,
  episode_id uuid not null,
  kind text not null check (kind in ('correction', 'end_of_validity', 'retraction')),
  -- The Version an End of validity ended or a Retraction retracted, as it
  -- stood. No foreign key: a Version its own transaction replaces is deleted.
  version_id uuid,
  valid_from timestamptz not null,
  valid_from_precision gnomon.time_precision not null,
  valid_to timestamptz not null,
  valid_to_precision gnomon.time_precision not null,
  recorded_at timestamptz not null default transaction_timestamp(),
  episode_reference_time timestamptz not null,
  episode_recorded_at timestamptz not null,
  primary key (tenant_id, id),
  unique (tenant_id, scope, id),
  foreign key (tenant_id, scope, node_id) references gnomon.node (tenant_id, scope, id),
  foreign key (tenant_id, scope, edge_id) references gnomon.edge (tenant_id, scope, id),
  foreign key (tenant_id, scope, episode_id, episode_reference_time, episode_recorded_at)
    references gnomon.episode (tenant_id, scope, id, reference_time, recorded_at),
  check (num_nonnulls(node_id, edge_id) = 1),
  check ((kind = 'correction') = (version_id is null)),
  check (valid_from < valid_to),
  check ((valid_from = '-infinity') = (valid_from_precision = 'unknown')),
  check ((valid_to = 'infinity') = (valid_to_precision = 'unknown'))
);
create index statement_versioned on gnomon.statement (tenant_id, scope, versioned_id, id);
create index statement_episode on gnomon.statement (tenant_id, episode_id);

alter table gnomon.statement enable row level security;
create policy statement_tenant_scope on gnomon.statement
  for all to authenticated, gnomon_worker
  using (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()))
  with check (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()));

-- The statement a Contribution belongs to, if it is part of one. Erasing a
-- statement erases its Contributions.
alter table gnomon.contribution add column statement_id bigint;
alter table gnomon.contribution
  add constraint contribution_statement foreign key (tenant_id, scope, statement_id)
    references gnomon.statement (tenant_id, scope, id) on delete cascade;
create index contribution_statement on gnomon.contribution (tenant_id, statement_id)
  where statement_id is not null;

-- A statement is recorded when its transaction started, and serialises with
-- writes and Erasure on its Node or Edge (ADR-0007).
create function gnomon.statement_check_insert() returns trigger
  language plpgsql security invoker set search_path = ''
  as $$
  begin
    if new.recorded_at is distinct from transaction_timestamp() then
      raise exception using errcode = 'insufficient_privilege',
        message = 'a statement''s Recorded time is the start of its write''s transaction';
    end if;
    if num_nonnulls(new.node_id, new.edge_id) = 1 then
      perform pg_catalog.pg_advisory_xact_lock(
        gnomon.node_lock_key(new.tenant_id, coalesce(new.node_id, new.edge_id)));
    end if;
    return new;
  end
  $$;
create trigger statement_check_insert before insert on gnomon.statement
  for each row execute function gnomon.statement_check_insert();

-- A Contribution is checked as in migration 0008. One that names a statement
-- must be the statement's own: the same Node or Edge, Episode, Valid time and
-- Recorded time, and a JSON null unless the statement is a Correction.
create or replace function gnomon.contribution_check_insert() returns trigger
  language plpgsql security invoker set search_path = ''
  as $$
  declare
    v_kind text;
    s gnomon.statement;
  begin
    if new.recorded_at is distinct from transaction_timestamp() then
      raise exception using errcode = 'insufficient_privilege',
        message = 'a Contribution''s Recorded time is the start of its write''s transaction';
    end if;
    if new.node_id is not null then
      select n.kind into v_kind
        from gnomon.node n
        where n.tenant_id = new.tenant_id and n.scope = new.scope and n.id = new.node_id;
      if found then
        perform gnomon.check_node_value(new.tenant_id, v_kind, new.property, new.value);
      end if;
    elsif new.edge_id is not null then
      select e.kind into v_kind
        from gnomon.edge e
        where e.tenant_id = new.tenant_id and e.scope = new.scope and e.id = new.edge_id;
      if found then
        perform gnomon.check_edge_value(new.tenant_id, v_kind, new.property, new.value);
      end if;
    end if;
    if new.statement_id is not null then
      select * into s from gnomon.statement x
        where x.tenant_id = new.tenant_id and x.scope = new.scope and x.id = new.statement_id;
      if found and (
        (s.node_id, s.edge_id, s.episode_id) is distinct from
          (new.node_id, new.edge_id, new.episode_id)
        or (s.valid_from, s.valid_from_precision, s.valid_to, s.valid_to_precision)
          is distinct from
          (new.valid_from, new.valid_from_precision, new.valid_to, new.valid_to_precision)
        or s.recorded_at <> new.recorded_at
        or (s.kind <> 'correction' and new.value <> 'null'::jsonb)
      ) then
        raise exception using errcode = 'GNW01',
          message = 'a Contribution of a statement is of the statement''s Node or Edge, '
            'Episode and Valid time, and an End of validity or a Retraction states only null';
      end if;
    end if;
    -- A Node, Edge, statement or Episode the caller cannot see fails a
    -- foreign key or the policy, and a row naming both or neither Node and
    -- Edge fails the check constraint.
    return new;
  end
  $$;

-- Writes a patch of property values for a Node or an Edge, as migration
-- 0008's `write_patch` does, and records it as a Correction when `p_kind` is
-- 'correction': a statement row, and the Contributions name it. A Correction
-- needs a current Version over the Valid time it states, because it
-- replaces what that Version said. `write_patch` is now this function with no
-- kind.
create function gnomon.write_patch_as(
  p_tenant_id uuid,
  p_id uuid,
  p_is_edge boolean,
  p_episode_id uuid,
  p_valid_from timestamptz,
  p_valid_from_precision text,
  p_valid_to timestamptz,
  p_valid_to_precision text,
  p_values jsonb,
  p_places jsonb,
  p_kind text
) returns table (status text, recorded_at timestamptz, versions uuid[])
  language plpgsql volatile security invoker set search_path = ''
  as $$
  declare
    v_of constant text := case when p_is_edge then 'Edge' else 'Node' end;
    v_values jsonb := p_values;
    vfp gnomon.time_precision;
    vtp gnomon.time_precision;
    vf timestamptz;
    vt timestamptz;
    v_scope text;
    v_reference_time timestamptz;
    v_recorded_at timestamptz;
    v_statement bigint;
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
    if p_is_edge then
      v_values := coalesce(v_values, '{}');
      if jsonb_typeof(v_values) <> 'object' then
        raise exception using errcode = 'GNW01',
          message = 'a write states its values as {property: value}';
      end if;
      if exists (select from gnomon.edge_builtin_rules() b where v_values ? b.property) then
        raise exception using errcode = 'GNW01',
          message = '"holds" is built in: every write to an Edge states that the Edge holds over its Valid time';
      end if;
    elsif jsonb_typeof(v_values) is distinct from 'object' or v_values = '{}' then
      raise exception using errcode = 'GNW01',
        message = 'a write states a value for at least one property';
    end if;
    if p_places is not null and (
      jsonb_typeof(p_places) <> 'object'
      or exists (
        select from jsonb_each(p_places) p
        where jsonb_typeof(p.value) <> 'string' or not v_values ? p.key
      )
    ) then
      raise exception using errcode = 'GNW01',
        message = 'places are {property: place}, a string for a property the write states';
    end if;

    select l.scope into v_scope from gnomon.lock_for_write(p_tenant_id, p_id, p_is_edge) l;
    select e.reference_time, e.recorded_at into v_reference_time, v_recorded_at
      from gnomon.episode e
      where e.tenant_id = p_tenant_id and e.scope = v_scope and e.id = p_episode_id;
    if not found then
      raise exception using errcode = 'GNW01',
        message = format('Episode %s is not visible in the %s''s Scope', p_episode_id, v_of);
    end if;

    if p_kind = 'correction' then
      if not exists (
        select from gnomon.version x
        where x.tenant_id = p_tenant_id and x.scope = v_scope and x.versioned_id = p_id
          and x.recorded_to = 'infinity' and x.valid_from < vt and x.valid_to > vf
      ) then
        raise exception using errcode = 'GNW01',
          message = format('%s %s has no current Version over [%s, %s) to correct', v_of, p_id, vf, vt);
      end if;
      insert into gnomon.statement (
        tenant_id, scope, node_id, edge_id, episode_id, kind,
        valid_from, valid_from_precision, valid_to, valid_to_precision,
        episode_reference_time, episode_recorded_at
      ) values (
        p_tenant_id, v_scope, case when not p_is_edge then p_id end,
        case when p_is_edge then p_id end, p_episode_id, 'correction',
        vf, vfp, vt, vtp, v_reference_time, v_recorded_at
      ) returning id into v_statement;
    end if;

    -- In property order, so the first refused value is the same on every
    -- host.
    insert into gnomon.contribution (
      tenant_id, scope, node_id, edge_id, episode_id, property, value, place,
      valid_from, valid_from_precision, valid_to, valid_to_precision,
      episode_reference_time, episode_recorded_at, statement_id
    )
    select p_tenant_id, v_scope,
           case when not p_is_edge then p_id end, case when p_is_edge then p_id end,
           p_episode_id, x.key, x.value, p_places ->> x.key,
           vf, vfp, vt, vtp, v_reference_time, v_recorded_at, v_statement
    from (
      select e.key, e.value from jsonb_each(v_values) e
      union all
      select b.property, 'true'::jsonb from gnomon.edge_builtin_rules() b where p_is_edge
    ) x
    order by x.key;

    select * into step from gnomon.recompute_versions(p_tenant_id, p_id, p_is_edge, vf, vt);
    status := case when step.changed then 'written' else 'unchanged' end;
    recorded_at := transaction_timestamp();
    versions := step.opened;
    return next;
  end
  $$;

create or replace function gnomon.write_patch(
  p_tenant_id uuid,
  p_id uuid,
  p_is_edge boolean,
  p_episode_id uuid,
  p_valid_from timestamptz,
  p_valid_from_precision text,
  p_valid_to timestamptz,
  p_valid_to_precision text,
  p_values jsonb,
  p_places jsonb
) returns table (status text, recorded_at timestamptz, versions uuid[])
  language sql volatile security invoker set search_path = ''
  as $$
    select w.status, w.recorded_at, w.versions
    from gnomon.write_patch_as(
      p_tenant_id, p_id, p_is_edge, p_episode_id, p_valid_from, p_valid_from_precision,
      p_valid_to, p_valid_to_precision, p_values, p_places, null) w
  $$;

-- Records a Correction of a Node: as `gnomon.write_node`, and the Node must
-- have a current Version over the Valid time.
create function gnomon.correct_node(
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
  language sql volatile security invoker set search_path = ''
  as $$
    select w.status, w.recorded_at, w.versions
    from gnomon.write_patch_as(
      p_tenant_id, p_node_id, false, p_episode_id, p_valid_from, p_valid_from_precision,
      p_valid_to, p_valid_to_precision, p_values, p_places, 'correction') w
  $$;

-- Records a Correction of an Edge, as `gnomon.write_edge` does for a write.
create function gnomon.correct_edge(
  p_tenant_id uuid,
  p_edge_id uuid,
  p_episode_id uuid,
  p_valid_from timestamptz,
  p_valid_from_precision text,
  p_valid_to timestamptz,
  p_valid_to_precision text,
  p_values jsonb,
  p_places jsonb
) returns table (status text, recorded_at timestamptz, versions uuid[])
  language sql volatile security invoker set search_path = ''
  as $$
    select w.status, w.recorded_at, w.versions
    from gnomon.write_patch_as(
      p_tenant_id, p_edge_id, true, p_episode_id, p_valid_from, p_valid_from_precision,
      p_valid_to, p_valid_to_precision, p_values, p_places, 'correction') w
  $$;

-- Records an End of validity or a Retraction of a Node or an Edge, citing one
-- Episode. An End of validity closes the fact at `p_at` (to `p_precision`): the
-- current Version that holds across that instant ends there, and `p_at` must
-- fall inside it, not at its start, which is a Retraction. A Retraction names
-- a current Version and states that what it said was never true. Either way
-- the statement is Contributions of JSON null, for each property of the
-- Version and, for an Edge, `holds`, over the stated period, so ranked merge
-- and the merge order decide whether they win.
create function gnomon.write_ending(
  p_tenant_id uuid,
  p_id uuid,
  p_is_edge boolean,
  p_episode_id uuid,
  p_kind text,
  p_version_id uuid,
  p_at timestamptz,
  p_precision text
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
      episode_reference_time, episode_recorded_at
    ) values (
      p_tenant_id, v_scope, case when not p_is_edge then p_id end,
      case when p_is_edge then p_id end, p_episode_id, p_kind, v.id,
      sf, sfp, v.valid_to, v.valid_to_precision, v_reference_time, v_recorded_at
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

create function gnomon.end_validity(
  p_tenant_id uuid,
  p_id uuid,
  p_is_edge boolean,
  p_episode_id uuid,
  p_at timestamptz,
  p_precision text
) returns table (status text, recorded_at timestamptz, versions uuid[])
  language sql volatile security invoker set search_path = ''
  as $$
    select w.status, w.recorded_at, w.versions
    from gnomon.write_ending(
      p_tenant_id, p_id, p_is_edge, p_episode_id, 'end_of_validity', null, p_at, p_precision) w
  $$;

create function gnomon.retract_version(
  p_tenant_id uuid,
  p_id uuid,
  p_is_edge boolean,
  p_episode_id uuid,
  p_version_id uuid
) returns table (status text, recorded_at timestamptz, versions uuid[])
  language sql volatile security invoker set search_path = ''
  as $$
    select w.status, w.recorded_at, w.versions
    from gnomon.write_ending(
      p_tenant_id, p_id, p_is_edge, p_episode_id, 'retraction', p_version_id, null, null) w
  $$;

-- Lists the statements about a Node or an Edge, in the order they were
-- recorded, with the Episode that made each. A statement that lost on
-- Authority is listed too: it is held.
create function gnomon.list_statements(p_tenant_id uuid, p_id uuid)
  returns table (
    kind text,
    episode_id uuid,
    version_id uuid,
    valid_from timestamptz,
    valid_from_precision gnomon.time_precision,
    valid_to timestamptz,
    valid_to_precision gnomon.time_precision,
    recorded_at timestamptz
  )
  language sql stable security invoker set search_path = ''
  as $$
    select s.kind, s.episode_id, s.version_id, s.valid_from, s.valid_from_precision,
           s.valid_to, s.valid_to_precision, s.recorded_at
    from gnomon.statement s
    where s.tenant_id = p_tenant_id and s.versioned_id = p_id
    order by s.id
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
      'gnomon.statement_check_insert()'::regprocedure,
      'gnomon.write_patch_as(uuid, uuid, boolean, uuid, timestamptz, text, timestamptz, text, jsonb, jsonb, text)'::regprocedure,
      'gnomon.correct_node(uuid, uuid, uuid, timestamptz, text, timestamptz, text, jsonb, jsonb)'::regprocedure,
      'gnomon.correct_edge(uuid, uuid, uuid, timestamptz, text, timestamptz, text, jsonb, jsonb)'::regprocedure,
      'gnomon.write_ending(uuid, uuid, boolean, uuid, text, uuid, timestamptz, text)'::regprocedure,
      'gnomon.end_validity(uuid, uuid, boolean, uuid, timestamptz, text)'::regprocedure,
      'gnomon.retract_version(uuid, uuid, boolean, uuid, uuid)'::regprocedure,
      'gnomon.list_statements(uuid, uuid)'::regprocedure
    ) and a.grantee <> p.proowner
    union
    select format('table %s', c.oid::regclass), a.grantee
    from pg_class c, aclexplode(c.relacl) a
    where c.oid = 'gnomon.statement'::regclass and a.grantee <> c.relowner
  loop
    execute format(
      'revoke all on %s from %s', g.object,
      case when g.grantee = 0 then 'public' else g.grantee::regrole::text end);
  end loop;
  revoke all on function gnomon.statement_check_insert() from public;
  revoke all on function gnomon.write_patch_as(
    uuid, uuid, boolean, uuid, timestamptz, text, timestamptz, text, jsonb, jsonb, text) from public;
  revoke all on function gnomon.correct_node(
    uuid, uuid, uuid, timestamptz, text, timestamptz, text, jsonb, jsonb) from public;
  revoke all on function gnomon.correct_edge(
    uuid, uuid, uuid, timestamptz, text, timestamptz, text, jsonb, jsonb) from public;
  revoke all on function gnomon.write_ending(
    uuid, uuid, boolean, uuid, text, uuid, timestamptz, text) from public;
  revoke all on function gnomon.end_validity(uuid, uuid, boolean, uuid, timestamptz, text)
    from public;
  revoke all on function gnomon.retract_version(uuid, uuid, boolean, uuid, uuid) from public;
  revoke all on function gnomon.list_statements(uuid, uuid) from public;
end
$$;

-- Callers read and insert statements as they do Contributions, and update and
-- delete nothing. They never insert a Recorded time.
grant select on gnomon.statement to authenticated, gnomon_worker;
grant insert (
  tenant_id, scope, node_id, edge_id, episode_id, kind, version_id,
  valid_from, valid_from_precision, valid_to, valid_to_precision,
  episode_reference_time, episode_recorded_at
) on gnomon.statement to authenticated, gnomon_worker;
grant insert (statement_id) on gnomon.contribution to authenticated, gnomon_worker;

-- The functions run as the invoker, so the callers need every function they
-- call.
grant execute on function gnomon.write_patch_as(
  uuid, uuid, boolean, uuid, timestamptz, text, timestamptz, text, jsonb, jsonb, text)
  to authenticated, gnomon_worker;
grant execute on function gnomon.correct_node(
  uuid, uuid, uuid, timestamptz, text, timestamptz, text, jsonb, jsonb)
  to authenticated, gnomon_worker;
grant execute on function gnomon.correct_edge(
  uuid, uuid, uuid, timestamptz, text, timestamptz, text, jsonb, jsonb)
  to authenticated, gnomon_worker;
grant execute on function gnomon.write_ending(
  uuid, uuid, boolean, uuid, text, uuid, timestamptz, text)
  to authenticated, gnomon_worker;
grant execute on function gnomon.end_validity(uuid, uuid, boolean, uuid, timestamptz, text)
  to authenticated, gnomon_worker;
grant execute on function gnomon.retract_version(uuid, uuid, boolean, uuid, uuid)
  to authenticated, gnomon_worker;
grant execute on function gnomon.list_statements(uuid, uuid) to authenticated, gnomon_worker;

insert into gnomon.schema_version (version, name) values (9, '0009_statements');
