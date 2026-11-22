-- gnomon migration 0004: Nodes, Contributions, Versions and Citations, the
-- write that turns a patch of property values into Versions, and the As-of
-- read.
--
-- A write states, for one Node and citing one Episode, a value for each of
-- some properties over one Valid time. Each value is stored as a
-- Contribution, and the Node's Versions are computed from its Contributions
-- (ADR-0007). This migration has the default merge order only: Reference
-- time, then Recorded time. Ranked merge is a later migration.
--
-- Every Version is bi-temporal. Valid time and Recorded time are four scalar
-- timestamptz columns with B-tree indexes (ADR-0004), both half-open,
-- `[from, to)`, with `infinity` for an open bound. Each Valid-time bound
-- carries a precision (ADR-0008). This migration writes only `instant` and
-- `unknown`, and an unknown bound is always the open one.
--
-- Exclusion constraints keep a Node's Versions from overlapping in both
-- times at once. They do not use btree_gist, which is outside the floor of
-- PostgreSQL 17 and pgvector (ADR-0006). Equality of the Tenant and of the
-- Node is written instead as overlap of one-point ranges of a range type over
-- uuid, `gnomon.uuid_range`, which the core GiST range operator class
-- indexes. Two one-point ranges overlap exactly when their points are equal.
--
-- Callers write these tables as the invoker, under RLS (ADR-0009). They get
-- SELECT and INSERT, with INSERT limited to the columns a caller may choose.
-- On Versions they also get UPDATE of the Recorded-time end column alone, and
-- on Versions and Citations a DELETE that a guard trigger refuses for every
-- row the current transaction did not write. Each Version and Citation
-- records the transaction that wrote it in `written_in`, which an insert
-- default sets and no caller may supply or change. Only Erasure, a definer
-- function in a later migration, deletes committed history.

-- Every Version and Contribution has a precision on each Valid-time bound
-- (ADR-0008). The coarsest is `unknown`, and an unknown bound is stored as
-- -infinity at the start and infinity at the end.
create type gnomon.time_precision as enum ('unknown', 'year', 'month', 'day', 'instant');

-- The rule that chose a Version's values (ADR-0007). `recency` orders by
-- Reference time, then Recorded time.
create type gnomon.merge_rule as enum ('recency');

-- A range of uuid, so that the exclusion constraints can test equality with
-- the core GiST range operators. Postgres also creates its multirange type and
-- the constructor functions of both, which are internal functions.
create type gnomon.uuid_range as range (subtype = uuid);

-- An Episode is cited only from its own Scope, so later foreign keys name the
-- Scope with the Tenant and the ID.
alter table gnomon.episode add constraint episode_tenant_scope_id unique (tenant_id, scope, id);

-- A Node: a stable identity of a Kind, in one Scope. What is known about it
-- is held in its Versions.
create table gnomon.node (
  tenant_id uuid not null,
  scope text not null,
  id uuid not null default gnomon.uuid_v7(),
  -- The root Kind `node` or a registered Node Kind of the Tenant. Root Kinds
  -- are not rows, so a trigger checks this, not a foreign key.
  kind text not null,
  created_at timestamptz not null default transaction_timestamp(),
  primary key (tenant_id, id),
  unique (tenant_id, scope, id)
);
create index node_tenant_scope on gnomon.node (tenant_id, scope, kind);

-- A Contribution: one Episode's value for one property of a Node over a Valid
-- time. A JSON null ends the property over that Valid time. Contributions are
-- never updated, and only Erasure deletes them.
create table gnomon.contribution (
  tenant_id uuid not null,
  scope text not null,
  -- The order Contributions were recorded in, which breaks a tie between two
  -- with the same Reference time and Recorded time: the later one wins.
  id bigint generated always as identity,
  node_id uuid not null,
  episode_id uuid not null,
  property text not null,
  value jsonb not null,
  -- Where in the Episode the value was stated, in the consumer's own terms.
  place text,
  valid_from timestamptz not null,
  valid_from_precision gnomon.time_precision not null,
  valid_to timestamptz not null,
  valid_to_precision gnomon.time_precision not null,
  recorded_at timestamptz not null default transaction_timestamp(),
  primary key (tenant_id, id),
  foreign key (tenant_id, scope, node_id) references gnomon.node (tenant_id, scope, id),
  foreign key (tenant_id, scope, episode_id) references gnomon.episode (tenant_id, scope, id),
  check (valid_from < valid_to),
  check ((valid_from = '-infinity') = (valid_from_precision = 'unknown')),
  check ((valid_to = 'infinity') = (valid_to_precision = 'unknown'))
);
create index contribution_node on gnomon.contribution (tenant_id, node_id, valid_from);
create index contribution_episode on gnomon.contribution (tenant_id, episode_id);

-- A Version: one state of a Node, its values over a Valid time, held as the
-- current belief over a Recorded time.
create table gnomon.version (
  tenant_id uuid not null,
  scope text not null,
  id uuid not null default gnomon.uuid_v7(),
  node_id uuid not null,
  -- The property values, {property: value}, with no nulls.
  props jsonb not null,
  valid_from timestamptz not null,
  valid_from_precision gnomon.time_precision not null,
  valid_to timestamptz not null,
  valid_to_precision gnomon.time_precision not null,
  -- Recorded time is assigned only by gnomon: it starts at the transaction
  -- time of the write that opened the Version and ends at the transaction
  -- time of the write that closed it.
  recorded_from timestamptz not null default transaction_timestamp(),
  recorded_to timestamptz not null default 'infinity',
  merge_rule gnomon.merge_rule not null,
  -- The transaction that wrote the row, and the one that closed it. Only
  -- that transaction may delete the row, or reopen it.
  written_in xid8 not null default pg_current_xact_id(),
  closed_in xid8,
  primary key (tenant_id, id),
  unique (tenant_id, scope, id),
  foreign key (tenant_id, scope, node_id) references gnomon.node (tenant_id, scope, id),
  check (jsonb_typeof(props) = 'object'),
  check (valid_from < valid_to),
  check (recorded_from < recorded_to),
  check ((valid_from = '-infinity') = (valid_from_precision = 'unknown')),
  check ((valid_to = 'infinity') = (valid_to_precision = 'unknown')),
  check ((recorded_to = 'infinity') = (closed_in is null)),
  constraint version_no_overlap exclude using gist (
    gnomon.uuid_range(tenant_id, tenant_id, '[]') with &&,
    gnomon.uuid_range(node_id, node_id, '[]') with &&,
    tstzrange(valid_from, valid_to) with &&,
    tstzrange(recorded_from, recorded_to) with &&
  )
);
-- As-of reads: the Node, then the four time columns (ADR-0004).
create index version_as_of on gnomon.version
  (tenant_id, node_id, valid_from, recorded_from) include (valid_to, recorded_to);
-- The write path reads only the open Versions.
create index version_open on gnomon.version (tenant_id, node_id, valid_from)
  where recorded_to = 'infinity';
create index version_tenant_scope on gnomon.version (tenant_id, scope, node_id);

-- A Citation: the link from a Version to an Episode it was derived from,
-- naming the property and, if the write gave one, the place.
create table gnomon.citation (
  tenant_id uuid not null,
  scope text not null,
  version_id uuid not null,
  episode_id uuid not null,
  property text not null,
  place text,
  -- When the Citation was recorded, the transaction time of its write, and
  -- the Episode's own Recorded time, which the insert trigger copies.
  recorded_at timestamptz not null default transaction_timestamp(),
  episode_recorded_at timestamptz not null,
  written_in xid8 not null default pg_current_xact_id(),
  foreign key (tenant_id, scope, version_id) references gnomon.version (tenant_id, scope, id),
  foreign key (tenant_id, scope, episode_id) references gnomon.episode (tenant_id, scope, id),
  constraint citation_once unique nulls not distinct
    (tenant_id, version_id, episode_id, property, place)
);
create index citation_episode on gnomon.citation (tenant_id, episode_id);

-- RLS: the Tenant and the Scopes of the call, exactly as on Episodes
-- (ADR-0005).
alter table gnomon.node enable row level security;
alter table gnomon.contribution enable row level security;
alter table gnomon.version enable row level security;
alter table gnomon.citation enable row level security;

create policy node_tenant_scope on gnomon.node
  for all to authenticated, gnomon_worker
  using (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()))
  with check (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()));
create policy contribution_tenant_scope on gnomon.contribution
  for all to authenticated, gnomon_worker
  using (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()))
  with check (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()));
create policy version_tenant_scope on gnomon.version
  for all to authenticated, gnomon_worker
  using (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()))
  with check (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()));
create policy citation_tenant_scope on gnomon.citation
  for all to authenticated, gnomon_worker
  using (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()))
  with check (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()));

-- The SQLSTATE a refused write raises: GNW01, an invalid write.

-- Checks a new Node's Kind: the root Kind `node`, or a Node Kind the Tenant
-- registered.
create function gnomon.node_check_kind() returns trigger
  language plpgsql security invoker set search_path = ''
  as $$
  begin
    if new.kind = 'node' then
      return new;
    end if;
    if not exists (
      select from gnomon.kind k
      where k.tenant_id = new.tenant_id and k.name = new.kind and k.kind_of = 'node'
    ) then
      raise exception using errcode = 'GNW01',
        message = format('"%s" is not a Node Kind of the Tenant', new.kind);
    end if;
    return new;
  end
  $$;
create trigger node_check_kind before insert on gnomon.node
  for each row execute function gnomon.node_check_kind();

-- Recorded time is the transaction time and nothing else. The defaults set
-- it, callers cannot insert those columns, and these triggers refuse any
-- other value from a role that can.
create function gnomon.contribution_check_insert() returns trigger
  language plpgsql security invoker set search_path = ''
  as $$
  begin
    if new.recorded_at is distinct from transaction_timestamp() then
      raise exception using errcode = 'insufficient_privilege',
        message = 'a Contribution''s Recorded time is the transaction time of its write';
    end if;
    return new;
  end
  $$;
create trigger contribution_check_insert before insert on gnomon.contribution
  for each row execute function gnomon.contribution_check_insert();

create function gnomon.version_check_insert() returns trigger
  language plpgsql security invoker set search_path = ''
  as $$
  begin
    if new.recorded_from is distinct from transaction_timestamp()
      or new.recorded_to is distinct from 'infinity'
    then
      raise exception using errcode = 'insufficient_privilege',
        message = 'a new Version''s Recorded time starts at the transaction time and is open';
    end if;
    if new.written_in is distinct from pg_current_xact_id() or new.closed_in is not null then
      raise exception using errcode = 'insufficient_privilege',
        message = 'a new Version is marked as written by the current transaction';
    end if;
    return new;
  end
  $$;
create trigger version_check_insert before insert on gnomon.version
  for each row execute function gnomon.version_check_insert();

-- The guard on Versions (ADR-0009). Nothing but the Recorded-time end
-- changes, and it changes in one of two ways only:
--
--   closing  an open Version that an earlier transaction opened, at the
--            transaction time, which marks it closed by this transaction;
--   reopening  a Version this transaction closed, which a second write to
--            the same Node in the same transaction does when it restores
--            the Version in place.
create function gnomon.version_guard_update() returns trigger
  language plpgsql security invoker set search_path = ''
  as $$
  begin
    if (new.tenant_id, new.scope, new.id, new.node_id, new.props, new.valid_from,
        new.valid_from_precision, new.valid_to, new.valid_to_precision, new.recorded_from,
        new.merge_rule, new.written_in, new.closed_in)
      is distinct from
       (old.tenant_id, old.scope, old.id, old.node_id, old.props, old.valid_from,
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
      message = 'a Version''s Recorded time may only be closed at the transaction time, '
        'or reopened by the transaction that closed it';
  end
  $$;
create trigger version_guard_update before update on gnomon.version
  for each row execute function gnomon.version_guard_update();

-- Versions and Citations are deleted only by the transaction that wrote
-- them, which is how a second write to a Node in one transaction replaces
-- the first in place (ADR-0008). Committed history is deleted only by
-- Erasure.
create function gnomon.guard_delete() returns trigger
  language plpgsql security invoker set search_path = ''
  as $$
  begin
    if old.written_in is distinct from pg_current_xact_id() then
      raise exception using errcode = 'insufficient_privilege',
        message = format(
          'a %s may be deleted only by the transaction that wrote it', tg_argv[0]);
    end if;
    return old;
  end
  $$;
create trigger version_guard_delete before delete on gnomon.version
  for each row execute function gnomon.guard_delete('Version');
create trigger citation_guard_delete before delete on gnomon.citation
  for each row execute function gnomon.guard_delete('Citation');

-- A Citation carries its Episode's Recorded time, copied here, and is
-- recorded at the transaction time.
create function gnomon.citation_check_insert() returns trigger
  language plpgsql security invoker set search_path = ''
  as $$
  begin
    if new.recorded_at is distinct from transaction_timestamp()
      or new.written_in is distinct from pg_current_xact_id()
    then
      raise exception using errcode = 'insufficient_privilege',
        message = 'a Citation is recorded at the transaction time of its write';
    end if;
    select e.recorded_at into new.episode_recorded_at
      from gnomon.episode e
      where e.tenant_id = new.tenant_id and e.scope = new.scope and e.id = new.episode_id;
    return new;
  end
  $$;
create trigger citation_check_insert before insert on gnomon.citation
  for each row execute function gnomon.citation_check_insert();

-- The properties every Node has whatever its Kind, and their rules: a name,
-- and Aliases, the other names it is known by (ADR-0002).
create function gnomon.node_builtin_rules()
  returns table (property text, rule jsonpath)
  language sql immutable parallel safe security invoker set search_path = ''
  as $$
    values
      ('name', 'strict $.type() == "string"'::jsonpath),
      ('aliases', 'strict $.type() == "array" && !exists($[*] ? (@.type() != "string"))'::jsonpath)
  $$;

-- The key of a Node's advisory lock. Writes to a Node, and Erasure, take it
-- for the rest of the transaction, so they serialise on each Node (ADR-0007).
create function gnomon.node_lock_key(p_tenant_id uuid, p_node_id uuid)
  returns bigint
  language sql immutable parallel safe security invoker set search_path = ''
  as $$ select pg_catalog.hashtextextended(p_tenant_id::text || '/' || p_node_id::text, 0) $$;

-- Recomputes a Node's current Versions over a Valid-time window from all of
-- its Contributions, at the transaction time. This is the step of the
-- `prototype/recompute-cost` branch (ADR-0007):
--
--   1. Widen the window to the whole of every open Version that overlaps or
--      touches it, so that equal neighbouring Versions merge as they would
--      in a full recompute.
--   2. Cut the window into segments at every Contribution bound, and pick
--      the winner of each property in each segment: the latest Reference
--      time, then the latest Recorded time, then the Contribution recorded
--      last.
--   3. Drop the null winners, which end a property, and merge equal
--      neighbouring segments into the new Versions.
--   4. Each open Version that no new Version matches is closed at the
--      transaction time. If this transaction wrote it, it is deleted
--      instead, with its Citations, so no empty Recorded time is left.
--   5. Each new Version that a Version this transaction closed matches is
--      reopened, and every other is inserted.
--   6. Every Contribution that overlaps a new Version and states the value
--      it holds is cited from it, once.
--
-- Returns what changed, and the IDs of the Versions the step opened.
create function gnomon.recompute_node(
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
    v_kind text;
    v_required jsonb;
    w0 timestamptz;
    w1 timestamptz;
    candidates jsonb;
    missing record;
    n integer;
  begin
    perform pg_catalog.pg_advisory_xact_lock(gnomon.node_lock_key(p_tenant_id, p_node_id));
    select n.scope, n.kind into v_scope, v_kind
      from gnomon.node n where n.tenant_id = p_tenant_id and n.id = p_node_id;
    if not found then
      raise exception using errcode = 'GNW01',
        message = format('Node %s is not visible to the caller', p_node_id);
    end if;
    select coalesce(k.properties -> 'required', '[]') into v_required
      from gnomon.kind k where k.tenant_id = p_tenant_id and k.name = v_kind;
    v_required := coalesce(v_required, '[]');

    -- 1. The window.
    select least(p_from, min(v.valid_from)), greatest(p_to, max(v.valid_to)) into w0, w1
      from gnomon.version v
      where v.tenant_id = p_tenant_id and v.node_id = p_node_id
        and v.recorded_to = 'infinity'
        and v.valid_from <= p_to and v.valid_to >= p_from;

    -- 2 and 3. The new Versions over the window, each with its Citations.
    with c as (
      select k.id, k.episode_id, k.property, k.value, k.place,
             greatest(k.valid_from, w0) as vf, least(k.valid_to, w1) as vt,
             e.reference_time as ref, k.recorded_at as rec
      from gnomon.contribution k
      join gnomon.episode e
        on e.tenant_id = k.tenant_id and e.scope = k.scope and e.id = k.episode_id
      where k.tenant_id = p_tenant_id and k.node_id = p_node_id
        and k.valid_from < w1 and k.valid_to > w0
    ), pts as (
      select vf as p from c union select vt from c union select w0 union select w1
    ), segs as (
      select p as sf, lead(p) over (order by p) as st from pts
    ), win as (
      select distinct on (s.sf, c.property) s.sf, s.st, c.property, c.value
      from segs s join c on c.vf <= s.sf and c.vt >= s.st
      where s.st is not null
      order by s.sf, c.property, c.ref desc, c.rec desc, c.id desc
    ), cit as (
      select w.sf, w.property, c.episode_id, c.place
      from win w
      join c on c.property = w.property and c.value = w.value and c.vf <= w.sf and c.vt >= w.st
      where w.value <> 'null'::jsonb
      group by w.sf, w.property, c.episode_id, c.place
    ), segp as (
      select sf, max(st) as st,
             jsonb_object_agg(property, value) filter (where value <> 'null'::jsonb) as props
      from win group by sf
    ), isl as (
      select sf, st, props,
             sum(case when lag_st = sf and lag_props = props then 0 else 1 end)
               over (order by sf) as grp
      from (
        select sf, st, props,
               lag(st) over (order by sf) as lag_st, lag(props) over (order by sf) as lag_props
        from segp where props is not null
      ) x
    ), vers as (
      select grp, min(sf) as vf, max(st) as vt, (array_agg(props))[1] as props
      from isl group by grp
    ), vc as (
      select i.grp,
             jsonb_agg(distinct jsonb_build_array(cit.property, cit.episode_id, cit.place)) as cites
      from isl i join cit on cit.sf = i.sf
      group by i.grp
    )
    select coalesce(jsonb_agg(jsonb_build_object(
             'vf', v.vf, 'vt', v.vt, 'props', v.props, 'cites', coalesce(vc.cites, '[]'::jsonb))),
           '[]'::jsonb)
      into candidates
      from vers v left join vc using (grp);

    -- Every new Version holds the Kind's required properties.
    select n.vf, n.vt, r.property into missing
      from jsonb_to_recordset(candidates) n(vf timestamptz, vt timestamptz, props jsonb)
      cross join lateral jsonb_array_elements_text(v_required) r(property)
      where not n.props ? r.property
      limit 1;
    if found then
      raise exception using errcode = 'GNW01',
        message = format(
          'the write leaves required property "%s" of Kind "%s" without a value over [%s, %s)',
          missing.property, v_kind, missing.vf, missing.vt);
    end if;

    changed := false;

    -- 4. Close, or delete, the open Versions no new Version matches. A
    -- Citation this transaction added to a Version it then closes is
    -- removed too, so the Version keeps only the Citations it held.
    delete from gnomon.citation x
      using gnomon.version v
      where x.tenant_id = p_tenant_id and x.version_id = v.id and x.written_in = me
        and v.tenant_id = p_tenant_id and v.node_id = p_node_id
        and v.recorded_to = 'infinity'
        and v.valid_from >= w0 and v.valid_to <= w1
        and not exists (
          select from jsonb_to_recordset(candidates) n(vf timestamptz, vt timestamptz, props jsonb)
          where n.vf = v.valid_from and n.vt = v.valid_to and n.props = v.props
        );
    delete from gnomon.version v
      where v.tenant_id = p_tenant_id and v.node_id = p_node_id
        and v.recorded_to = 'infinity' and v.written_in = me
        and v.valid_from >= w0 and v.valid_to <= w1
        and not exists (
          select from jsonb_to_recordset(candidates) n(vf timestamptz, vt timestamptz, props jsonb)
          where n.vf = v.valid_from and n.vt = v.valid_to and n.props = v.props
        );
    get diagnostics n = row_count;
    changed := changed or n > 0;
    update gnomon.version v set recorded_to = t
      where v.tenant_id = p_tenant_id and v.node_id = p_node_id
        and v.recorded_to = 'infinity'
        and v.valid_from >= w0 and v.valid_to <= w1
        and not exists (
          select from jsonb_to_recordset(candidates) n(vf timestamptz, vt timestamptz, props jsonb)
          where n.vf = v.valid_from and n.vt = v.valid_to and n.props = v.props
        );
    get diagnostics n = row_count;
    changed := changed or n > 0;

    -- 5. Reopen what this transaction closed and now restores, and insert
    -- the rest.
    with reopened as (
      update gnomon.version v set recorded_to = 'infinity'
        from jsonb_to_recordset(candidates) n(vf timestamptz, vt timestamptz, props jsonb)
        where v.tenant_id = p_tenant_id and v.node_id = p_node_id
          and v.closed_in = me and v.recorded_to = t
          and v.valid_from = n.vf and v.valid_to = n.vt and v.props = n.props
        returning v.id
    ), inserted as (
      insert into gnomon.version (
        tenant_id, scope, node_id, props, valid_from, valid_from_precision,
        valid_to, valid_to_precision, merge_rule
      )
      select p_tenant_id, v_scope, p_node_id, n.props,
             n.vf, case when n.vf = '-infinity' then 'unknown' else 'instant' end::gnomon.time_precision,
             n.vt, case when n.vt = 'infinity' then 'unknown' else 'instant' end::gnomon.time_precision,
             'recency'
      from jsonb_to_recordset(candidates) n(vf timestamptz, vt timestamptz, props jsonb)
      where not exists (
        select from gnomon.version v
        where v.tenant_id = p_tenant_id and v.node_id = p_node_id
          and v.valid_from = n.vf and v.valid_to = n.vt and v.props = n.props
          and (v.recorded_to = 'infinity' or (v.closed_in = me and v.recorded_to = t))
      )
      returning id
    )
    select coalesce(array_agg(id), '{}') into opened
      from (select id from reopened union all select id from inserted) o;
    changed := changed or cardinality(opened) > 0;

    -- 6. The Citations.
    insert into gnomon.citation (tenant_id, scope, version_id, episode_id, property, place)
      select p_tenant_id, v_scope, v.id, (cc ->> 1)::uuid, cc ->> 0, cc ->> 2
      from jsonb_to_recordset(candidates) n(vf timestamptz, vt timestamptz, cites jsonb)
      join gnomon.version v
        on v.tenant_id = p_tenant_id and v.node_id = p_node_id and v.recorded_to = 'infinity'
       and v.valid_from = n.vf and v.valid_to = n.vt
      cross join lateral jsonb_array_elements(n.cites) cc
      on conflict do nothing;
  end
  $$;

-- Creates a Node of a Kind in a Scope and returns its ID, a UUID v7. The Node
-- holds no Version until a write states a value for it.
create function gnomon.create_node(p_tenant_id uuid, p_scope text, p_kind text)
  returns uuid
  language sql volatile security invoker set search_path = ''
  as $$
    insert into gnomon.node (tenant_id, scope, kind) values (p_tenant_id, p_scope, p_kind)
    returning id
  $$;

-- Writes a patch of property values for a Node, citing one Episode, over one
-- Valid time.
--
--   p_values      {property: value}; a JSON null ends the property over the
--                 Valid time. At least one property.
--   p_places      {property: place} for the properties whose place in the
--                 Episode the caller knows, or null.
--   p_valid_from  the start of the Valid time, or null for an unknown start
--   p_valid_to    the end, or null for an open end
--
-- Each value is checked against the Node Kind's compiled rule for its
-- property (ADR-0002); a null value is not checked. Each value becomes a
-- Contribution, then the Node's Versions are recomputed over the Valid time.
--
-- Returns `unchanged` when the Node's Versions are as they were, and
-- `written` otherwise, with the transaction time and the IDs of the Versions
-- the write opened. An unchanged write still cites the Episode from every
-- Version whose value it states.
create function gnomon.write_node(
  p_tenant_id uuid,
  p_node_id uuid,
  p_episode_id uuid,
  p_valid_from timestamptz,
  p_valid_to timestamptz,
  p_values jsonb,
  p_places jsonb
) returns table (status text, recorded_at timestamptz, versions uuid[])
  language plpgsql volatile security invoker set search_path = ''
  as $$
  declare
    t constant timestamptz := transaction_timestamp();
    me constant xid8 := pg_current_xact_id();
    vf timestamptz := coalesce(p_valid_from, '-infinity');
    vt timestamptz := coalesce(p_valid_to, 'infinity');
    v_scope text;
    v_kind text;
    k text;
    v jsonb;
    rule jsonpath;
    r record;
  begin
    if p_tenant_id is null or p_tenant_id is distinct from gnomon.tenant_id() then
      raise exception using errcode = 'insufficient_privilege',
        message = 'a write may write only in the caller''s own Tenant';
    end if;
    if p_episode_id is null then
      raise exception using errcode = 'GNW01',
        message = 'every write cites an Episode';
    end if;
    if p_valid_from in ('-infinity', 'infinity') or p_valid_to in ('-infinity', 'infinity') then
      raise exception using errcode = 'GNW01',
        message = 'a stated Valid-time bound is an instant; leave an unknown bound null';
    end if;
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

    perform pg_catalog.pg_advisory_xact_lock(gnomon.node_lock_key(p_tenant_id, p_node_id));

    select n.scope, n.kind into v_scope, v_kind
      from gnomon.node n where n.tenant_id = p_tenant_id and n.id = p_node_id;
    if not found then
      raise exception using errcode = 'GNW01',
        message = format('Node %s is not visible to the caller', p_node_id);
    end if;
    if not exists (
      select from gnomon.episode e
      where e.tenant_id = p_tenant_id and e.scope = v_scope and e.id = p_episode_id
    ) then
      raise exception using errcode = 'GNW01',
        message = format('Episode %s is not visible in the Node''s Scope', p_episode_id);
    end if;

    -- A transaction that started before another wrote this Node, but took
    -- the lock after it committed, would record a Version earlier than one
    -- already recorded. It fails, and the caller retries.
    if exists (
      select from gnomon.contribution c
      where c.tenant_id = p_tenant_id and c.node_id = p_node_id and c.recorded_at > t
    ) or exists (
      select from gnomon.version x
      where x.tenant_id = p_tenant_id and x.node_id = p_node_id
        and x.recorded_from = t and x.written_in <> me
    ) then
      raise exception using errcode = 'serialization_failure',
        message = format('a later transaction wrote Node %s first; retry the write', p_node_id);
    end if;

    -- Each value against its property's rule.
    for k, v in select e.key, e.value from jsonb_each(p_values) e order by e.key loop
      rule := coalesce(
        (select x.rule from gnomon.kind_rule x
         where x.tenant_id = p_tenant_id and x.kind = v_kind and x.property = k),
        (select b.rule from gnomon.node_builtin_rules() b where b.property = k)
      );
      if rule is null then
        raise exception using errcode = 'GNW01',
          message = format('Kind "%s" has no property "%s"', v_kind, k);
      end if;
      if v <> 'null'::jsonb and not jsonb_path_match(v, rule) then
        raise exception using errcode = 'GNW01',
          message = format(
            'the value %s of property "%s" does not satisfy Kind "%s"', v, k, v_kind);
      end if;
    end loop;

    insert into gnomon.contribution (
      tenant_id, scope, node_id, episode_id, property, value, place,
      valid_from, valid_from_precision, valid_to, valid_to_precision
    )
    select p_tenant_id, v_scope, p_node_id, p_episode_id, e.key, e.value, p_places ->> e.key,
           vf, case when vf = '-infinity' then 'unknown' else 'instant' end::gnomon.time_precision,
           vt, case when vt = 'infinity' then 'unknown' else 'instant' end::gnomon.time_precision
    from jsonb_each(p_values) e
    order by e.key;

    select * into r from gnomon.recompute_node(p_tenant_id, p_node_id, vf, vt);
    status := case when r.changed then 'written' else 'unchanged' end;
    recorded_at := t;
    versions := r.opened;
    return next;
  end
  $$;

-- Reads a Node As-of a valid-at and a known-at instant: the Version that
-- held at the valid-at instant according to what gnomon believed at the
-- known-at instant. A null known-at reads the current belief.
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
    merge_rule gnomon.merge_rule
  )
  language sql stable security invoker set search_path = ''
  as $$
    select v.id, v.node_id, v.scope, n.kind, v.props,
           v.valid_from, v.valid_from_precision, v.valid_to, v.valid_to_precision,
           v.recorded_from, v.recorded_to, v.merge_rule
    from gnomon.version v
    join gnomon.node n on n.tenant_id = v.tenant_id and n.id = v.node_id
    where v.tenant_id = p_tenant_id and v.node_id = p_node_id
      and v.valid_from <= p_valid_at and v.valid_to > p_valid_at
      and case
            when p_known_at is null then v.recorded_to = 'infinity'
            else v.recorded_from <= p_known_at and v.recorded_to > p_known_at
          end
  $$;

-- Lists the Citations of a Version: each Episode it was derived from, with
-- the property and the place.
create function gnomon.list_citations(p_tenant_id uuid, p_version_id uuid)
  returns table (
    episode_id uuid,
    property text,
    place text,
    recorded_at timestamptz,
    episode_recorded_at timestamptz
  )
  language sql stable security invoker set search_path = ''
  as $$
    select c.episode_id, c.property, c.place, c.recorded_at, c.episode_recorded_at
    from gnomon.citation c
    where c.tenant_id = p_tenant_id and c.version_id = p_version_id
    order by c.property, c.episode_recorded_at, c.episode_id, c.place
  $$;

-- Grants. First every grant a default privilege made on this migration's
-- objects is revoked, so each object holds only the grants below.
do $$
declare
  g record;
begin
  for g in
    select format('function %s', p.oid::regprocedure) as object, a.grantee
    from pg_proc p, aclexplode(p.proacl) a
    where p.pronamespace = 'gnomon'::regnamespace
      and p.oid not in (
        select p0.oid from pg_proc p0
        where p0.proname in (
          'embedding_dimension', 'uuid_v7', 'tenant_id', 'allowed_scopes',
          'record_episode', 'read_episode', 'kind_profile_keywords', 'json_pointer_token',
          'root_kinds', 'compile_kind_schema', 'compile_kind_property', 'kind_endpoints',
          'read_kind', 'register_kind'
        ) and p0.pronamespace = 'gnomon'::regnamespace
      )
      and a.grantee <> p.proowner
    union
    select format('table %s', c.oid::regclass), a.grantee
    from pg_class c, aclexplode(c.relacl) a
    where c.oid in (
      'gnomon.node'::regclass, 'gnomon.contribution'::regclass,
      'gnomon.version'::regclass, 'gnomon.citation'::regclass
    ) and a.grantee <> c.relowner
  loop
    execute format(
      'revoke all on %s from %s', g.object,
      case when g.grantee = 0 then 'public' else g.grantee::regrole::text end);
  end loop;
  -- A function is executable by PUBLIC unless revoked, with no ACL entry
  -- until then. That includes the range type's constructors.
  revoke all on function gnomon.uuid_range(uuid, uuid) from public;
  revoke all on function gnomon.uuid_range(uuid, uuid, text) from public;
  revoke all on function gnomon.uuid_multirange() from public;
  revoke all on function gnomon.uuid_multirange(gnomon.uuid_range) from public;
  revoke all on function gnomon.uuid_multirange(gnomon.uuid_range[]) from public;
  revoke all on function gnomon.node_check_kind() from public;
  revoke all on function gnomon.contribution_check_insert() from public;
  revoke all on function gnomon.version_check_insert() from public;
  revoke all on function gnomon.version_guard_update() from public;
  revoke all on function gnomon.guard_delete() from public;
  revoke all on function gnomon.citation_check_insert() from public;
  revoke all on function gnomon.node_builtin_rules() from public;
  revoke all on function gnomon.node_lock_key(uuid, uuid) from public;
  revoke all on function gnomon.recompute_node(uuid, uuid, timestamptz, timestamptz) from public;
  revoke all on function gnomon.create_node(uuid, text, text) from public;
  revoke all on function gnomon.write_node(uuid, uuid, uuid, timestamptz, timestamptz, jsonb, jsonb)
    from public;
  revoke all on function gnomon.read_node(uuid, uuid, timestamptz, timestamptz) from public;
  revoke all on function gnomon.list_citations(uuid, uuid) from public;
end
$$;

-- Callers select every column and insert only the columns they may choose:
-- never a Recorded time, a sequence number or a transaction marker. On
-- Versions they update only the Recorded-time end, and on Versions and
-- Citations they delete only what the guard allows.
grant select on gnomon.node, gnomon.contribution, gnomon.version, gnomon.citation
  to authenticated, gnomon_worker;
grant insert (tenant_id, scope, id, kind) on gnomon.node to authenticated, gnomon_worker;
grant insert (
  tenant_id, scope, node_id, episode_id, property, value, place,
  valid_from, valid_from_precision, valid_to, valid_to_precision
) on gnomon.contribution to authenticated, gnomon_worker;
grant insert (
  tenant_id, scope, id, node_id, props, valid_from, valid_from_precision,
  valid_to, valid_to_precision, merge_rule
) on gnomon.version to authenticated, gnomon_worker;
grant update (recorded_to) on gnomon.version to authenticated, gnomon_worker;
grant insert (tenant_id, scope, version_id, episode_id, property, place)
  on gnomon.citation to authenticated, gnomon_worker;
grant delete on gnomon.version, gnomon.citation to authenticated, gnomon_worker;

-- Inserting a Version evaluates the exclusion constraint's index
-- expressions as the caller, so the callers need the range constructor.
grant execute on function gnomon.uuid_range(uuid, uuid, text) to authenticated, gnomon_worker;
grant execute on function gnomon.node_builtin_rules() to authenticated, gnomon_worker;
grant execute on function gnomon.node_lock_key(uuid, uuid) to authenticated, gnomon_worker;
grant execute on function gnomon.recompute_node(uuid, uuid, timestamptz, timestamptz)
  to authenticated, gnomon_worker;
grant execute on function gnomon.create_node(uuid, text, text) to authenticated, gnomon_worker;
grant execute on function gnomon.write_node(uuid, uuid, uuid, timestamptz, timestamptz, jsonb, jsonb)
  to authenticated, gnomon_worker;
grant execute on function gnomon.read_node(uuid, uuid, timestamptz, timestamptz)
  to authenticated, gnomon_worker;
grant execute on function gnomon.list_citations(uuid, uuid) to authenticated, gnomon_worker;

insert into gnomon.schema_version (version, name) values (4, '0004_nodes_and_versions');
