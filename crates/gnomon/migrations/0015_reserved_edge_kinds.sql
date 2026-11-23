-- gnomon migration 0015: the reserved Edge Kinds `same_as` and
-- `different_from`.
--
-- Resolution records its findings as Edges, not as merged Nodes (ADR-0012).
-- When two Nodes prove to be one thing, a `same_as` Edge says so, and a
-- `different_from` Edge records that two are not. Every Ontology has both
-- Kinds, each joining any Node to any other Node. They are ordinary Edges,
-- so Valid time, Citations, Retraction and Erasure apply to them with no
-- new mechanism, and reads do not collapse the Nodes they join.
--
-- Like the root Kinds of migration 0003, the reserved Kinds are not rows:
-- `gnomon.reserved_kinds()` defines them once for every Tenant, so a Tenant
-- has them from the moment it first appears, whenever that is, and no
-- consumer registers them. Each is an Edge Kind below the root Kind `edge`,
-- with no properties of its own.
--
-- Both are symmetric: an Edge of either joins its two Nodes in no direction.
-- The Store keeps one such Edge per Kind, pair of Nodes and Scope, with its
-- endpoints stored in a canonical order, the lower Node ID as the source.
-- Creating one from A to B and then from B to A returns the same Edge, and a
-- traversal follows it from either Node in either direction. A Retraction
-- ends it and it stays the same Edge, so a later write states that it holds
-- again. An Erasure deletes it, and creating it again creates a new Edge.
-- The Edge lives in one Scope like any Edge, so the same pair of Nodes may
-- have one in each Scope.
--
-- No consumer can remove, redefine or reuse the two names. They are not
-- rows, so there is nothing to delete or rewrite, and Kind registration,
-- the definer function of migration 0003 (ADR-0009), refuses to register a
-- Kind of either name: a trigger on `gnomon.kind` refuses the row it would
-- insert. A Kind cannot specialize one, because registration finds no
-- registered parent of that name, and no Edge Kind can name one as an
-- endpoint, because neither is a Node Kind. The merge-rule setter finds no
-- registered Kind of either name either, so both keep the recency rule.
-- This migration adds no definer function.

-- A Tenant that registered a Kind of either name before this migration must
-- rename it first, as migration 0008 asks of a property named `holds`.
do $$
begin
  if exists (select from gnomon.kind k where k.name in ('same_as', 'different_from')) then
    raise exception 'a Tenant registered a Kind named same_as or different_from, which migration '
      '0015 reserves for every Ontology; rename the Kind before applying it';
  end if;
end
$$;

-- The reserved Edge Kinds every Ontology has. Each searches the Sentence, as
-- the root Edge Kind does, joins any Node to any Node, and is symmetric.
create function gnomon.reserved_kinds()
  returns table (
    name text,
    kind_of text,
    parent text,
    search text[],
    source_kinds text[],
    target_kinds text[],
    is_symmetric boolean
  )
  language sql immutable parallel safe security invoker set search_path = ''
  as $$
    values
      ('same_as', 'edge', 'edge', array['sentence'], array['node'], array['node'], true),
      ('different_from', 'edge', 'edge', array['sentence'], array['node'], array['node'], true)
  $$;

-- Kind registration refuses a Kind of either name, of Nodes or of Edges,
-- whatever its definition. It inserts each Kind it registers, so this
-- trigger refuses the row before anything is written. The function runs as
-- the definer during registration, which holds EXECUTE on
-- `gnomon.reserved_kinds()`.
create function gnomon.kind_check_reserved_name() returns trigger
  language plpgsql security invoker set search_path = ''
  as $$
  begin
    if exists (select from gnomon.reserved_kinds() r where r.name = new.name) then
      raise exception using errcode = 'GNK01',
        message = format(
          '"%s" is a reserved Edge Kind, which every Ontology already has; '
          'a Kind cannot take its name', new.name);
    end if;
    return new;
  end
  $$;
create trigger kind_check_reserved_name before insert on gnomon.kind
  for each row execute function gnomon.kind_check_reserved_name();

-- Whether an Edge of `p_kind` joins its Nodes in no direction.
create function gnomon.is_symmetric_kind(p_kind text)
  returns boolean
  language sql immutable parallel safe security invoker set search_path = ''
  as $$
    select coalesce(
      (select r.is_symmetric from gnomon.reserved_kinds() r where r.name = p_kind), false)
  $$;

-- The Kinds a Node or an Edge of `p_kind` belongs to, as migration 0008
-- defines it, with the reserved Kinds below the root Kind `edge`.
create or replace function gnomon.kind_lineage(p_tenant_id uuid, p_kind text)
  returns text[]
  language sql stable security invoker set search_path = ''
  as $$
    select coalesce(
      (select array[r.name] from gnomon.root_kinds() r where r.name = p_kind),
      (select array[r.parent, r.name] from gnomon.reserved_kinds() r where r.name = p_kind),
      (select k.ancestors || k.name from gnomon.kind k
       where k.tenant_id = p_tenant_id and k.name = p_kind)
    )
  $$;

-- The insert check of migration 0008, with the reserved Kinds among the
-- Edge Kinds of every Tenant. An Edge of a symmetric Kind joins two
-- different Nodes, and once both are checked its endpoints are stored in
-- the canonical order, the lower ID as the source, whatever order the caller
-- gave, so the unique index below sees one Edge per pair.
create or replace function gnomon.edge_check_insert() returns trigger
  language plpgsql security invoker set search_path = ''
  as $$
  declare
    allowed record;
    v_end record;
    v_kind text;
  begin
    select r.source_kinds, r.target_kinds into allowed
      from gnomon.root_kinds() r where r.name = new.kind and r.kind_of = 'edge';
    if not found then
      select r.source_kinds, r.target_kinds into allowed
        from gnomon.reserved_kinds() r where r.name = new.kind;
    end if;
    if not found then
      select k.source_kinds, k.target_kinds into allowed
        from gnomon.kind k
        where k.tenant_id = new.tenant_id and k.name = new.kind and k.kind_of = 'edge';
      if not found then
        raise exception using errcode = 'GNW01',
          message = format('"%s" is not an Edge Kind of the Tenant', new.kind);
      end if;
    end if;
    if gnomon.is_symmetric_kind(new.kind) and new.source_id = new.target_id then
      raise exception using errcode = 'GNW01',
        message = format('an Edge of the symmetric Kind "%s" joins two different Nodes', new.kind);
    end if;
    for v_end in
      select * from (values
        ('source', new.source_id, allowed.source_kinds, 'start at'),
        ('target', new.target_id, allowed.target_kinds, 'end at')
      ) x(side, node_id, kinds, verb)
    loop
      select n.kind into v_kind
        from gnomon.node n where n.tenant_id = new.tenant_id and n.id = v_end.node_id;
      if not found then
        raise exception using errcode = 'GNW01',
          message = format('the %s Node %s is not visible to the caller', v_end.side, v_end.node_id);
      end if;
      if not gnomon.kind_lineage(new.tenant_id, v_kind) && v_end.kinds then
        raise exception using errcode = 'GNW01',
          message = format(
            'Edge Kind "%s" may not %s a Node of Kind "%s"; it allows %s',
            new.kind, v_end.verb, v_kind, array_to_string(v_end.kinds, ', '));
      end if;
    end loop;
    if gnomon.is_symmetric_kind(new.kind) then
      select least(new.source_id, new.target_id), greatest(new.source_id, new.target_id)
        into new.source_id, new.target_id;
    end if;
    return new;
  end
  $$;

-- One Edge of a symmetric Kind per pair of Nodes in a Scope. The Scope is
-- part of the key, as every Edge lives in one Scope, so a caller's Edge never
-- collides with one in a Scope it cannot see.
create unique index edge_symmetric on gnomon.edge (tenant_id, scope, kind, source_id, target_id)
  where gnomon.is_symmetric_kind(kind);

-- Creates an Edge as migration 0008 does and returns its ID. For a
-- symmetric Kind it returns the Scope's Edge of that Kind between the two
-- Nodes when one exists, in either order, so creating the Edge from B to A
-- after the one from A to B addresses the same Edge. The insert trigger
-- checks the Kind and the endpoints first, in both cases.
create or replace function gnomon.create_edge(
  p_tenant_id uuid,
  p_scope text,
  p_kind text,
  p_source_id uuid,
  p_target_id uuid
) returns uuid
  language plpgsql volatile security invoker set search_path = ''
  as $$
  declare
    v_id uuid;
  begin
    insert into gnomon.edge (tenant_id, scope, kind, source_id, target_id)
    values (p_tenant_id, p_scope, p_kind, p_source_id, p_target_id)
    on conflict (tenant_id, scope, kind, source_id, target_id)
      where gnomon.is_symmetric_kind(kind)
      do nothing
    returning id into v_id;
    if v_id is null then
      select e.id into v_id
        from gnomon.edge e
        where e.tenant_id = p_tenant_id and e.scope = p_scope and e.kind = p_kind
          and e.source_id = least(p_source_id, p_target_id)
          and e.target_id = greatest(p_source_id, p_target_id);
    end if;
    return v_id;
  end
  $$;

-- Traverses the Edges of a Node As-of a valid-at and a known-at instant, as
-- migration 0008's `traverse` does, following an Edge of a symmetric Kind
-- from either of its Nodes: `out` and `in` each find it from both ends, and
-- `both` finds it once, as `out`. Its source and target are the canonical
-- order, and the neighbour is the Node at the other end. Every other Edge is
-- followed as before.
create or replace function gnomon.traverse(
  p_tenant_id uuid,
  p_node_id uuid,
  p_valid_at timestamptz,
  p_known_at timestamptz,
  p_direction text,
  p_kinds text[]
) returns table (
    direction text,
    id uuid,
    edge_id uuid,
    scope text,
    kind text,
    source_id uuid,
    target_id uuid,
    props jsonb,
    valid_from timestamptz,
    valid_from_precision gnomon.time_precision,
    valid_to timestamptz,
    valid_to_precision gnomon.time_precision,
    recorded_from timestamptz,
    recorded_to timestamptz,
    merge_rule gnomon.merge_rule,
    uncertain boolean,
    text_hash text,
    sentence text,
    neighbour_id uuid,
    neighbour_scope text,
    neighbour_kind text,
    neighbour_version_id uuid,
    neighbour_props jsonb,
    neighbour_valid_from timestamptz,
    neighbour_valid_from_precision gnomon.time_precision,
    neighbour_valid_to timestamptz,
    neighbour_valid_to_precision gnomon.time_precision,
    neighbour_recorded_from timestamptz,
    neighbour_recorded_to timestamptz,
    neighbour_merge_rule gnomon.merge_rule,
    neighbour_uncertain boolean,
    neighbour_text_hash text
  )
  language plpgsql stable security invoker set search_path = ''
  as $$
  begin
    if p_direction is null or p_direction not in ('out', 'in', 'both') then
      raise exception using errcode = 'invalid_parameter_value',
        message = 'a traversal follows Edges out, in or both';
    end if;
    return query
      with origin as (
        select o.tenant_id, o.id from gnomon.node o
        where o.tenant_id = p_tenant_id and o.id = p_node_id
      ), steps as (
        -- An Edge of a symmetric Kind is followed whichever end the Node is,
        -- in the direction asked for, and once, as `out`, for `both`. It
        -- never joins a Node to itself.
        select case when p_direction = 'in' and gnomon.is_symmetric_kind(e.kind)
                    then 'in' else 'out' end as direction,
               e.tenant_id, e.scope, e.id, e.kind,
               e.source_id, e.target_id, e.target_id as neighbour
        from origin o join gnomon.edge e on e.tenant_id = o.tenant_id and e.source_id = o.id
        where p_direction in ('out', 'both') or gnomon.is_symmetric_kind(e.kind)
        union all
        select case when p_direction <> 'in' and gnomon.is_symmetric_kind(e.kind)
                    then 'out' else 'in' end,
               e.tenant_id, e.scope, e.id, e.kind, e.source_id, e.target_id, e.source_id
        from origin o join gnomon.edge e on e.tenant_id = o.tenant_id and e.target_id = o.id
        where (p_direction in ('in', 'both') or gnomon.is_symmetric_kind(e.kind))
          and not (p_direction = 'both' and e.source_id = e.target_id)
      )
      select s.direction, v.id, s.id, s.scope, s.kind, s.source_id, s.target_id, v.props,
             v.valid_from, v.valid_from_precision, v.valid_to, v.valid_to_precision,
             v.recorded_from, v.recorded_to, v.merge_rule,
             gnomon.uncertain_at(p_valid_at, v.valid_from, v.valid_from_precision),
             v.text_hash, st.sentence,
             nb.id, nb.scope, nb.kind, nv.id, nv.props,
             nv.valid_from, nv.valid_from_precision, nv.valid_to, nv.valid_to_precision,
             nv.recorded_from, nv.recorded_to, nv.merge_rule,
             case when nv.id is not null
                  then gnomon.uncertain_at(p_valid_at, nv.valid_from, nv.valid_from_precision) end,
             nv.text_hash
      from steps s
      join gnomon.node nb on nb.tenant_id = s.tenant_id and nb.id = s.neighbour
      join gnomon.version v
        on v.tenant_id = s.tenant_id and v.scope = s.scope and v.edge_id = s.id
       and v.valid_from <= p_valid_at and v.valid_to > p_valid_at
       and case
             when p_known_at is null then v.recorded_to = 'infinity'
             else v.recorded_from <= p_known_at and v.recorded_to > p_known_at
           end
      left join gnomon.sentence st
        on st.tenant_id = v.tenant_id and st.scope = v.scope and st.version_id = v.id
      left join lateral (
        select x.* from gnomon.version x
        where x.tenant_id = nb.tenant_id and x.scope = nb.scope and x.node_id = nb.id
          and x.valid_from <= p_valid_at and x.valid_to > p_valid_at
          and case
                when p_known_at is null then x.recorded_to = 'infinity'
                else x.recorded_from <= p_known_at and x.recorded_to > p_known_at
              end
        limit 1
      ) nv on true
      where p_kinds is null or cardinality(p_kinds) = 0
         or gnomon.kind_lineage(s.tenant_id, s.kind) && p_kinds
      order by s.id, s.direction desc;
  end
  $$;

-- Reads one Kind of the caller's Tenant, flattened, as migration 0003's
-- `read_kind` does, with the reserved Kinds and whether the Kind is
-- symmetric. The column is new, so the function is dropped and created
-- again with the grants it had.
drop function gnomon.read_kind(uuid, text);
create function gnomon.read_kind(p_tenant_id uuid, p_name text)
  returns table (
    name text,
    kind_of text,
    parent text,
    ancestors text[],
    properties jsonb,
    rules jsonb,
    search text[],
    source_kinds text[],
    target_kinds text[],
    is_symmetric boolean
  )
  language sql stable security invoker set search_path = ''
  as $$
    select r.name, r.kind_of, null::text, '{}'::text[],
           '{"type": "object", "properties": {}, "required": []}'::jsonb, '{}'::jsonb,
           r.search, r.source_kinds, r.target_kinds, false
    from gnomon.root_kinds() r
    where r.name = p_name and p_tenant_id = gnomon.tenant_id()
    union all
    select r.name, r.kind_of, r.parent, array[r.parent],
           '{"type": "object", "properties": {}, "required": []}'::jsonb, '{}'::jsonb,
           r.search, r.source_kinds, r.target_kinds, r.is_symmetric
    from gnomon.reserved_kinds() r
    where r.name = p_name and p_tenant_id = gnomon.tenant_id()
    union all
    select k.name, k.kind_of, k.parent, k.ancestors, k.properties,
           coalesce((
             select jsonb_object_agg(x.property, x.rule::text)
             from gnomon.kind_rule x
             where x.tenant_id = k.tenant_id and x.kind = k.name
           ), '{}'::jsonb),
           k.search, k.source_kinds, k.target_kinds, false
    from gnomon.kind k
    where k.tenant_id = p_tenant_id and k.name = p_name
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
      'gnomon.reserved_kinds()'::regprocedure,
      'gnomon.kind_check_reserved_name()'::regprocedure,
      'gnomon.is_symmetric_kind(text)'::regprocedure,
      'gnomon.read_kind(uuid, text)'::regprocedure
    ) and a.grantee <> p.proowner
  loop
    execute format(
      'revoke all on %s from %s', g.object,
      case when g.grantee = 0 then 'public' else g.grantee::regrole::text end);
  end loop;
  revoke all on function gnomon.reserved_kinds() from public;
  revoke all on function gnomon.kind_check_reserved_name() from public;
  revoke all on function gnomon.is_symmetric_kind(text) from public;
  revoke all on function gnomon.read_kind(uuid, text) from public;
end
$$;

grant execute on function gnomon.reserved_kinds() to authenticated, gnomon_worker, gnomon_definer;
grant execute on function gnomon.is_symmetric_kind(text) to authenticated, gnomon_worker;
grant execute on function gnomon.read_kind(uuid, text) to authenticated, gnomon_worker;

insert into gnomon.schema_version (version, name) values (15, '0015_reserved_edge_kinds');
