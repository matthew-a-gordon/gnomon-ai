-- gnomon migration 0012: Erase a Node, an Edge or a Scope.
--
-- Erasure physically deletes what it names and leaves no tombstone
-- (ADR-0003, GLOSSARY.md: Erasure). Beside Erase Episode (migration 0011)
-- there are now three more:
--
--   gnomon.erase_node(tenant, node)    the Node, its Versions, Contributions,
--                                      statements and Citations, and every
--                                      Edge that joins it.
--   gnomon.erase_edge(tenant, edge)    the Edge, its Versions, Contributions,
--                                      statements, Citations and Sentences.
--   gnomon.erase_scope(tenant, scope)  every row in the Scope, its Episodes
--                                      among them, and every Edge of another
--                                      Scope that joins one of its Nodes.
--
-- Each erased Edge goes as in Erase Episode: its rows, the Sentence of each of
-- its Versions (ADR-0013), and then each endpoint it leaves with nothing. Such
-- an endpoint is erased when no row names it and no other Edge joins it, and
-- only when it is in a Scope the caller can see. One in a Scope the caller
-- cannot see stays as a bare identity, as it does after Erase Episode.
--
-- None of them recomputes anything, because no Version that survives was
-- derived from what they erase. A Contribution, statement or Citation names an
-- Episode of its own Node's or Edge's Scope. So an Episode that a Scope's
-- Erasure deletes touched only that Scope's Nodes and Edges. A Node's Versions
-- owe nothing to its Edges. The Episodes that wrote an erased Node or Edge
-- stay, apart from a Scope's own. A statement of another Episode about an
-- erased Node or Edge is that Node's or Edge's, so it goes with it.
--
-- An Edge lives in one Scope and may join a Node in any Scope of the Tenant
-- that its writer could see (migration 0008). Erasing a Node, or the Nodes of
-- a Scope, erases every Edge that joins them, whatever the Edge's Scope, even
-- one the caller cannot see, because the foreign key on the Node would refuse
-- the Node's delete while it stood. Erasing a Scope removes nothing else
-- outside it but those Edges' rows and the endpoints they leave bare.
--
-- These three are Erasure, the first of the definers ADR-0009 names. Like
-- Erase Episode they are SECURITY DEFINER, owned by the NOLOGIN
-- `gnomon_definer`, check the Tenant against `gnomon.tenant_id()` and are
-- granted to no caller role. The Node, Edge or Scope must be one the caller
-- can see, and the definer reads under the caller's Tenant and Scope
-- policies (migration 0011).
--
-- Reaching Edges the caller cannot see. The definer gets one more policy on
-- each table that holds an Edge's rows: it may read and delete a row of an
-- Edge, in any Scope of the Tenant, that joins a Node its own Erasure is
-- erasing, and that Edge's rows. Its Erasure lists those Nodes, and then those
-- Edges, in `gnomon.erasure_reach`, marked with its own transaction, and
-- deletes the list before it returns. No caller role may read or write that
-- table, so no caller can widen what the definer reaches, and a row is visible
-- only to the transaction that wrote it. Nodes get no such policy, so a bare
-- endpoint in a Scope the caller cannot see is never erased.
--
-- Locks (ADR-0007), in the order Erase Episode takes them:
--
--   1. for a Scope, the advisory lock of each of its Episodes' IDs, in order
--      of ID, which Retract Episode and Erase Episode take too;
--   2. the advisory lock (`gnomon.node_lock_key`) of every Node and Edge it
--      erases and of both endpoints of every such Edge, in order of their
--      IDs. A write to one of them, and an Edge that would join one, waits
--      until the Erasure commits;
--   3. for a Scope, its Episodes' rows FOR UPDATE, so a write citing one of
--      them waits, and fails its foreign key once the Erasure commits.
--
-- Each lock step re-reads what it erases, since a write that held a lock
-- first may have added an Edge or a Node, and locks what is new until nothing
-- is. An Episode recorded in the Scope after step 1 fails the Erasure with a
-- serialization failure, which the caller retries, because its lock would
-- come after the Nodes' locks. Each advisory lock is an entry in the shared
-- lock table, as migration 0011's header says: an Erasure that needs more
-- than the table has free fails with SQLSTATE 53200 and erases nothing.
--
-- An Erasure tells the caller only of what it can see. Its result names the
-- Edges it erased in the caller's Scopes, and not those of other Scopes, so a
-- caller learns no ID from a Scope it cannot see. Every Node it names is in
-- the caller's Scopes already.
--
-- Unlike Erase Episode, these Erasures run in any transaction and need no
-- check that a later transaction wrote first. They replay nothing and keep no
-- history of what they erase: every row of it goes, whoever wrote it and
-- whenever. A write that still holds a lock commits first, and the Erasure
-- then erases what it wrote too.
--
-- Copies of the erased bytes stay in dead tuples and index pages until the
-- tables are vacuumed (ADR-0003), and so do the dead rows of
-- `gnomon.erasure_reach`, which hold the IDs of what was erased.
--
-- Later tables extend these Erasures in three places, and only there:
--
--   `gnomon.erase_versioned_rows`  every row of a Node or an Edge: name and
--                                  Alias rows (#17), Summaries and
--                                  Communities that drew on a Node (#16).
--                                  There is no adjacency table: Search's
--                                  neighbourhood reads the Edges and their
--                                  Versions (ADR-0010).
--   `gnomon.erase_version_text`    every row derived from a Version's text
--                                  (migration 0011).
--   `gnomon.erase_episode_rows`    every row that names an Episode, which a
--                                  Scope's Erasure calls for each of its
--                                  Episodes (migration 0011).
--
-- A table of Edge rows that a later migration adds also gets the
-- `*_erasure_reach` policies below, so an Erasure reaches it in every Scope.

-- What the running Erasure of a Node, an Edge or a Scope may reach outside
-- the caller's Scopes: the Nodes it erases and the Edges that join them.
create table gnomon.erasure_reach (
  tenant_id uuid not null,
  written_in xid8 not null default pg_current_xact_id(),
  id uuid not null,
  primary key (tenant_id, written_in, id)
);
alter table gnomon.erasure_reach enable row level security;
create policy erasure_reach_own on gnomon.erasure_reach
  for all to gnomon_definer
  using (tenant_id = gnomon.tenant_id() and written_in = pg_current_xact_id_if_assigned())
  with check (tenant_id = gnomon.tenant_id() and written_in = pg_current_xact_id());

-- Whether the running Erasure listed one of `p_ids`.
create function gnomon.erasure_reaches(p_tenant_id uuid, p_ids uuid[]) returns boolean
  language sql stable security invoker set search_path = ''
  as $$
    select exists (
      select from gnomon.erasure_reach r
      where r.tenant_id = p_tenant_id and r.written_in = pg_current_xact_id_if_assigned()
        and r.id = any(p_ids)
    )
  $$;

-- Whether a Version belongs to an Edge the running Erasure listed.
create function gnomon.erasure_reaches_version(p_tenant_id uuid, p_version_id uuid)
  returns boolean
  language sql stable security invoker set search_path = ''
  as $$
    select exists (
      select from gnomon.version v
      where v.tenant_id = p_tenant_id and v.id = p_version_id and v.edge_id is not null
        and gnomon.erasure_reaches(p_tenant_id, array[v.edge_id])
    )
  $$;

-- The definer's reach into Scopes the caller cannot see, for reading and
-- deleting only: an Edge that joins a listed Node, and the rows of a listed
-- Edge.
create policy edge_erasure_reach_read on gnomon.edge for select to gnomon_definer
  using (tenant_id = gnomon.tenant_id()
         and gnomon.erasure_reaches(tenant_id, array[id, source_id, target_id]));
create policy edge_erasure_reach_delete on gnomon.edge for delete to gnomon_definer
  using (tenant_id = gnomon.tenant_id() and gnomon.erasure_reaches(tenant_id, array[id]));
do $$
declare
  t text;
begin
  foreach t in array array['contribution', 'version', 'statement', 'bound_statement'] loop
    execute format(
      'create policy %1$s_erasure_reach_read on gnomon.%1$I for select to gnomon_definer
         using (tenant_id = gnomon.tenant_id() and edge_id is not null
                and gnomon.erasure_reaches(tenant_id, array[edge_id]))', t);
    execute format(
      'create policy %1$s_erasure_reach_delete on gnomon.%1$I for delete to gnomon_definer
         using (tenant_id = gnomon.tenant_id() and edge_id is not null
                and gnomon.erasure_reaches(tenant_id, array[edge_id]))', t);
  end loop;
  foreach t in array array['citation', 'sentence'] loop
    execute format(
      'create policy %1$s_erasure_reach_read on gnomon.%1$I for select to gnomon_definer
         using (tenant_id = gnomon.tenant_id()
                and gnomon.erasure_reaches_version(tenant_id, version_id))', t);
    execute format(
      'create policy %1$s_erasure_reach_delete on gnomon.%1$I for delete to gnomon_definer
         using (tenant_id = gnomon.tenant_id()
                and gnomon.erasure_reaches_version(tenant_id, version_id))', t);
  end loop;
end
$$;

-- Deletes every row of these Nodes and Edges but the Node and Edge rows
-- themselves: their bound statements, Contributions, statements (whose
-- Contributions would cascade), Citations and Versions. Their text-derived
-- rows go first, through `gnomon.erase_version_text`. Ticket #16 adds the
-- Summaries and Communities that drew on a Node here, and ticket #17 the name
-- and Alias rows.
create function gnomon.erase_versioned_rows(p_tenant_id uuid, p_ids uuid[])
  returns void
  language plpgsql volatile security invoker set search_path = ''
  as $$
  begin
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

-- The Erasure the three definers share (see the header). It erases the Nodes
-- `p_nodes`, the Edges `p_edges`, and, when `p_scope` is given, every Episode,
-- Node and Edge of that Scope. The caller has checked that it sees them.
create function gnomon.erase_graph(
  p_tenant_id uuid,
  p_scope text,
  p_nodes uuid[],
  p_edges uuid[]
)
  returns table (
    erased_at timestamptz,
    erased_episodes uuid[],
    erased_nodes uuid[],
    erased_edges uuid[]
  )
  language plpgsql volatile security invoker set search_path = ''
  as $$
  declare
    v_episodes uuid[] := '{}';
    v_nodes uuid[];
    v_edges uuid[];
    v_lock uuid[];
    v_locked uuid[] := '{}';
    -- Whether the Episodes' rows are locked. An Erasure of a Node or an Edge
    -- erases no Episode, so it has none to lock.
    v_episodes_locked boolean := p_scope is null;
    v_candidates uuid[];
    v_bare uuid[] := '{}';
    v_seen_edges uuid[];
    v_id uuid;
  begin
    if not gnomon.running_as_definer() then
      raise exception using errcode = 'insufficient_privilege',
        message = 'only Erasure erases Nodes, Edges and Scopes';
    end if;

    -- 1. The Scope's Episodes, each locked as Retract Episode and Erase
    -- Episode lock one.
    if p_scope is not null then
      v_episodes := array(
        select e.id from gnomon.episode e
        where e.tenant_id = p_tenant_id and e.scope = p_scope order by e.id);
      foreach v_id in array v_episodes loop
        perform pg_catalog.pg_advisory_xact_lock(gnomon.node_lock_key(p_tenant_id, v_id));
      end loop;
    end if;

    -- 2 and 3. The Nodes and Edges, the Edges that join the Nodes in every
    -- Scope, and their endpoints, locked in order of ID; then the Episodes'
    -- rows. Each pass reads them again and locks what a write that held a
    -- lock first added, until nothing is new.
    loop
      v_nodes := array(
        select distinct x.id from (
          select unnest(p_nodes) as id
          union
          select n.id from gnomon.node n
            where p_scope is not null and n.tenant_id = p_tenant_id and n.scope = p_scope
        ) x order by x.id);
      insert into gnomon.erasure_reach (tenant_id, id)
        select p_tenant_id, x.id from unnest(v_nodes) x(id)
        on conflict do nothing;
      v_edges := array(
        select distinct x.id from (
          select unnest(p_edges) as id
          union
          select e.id from gnomon.edge e
            where p_scope is not null and e.tenant_id = p_tenant_id and e.scope = p_scope
          union
          select e.id from gnomon.edge e
            where e.tenant_id = p_tenant_id
              and (e.source_id = any(v_nodes) or e.target_id = any(v_nodes))
        ) x order by x.id);
      insert into gnomon.erasure_reach (tenant_id, id)
        select p_tenant_id, x.id from unnest(v_edges) x(id)
        on conflict do nothing;
      v_lock := array(
        select distinct l.id from (
          select unnest(v_nodes || v_edges) as id
          union
          select e.source_id from gnomon.edge e
            where e.tenant_id = p_tenant_id and e.id = any(v_edges)
          union
          select e.target_id from gnomon.edge e
            where e.tenant_id = p_tenant_id and e.id = any(v_edges)
        ) l
        where l.id <> all (v_locked)
        order by l.id);
      exit when cardinality(v_lock) = 0 and v_episodes_locked;
      foreach v_id in array v_lock loop
        perform pg_catalog.pg_advisory_xact_lock(gnomon.node_lock_key(p_tenant_id, v_id));
      end loop;
      v_locked := v_locked || v_lock;
      if not v_episodes_locked then
        perform from gnomon.episode e
          where e.tenant_id = p_tenant_id and e.id = any(v_episodes)
          for update;
        v_episodes_locked := true;
      end if;
      if p_scope is not null and exists (
        select from gnomon.episode e
        where e.tenant_id = p_tenant_id and e.scope = p_scope and e.id <> all (v_episodes)
      ) then
        raise exception using errcode = 'serialization_failure',
          message = format('an Episode was recorded in Scope %s while it was erased; '
                           'retry the Erasure', p_scope);
      end if;
    end loop;

    -- The erased Edges the caller can see, which are all it is told of, and
    -- the endpoints the erased Edges may leave with nothing, read before the
    -- Edges go.
    v_seen_edges := array(
      select e.id from gnomon.edge e
      where e.tenant_id = p_tenant_id and e.id = any(v_edges)
        and e.scope = any(gnomon.allowed_scopes())
      order by e.id);
    v_candidates := array(
      select distinct x.id from (
        select e.source_id as id from gnomon.edge e
          where e.tenant_id = p_tenant_id and e.id = any(v_edges)
        union
        select e.target_id from gnomon.edge e
          where e.tenant_id = p_tenant_id and e.id = any(v_edges)
      ) x
      where x.id <> all (v_nodes)
      order by x.id);

    perform gnomon.erase_versioned_rows(p_tenant_id, v_edges || v_nodes);
    delete from gnomon.edge e where e.tenant_id = p_tenant_id and e.id = any(v_edges);
    foreach v_id in array v_episodes loop
      perform gnomon.erase_episode_rows(p_tenant_id, v_id);
    end loop;
    delete from gnomon.node n where n.tenant_id = p_tenant_id and n.id = any(v_nodes);

    -- Each endpoint left with nothing, one at a time, as Erase Episode erases
    -- one. This repeats migration 0011's loop, which a released migration
    -- cannot share: a Node in a Scope the caller cannot see is not visible to the
    -- delete and stays, and one that an Edge in such a Scope still joins
    -- fails the foreign key and stays.
    foreach v_id in array v_candidates loop
      begin
        delete from gnomon.node n
          where n.tenant_id = p_tenant_id and n.id = v_id
            and not exists (select from gnomon.contribution c
                            where c.tenant_id = n.tenant_id and c.node_id = n.id)
            and not exists (select from gnomon.statement s
                            where s.tenant_id = n.tenant_id and s.node_id = n.id)
            and not exists (select from gnomon.bound_statement b
                            where b.tenant_id = n.tenant_id and b.node_id = n.id)
            and not exists (select from gnomon.version v
                            where v.tenant_id = n.tenant_id and v.node_id = n.id)
            and not exists (select from gnomon.edge e
                            where e.tenant_id = n.tenant_id
                              and (e.source_id = n.id or e.target_id = n.id));
        if found then
          v_bare := v_bare || v_id;
        end if;
      exception when foreign_key_violation then
        null;
      end;
    end loop;

    delete from gnomon.erasure_reach r
      where r.tenant_id = p_tenant_id and r.written_in = pg_current_xact_id();

    erased_at := transaction_timestamp();
    erased_episodes := v_episodes;
    erased_nodes := array(select u.id from unnest(v_nodes || v_bare) u(id) order by u.id);
    erased_edges := v_seen_edges;
    return next;
  end
  $$;

-- The Tenant check every Erasure makes first.
create function gnomon.erasure_check_tenant(p_tenant_id uuid) returns void
  language plpgsql stable security invoker set search_path = ''
  as $$
  begin
    if p_tenant_id is null or p_tenant_id is distinct from gnomon.tenant_id() then
      raise exception using errcode = 'insufficient_privilege',
        message = 'Erasure may erase only in the caller''s own Tenant';
    end if;
  end
  $$;

-- Erase a Node. It returns when the Erasure ran, the Episodes it erased
-- (none), the Nodes it erased (this one and each endpoint left with nothing)
-- and the Edges, each list in order of ID.
create function gnomon.erase_node(p_tenant_id uuid, p_node_id uuid)
  returns table (
    erased_at timestamptz,
    erased_episodes uuid[],
    erased_nodes uuid[],
    erased_edges uuid[]
  )
  language plpgsql volatile security definer set search_path = ''
  as $$
  begin
    perform gnomon.erasure_check_tenant(p_tenant_id);
    if not exists (select from gnomon.node n
                   where n.tenant_id = p_tenant_id and n.id = p_node_id) then
      raise exception using errcode = 'GNW01',
        message = format('Node %s is not visible to the caller', p_node_id);
    end if;
    return query select * from gnomon.erase_graph(p_tenant_id, null, array[p_node_id], '{}');
  end
  $$;

-- Erase an Edge, returning as Erase a Node does.
create function gnomon.erase_edge(p_tenant_id uuid, p_edge_id uuid)
  returns table (
    erased_at timestamptz,
    erased_episodes uuid[],
    erased_nodes uuid[],
    erased_edges uuid[]
  )
  language plpgsql volatile security definer set search_path = ''
  as $$
  begin
    perform gnomon.erasure_check_tenant(p_tenant_id);
    -- The caller's own policy decides, not the reach an Erasure in the same
    -- transaction might still hold.
    if not exists (select from gnomon.edge e
                   where e.tenant_id = p_tenant_id and e.id = p_edge_id
                     and e.scope = any(gnomon.allowed_scopes())) then
      raise exception using errcode = 'GNW01',
        message = format('Edge %s is not visible to the caller', p_edge_id);
    end if;
    return query select * from gnomon.erase_graph(p_tenant_id, null, '{}', array[p_edge_id]);
  end
  $$;

-- Erase a Scope, returning as Erase a Node does, with the Scope's Episodes.
create function gnomon.erase_scope(p_tenant_id uuid, p_scope text)
  returns table (
    erased_at timestamptz,
    erased_episodes uuid[],
    erased_nodes uuid[],
    erased_edges uuid[]
  )
  language plpgsql volatile security definer set search_path = ''
  as $$
  begin
    perform gnomon.erasure_check_tenant(p_tenant_id);
    if p_scope is null or not (p_scope = any(gnomon.allowed_scopes())) then
      raise exception using errcode = 'GNW01',
        message = format('Scope %s is not visible to the caller', p_scope);
    end if;
    return query select * from gnomon.erase_graph(p_tenant_id, p_scope, '{}', '{}');
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
      'gnomon.erasure_reaches(uuid, uuid[])'::regprocedure,
      'gnomon.erasure_reaches_version(uuid, uuid)'::regprocedure,
      'gnomon.erase_versioned_rows(uuid, uuid[])'::regprocedure,
      'gnomon.erase_graph(uuid, text, uuid[], uuid[])'::regprocedure,
      'gnomon.erasure_check_tenant(uuid)'::regprocedure,
      'gnomon.erase_node(uuid, uuid)'::regprocedure,
      'gnomon.erase_edge(uuid, uuid)'::regprocedure,
      'gnomon.erase_scope(uuid, text)'::regprocedure
    ) and a.grantee <> p.proowner
    union all
    select 'table gnomon.erasure_reach', a.grantee
    from pg_class c, aclexplode(c.relacl) a
    where c.oid = 'gnomon.erasure_reach'::regclass and a.grantee <> c.relowner
  loop
    execute format(
      'revoke all on %s from %s', g.object,
      case when g.grantee = 0 then 'public' else g.grantee::regrole::text end);
  end loop;
  revoke all on function gnomon.erasure_reaches(uuid, uuid[]) from public;
  revoke all on function gnomon.erasure_reaches_version(uuid, uuid) from public;
  revoke all on function gnomon.erase_versioned_rows(uuid, uuid[]) from public;
  revoke all on function gnomon.erase_graph(uuid, text, uuid[], uuid[]) from public;
  revoke all on function gnomon.erasure_check_tenant(uuid) from public;
  revoke all on function gnomon.erase_node(uuid, uuid) from public;
  revoke all on function gnomon.erase_edge(uuid, uuid) from public;
  revoke all on function gnomon.erase_scope(uuid, text) from public;
  revoke all on gnomon.erasure_reach from public;
end
$$;

-- Hand the three Erasures to the definer role, as migration 0011 hands it
-- Erase Episode.
do $$
declare
  joined boolean := not pg_has_role(current_user, 'gnomon_definer', 'SET');
begin
  if joined then
    execute format('grant gnomon_definer to %I', current_user);
  end if;
  grant create on schema gnomon to gnomon_definer;
  alter function gnomon.erase_node(uuid, uuid) owner to gnomon_definer;
  alter function gnomon.erase_edge(uuid, uuid) owner to gnomon_definer;
  alter function gnomon.erase_scope(uuid, text) owner to gnomon_definer;
  revoke create on schema gnomon from gnomon_definer;
  if joined then
    execute format('revoke gnomon_definer from %I', current_user);
  end if;
end
$$;

-- What these Erasures need as gnomon_definer beside what migration 0011
-- granted it. Callers get none of this.
grant select, insert, delete on gnomon.erasure_reach to gnomon_definer;
grant execute on function gnomon.erasure_reaches(uuid, uuid[]) to gnomon_definer;
grant execute on function gnomon.erasure_reaches_version(uuid, uuid) to gnomon_definer;
grant execute on function gnomon.erase_versioned_rows(uuid, uuid[]) to gnomon_definer;
grant execute on function gnomon.erase_graph(uuid, text, uuid[], uuid[]) to gnomon_definer;
grant execute on function gnomon.erasure_check_tenant(uuid) to gnomon_definer;

insert into gnomon.schema_version (version, name) values (12, '0012_erase_node_edge_scope');
