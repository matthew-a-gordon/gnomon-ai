-- gnomon migration 0008: Edges, their Sentences, traversal and the batch
-- As-of read.
--
-- An Edge is a stable identity for a directed relationship between two
-- Nodes, of an Edge Kind, in one Scope. It joins the Nodes themselves, not
-- their Versions. What is known about it is held in its Versions, which are
-- computed from its Contributions exactly as a Node's are (ADR-0007): the
-- Contribution, Version, Citation and bound statement tables now hold the
-- rows of Edges beside those of Nodes. Each such row names its Node in
-- `node_id` or its Edge in `edge_id`, never both, and `versioned_id` is
-- whichever it names. The merge, the guard triggers, bound precision and
-- ranked merge read `versioned_id`, so they cover Edge Versions with no
-- second copy of their rules. Callers get no new right on Versions: the one
-- new column they may insert is `edge_id`, beside `node_id`.
--
-- An Edge holds over a Valid time. Every write to an Edge records, besides
-- the property values it states, a Contribution of the built-in property
-- `holds` with the value true over the write's Valid time. An Edge Version
-- exists only where `holds` wins, and its values are the Edge Kind's
-- properties there: `holds` never appears among them, but the Citations name
-- it, so a write that states no property value still cites its Episode. A
-- write to an Edge may state no property value at all. An End of validity of
-- an Edge (ticket #11) is a Contribution of `holds` with a JSON null.
--
-- The Store checks only that both endpoints exist in the Tenant and that the
-- Edge Kind allows their Kinds (ADR-0008). Temporal referential integrity is
-- not enforced: a Node's Valid time belongs to each Version, so there is no
-- lifespan to check an Edge against. An Edge lives in one Scope, and each
-- endpoint may be in any Scope of the Tenant that the caller can see. A
-- foreign key on the Tenant and the Node refuses an endpoint in another
-- Tenant whatever RLS allows, and the insert trigger, which runs as the
-- caller under RLS, refuses a Node the caller cannot see. A Node with no
-- Versions stays as a bare identity while any Edge joins it: the foreign keys
-- keep it.
--
-- A Sentence states in plain language what one Edge Version says. The
-- Pipeline writes it after the Version commits, from the Version as
-- committed (ADR-0013), so it is held outside the Version's values, in a
-- table keyed by the Edge Version, and a write to an Edge takes none. Reads
-- and traversal return each Edge Version's Sentence, or none until one is
-- stored. This migration grants callers no way to store one: ticket #26
-- adds that function.

-- An Edge: a stable identity of an Edge Kind, in one Scope, from a source
-- Node to a target Node.
create table gnomon.edge (
  tenant_id uuid not null,
  scope text not null,
  id uuid not null default gnomon.uuid_v7(),
  -- The root Kind `edge` or an Edge Kind the Tenant registered. Root Kinds
  -- are not rows, so a trigger checks this, not a foreign key.
  kind text not null,
  source_id uuid not null,
  target_id uuid not null,
  created_at timestamptz not null default transaction_timestamp(),
  primary key (tenant_id, id),
  unique (tenant_id, scope, id),
  -- The Tenant leads each key, so an endpoint is always a Node of the Edge's
  -- own Tenant, in any of its Scopes.
  foreign key (tenant_id, source_id) references gnomon.node (tenant_id, id),
  foreign key (tenant_id, target_id) references gnomon.node (tenant_id, id)
);
-- Traversal from a Node, and the foreign keys' checks when a Node is
-- erased. The policy's Tenant and Scope conditions are still index
-- conditions, after the endpoint (ADR-0005).
create index edge_source on gnomon.edge (tenant_id, source_id, scope);
create index edge_target on gnomon.edge (tenant_id, target_id, scope);

alter table gnomon.edge enable row level security;
create policy edge_tenant_scope on gnomon.edge
  for all to authenticated, gnomon_worker
  using (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()))
  with check (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()));

-- Contributions, Versions and bound statements of Edges. Each row names one
-- Node or one Edge.
alter table gnomon.contribution alter column node_id drop not null;
alter table gnomon.contribution add column edge_id uuid;
alter table gnomon.contribution add column versioned_id uuid
  generated always as (coalesce(node_id, edge_id)) stored;
alter table gnomon.contribution
  add constraint contribution_edge foreign key (tenant_id, scope, edge_id)
    references gnomon.edge (tenant_id, scope, id),
  add constraint contribution_node_or_edge check (num_nonnulls(node_id, edge_id) = 1);
-- The merge reads a Node's or an Edge's Contributions by `versioned_id`.
create index contribution_versioned on gnomon.contribution
  (tenant_id, scope, versioned_id, valid_from);
create index contribution_edge on gnomon.contribution (tenant_id, scope, edge_id)
  where edge_id is not null;

alter table gnomon.version alter column node_id drop not null;
alter table gnomon.version add column edge_id uuid;
alter table gnomon.version add column versioned_id uuid
  generated always as (coalesce(node_id, edge_id)) stored;
alter table gnomon.version
  add constraint version_edge foreign key (tenant_id, scope, edge_id)
    references gnomon.edge (tenant_id, scope, id),
  add constraint version_node_or_edge check (num_nonnulls(node_id, edge_id) = 1);
-- A Node's or an Edge's Versions never overlap in both times at once. The
-- constraint of migration 0004 named only the Node, and a one-point range
-- of a null Node is unbounded, so it is replaced by one over `versioned_id`.
alter table gnomon.version drop constraint version_no_overlap;
alter table gnomon.version add constraint version_no_overlap exclude using gist (
  gnomon.uuid_range(tenant_id, tenant_id, '[]') with &&,
  gnomon.uuid_range(versioned_id, versioned_id, '[]') with &&,
  tstzrange(valid_from, valid_to) with &&,
  tstzrange(recorded_from, recorded_to) with &&
);
-- The write path reads the open Versions of a Node or an Edge.
drop index gnomon.version_open;
create index version_open on gnomon.version (tenant_id, scope, versioned_id, valid_from)
  where recorded_to = 'infinity';
-- As-of reads of an Edge, as `version_as_of` serves a Node's.
create index version_edge_as_of on gnomon.version
  (tenant_id, scope, edge_id, valid_from, recorded_from) include (valid_to, recorded_to)
  where edge_id is not null;

alter table gnomon.bound_statement alter column node_id drop not null;
alter table gnomon.bound_statement add column edge_id uuid;
alter table gnomon.bound_statement add column versioned_id uuid
  generated always as (coalesce(node_id, edge_id)) stored;
alter table gnomon.bound_statement
  add constraint bound_statement_edge foreign key (tenant_id, scope, edge_id)
    references gnomon.edge (tenant_id, scope, id),
  add constraint bound_statement_node_or_edge check (num_nonnulls(node_id, edge_id) = 1);

-- The Sentence of an Edge Version (ADR-0013). Closed Versions keep theirs. A
-- Version deleted by the transaction that wrote it takes its Sentence with
-- it, and so will an Erasure that deletes a Version.
create table gnomon.sentence (
  tenant_id uuid not null,
  scope text not null,
  version_id uuid not null,
  sentence text not null check (sentence <> ''),
  primary key (tenant_id, scope, version_id),
  foreign key (tenant_id, scope, version_id)
    references gnomon.version (tenant_id, scope, id) on delete cascade
);

alter table gnomon.sentence enable row level security;
create policy sentence_tenant_scope on gnomon.sentence
  for all to authenticated, gnomon_worker
  using (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()))
  with check (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()));

-- `holds` is built into every Edge, so no Edge Kind may declare it. Kind
-- registration inserts a Kind flattened with its ancestors, so this checks
-- inherited properties too.
do $$
begin
  if exists (
    select from gnomon.kind k
    where k.kind_of = 'edge' and k.properties -> 'properties' ? 'holds'
  ) then
    raise exception 'an Edge Kind declares the property "holds", which migration 0008 builds '
      'into every Edge; rename the property before applying it';
  end if;
end
$$;

create function gnomon.kind_check_reserved() returns trigger
  language plpgsql security invoker set search_path = ''
  as $$
  begin
    if new.kind_of = 'edge' and new.properties -> 'properties' ? 'holds' then
      raise exception using errcode = 'GNK01',
        message = 'property name "holds" is reserved: every Edge records through it where the Edge holds';
    end if;
    return new;
  end
  $$;
create trigger kind_check_reserved before insert on gnomon.kind
  for each row execute function gnomon.kind_check_reserved();

-- The property every Edge has whatever its Kind, and its rule: `holds`,
-- which only true states. A JSON null ends it, which ends the Edge.
create function gnomon.edge_builtin_rules()
  returns table (property text, rule jsonpath)
  language sql immutable parallel safe security invoker set search_path = ''
  as $$
    values ('holds', 'strict $.type() == "boolean" && $ == true'::jsonpath)
  $$;

-- Checks one value of a property of an Edge of `p_kind`, as
-- `gnomon.check_node_value` does for a Node. The built-in comes first.
create function gnomon.check_edge_value(p_tenant_id uuid, p_kind text, p_property text, p_value jsonb)
  returns void
  language plpgsql stable security invoker set search_path = ''
  as $$
  declare
    rule jsonpath := coalesce(
      (select b.rule from gnomon.edge_builtin_rules() b where b.property = p_property),
      (select x.rule from gnomon.kind_rule x
       where x.tenant_id = p_tenant_id and x.kind = p_kind and x.property = p_property)
    );
  begin
    if rule is null then
      raise exception using errcode = 'GNW01',
        message = format('Kind "%s" has no property "%s"', p_kind, p_property);
    end if;
    if p_value <> 'null'::jsonb and not jsonb_path_match(p_value, rule) then
      raise exception using errcode = 'GNW01',
        message = format(
          'the value %s of property "%s" does not satisfy Kind "%s"', p_value, p_property, p_kind);
    end if;
  end
  $$;

-- The Kinds a Node or an Edge of `p_kind` belongs to: its own and every
-- ancestor.
create function gnomon.kind_lineage(p_tenant_id uuid, p_kind text)
  returns text[]
  language sql stable security invoker set search_path = ''
  as $$
    select coalesce(
      (select array[r.name] from gnomon.root_kinds() r where r.name = p_kind),
      (select k.ancestors || k.name from gnomon.kind k
       where k.tenant_id = p_tenant_id and k.name = p_kind)
    )
  $$;

-- A new Edge is of the root Kind `edge` or of an Edge Kind the Tenant
-- registered, and joins two Nodes the caller can see, whose Kinds the Edge
-- Kind allows at each end. A Node of a Kind below an allowed one qualifies.
-- The trigger runs as the caller, so RLS decides what it can see, and it
-- refuses a Node that does not exist and one the caller cannot see with the
-- same message.
create function gnomon.edge_check_insert() returns trigger
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
      select k.source_kinds, k.target_kinds into allowed
        from gnomon.kind k
        where k.tenant_id = new.tenant_id and k.name = new.kind and k.kind_of = 'edge';
      if not found then
        raise exception using errcode = 'GNW01',
          message = format('"%s" is not an Edge Kind of the Tenant', new.kind);
      end if;
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
    return new;
  end
  $$;
create trigger edge_check_insert before insert on gnomon.edge
  for each row execute function gnomon.edge_check_insert();

-- A Contribution is recorded when its write's transaction started, and its
-- value satisfies the Kind of its Node or Edge. This is migration 0004's
-- check, with Edges.
create or replace function gnomon.contribution_check_insert() returns trigger
  language plpgsql security invoker set search_path = ''
  as $$
  declare
    v_kind text;
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
    -- A Node, Edge or Episode the caller cannot see fails a foreign key or
    -- the policy, and a row naming both or neither fails the check
    -- constraint.
    return new;
  end
  $$;

-- A new Version opens when its write's transaction started, is marked as
-- written by that transaction, records the rule of its Node's or Edge's
-- Kind, holds only values that Kind's rules accept and holds every property
-- the Kind requires. An Edge Version's values never hold the built-in
-- `holds`. This is migration 0007's check, with Edges.
create or replace function gnomon.version_check_insert() returns trigger
  language plpgsql security invoker set search_path = ''
  as $$
  declare
    v_kind text;
    v_of text;
    v_rule text;
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
    if new.node_id is not null then
      v_of := 'Node';
      select n.kind into v_kind
        from gnomon.node n
        where n.tenant_id = new.tenant_id and n.scope = new.scope and n.id = new.node_id;
    elsif new.edge_id is not null then
      v_of := 'Edge';
      select e.kind into v_kind
        from gnomon.edge e
        where e.tenant_id = new.tenant_id and e.scope = new.scope and e.id = new.edge_id;
    end if;
    if v_kind is null then
      return new; -- The foreign keys, the check or the policy refuse it.
    end if;
    v_rule := gnomon.kind_merge_rule(new.tenant_id, v_kind);
    if new.merge_rule::text is distinct from v_rule then
      raise exception using errcode = 'GNW01',
        message = format('a new Version records the merge rule of its %s''s Kind, %s', v_of, v_rule);
    end if;
    for k, v in select e.key, e.value from jsonb_each(new.props) e order by e.key loop
      if v = 'null'::jsonb then
        raise exception using errcode = 'GNW01',
          message = format('a Version holds no null; property "%s" is null', k);
      end if;
      if v_of = 'Node' then
        perform gnomon.check_node_value(new.tenant_id, v_kind, k, v);
      elsif exists (select from gnomon.edge_builtin_rules() b where b.property = k) then
        raise exception using errcode = 'GNW01',
          message = format('an Edge Version''s values never hold the built-in property "%s"', k);
      else
        perform gnomon.check_edge_value(new.tenant_id, v_kind, k, v);
      end if;
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

-- The guard on Versions (ADR-0009), as in migration 0004, with the Edge
-- column among those that never change. Edge Versions get the same guard.
create or replace function gnomon.version_guard_update() returns trigger
  language plpgsql security invoker set search_path = ''
  as $$
  begin
    if (new.tenant_id, new.scope, new.id, new.node_id, new.edge_id, new.props, new.valid_from,
        new.valid_from_precision, new.valid_to, new.valid_to_precision, new.recorded_from,
        new.merge_rule, new.written_in, new.closed_in)
      is distinct from
       (old.tenant_id, old.scope, old.id, old.node_id, old.edge_id, old.props, old.valid_from,
        old.valid_from_precision, old.valid_to, old.valid_to_precision, old.recorded_from,
        old.merge_rule, old.written_in, old.closed_in)
    then
      raise exception using errcode = 'insufficient_privilege',
        message = 'a Version is immutable apart from the end of its Recorded time';
    end if;
    if old.recorded_to = 'infinity'
      and new.recorded_to = transaction_timestamp()
      and old.recorded_from < new.recorded_to
    then
      new.closed_in := pg_current_xact_id();
      return new;
    end if;
    if old.closed_in = pg_current_xact_id() and new.recorded_to = 'infinity' then
      new.closed_in := null;
      return new;
    end if;
    raise exception using errcode = 'insufficient_privilege',
      message = 'a Version''s Recorded time may only be closed at the start of the current '
        'transaction, or reopened by the transaction that closed it';
  end
  $$;

-- A Sentence belongs to an Edge Version.
create function gnomon.sentence_check_insert() returns trigger
  language plpgsql security invoker set search_path = ''
  as $$
  begin
    if not exists (
      select from gnomon.version v
      where v.tenant_id = new.tenant_id and v.scope = new.scope and v.id = new.version_id
        and v.edge_id is not null
    ) then
      raise exception using errcode = 'GNW01',
        message = format('a Sentence states what an Edge Version says; %s is not one', new.version_id);
    end if;
    return new;
  end
  $$;
create trigger sentence_check_insert before insert on gnomon.sentence
  for each row execute function gnomon.sentence_check_insert();

-- Opens a write to a Node or an Edge: checks the Tenant against the
-- caller's, takes the lock of the Node or Edge, and returns its Scope and
-- Kind. An Edge's lock has the key `gnomon.node_lock_key` gives its ID, so
-- writes to one Edge serialise as writes to one Node do (ADR-0007).
create function gnomon.lock_for_write(
  p_tenant_id uuid,
  p_id uuid,
  p_is_edge boolean,
  out scope text,
  out kind text
)
  language plpgsql volatile security invoker set search_path = ''
  as $$
  begin
    if p_tenant_id is null or p_tenant_id is distinct from gnomon.tenant_id() then
      raise exception using errcode = 'insufficient_privilege',
        message = 'a write may write only in the caller''s own Tenant';
    end if;
    perform pg_catalog.pg_advisory_xact_lock(gnomon.node_lock_key(p_tenant_id, p_id));
    if p_is_edge then
      select e.scope, e.kind into scope, kind
        from gnomon.edge e where e.tenant_id = p_tenant_id and e.id = p_id;
    else
      select n.scope, n.kind into scope, kind
        from gnomon.node n where n.tenant_id = p_tenant_id and n.id = p_id;
    end if;
    if not found then
      raise exception using errcode = 'GNW01',
        message = format('%s %s is not visible to the caller',
                         case when p_is_edge then 'Edge' else 'Node' end, p_id);
    end if;
  end
  $$;

-- The Kind of the Node or Edge a Contribution is of.
create function gnomon.contribution_kind(p_tenant_id uuid, p_node_id uuid, p_edge_id uuid)
  returns text
  language sql stable security invoker set search_path = ''
  as $$
    select coalesce(
      (select n.kind from gnomon.node n where n.tenant_id = p_tenant_id and n.id = p_node_id),
      (select e.kind from gnomon.edge e where e.tenant_id = p_tenant_id and e.id = p_edge_id)
    )
  $$;

-- The bound a Contribution has now, as in migration 0007, for the
-- Contribution of a Node or of an Edge.
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
      select gnomon.kind_merge_rule(
               c.tenant_id, gnomon.contribution_kind(c.tenant_id, c.node_id, c.edge_id)
             ) = 'ranked' as ranked
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

-- A bound statement is checked as in migration 0006, for a Contribution of
-- a Node or of an Edge. The statement names the Node or Edge of its
-- Contribution, and a direct insert takes that one's lock.
create or replace function gnomon.bound_statement_check_insert() returns trigger
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
    if num_nonnulls(new.node_id, new.edge_id) <> 1 then
      return new; -- The check constraint refuses it.
    end if;
    -- A direct insert serialises with writes and Erasure (ADR-0007). The
    -- generated `versioned_id` is not yet set in a before trigger.
    perform pg_catalog.pg_advisory_xact_lock(
      gnomon.node_lock_key(new.tenant_id, coalesce(new.node_id, new.edge_id)));
    if exists (
      select from gnomon.contribution c
      where c.tenant_id = new.tenant_id and c.id = new.contribution_id
        and (c.node_id, c.edge_id) is distinct from (new.node_id, new.edge_id)
    ) then
      raise exception using errcode = 'GNW01',
        message = format('a statement names the %s of the Contribution it is about',
                         case when new.edge_id is null then 'Node' else 'Edge' end);
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

-- Recomputes the current Versions of a Node or an Edge over a Valid-time
-- window. This is migration 0007's `recompute_node`, reading the rows of
-- `p_id` by `versioned_id`, with one difference for an Edge: a segment
-- holds a Version only where the built-in `holds` wins with a value, and
-- `holds` is left out of the Version's values. A Node's segment holds a
-- Version where any property has a value, as before. Erasure (ticket #13)
-- recomputes Nodes and Edges through this function.
create function gnomon.recompute_versions(
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
  begin
    select l.scope, l.kind into v_scope, v_kind
      from gnomon.lock_for_write(p_tenant_id, p_id, p_is_edge) l;
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
      where c.tenant_id = p_tenant_id and c.scope = v_scope and c.versioned_id = p_id
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

-- A Node's recompute is the shared one.
create or replace function gnomon.recompute_node(
  p_tenant_id uuid,
  p_node_id uuid,
  p_from timestamptz,
  p_to timestamptz,
  out changed boolean,
  out opened uuid[]
)
  language sql volatile security invoker set search_path = ''
  as $$
    select r.changed, r.opened
    from gnomon.recompute_versions(p_tenant_id, p_node_id, false, p_from, p_to) r
  $$;

-- Writes a patch of property values for a Node or an Edge, citing one
-- Episode, over one Valid time. This is migration 0006's `write_node`, which
-- now calls it, generalised to Edges:
--
--   - a write to a Node states at least one property value, as before;
--   - a write to an Edge may state none, and always records a Contribution
--     of the built-in `holds`, true over the write's Valid time, so the Edge
--     holds then. The caller never states `holds` itself.
create function gnomon.write_patch(
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

    -- In property order, so the first refused value is the same on every
    -- host.
    insert into gnomon.contribution (
      tenant_id, scope, node_id, edge_id, episode_id, property, value, place,
      valid_from, valid_from_precision, valid_to, valid_to_precision,
      episode_reference_time, episode_recorded_at
    )
    select p_tenant_id, v_scope,
           case when not p_is_edge then p_id end, case when p_is_edge then p_id end,
           p_episode_id, x.key, x.value, p_places ->> x.key,
           vf, vfp, vt, vtp, v_reference_time, v_recorded_at
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

-- Writes a patch of property values for a Node, as migration 0006 does.
create or replace function gnomon.write_node(
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
    from gnomon.write_patch(
      p_tenant_id, p_node_id, false, p_episode_id, p_valid_from, p_valid_from_precision,
      p_valid_to, p_valid_to_precision, p_values, p_places) w
  $$;

-- Creates an Edge of a Kind in a Scope, from a source Node to a target Node,
-- and returns its ID, a UUID v7. The Edge holds no Version until a write
-- states that it holds. The insert trigger checks the Kind and the
-- endpoints.
create function gnomon.create_edge(
  p_tenant_id uuid,
  p_scope text,
  p_kind text,
  p_source_id uuid,
  p_target_id uuid
) returns uuid
  language sql volatile security invoker set search_path = ''
  as $$
    insert into gnomon.edge (tenant_id, scope, kind, source_id, target_id)
    values (p_tenant_id, p_scope, p_kind, p_source_id, p_target_id)
    returning id
  $$;

-- Writes a patch of property values for an Edge, citing one Episode, over
-- one Valid time, and records that the Edge holds then. `p_values` may be
-- null or empty. The rest is as `gnomon.write_node`.
create function gnomon.write_edge(
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
    from gnomon.write_patch(
      p_tenant_id, p_edge_id, true, p_episode_id, p_valid_from, p_valid_from_precision,
      p_valid_to, p_valid_to_precision, p_values, p_places) w
  $$;

-- States a bound of a current Version's Valid time, citing one Episode, for
-- a Node or an Edge. This is migration 0006's `state_bound`, which now calls
-- it. For an Edge the statement is about the Contributions of `holds` too,
-- so the Edge holds over the stated bound.
create function gnomon.state_bound_of(
  p_tenant_id uuid,
  p_id uuid,
  p_is_edge boolean,
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
    v_of constant text := case when p_is_edge then 'Edge' else 'Node' end;
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

    select l.scope into v_scope from gnomon.lock_for_write(p_tenant_id, p_id, p_is_edge) l;
    select e.reference_time, e.recorded_at into v_reference_time, v_recorded_at
      from gnomon.episode e
      where e.tenant_id = p_tenant_id and e.scope = v_scope and e.id = p_episode_id;
    if not found then
      raise exception using errcode = 'GNW01',
        message = format('Episode %s is not visible in the %s''s Scope', p_episode_id, v_of);
    end if;
    select * into v from gnomon.version x
      where x.tenant_id = p_tenant_id and x.scope = v_scope and x.versioned_id = p_id
        and x.id = p_version_id and x.recorded_to = 'infinity';
    if not found then
      raise exception using errcode = 'GNW01',
        message = format('Version %s is not a current Version of %s %s', p_version_id, v_of, p_id);
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
      where c.tenant_id = p_tenant_id and c.scope = v_scope and c.versioned_id = p_id
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
      tenant_id, scope, node_id, edge_id, episode_id, contribution_id, side, bound_at,
      bound_precision, episode_reference_time, episode_recorded_at
    )
    select p_tenant_id, v_scope,
           case when not p_is_edge then p_id end, case when p_is_edge then p_id end,
           p_episode_id, c, p_side, v_at, v_precision, v_reference_time, v_recorded_at
    from unnest(v_contributions) c;

    select * into step from gnomon.recompute_versions(
      p_tenant_id, p_id, p_is_edge, least(v.valid_from, v_at), greatest(v.valid_to, v_at));
    versions := step.opened;
    return next;
  end
  $$;

-- States a bound of a Node's current Version, as migration 0006 does.
create or replace function gnomon.state_bound(
  p_tenant_id uuid,
  p_node_id uuid,
  p_episode_id uuid,
  p_version_id uuid,
  p_side text,
  p_bound_at timestamptz,
  p_precision text
) returns table (change text, recorded_at timestamptz, versions uuid[])
  language sql volatile security invoker set search_path = ''
  as $$
    select s.change, s.recorded_at, s.versions
    from gnomon.state_bound_of(
      p_tenant_id, p_node_id, false, p_episode_id, p_version_id, p_side, p_bound_at, p_precision) s
  $$;

-- States a bound of an Edge's current Version.
create function gnomon.state_edge_bound(
  p_tenant_id uuid,
  p_edge_id uuid,
  p_episode_id uuid,
  p_version_id uuid,
  p_side text,
  p_bound_at timestamptz,
  p_precision text
) returns table (change text, recorded_at timestamptz, versions uuid[])
  language sql volatile security invoker set search_path = ''
  as $$
    select s.change, s.recorded_at, s.versions
    from gnomon.state_bound_of(
      p_tenant_id, p_edge_id, true, p_episode_id, p_version_id, p_side, p_bound_at, p_precision) s
  $$;

-- Whether an As-of read at `p_valid_at` marks a Version `uncertain`: the
-- instant falls inside the period of an imprecise start bound (ADR-0008).
create function gnomon.uncertain_at(
  p_valid_at timestamptz,
  p_valid_from timestamptz,
  p_valid_from_precision gnomon.time_precision
) returns boolean
  language sql immutable parallel safe security invoker set search_path = ''
  as $$
    select p_valid_from_precision in ('year', 'month', 'day')
      and p_valid_at < gnomon.period_end(p_valid_from, p_valid_from_precision)
  $$;

-- Reads an Edge As-of a valid-at and a known-at instant, as `read_node`
-- reads a Node, with its Kind, its endpoints and its Sentence, or a null
-- Sentence until one is stored. A null known-at reads the current belief.
-- The read follows the rule of `traverse`: an Edge with an endpoint the
-- caller cannot see reads as no Edge, so a read never names a Node outside
-- the caller's Scopes.
create function gnomon.read_edge(
  p_tenant_id uuid,
  p_edge_id uuid,
  p_valid_at timestamptz,
  p_known_at timestamptz
) returns table (
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
    sentence text
  )
  language sql stable security invoker set search_path = ''
  as $$
    select v.id, v.edge_id, v.scope, e.kind, e.source_id, e.target_id, v.props,
           v.valid_from, v.valid_from_precision, v.valid_to, v.valid_to_precision,
           v.recorded_from, v.recorded_to, v.merge_rule,
           gnomon.uncertain_at(p_valid_at, v.valid_from, v.valid_from_precision),
           v.text_hash, s.sentence
    from gnomon.version v
    join gnomon.edge e on e.tenant_id = v.tenant_id and e.id = v.edge_id
    join gnomon.node src on src.tenant_id = e.tenant_id and src.id = e.source_id
    join gnomon.node tgt on tgt.tenant_id = e.tenant_id and tgt.id = e.target_id
    left join gnomon.sentence s
      on s.tenant_id = v.tenant_id and s.scope = v.scope and s.version_id = v.id
    where v.tenant_id = p_tenant_id and v.edge_id = p_edge_id
      and v.valid_from <= p_valid_at and v.valid_to > p_valid_at
      and case
            when p_known_at is null then v.recorded_to = 'infinity'
            else v.recorded_from <= p_known_at and v.recorded_to > p_known_at
          end
  $$;

-- Reads many Nodes As-of one valid-at and one known-at instant, in one
-- call: for each Node in `p_node_ids` that the caller can see and that held
-- a Version then, that Version, as `read_node` returns it. The rows come in
-- the order the Nodes were given, each Node once.
create function gnomon.read_nodes(
  p_tenant_id uuid,
  p_node_ids uuid[],
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
           gnomon.uncertain_at(p_valid_at, v.valid_from, v.valid_from_precision),
           v.text_hash
    from (
      select x.node_id, min(x.ord) as ord
      from unnest(p_node_ids) with ordinality as x(node_id, ord)
      group by x.node_id
    ) w
    join gnomon.node n on n.tenant_id = p_tenant_id and n.id = w.node_id
    join gnomon.version v on v.tenant_id = n.tenant_id and v.scope = n.scope and v.node_id = n.id
    where v.valid_from <= p_valid_at and v.valid_to > p_valid_at
      and case
            when p_known_at is null then v.recorded_to = 'infinity'
            else v.recorded_from <= p_known_at and v.recorded_to > p_known_at
          end
    order by w.ord
  $$;

-- Traverses the Edges of a Node As-of a valid-at and a known-at instant.
-- Each row is one Edge that joins the Node and held a Version then. It holds
-- that Version, with its Sentence or a null one, and the direction the Edge
-- was followed in. It also holds the neighbouring Node and that Node's
-- Version then. The neighbour's columns are null where it held no Version,
-- as a bare identity holds none: temporal referential integrity is not
-- enforced (ADR-0008).
--
--   p_direction  `out` follows Edges from the Node, `in` Edges to it, and
--                `both` either. An Edge from the Node to itself is returned
--                once, as `out`.
--   p_kinds      only Edges of these Edge Kinds or of Kinds below them. Null
--                or empty follows every Edge.
--
-- The As-of conditions are written out in each read, not called as a
-- function, so that they stay index conditions (ADR-0004).
--
-- Only Edges and Nodes the caller can see are returned: an Edge whose
-- neighbour is in a Scope the caller cannot see is left out, and a Node the
-- caller cannot see has no Edges to traverse. The rows come in Edge ID
-- order, `out` before `in`.
create function gnomon.traverse(
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
        select 'out'::text as direction, e.tenant_id, e.scope, e.id, e.kind,
               e.source_id, e.target_id, e.target_id as neighbour
        from origin o join gnomon.edge e on e.tenant_id = o.tenant_id and e.source_id = o.id
        where p_direction in ('out', 'both')
        union all
        select 'in', e.tenant_id, e.scope, e.id, e.kind, e.source_id, e.target_id, e.source_id
        from origin o join gnomon.edge e on e.tenant_id = o.tenant_id and e.target_id = o.id
        where p_direction in ('in', 'both')
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
      'gnomon.kind_check_reserved()'::regprocedure,
      'gnomon.edge_builtin_rules()'::regprocedure,
      'gnomon.check_edge_value(uuid, text, text, jsonb)'::regprocedure,
      'gnomon.kind_lineage(uuid, text)'::regprocedure,
      'gnomon.edge_check_insert()'::regprocedure,
      'gnomon.sentence_check_insert()'::regprocedure,
      'gnomon.lock_for_write(uuid, uuid, boolean)'::regprocedure,
      'gnomon.contribution_kind(uuid, uuid, uuid)'::regprocedure,
      'gnomon.recompute_versions(uuid, uuid, boolean, timestamptz, timestamptz)'::regprocedure,
      'gnomon.write_patch(uuid, uuid, boolean, uuid, timestamptz, text, timestamptz, text, jsonb, jsonb)'::regprocedure,
      'gnomon.create_edge(uuid, text, text, uuid, uuid)'::regprocedure,
      'gnomon.write_edge(uuid, uuid, uuid, timestamptz, text, timestamptz, text, jsonb, jsonb)'::regprocedure,
      'gnomon.state_bound_of(uuid, uuid, boolean, uuid, uuid, text, timestamptz, text)'::regprocedure,
      'gnomon.state_edge_bound(uuid, uuid, uuid, uuid, text, timestamptz, text)'::regprocedure,
      'gnomon.uncertain_at(timestamptz, timestamptz, gnomon.time_precision)'::regprocedure,
      'gnomon.read_edge(uuid, uuid, timestamptz, timestamptz)'::regprocedure,
      'gnomon.read_nodes(uuid, uuid[], timestamptz, timestamptz)'::regprocedure,
      'gnomon.traverse(uuid, uuid, timestamptz, timestamptz, text, text[])'::regprocedure
    ) and a.grantee <> p.proowner
    union
    select format('table %s', c.oid::regclass), a.grantee
    from pg_class c, aclexplode(c.relacl) a
    where c.oid in ('gnomon.edge'::regclass, 'gnomon.sentence'::regclass)
      and a.grantee <> c.relowner
  loop
    execute format(
      'revoke all on %s from %s', g.object,
      case when g.grantee = 0 then 'public' else g.grantee::regrole::text end);
  end loop;
  revoke all on function gnomon.kind_check_reserved() from public;
  revoke all on function gnomon.edge_builtin_rules() from public;
  revoke all on function gnomon.check_edge_value(uuid, text, text, jsonb) from public;
  revoke all on function gnomon.kind_lineage(uuid, text) from public;
  revoke all on function gnomon.edge_check_insert() from public;
  revoke all on function gnomon.sentence_check_insert() from public;
  revoke all on function gnomon.lock_for_write(uuid, uuid, boolean) from public;
  revoke all on function gnomon.contribution_kind(uuid, uuid, uuid) from public;
  revoke all on function gnomon.recompute_versions(uuid, uuid, boolean, timestamptz, timestamptz)
    from public;
  revoke all on function gnomon.write_patch(
    uuid, uuid, boolean, uuid, timestamptz, text, timestamptz, text, jsonb, jsonb) from public;
  revoke all on function gnomon.create_edge(uuid, text, text, uuid, uuid) from public;
  revoke all on function gnomon.write_edge(
    uuid, uuid, uuid, timestamptz, text, timestamptz, text, jsonb, jsonb) from public;
  revoke all on function gnomon.state_bound_of(
    uuid, uuid, boolean, uuid, uuid, text, timestamptz, text) from public;
  revoke all on function gnomon.state_edge_bound(uuid, uuid, uuid, uuid, text, timestamptz, text)
    from public;
  revoke all on function gnomon.uncertain_at(timestamptz, timestamptz, gnomon.time_precision)
    from public;
  revoke all on function gnomon.read_edge(uuid, uuid, timestamptz, timestamptz) from public;
  revoke all on function gnomon.read_nodes(uuid, uuid[], timestamptz, timestamptz) from public;
  revoke all on function gnomon.traverse(uuid, uuid, timestamptz, timestamptz, text, text[])
    from public;
end
$$;

-- Callers select Edges and insert only the columns they may choose, never a
-- creation time. They select Sentences and write none: ticket #26 adds the
-- function that stores one. On Contributions, Versions and bound statements
-- the one new column a caller may insert is `edge_id`. No caller gets a new
-- UPDATE or DELETE right: the guard triggers of migration 0004 cover Edge
-- Versions and their Citations as they cover a Node's.
grant select on gnomon.edge, gnomon.sentence to authenticated, gnomon_worker;
grant insert (tenant_id, scope, id, kind, source_id, target_id) on gnomon.edge
  to authenticated, gnomon_worker;
grant insert (edge_id) on gnomon.contribution, gnomon.version, gnomon.bound_statement
  to authenticated, gnomon_worker;

-- The functions run as the invoker, so the callers need every function they
-- call.
grant execute on function gnomon.edge_builtin_rules() to authenticated, gnomon_worker;
grant execute on function gnomon.check_edge_value(uuid, text, text, jsonb)
  to authenticated, gnomon_worker;
grant execute on function gnomon.kind_lineage(uuid, text) to authenticated, gnomon_worker;
grant execute on function gnomon.lock_for_write(uuid, uuid, boolean) to authenticated, gnomon_worker;
grant execute on function gnomon.contribution_kind(uuid, uuid, uuid) to authenticated, gnomon_worker;
grant execute on function gnomon.recompute_versions(uuid, uuid, boolean, timestamptz, timestamptz)
  to authenticated, gnomon_worker;
grant execute on function gnomon.write_patch(
  uuid, uuid, boolean, uuid, timestamptz, text, timestamptz, text, jsonb, jsonb)
  to authenticated, gnomon_worker;
grant execute on function gnomon.create_edge(uuid, text, text, uuid, uuid)
  to authenticated, gnomon_worker;
grant execute on function gnomon.write_edge(
  uuid, uuid, uuid, timestamptz, text, timestamptz, text, jsonb, jsonb)
  to authenticated, gnomon_worker;
grant execute on function gnomon.state_bound_of(
  uuid, uuid, boolean, uuid, uuid, text, timestamptz, text)
  to authenticated, gnomon_worker;
grant execute on function gnomon.state_edge_bound(uuid, uuid, uuid, uuid, text, timestamptz, text)
  to authenticated, gnomon_worker;
grant execute on function gnomon.uncertain_at(timestamptz, timestamptz, gnomon.time_precision)
  to authenticated, gnomon_worker;
grant execute on function gnomon.read_edge(uuid, uuid, timestamptz, timestamptz)
  to authenticated, gnomon_worker;
grant execute on function gnomon.read_nodes(uuid, uuid[], timestamptz, timestamptz)
  to authenticated, gnomon_worker;
grant execute on function gnomon.traverse(uuid, uuid, timestamptz, timestamptz, text, text[])
  to authenticated, gnomon_worker;

insert into gnomon.schema_version (version, name) values (8, '0008_edges');
