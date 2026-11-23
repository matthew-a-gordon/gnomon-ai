-- gnomon migration 0018: Summaries and Communities.
--
-- The Pipeline writes a Summary of a Node, generated prose describing it,
-- and a Community of Nodes, a group of closely connected Nodes with a
-- generated Summary of the group (GLOSSARY.md). The Store writes and reads
-- them with their Citations under the same Tenant and Scope rules as every
-- other row. It makes no model calls: the text is the Pipeline's.
--
-- One Scope (ADR-0014). A Summary or Community belongs to one Scope and
-- cites only Episodes of that Scope. The foreign key from a Citation to its
-- Episode carries the Scope of the Summary or Community, so a Citation of an
-- Episode in another Scope is refused whoever writes it. A Summary describes
-- one Node the writer can see, in any of the writer's Scopes: a Node in Scope
-- A that Edges in Scope B join may have a Summary in Scope B too, built from
-- Scope B's Episodes, which is how a Node cited from two Scopes has one
-- Summary per Scope. A Community is detected over the Nodes and Edges of one
-- Scope, so every Node it holds is in its own Scope. The foreign key from a
-- member to its Node carries the Community's Scope, so a Node of another
-- Scope is refused, even one the writer can see.
--
-- Versioned by Recorded time (ADR-0015). Each row carries a Recorded time,
-- which only gnomon assigns, and `as_of_valid`, the valid-at instant the
-- Pipeline wrote it for. A read at `(v, k)` finds a row only when its
-- Recorded time contains `k` and `v >= as_of_valid`. Regenerating one closes
-- the old row at the start of the current transaction and adds a new one, so
-- nothing is deleted and a read at an earlier known-at instant still finds
-- the old row. A Node has at most one current Summary in each Scope. A new
-- Community names the current Communities it replaces, which it closes.
-- Replacing a row the current transaction wrote deletes it instead, as a
-- second write to a Node in one transaction replaces the first (ADR-0008).
--
-- Who writes (ADR-0009). Callers write through `gnomon.write_summary` and
-- `gnomon.write_community`, invokers under RLS with an empty search path. No
-- definer is added. A caller may also insert rows directly, as any SQL client
-- could, so the checks a write depends on are the tables' own: RLS, foreign
-- keys that carry the Scope, insert grants on the columns a caller may
-- choose, so that the ID, the Recorded time and the transaction mark always
-- take their defaults, and triggers that
--
--   * refuse a Summary of a Node the caller cannot see, and a Summary whose
--     Recorded time would overlap another of the same Node and Scope;
--   * refuse a Citation or a member added to a row an earlier transaction
--     wrote, so a row's Citations and members are fixed when it commits;
--   * refuse, at commit, a Summary with no Citation and a Community with no
--     Citation or no member;
--   * let a row's Recorded-time end change only by closing an open row at
--     the start of the current transaction;
--   * let a row be deleted only by the transaction that wrote it.
--
-- Both write functions take the advisory lock of every Node they describe or
-- hold (`gnomon.lock_for_write`), in order of ID, as an Erasure does, so a
-- write and an Erasure of the same Node serialise. A Citation's foreign key
-- takes a share lock on its Episode's row, which Erase Episode locks FOR
-- UPDATE, so a write citing an Episode and its Erasure serialise too.
--
-- Erasure (ADR-0003). Every Summary and Community that cites an erased
-- Episode goes, closed or open, with its Citations and members:
-- `gnomon.erase_episode_rows` now deletes them before the Episode. Erasing a
-- Node removes every Summary of the Node, whatever its Scope, and every
-- Community that holds it: `gnomon.erase_versioned_rows` now deletes them.
-- Erasing a Scope removes every Summary and Community in it, because each
-- cites at least one of the Scope's Episodes, and every Summary of one of its
-- Nodes in another Scope. Both go through `gnomon.erase_summaries_and_communities`,
-- the one place a later migration adds the rows it derives from a Summary's
-- or Community's text, such as Search's (ticket #20). A Retraction changes
-- neither table: a Citation of a retracted Episode still counts.
--
-- Reaching Summaries the caller cannot see. A Summary of a Node in Scope B
-- may describe a Node in Scope A. Erasing the Node, or Scope A, must remove
-- it, even when the caller cannot see Scope B, because the foreign key on the
-- Node would refuse the Node's delete while it stood. So the definer gets the
-- reach migration 0012 gives it over Edges (ADR-0009): it may read and delete
-- a Summary, in any Scope of the Tenant, of a Node its own Erasure listed in
-- `gnomon.erasure_reach`, and that Summary's Citations. No caller role holds
-- these policies, and a listed Node is visible only to the transaction that
-- listed it. Communities need no reach, because their Nodes are in their own
-- Scope, which the caller of the Erasure sees.
--
-- A Node that an Erasure would leave bare and delete stays as a bare identity
-- while a Summary of it, or a Community holding it, still names it, as it
-- does while an Edge still joins it. Only a Summary that cites no erased
-- Episode can name it then.
--
-- The vacuum call (migration 0013) vacuums all five tables, because the
-- definer may delete from them.

create table gnomon.summary (
  tenant_id uuid not null,
  scope text not null,
  id uuid not null default gnomon.uuid_v7(),
  node_id uuid not null,
  -- The generated prose.
  content text not null check (content <> ''),
  -- The valid-at instant the Pipeline wrote it for (ADR-0015).
  as_of_valid timestamptz not null check (isfinite(as_of_valid)),
  recorded_from timestamptz not null default transaction_timestamp(),
  recorded_to timestamptz not null default 'infinity',
  -- The transaction that wrote the row. Only that transaction may delete it.
  written_in xid8 not null default pg_current_xact_id(),
  primary key (tenant_id, id),
  unique (tenant_id, scope, id),
  -- The Node may be in any Scope of the Tenant the writer can see, which a
  -- trigger checks.
  foreign key (tenant_id, node_id) references gnomon.node (tenant_id, id),
  check (recorded_from < recorded_to)
);
-- At most one current Summary of a Node in each Scope.
create unique index summary_current on gnomon.summary (tenant_id, scope, node_id)
  where recorded_to = 'infinity';
-- As-of reads: the policy's (tenant_id, scope), the Node, then Recorded time
-- (ADR-0004, ADR-0005).
create index summary_as_of on gnomon.summary
  (tenant_id, scope, node_id, recorded_from) include (recorded_to, as_of_valid);
-- Erasure finds every Summary of a Node, whatever its Scope.
create index summary_node on gnomon.summary (tenant_id, node_id);

-- A Citation of a Summary: an Episode of the Summary's Scope it was derived
-- from, optionally naming a property and a place in the Episode.
create table gnomon.summary_citation (
  tenant_id uuid not null,
  scope text not null,
  summary_id uuid not null,
  episode_id uuid not null,
  property text,
  place text,
  -- The Episode's Reference time and Recorded time, as a Version's Citation
  -- carries them.
  episode_reference_time timestamptz not null,
  episode_recorded_at timestamptz not null,
  written_in xid8 not null default pg_current_xact_id(),
  foreign key (tenant_id, scope, summary_id) references gnomon.summary (tenant_id, scope, id),
  foreign key (tenant_id, scope, episode_id, episode_reference_time, episode_recorded_at)
    references gnomon.episode (tenant_id, scope, id, reference_time, recorded_at),
  constraint summary_citation_once unique nulls not distinct
    (tenant_id, summary_id, episode_id, property, place)
);
create index summary_citation_episode on gnomon.summary_citation (tenant_id, episode_id);
create index summary_citation_tenant_scope on gnomon.summary_citation
  (tenant_id, scope, summary_id);

create table gnomon.community (
  tenant_id uuid not null,
  scope text not null,
  id uuid not null default gnomon.uuid_v7(),
  -- The generated Summary of the group.
  content text not null check (content <> ''),
  as_of_valid timestamptz not null check (isfinite(as_of_valid)),
  recorded_from timestamptz not null default transaction_timestamp(),
  recorded_to timestamptz not null default 'infinity',
  written_in xid8 not null default pg_current_xact_id(),
  primary key (tenant_id, id),
  unique (tenant_id, scope, id),
  check (recorded_from < recorded_to)
);
create index community_as_of on gnomon.community
  (tenant_id, scope, recorded_from) include (recorded_to, as_of_valid);

-- A Node a Community holds. The foreign key to the Node carries the
-- Community's Scope (ADR-0014).
create table gnomon.community_member (
  tenant_id uuid not null,
  scope text not null,
  community_id uuid not null,
  node_id uuid not null,
  written_in xid8 not null default pg_current_xact_id(),
  primary key (tenant_id, community_id, node_id),
  foreign key (tenant_id, scope, community_id) references gnomon.community (tenant_id, scope, id),
  foreign key (tenant_id, scope, node_id) references gnomon.node (tenant_id, scope, id)
);
create index community_member_node on gnomon.community_member (tenant_id, scope, node_id);

create table gnomon.community_citation (
  tenant_id uuid not null,
  scope text not null,
  community_id uuid not null,
  episode_id uuid not null,
  property text,
  place text,
  episode_reference_time timestamptz not null,
  episode_recorded_at timestamptz not null,
  written_in xid8 not null default pg_current_xact_id(),
  foreign key (tenant_id, scope, community_id) references gnomon.community (tenant_id, scope, id),
  foreign key (tenant_id, scope, episode_id, episode_reference_time, episode_recorded_at)
    references gnomon.episode (tenant_id, scope, id, reference_time, recorded_at),
  constraint community_citation_once unique nulls not distinct
    (tenant_id, community_id, episode_id, property, place)
);
create index community_citation_episode on gnomon.community_citation (tenant_id, episode_id);
create index community_citation_tenant_scope on gnomon.community_citation
  (tenant_id, scope, community_id);

-- RLS: the Tenant and the Scopes of the call, for the callers and for the
-- definer, as on every other table (ADR-0005, migration 0011).
do $$
declare
  t text;
begin
  foreach t in array array['summary', 'summary_citation', 'community', 'community_member',
                           'community_citation'] loop
    execute format('alter table gnomon.%I enable row level security', t);
    execute format(
      'create policy %1$s_tenant_scope on gnomon.%1$I
         for all to authenticated, gnomon_worker, gnomon_definer
         using (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()))
         with check (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()))', t);
  end loop;
end
$$;

-- Whether a Summary is of a Node the running Erasure listed.
create function gnomon.erasure_reaches_summary(p_tenant_id uuid, p_summary_id uuid)
  returns boolean
  language sql stable security invoker set search_path = ''
  as $$
    select exists (
      select from gnomon.summary s
      where s.tenant_id = p_tenant_id and s.id = p_summary_id
        and gnomon.erasure_reaches(p_tenant_id, array[s.node_id])
    )
  $$;

-- The definer's reach into Scopes the caller cannot see, for reading and
-- deleting only: a Summary of a listed Node, and its Citations (see the
-- header).
create policy summary_erasure_reach_read on gnomon.summary for select to gnomon_definer
  using (tenant_id = gnomon.tenant_id() and gnomon.erasure_reaches(tenant_id, array[node_id]));
create policy summary_erasure_reach_delete on gnomon.summary for delete to gnomon_definer
  using (tenant_id = gnomon.tenant_id() and gnomon.erasure_reaches(tenant_id, array[node_id]));
create policy summary_citation_erasure_reach_read on gnomon.summary_citation
  for select to gnomon_definer
  using (tenant_id = gnomon.tenant_id()
         and gnomon.erasure_reaches_summary(tenant_id, summary_id));
create policy summary_citation_erasure_reach_delete on gnomon.summary_citation
  for delete to gnomon_definer
  using (tenant_id = gnomon.tenant_id()
         and gnomon.erasure_reaches_summary(tenant_id, summary_id));

-- A refused write raises SQLSTATE GNW01 with a message that says why, as on
-- every other table.

-- A new Summary is of a Node the caller can see, and its Recorded time
-- overlaps no other Summary of that Node in its Scope: none is current, and
-- none was closed after the current transaction started.
create function gnomon.summary_check_insert() returns trigger
  language plpgsql security invoker set search_path = ''
  as $$
  begin
    if not exists (select from gnomon.node n
                   where n.tenant_id = new.tenant_id and n.id = new.node_id) then
      raise exception using errcode = 'GNW01',
        message = pg_catalog.format('Node %s is not visible to the caller', new.node_id);
    end if;
    if exists (select from gnomon.summary s
               where s.tenant_id = new.tenant_id and s.scope = new.scope
                 and s.node_id = new.node_id and s.id <> new.id
                 and s.recorded_to > new.recorded_from) then
      raise exception using errcode = 'GNW01',
        message = pg_catalog.format(
          'Node %s has a Summary in Scope %s whose Recorded time a new one would overlap; '
          'close it first', new.node_id, new.scope);
    end if;
    return new;
  end
  $$;
create trigger summary_check_insert before insert on gnomon.summary
  for each row when (not gnomon.running_as_definer())
  execute function gnomon.summary_check_insert();

-- A Citation or a member is added only to a Summary or Community the current
-- transaction wrote, so what a row cites and holds is fixed when it commits.
-- `tg_argv[0]` names the parent table and `tg_argv[1]` the column naming it.
create function gnomon.derived_check_insert() returns trigger
  language plpgsql security invoker set search_path = ''
  as $$
  declare
    v_parent uuid := (pg_catalog.to_jsonb(new) ->> tg_argv[1])::uuid;
    v_written_in xid8;
  begin
    if tg_argv[0] = 'summary' then
      select s.written_in into v_written_in from gnomon.summary s
        where s.tenant_id = new.tenant_id and s.scope = new.scope and s.id = v_parent;
    else
      select c.written_in into v_written_in from gnomon.community c
        where c.tenant_id = new.tenant_id and c.scope = new.scope and c.id = v_parent;
    end if;
    if v_written_in is distinct from pg_current_xact_id() then
      raise exception using errcode = 'GNW01',
        message = pg_catalog.format(
          'a %s''s Citations and Nodes are written with it, and %s was written earlier',
          case when tg_argv[0] = 'summary' then 'Summary' else 'Community' end, v_parent);
    end if;
    return new;
  end
  $$;
create trigger summary_citation_check_insert before insert on gnomon.summary_citation
  for each row when (not gnomon.running_as_definer())
  execute function gnomon.derived_check_insert('summary', 'summary_id');
create trigger community_member_check_insert before insert on gnomon.community_member
  for each row when (not gnomon.running_as_definer())
  execute function gnomon.derived_check_insert('community', 'community_id');
create trigger community_citation_check_insert before insert on gnomon.community_citation
  for each row when (not gnomon.running_as_definer())
  execute function gnomon.derived_check_insert('community', 'community_id');

-- At commit, a Summary that still stands cites an Episode, and a Community
-- cites an Episode and holds a Node. Erasure deletes a Summary or Community
-- with its Citations, so none is left uncited.
create function gnomon.derived_check_complete() returns trigger
  language plpgsql security invoker set search_path = ''
  as $$
  begin
    if tg_table_name = 'summary' then
      if exists (select from gnomon.summary s
                 where s.tenant_id = new.tenant_id and s.id = new.id)
         and not exists (select from gnomon.summary_citation c
                         where c.tenant_id = new.tenant_id and c.scope = new.scope
                           and c.summary_id = new.id) then
        raise exception using errcode = 'GNW01',
          message = pg_catalog.format('Summary %s cites no Episode', new.id);
      end if;
    elsif exists (select from gnomon.community c
                  where c.tenant_id = new.tenant_id and c.id = new.id) then
      if not exists (select from gnomon.community_citation c
                     where c.tenant_id = new.tenant_id and c.scope = new.scope
                       and c.community_id = new.id) then
        raise exception using errcode = 'GNW01',
          message = pg_catalog.format('Community %s cites no Episode', new.id);
      end if;
      if not exists (select from gnomon.community_member m
                     where m.tenant_id = new.tenant_id and m.scope = new.scope
                       and m.community_id = new.id) then
        raise exception using errcode = 'GNW01',
          message = pg_catalog.format('Community %s holds no Node', new.id);
      end if;
    end if;
    return null;
  end
  $$;
create constraint trigger summary_check_complete after insert on gnomon.summary
  deferrable initially deferred
  for each row execute function gnomon.derived_check_complete();
create constraint trigger community_check_complete after insert on gnomon.community
  deferrable initially deferred
  for each row execute function gnomon.derived_check_complete();

-- A Summary or Community changes only by closing: its Recorded-time end goes
-- from open to the start of the current transaction, after its start.
create function gnomon.derived_guard_update() returns trigger
  language plpgsql security invoker set search_path = ''
  as $$
  begin
    if (pg_catalog.to_jsonb(new) - 'recorded_to') is distinct from
       (pg_catalog.to_jsonb(old) - 'recorded_to') then
      raise exception using errcode = 'insufficient_privilege',
        message = pg_catalog.format(
          'a %s is immutable apart from the end of its Recorded time', tg_argv[0]);
    end if;
    if old.recorded_to = 'infinity' and new.recorded_to = transaction_timestamp()
       and old.recorded_from < new.recorded_to then
      return new;
    end if;
    raise exception using errcode = 'insufficient_privilege',
      message = pg_catalog.format(
        'a %s''s Recorded time may only be closed, at the start of the current transaction',
        tg_argv[0]);
  end
  $$;
create trigger summary_guard_update before update on gnomon.summary
  for each row when (not gnomon.running_as_definer())
  execute function gnomon.derived_guard_update('Summary');
create trigger community_guard_update before update on gnomon.community
  for each row when (not gnomon.running_as_definer())
  execute function gnomon.derived_guard_update('Community');

-- Only the transaction that wrote a row deletes it. Committed rows go only
-- by Erasure (ADR-0009).
create trigger summary_guard_delete before delete on gnomon.summary
  for each row when (not gnomon.running_as_definer())
  execute function gnomon.guard_delete('Summary');
create trigger summary_citation_guard_delete before delete on gnomon.summary_citation
  for each row when (not gnomon.running_as_definer())
  execute function gnomon.guard_delete('Citation');
create trigger community_guard_delete before delete on gnomon.community
  for each row when (not gnomon.running_as_definer())
  execute function gnomon.guard_delete('Community');
create trigger community_member_guard_delete before delete on gnomon.community_member
  for each row when (not gnomon.running_as_definer())
  execute function gnomon.guard_delete('Community member');
create trigger community_citation_guard_delete before delete on gnomon.community_citation
  for each row when (not gnomon.running_as_definer())
  execute function gnomon.guard_delete('Citation');

-- The Citations a write names, checked: at least one, each of an Episode the
-- caller sees in `p_scope`, with that Episode's Reference time and Recorded
-- time. `p_properties` and `p_places` are as long as `p_episodes`, or null
-- for none. An Episode of another Scope the caller sees is refused by name;
-- one the caller cannot see is not visible, wherever it is.
create function gnomon.derived_citations(
  p_tenant_id uuid,
  p_scope text,
  p_episodes uuid[],
  p_properties text[],
  p_places text[]
) returns table (
    episode_id uuid,
    property text,
    place text,
    episode_reference_time timestamptz,
    episode_recorded_at timestamptz
  )
  language plpgsql stable security invoker set search_path = ''
  as $$
  declare
    n integer := coalesce(pg_catalog.cardinality(p_episodes), 0);
    i integer;
    e record;
  begin
    if n = 0 then
      raise exception using errcode = 'GNW01',
        message = 'a Summary or Community cites at least one Episode';
    end if;
    if pg_catalog.cardinality(p_properties) is distinct from n and p_properties is not null
       or pg_catalog.cardinality(p_places) is distinct from n and p_places is not null then
      raise exception using errcode = 'GNW01',
        message = 'each Citation names its Episode, and its property and place or none';
    end if;
    for i in 1 .. n loop
      if p_episodes[i] is null then
        raise exception using errcode = 'GNW01', message = 'a Citation names its Episode';
      end if;
      select x.scope, x.reference_time, x.recorded_at into e
        from gnomon.episode x
        where x.tenant_id = p_tenant_id and x.id = p_episodes[i];
      if not found then
        raise exception using errcode = 'GNW01',
          message = pg_catalog.format('Episode %s is not visible to the caller', p_episodes[i]);
      end if;
      if e.scope <> p_scope then
        raise exception using errcode = 'GNW01',
          message = pg_catalog.format(
            'Episode %s is in Scope %s, and a Summary or Community in Scope %s cites only '
            'Episodes of its own Scope', p_episodes[i], e.scope, p_scope);
      end if;
      episode_id := p_episodes[i];
      property := p_properties[i];
      place := p_places[i];
      episode_reference_time := e.reference_time;
      episode_recorded_at := e.recorded_at;
      return next;
    end loop;
  end
  $$;

-- The Tenant and Scope of a write, checked before anything is read.
create function gnomon.derived_check_scope(p_tenant_id uuid, p_scope text) returns void
  language plpgsql stable security invoker set search_path = ''
  as $$
  begin
    if p_tenant_id is null or p_tenant_id is distinct from gnomon.tenant_id() then
      raise exception using errcode = 'insufficient_privilege',
        message = 'a write may write only in the caller''s own Tenant';
    end if;
    if p_scope is null or not (p_scope = any(gnomon.allowed_scopes())) then
      raise exception using errcode = 'GNW01',
        message = pg_catalog.format('Scope %s is not visible to the caller', p_scope);
    end if;
  end
  $$;

-- Writes a Summary of a Node in a Scope of the caller's and returns its ID.
-- The Node's current Summary in that Scope, if any, is closed, or deleted
-- when the current transaction wrote it. A current Summary that a
-- transaction started later than this one wrote fails the write with a
-- serialization failure, which the caller retries.
create function gnomon.write_summary(
  p_tenant_id uuid,
  p_scope text,
  p_node_id uuid,
  p_content text,
  p_as_of_valid timestamptz,
  p_episodes uuid[],
  p_properties text[],
  p_places text[]
) returns uuid
  language plpgsql volatile security invoker set search_path = ''
  as $$
  declare
    t constant timestamptz := transaction_timestamp();
    v_old record;
    v_id uuid;
  begin
    perform gnomon.derived_check_scope(p_tenant_id, p_scope);
    perform from gnomon.lock_for_write(p_tenant_id, p_node_id, false);
    select s.id, s.recorded_from, s.written_in into v_old
      from gnomon.summary s
      where s.tenant_id = p_tenant_id and s.scope = p_scope and s.node_id = p_node_id
        and s.recorded_to = 'infinity';
    if found then
      if v_old.written_in = pg_current_xact_id() then
        delete from gnomon.summary_citation c
          where c.tenant_id = p_tenant_id and c.scope = p_scope and c.summary_id = v_old.id;
        delete from gnomon.summary s where s.tenant_id = p_tenant_id and s.id = v_old.id;
      elsif v_old.recorded_from >= t then
        raise exception using errcode = 'serialization_failure',
          message = pg_catalog.format(
            'a later transaction wrote the Summary of Node %s in Scope %s first; retry the write',
            p_node_id, p_scope);
      else
        update gnomon.summary s set recorded_to = t
          where s.tenant_id = p_tenant_id and s.id = v_old.id;
      end if;
    end if;
    insert into gnomon.summary (tenant_id, scope, node_id, content, as_of_valid)
      values (p_tenant_id, p_scope, p_node_id, p_content, p_as_of_valid)
      returning id into v_id;
    insert into gnomon.summary_citation
        (tenant_id, scope, summary_id, episode_id, property, place,
         episode_reference_time, episode_recorded_at)
      select distinct p_tenant_id, p_scope, v_id, c.episode_id, c.property, c.place,
             c.episode_reference_time, c.episode_recorded_at
      from gnomon.derived_citations(p_tenant_id, p_scope, p_episodes, p_properties, p_places) c;
    return v_id;
  end
  $$;

-- Writes a Community of Nodes in a Scope of the caller's, with its Summary,
-- and returns its ID. Every Node is in that Scope (ADR-0014). The current
-- Communities `p_replaces` names, all of that Scope, are closed, or deleted
-- when the current transaction wrote them.
create function gnomon.write_community(
  p_tenant_id uuid,
  p_scope text,
  p_replaces uuid[],
  p_members uuid[],
  p_content text,
  p_as_of_valid timestamptz,
  p_episodes uuid[],
  p_properties text[],
  p_places text[]
) returns uuid
  language plpgsql volatile security invoker set search_path = ''
  as $$
  declare
    t constant timestamptz := transaction_timestamp();
    v_members uuid[] := array(select distinct m from unnest(p_members) m order by m);
    v_member uuid;
    v_member_scope text;
    v_old record;
    v_id uuid;
  begin
    perform gnomon.derived_check_scope(p_tenant_id, p_scope);
    if cardinality(v_members) = 0 or array_position(v_members, null) is not null then
      raise exception using errcode = 'GNW01',
        message = 'a Community holds at least one Node, and names each';
    end if;
    foreach v_member in array v_members loop
      select l.scope into v_member_scope
        from gnomon.lock_for_write(p_tenant_id, v_member, false) l;
      if v_member_scope <> p_scope then
        raise exception using errcode = 'GNW01',
          message = pg_catalog.format(
            'Node %s is in Scope %s, and a Community in Scope %s holds only Nodes of its own '
            'Scope', v_member, v_member_scope, p_scope);
      end if;
    end loop;
    for v_old in
      select r.id, c.recorded_from, c.recorded_to, c.written_in
      from (select distinct x as id from unnest(p_replaces) x where x is not null) r
      left join gnomon.community c
        on c.tenant_id = p_tenant_id and c.scope = p_scope and c.id = r.id
      order by r.id
    loop
      if v_old.recorded_to is distinct from 'infinity' then
        raise exception using errcode = 'GNW01',
          message = pg_catalog.format(
            'Community %s is not a current Community of Scope %s the caller sees',
            v_old.id, p_scope);
      end if;
      if v_old.written_in = pg_current_xact_id() then
        delete from gnomon.community_citation c
          where c.tenant_id = p_tenant_id and c.scope = p_scope and c.community_id = v_old.id;
        delete from gnomon.community_member m
          where m.tenant_id = p_tenant_id and m.scope = p_scope and m.community_id = v_old.id;
        delete from gnomon.community c where c.tenant_id = p_tenant_id and c.id = v_old.id;
      elsif v_old.recorded_from >= t then
        raise exception using errcode = 'serialization_failure',
          message = pg_catalog.format(
            'a later transaction wrote Community %s first; retry the write', v_old.id);
      else
        update gnomon.community c set recorded_to = t
          where c.tenant_id = p_tenant_id and c.id = v_old.id and c.recorded_to = 'infinity';
        if not found then
          raise exception using errcode = 'serialization_failure',
            message = pg_catalog.format(
              'another transaction replaced Community %s first; retry the write', v_old.id);
        end if;
      end if;
    end loop;
    insert into gnomon.community (tenant_id, scope, content, as_of_valid)
      values (p_tenant_id, p_scope, p_content, p_as_of_valid)
      returning id into v_id;
    insert into gnomon.community_member (tenant_id, scope, community_id, node_id)
      select p_tenant_id, p_scope, v_id, m from unnest(v_members) m;
    insert into gnomon.community_citation
        (tenant_id, scope, community_id, episode_id, property, place,
         episode_reference_time, episode_recorded_at)
      select distinct p_tenant_id, p_scope, v_id, c.episode_id, c.property, c.place,
             c.episode_reference_time, c.episode_recorded_at
      from gnomon.derived_citations(p_tenant_id, p_scope, p_episodes, p_properties, p_places) c;
    return v_id;
  end
  $$;

-- The Summaries of a Node As-of `p_valid_at` and `p_known_at`, one in each
-- of the caller's Scopes that has one, with their Citations. A null
-- `p_known_at` reads the current Summaries. A Summary is found only at a
-- valid-at instant on or after the one it was written for (ADR-0015).
create function gnomon.read_summaries(
  p_tenant_id uuid,
  p_node_id uuid,
  p_valid_at timestamptz,
  p_known_at timestamptz
) returns table (
    id uuid,
    scope text,
    node_id uuid,
    content text,
    as_of_valid timestamptz,
    recorded_from timestamptz,
    recorded_to timestamptz,
    cited_episodes uuid[],
    cited_properties text[],
    cited_places text[]
  )
  language sql stable security invoker set search_path = ''
  as $$
    select s.id, s.scope, s.node_id, s.content, s.as_of_valid, s.recorded_from,
           nullif(s.recorded_to, 'infinity'),
           coalesce(c.episodes, '{}'), coalesce(c.properties, '{}'), coalesce(c.places, '{}')
    from gnomon.summary s
    cross join lateral (
      select array_agg(x.episode_id order by x.episode_id, x.property, x.place) as episodes,
             array_agg(x.property order by x.episode_id, x.property, x.place) as properties,
             array_agg(x.place order by x.episode_id, x.property, x.place) as places
      from gnomon.summary_citation x
      where x.tenant_id = s.tenant_id and x.scope = s.scope and x.summary_id = s.id
    ) c
    where s.tenant_id = p_tenant_id and s.node_id = p_node_id
      and s.as_of_valid <= p_valid_at
      and case
            when p_known_at is null then s.recorded_to = 'infinity'
            else s.recorded_from <= p_known_at and s.recorded_to > p_known_at
          end
    order by s.scope
  $$;

-- The Communities that hold a Node As-of `p_valid_at` and `p_known_at`, with
-- their Nodes and Citations, as `gnomon.read_summaries` reads Summaries.
create function gnomon.read_communities(
  p_tenant_id uuid,
  p_node_id uuid,
  p_valid_at timestamptz,
  p_known_at timestamptz
) returns table (
    id uuid,
    scope text,
    members uuid[],
    content text,
    as_of_valid timestamptz,
    recorded_from timestamptz,
    recorded_to timestamptz,
    cited_episodes uuid[],
    cited_properties text[],
    cited_places text[]
  )
  language sql stable security invoker set search_path = ''
  as $$
    select k.id, k.scope,
           array(select m.node_id from gnomon.community_member m
                 where m.tenant_id = k.tenant_id and m.scope = k.scope and m.community_id = k.id
                 order by m.node_id),
           k.content, k.as_of_valid, k.recorded_from, nullif(k.recorded_to, 'infinity'),
           coalesce(c.episodes, '{}'), coalesce(c.properties, '{}'), coalesce(c.places, '{}')
    from gnomon.community_member h
    join gnomon.community k
      on k.tenant_id = h.tenant_id and k.scope = h.scope and k.id = h.community_id
    cross join lateral (
      select array_agg(x.episode_id order by x.episode_id, x.property, x.place) as episodes,
             array_agg(x.property order by x.episode_id, x.property, x.place) as properties,
             array_agg(x.place order by x.episode_id, x.property, x.place) as places
      from gnomon.community_citation x
      where x.tenant_id = k.tenant_id and x.scope = k.scope and x.community_id = k.id
    ) c
    where h.tenant_id = p_tenant_id and h.node_id = p_node_id
      and k.as_of_valid <= p_valid_at
      and case
            when p_known_at is null then k.recorded_to = 'infinity'
            else k.recorded_from <= p_known_at and k.recorded_to > p_known_at
          end
    order by k.scope, k.id
  $$;

-- Deletes these Summaries and Communities with their Citations and members,
-- closed or open. Every Erasure that removes one comes here, so a later
-- migration that derives rows from their text deletes those rows here too.
create function gnomon.erase_summaries_and_communities(
  p_tenant_id uuid,
  p_summaries uuid[],
  p_communities uuid[]
) returns void
  language plpgsql volatile security invoker set search_path = ''
  as $$
  begin
    delete from gnomon.summary_citation c
      where c.tenant_id = p_tenant_id and c.summary_id = any(p_summaries);
    delete from gnomon.summary s
      where s.tenant_id = p_tenant_id and s.id = any(p_summaries);
    delete from gnomon.community_citation c
      where c.tenant_id = p_tenant_id and c.community_id = any(p_communities);
    delete from gnomon.community_member m
      where m.tenant_id = p_tenant_id and m.community_id = any(p_communities);
    delete from gnomon.community c
      where c.tenant_id = p_tenant_id and c.id = any(p_communities);
  end
  $$;

-- Migration 0014's function with the Summaries and Communities that cite the
-- Episode deleted before it. Everything else is as migration 0014 wrote it.
create or replace function gnomon.erase_episode_rows(p_tenant_id uuid, p_episode_id uuid)
  returns void
  language plpgsql volatile security invoker set search_path = ''
  as $$
  begin
    delete from gnomon.statement s
      where s.tenant_id = p_tenant_id and s.episode_id <> p_episode_id
        and s.kind = 'retraction'
        and exists (select from gnomon.citation c
                    where c.tenant_id = s.tenant_id and c.version_id = s.version_id
                      and c.episode_id = p_episode_id)
        and not exists (select from gnomon.citation c
                        where c.tenant_id = s.tenant_id and c.version_id = s.version_id
                          and c.episode_id <> p_episode_id);
    delete from gnomon.bound_statement b
      where b.tenant_id = p_tenant_id
        and (b.episode_id = p_episode_id
             or b.contribution_id in (select c.id from gnomon.contribution c
                                      where c.tenant_id = p_tenant_id
                                        and c.episode_id = p_episode_id));
    delete from gnomon.contribution c
      where c.tenant_id = p_tenant_id and c.episode_id = p_episode_id;
    delete from gnomon.statement s
      where s.tenant_id = p_tenant_id and s.episode_id = p_episode_id;
    delete from gnomon.citation c
      where c.tenant_id = p_tenant_id and c.episode_id = p_episode_id;
    delete from gnomon.episode_retraction r
      where r.tenant_id = p_tenant_id and r.episode_id = p_episode_id;
    delete from gnomon.stage_usage u
      where u.tenant_id = p_tenant_id and u.episode_id = p_episode_id;
    delete from gnomon.stage_record r
      where r.tenant_id = p_tenant_id and r.episode_id = p_episode_id;
    perform gnomon.erase_summaries_and_communities(
      p_tenant_id,
      array(select distinct c.summary_id from gnomon.summary_citation c
            where c.tenant_id = p_tenant_id and c.episode_id = p_episode_id),
      array(select distinct c.community_id from gnomon.community_citation c
            where c.tenant_id = p_tenant_id and c.episode_id = p_episode_id));
    delete from gnomon.episode e
      where e.tenant_id = p_tenant_id and e.id = p_episode_id;
  end
  $$;

-- Migration 0012's function with the Summaries of these Nodes, in every Scope
-- the Erasure reaches, and the Communities that hold them, deleted first.
-- Everything else is as migration 0012 wrote it.
create or replace function gnomon.erase_versioned_rows(p_tenant_id uuid, p_ids uuid[])
  returns void
  language plpgsql volatile security invoker set search_path = ''
  as $$
  begin
    perform gnomon.erase_summaries_and_communities(
      p_tenant_id,
      array(select s.id from gnomon.summary s
            where s.tenant_id = p_tenant_id and s.node_id = any(p_ids)),
      array(select distinct m.community_id from gnomon.community_member m
            where m.tenant_id = p_tenant_id and m.node_id = any(p_ids)));
    perform gnomon.erase_version_text(p_tenant_id, array(
      select v.id from gnomon.version v
      where v.tenant_id = p_tenant_id and v.versioned_id = any(p_ids)));
    delete from gnomon.bound_statement b
      where b.tenant_id = p_tenant_id and b.versioned_id = any(p_ids);
    delete from gnomon.contribution c
      where c.tenant_id = p_tenant_id and c.versioned_id = any(p_ids);
    delete from gnomon.statement s
      where s.tenant_id = p_tenant_id and s.versioned_id = any(p_ids);
    delete from gnomon.citation c
      using gnomon.version v
      where c.tenant_id = p_tenant_id and v.tenant_id = p_tenant_id and v.id = c.version_id
        and v.versioned_id = any(p_ids);
    delete from gnomon.version v
      where v.tenant_id = p_tenant_id and v.versioned_id = any(p_ids);
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
      'gnomon.erasure_reaches_summary(uuid, uuid)'::regprocedure,
      'gnomon.summary_check_insert()'::regprocedure,
      'gnomon.derived_check_insert()'::regprocedure,
      'gnomon.derived_check_complete()'::regprocedure,
      'gnomon.derived_guard_update()'::regprocedure,
      'gnomon.derived_citations(uuid, text, uuid[], text[], text[])'::regprocedure,
      'gnomon.derived_check_scope(uuid, text)'::regprocedure,
      'gnomon.write_summary(uuid, text, uuid, text, timestamptz, uuid[], text[], text[])'::regprocedure,
      'gnomon.write_community(uuid, text, uuid[], uuid[], text, timestamptz, uuid[], text[], text[])'::regprocedure,
      'gnomon.read_summaries(uuid, uuid, timestamptz, timestamptz)'::regprocedure,
      'gnomon.read_communities(uuid, uuid, timestamptz, timestamptz)'::regprocedure,
      'gnomon.erase_summaries_and_communities(uuid, uuid[], uuid[])'::regprocedure
    ) and a.grantee <> p.proowner
    union all
    select format('table %s', c.oid::regclass), a.grantee
    from pg_class c, aclexplode(c.relacl) a
    where c.oid in ('gnomon.summary'::regclass, 'gnomon.summary_citation'::regclass,
                    'gnomon.community'::regclass, 'gnomon.community_member'::regclass,
                    'gnomon.community_citation'::regclass)
      and a.grantee <> c.relowner
  loop
    execute format(
      'revoke all on %s from %s', g.object,
      case when g.grantee = 0 then 'public' else g.grantee::regrole::text end);
  end loop;
  revoke all on function gnomon.erasure_reaches_summary(uuid, uuid) from public;
  revoke all on function gnomon.summary_check_insert() from public;
  revoke all on function gnomon.derived_check_insert() from public;
  revoke all on function gnomon.derived_check_complete() from public;
  revoke all on function gnomon.derived_guard_update() from public;
  revoke all on function gnomon.derived_citations(uuid, text, uuid[], text[], text[]) from public;
  revoke all on function gnomon.derived_check_scope(uuid, text) from public;
  revoke all on function gnomon.write_summary(
    uuid, text, uuid, text, timestamptz, uuid[], text[], text[]) from public;
  revoke all on function gnomon.write_community(
    uuid, text, uuid[], uuid[], text, timestamptz, uuid[], text[], text[]) from public;
  revoke all on function gnomon.read_summaries(uuid, uuid, timestamptz, timestamptz) from public;
  revoke all on function gnomon.read_communities(uuid, uuid, timestamptz, timestamptz) from public;
  revoke all on function gnomon.erase_summaries_and_communities(uuid, uuid[], uuid[]) from public;
  revoke all on gnomon.summary, gnomon.summary_citation, gnomon.community,
    gnomon.community_member, gnomon.community_citation from public;
end
$$;

-- Callers read the five tables, insert the columns they may choose, close a
-- Summary or Community, and delete what their own transaction wrote, which
-- the guards enforce (see the header).
grant select, delete on gnomon.summary, gnomon.summary_citation, gnomon.community,
  gnomon.community_member, gnomon.community_citation to authenticated, gnomon_worker;
grant insert (tenant_id, scope, node_id, content, as_of_valid) on gnomon.summary
  to authenticated, gnomon_worker;
grant insert (tenant_id, scope, content, as_of_valid) on gnomon.community
  to authenticated, gnomon_worker;
grant insert (tenant_id, scope, summary_id, episode_id, property, place,
              episode_reference_time, episode_recorded_at)
  on gnomon.summary_citation to authenticated, gnomon_worker;
grant insert (tenant_id, scope, community_id, episode_id, property, place,
              episode_reference_time, episode_recorded_at)
  on gnomon.community_citation to authenticated, gnomon_worker;
grant insert (tenant_id, scope, community_id, node_id) on gnomon.community_member
  to authenticated, gnomon_worker;
grant update (recorded_to) on gnomon.summary, gnomon.community to authenticated, gnomon_worker;
grant execute on function gnomon.derived_citations(uuid, text, uuid[], text[], text[])
  to authenticated, gnomon_worker;
grant execute on function gnomon.derived_check_scope(uuid, text) to authenticated, gnomon_worker;
grant execute on function gnomon.write_summary(
  uuid, text, uuid, text, timestamptz, uuid[], text[], text[]) to authenticated, gnomon_worker;
grant execute on function gnomon.write_community(
  uuid, text, uuid[], uuid[], text, timestamptz, uuid[], text[], text[])
  to authenticated, gnomon_worker;
grant execute on function gnomon.read_summaries(uuid, uuid, timestamptz, timestamptz)
  to authenticated, gnomon_worker;
grant execute on function gnomon.read_communities(uuid, uuid, timestamptz, timestamptz)
  to authenticated, gnomon_worker;

-- What Erasure needs as gnomon_definer: to read and delete the five tables,
-- and the functions its policies and deletes call. Callers get none of the
-- reach.
grant select, delete on gnomon.summary, gnomon.summary_citation, gnomon.community,
  gnomon.community_member, gnomon.community_citation to gnomon_definer;
grant execute on function gnomon.erasure_reaches_summary(uuid, uuid) to gnomon_definer;
grant execute on function gnomon.erase_summaries_and_communities(uuid, uuid[], uuid[])
  to gnomon_definer;

-- The vacuum call vacuums all five, which `gnomon.erasure_vacuum_tables()`
-- now lists, since the definer may delete from them.
grant maintain on gnomon.summary, gnomon.summary_citation, gnomon.community,
  gnomon.community_member, gnomon.community_citation to gnomon_maintainer;

insert into gnomon.schema_version (version, name) values (18, '0018_summaries_and_communities');
