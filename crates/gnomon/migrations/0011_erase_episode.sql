-- gnomon migration 0011: Erase Episode.
--
-- Erasure physically deletes an Episode and leaves no tombstone (ADR-0003).
-- In one transaction it deletes the Episode, its Contributions, its
-- statements, its Citations and every Version derived from it alone. It then
-- recomputes every surviving Version the Episode touched, current and
-- closed, from the Contributions that remain (ADR-0007). It is the only route
-- that deletes or rewrites committed history.
--
-- Erasure is one of the four SECURITY DEFINER groups ADR-0009 allows,
-- beside Kind registration, the merge-rule setter and the search index
-- trigger. Like them it is owned
-- by the NOLOGIN `gnomon_definer` and checks the Tenant against
-- `gnomon.tenant_id()`. This migration grants it to no caller role: each
-- deployment grants it to the roles it chooses.
--
-- Recompute is replay, from prototype 4 (`prototype/recompute-cost`,
-- FINDINGS.md). For each Node and Edge the Episode touched:
--
--   1. T0 is the earliest Recorded time at which the Episode touched it.
--      History before T0 owes nothing to the Episode and stays as it is.
--   2. The Episode's rows are deleted (`gnomon.erase_episode_rows`).
--   3. Every Version recorded from T0 on is deleted, with its Citations.
--      Every Version current at T0 is reopened and loses the Citations added
--      from T0 on. The Node or Edge now stands as it stood just before T0.
--   4. Every Recorded time from T0 on at which something that remains was
--      written to the Node or Edge is a step. Each step runs the write
--      path's merge, `gnomon.merge_versions`, at that Recorded time. It
--      merges over the Valid time that step's write named, reads only the
--      rows recorded by then, and uses the merge rule that step's Versions
--      recorded (ADR-0007). A rule change applies to later writes only, so a
--      history can mix rules, and each step keeps the rule it had. A step
--      that opened no Version takes the rule the Kind had at that Recorded
--      time.
--
-- The result is the history as if the Episode had never existed. A statement
-- that lost to a value the erased Episode alone supplied, such as a
-- lower-Authority End of validity, takes effect at the step that recorded
-- it, because the merge no longer finds the value it lost to. Erasing the
-- Episode behind a Retraction makes the retracted Version current again,
-- because the Retraction's null Contributions are gone. As-of reads at a
-- past known-at instant are stable except across an Erasure, which rewrites
-- Recorded-time history (ADR-0007).
--
-- Another Episode's Retraction of a Version that only the erased Episode
-- cites is deleted with it. Its Valid time is that Version's, so it would
-- keep a value of the erased Episode's and blank other values over it. As if
-- the Episode never existed, the Version it retracts was never there. A bound
-- statement of another Episode about the erased Episode's Contributions goes
-- too. Another Episode's End of validity stays, because the instant it states
-- is its own, and it takes effect once the values it lost to are gone.
--
-- A Citation of a retracted Episode counts as a Citation of another Episode.
-- Its value Contributions still count in the merge, as Retract Episode left
-- them (migration 0010). But Retract Episode retracted only the Versions that
-- the Episode alone cited, and a Version that the erased Episode also cited
-- was not one of them. So the replay makes Retract Episode's choice again at
-- the step where the Episode was retracted. Each current Version there that
-- only the retracted Episode cites gets the Retraction Retract Episode would
-- have made. A retracted Episode's value is never left current because
-- Erasure removed the Episode that corroborated it.
--
-- Erasure also deletes what was derived from the text of every Version it
-- deletes or recomputes, which is every Version of an affected Node or Edge
-- held at or after T0. Today that is the Sentence of each such Edge Version
-- (ADR-0013). The Pipeline writes a new Sentence afterwards.
--
-- A Node or Edge the Episode touched that is left with no Contribution, and
-- so with no Version, is erased with it. A Node left so stays as a bare
-- identity while any Edge joins it, and is erased with its last Edge
-- (ADR-0007). An endpoint of an Edge the Erasure erases goes too when nothing
-- is left of it and no other Edge joins it. A Node no Edge joins and the
-- Episode never touched is left alone, bare or not.
--
-- Erasure acts in the caller's Tenant and Scopes only. The definer role is
-- under RLS, and this migration gives it the same Tenant and Scope policies
-- as the callers. An endpoint Node in a Scope the caller cannot see is never
-- erased and stays bare. A Node joined by an Edge in a Scope the caller
-- cannot see stays too, because the Edge still joins it.
--
-- Locks (ADR-0007). An Erasure takes these, for the rest of its transaction:
--
--   1. the advisory lock of the Episode's ID, which Retract Episode also
--      takes;
--   2. the advisory lock (`gnomon.node_lock_key`) of every Node and Edge the
--      Episode touched and of both endpoints of every such Edge, in order of
--      their IDs, so two Erasures, or an Erasure and a Retract Episode,
--      cannot deadlock;
--   3. the Episode's row FOR UPDATE. A write takes its Node's or Edge's lock
--      and then, through a Contribution's foreign key, a share lock on the
--      Episode's row, so this is the same order. Taking the row waits for
--      every write citing the Episode that is still running. The Erasure
--      then locks whatever those writes touched, and from then on a write
--      that cites the Episode waits for the Erasure and fails its foreign key.
--
-- Every write to a Node or Edge takes that Node's or Edge's lock. From this
-- migration, creating an Edge takes the locks of both its endpoints, in
-- order of their IDs. A write to an affected Node therefore waits until the
-- Erasure commits, and a write to an unaffected one does not. An Edge cannot
-- join a Node while an Erasure decides whether to erase it. Reads never wait.
-- A transaction that writes several Nodes takes their locks in the order it
-- writes them, so it can still deadlock with an Erasure. Postgres detects
-- the deadlock and fails one of them, and the Store retries it.
--
-- Each advisory lock is an entry in the server's shared lock table, which
-- holds `max_locks_per_transaction * (max_connections +
-- max_prepared_transactions)` entries for every session together. One
-- transaction may hold more than `max_locks_per_transaction` while the table
-- has room. An Erasure holds one advisory lock for the Episode, one for each
-- Node and Edge it touched and one for each endpoint of those Edges, beside
-- its ordinary table locks. An Erasure that needs more than the table has
-- free fails with SQLSTATE 53200 (out of shared memory). It rolls back and
-- erases nothing. The locks are not batched, because a batch would have to
-- commit, and an Erasure is one transaction. A deployment that erases
-- Episodes cited across many thousands of Nodes raises
-- `max_locks_per_transaction` first. Prototype 4 needed 1024 for 500 Nodes
-- on a server with few connections.
--
-- The guard triggers (ADR-0009) refuse every caller's delete or rewrite of
-- committed history. Erasure passes them because it runs as `gnomon_definer`.
-- `gnomon.running_as_definer()` is true only when the current user is that
-- role. No caller role can become it, because it has no members and never
-- logs in, and no setting a caller can change affects the check. The
-- Versions, Citations, statements and Contributions the replay writes carry
-- the Recorded times of the steps they replay. Their transaction mark,
-- `written_in`, and the `closed_in` of each Version the replay closes are
-- '0', which no transaction ever has. So no transaction, not even the
-- Erasure's own, may delete or reopen them afterwards.
--
-- An Erasure runs in a transaction of its own. It refuses to erase when its
-- transaction already wrote to a Node or Edge the Episode touched. It fails
-- with a serialization failure, which the caller retries, when a transaction
-- that started later wrote one first.
--
-- Later tables extend Erasure in two places, and only there:
--
--   `gnomon.erase_episode_rows`  every row that names the Episode: stage
--                                records and usage counts (#24), Summary and
--                                Community Citations (#16).
--   `gnomon.erase_version_text`  every row derived from a Version's text:
--                                token rows and BM25 counts (#17), full and
--                                quantized embeddings (#18).

-- Whether the current user is the NOLOGIN definer role, which is true only
-- inside a function that role owns. Erasure is the one such function that
-- writes the tables whose guards this lets past. Kind registration and the
-- merge-rule setter run as the role too, and write only the Ontology.
create function gnomon.running_as_definer() returns boolean
  language sql stable parallel safe security invoker set search_path = ''
  as $$ select current_user = 'gnomon_definer'::name $$;

-- The guard triggers stand aside for Erasure and for nothing else. Each is
-- recreated with the condition, so its function, and its checks for every
-- caller, stay as earlier migrations wrote them.
drop trigger version_check_insert on gnomon.version;
create trigger version_check_insert before insert on gnomon.version
  for each row when (not gnomon.running_as_definer())
  execute function gnomon.version_check_insert();
drop trigger version_guard_update on gnomon.version;
create trigger version_guard_update before update on gnomon.version
  for each row when (not gnomon.running_as_definer())
  execute function gnomon.version_guard_update();
drop trigger version_guard_delete on gnomon.version;
create trigger version_guard_delete before delete on gnomon.version
  for each row when (not gnomon.running_as_definer())
  execute function gnomon.guard_delete('Version');
drop trigger citation_guard_delete on gnomon.citation;
create trigger citation_guard_delete before delete on gnomon.citation
  for each row when (not gnomon.running_as_definer())
  execute function gnomon.guard_delete('Citation');
drop trigger citation_check_insert on gnomon.citation;
create trigger citation_check_insert before insert on gnomon.citation
  for each row when (not gnomon.running_as_definer())
  execute function gnomon.citation_check_insert();
drop trigger contribution_check_insert on gnomon.contribution;
create trigger contribution_check_insert before insert on gnomon.contribution
  for each row when (not gnomon.running_as_definer())
  execute function gnomon.contribution_check_insert();
drop trigger statement_check_insert on gnomon.statement;
create trigger statement_check_insert before insert on gnomon.statement
  for each row when (not gnomon.running_as_definer())
  execute function gnomon.statement_check_insert();

-- At commit, every Version a caller inserted that still stands has a
-- Citation, so no caller can commit a Version that no Episode cites: Erase
-- Episode starts from an Episode's Citations and would never find it. The
-- write path inserts each Version's Citations in the same transaction, so
-- this finds one at once. A Version the transaction replaced is gone and
-- needs none. Which rows the check sees depends on the claims at commit, so
-- a caller whose claims no longer reach the Version's Tenant and Scope is
-- refused rather than let through. The trigger's condition is evaluated at
-- insert, so a Version Erasure writes as the definer is never queued, and
-- Erasure may delete Citations as its replay requires.
create function gnomon.version_check_cited() returns trigger
  language plpgsql security invoker set search_path = ''
  as $$
  begin
    if exists (select from gnomon.citation c
               where c.tenant_id = new.tenant_id and c.scope = new.scope
                 and c.version_id = new.id) then
      return null;
    end if;
    if new.tenant_id is distinct from gnomon.tenant_id()
       or not new.scope = any(gnomon.allowed_scopes())
       or exists (select from gnomon.version v
                  where v.tenant_id = new.tenant_id and v.id = new.id) then
      raise exception using errcode = 'GNW01',
        message = pg_catalog.format('Version %s cites no Episode', new.id);
    end if;
    return null;
  end
  $$;
create constraint trigger version_check_cited after insert on gnomon.version
  deferrable initially deferred
  for each row when (not gnomon.running_as_definer())
  execute function gnomon.version_check_cited();

-- During an Erasure a Version still changes only in its Recorded-time end.
-- One the replay closes is marked closed by no transaction, and one it
-- reopens is open again.
create function gnomon.version_erasure_update() returns trigger
  language plpgsql security invoker set search_path = ''
  as $$
  begin
    if (new.tenant_id, new.scope, new.id, new.node_id, new.edge_id, new.props, new.valid_from,
        new.valid_from_precision, new.valid_to, new.valid_to_precision, new.recorded_from,
        new.merge_rule, new.written_in)
      is distinct from
       (old.tenant_id, old.scope, old.id, old.node_id, old.edge_id, old.props, old.valid_from,
        old.valid_from_precision, old.valid_to, old.valid_to_precision, old.recorded_from,
        old.merge_rule, old.written_in)
    then
      raise exception using errcode = 'insufficient_privilege',
        message = 'a Version is immutable apart from the end of its Recorded time';
    end if;
    new.closed_in := case when new.recorded_to = 'infinity' then null else '0'::xid8 end;
    return new;
  end
  $$;
create trigger version_erasure_update before update on gnomon.version
  for each row when (gnomon.running_as_definer())
  execute function gnomon.version_erasure_update();

-- Creating an Edge takes the locks of both its endpoints, in order of their
-- IDs, as an Erasure does (see the header). The trigger's name sorts before
-- `edge_check_insert`, so the check runs after the lock and sees an endpoint
-- an Erasure erased as gone.
create function gnomon.edge_await_endpoints() returns trigger
  language plpgsql security invoker set search_path = ''
  as $$
  declare
    x uuid;
  begin
    for x in select distinct e.id from unnest(array[new.source_id, new.target_id]) e(id)
             where e.id is not null order by e.id
    loop
      perform pg_catalog.pg_advisory_xact_lock(gnomon.node_lock_key(new.tenant_id, x));
    end loop;
    return new;
  end
  $$;
create trigger edge_await_endpoints before insert on gnomon.edge
  for each row execute function gnomon.edge_await_endpoints();

-- The merge rule a Kind had at a Recorded time: as `gnomon.kind_merge_rule`,
-- reading only the rule changes made by then.
create function gnomon.kind_merge_rule_at(p_tenant_id uuid, p_kind text, p_at timestamptz)
  returns text
  language sql stable security invoker set search_path = ''
  as $$
    select case when exists (
      select from gnomon.kind k
      cross join lateral unnest(k.ancestors || k.name) a(name)
      where k.tenant_id = p_tenant_id and k.name = p_kind
        and (select m.merge_rule from gnomon.kind_merge m
              where m.tenant_id = k.tenant_id and m.kind = a.name and m.set_at <= p_at
              order by m.id desc limit 1) = 'ranked'
    ) then 'ranked' else 'recency' end
  $$;

-- The merge of migration 0010, at a chosen Recorded time and under a chosen
-- rule, so that the write path and Erasure share it (ADR-0007). It reads only
-- the Contributions, bound statements and Episode Retractions recorded by
-- `p_at`, and recomputes the open Versions of a Node or an Edge over a
-- Valid-time window as migration 0010's `recompute_versions` did:
--
--   p_at      the step's Recorded time. The write path passes the start of
--             its transaction; an Erasure passes the step it replays.
--   p_rule    `recency` or `ranked`. The Versions the step opens record it.
--   p_replay  false for the write path. A Version or Citation the current
--             step wrote is one its transaction wrote, and a Version the step
--             closed is one its transaction closed, as before.
--             true for an Erasure's replay, which runs as `gnomon_definer`
--             past the guards: the step wrote what was recorded at `p_at`
--             and closed what it closed at `p_at`, and what it writes is
--             recorded at `p_at` and marked written by no transaction.
--
-- A write takes the Node's or Edge's lock here, and is refused when a
-- transaction that started later wrote the Node or Edge first, so a caller
-- that calls the merge directly gets the write path's guarantees. An Erasure
-- has taken its locks already.
create function gnomon.merge_versions(
  p_tenant_id uuid,
  p_scope text,
  p_id uuid,
  p_is_edge boolean,
  p_from timestamptz,
  p_to timestamptz,
  p_at timestamptz,
  p_rule text,
  p_replay boolean,
  out changed boolean,
  out opened uuid[]
)
  language plpgsql volatile security invoker set search_path = ''
  as $$
  declare
    t constant timestamptz := p_at;
    me constant xid8 := pg_current_xact_id();
    v_scope constant text := p_scope;
    v_ranked constant boolean := p_rule = 'ranked';
    w0 timestamptz;
    w1 timestamptz;
    candidates jsonb;
    v_fresh jsonb;
    stale uuid[];
    -- The statements and bound statements of this Node or Edge that belong to
    -- an Episode retracted by `p_at`, which the merge leaves out.
    ignored_statements bigint[];
    ignored_bounds bigint[];
  begin
    if p_rule is null or p_rule not in ('recency', 'ranked') then
      raise exception using errcode = 'GNW01', message = 'a merge rule is recency or ranked';
    end if;
    if p_replay and not gnomon.running_as_definer() then
      raise exception using errcode = 'insufficient_privilege',
        message = 'only Erasure replays history';
    end if;
    if not p_replay then
      if p_at is distinct from transaction_timestamp() then
        raise exception using errcode = 'insufficient_privilege',
          message = 'a write merges at the start of its own transaction';
      end if;
      if (select l.scope from gnomon.lock_for_write(p_tenant_id, p_id, p_is_edge) l)
           is distinct from p_scope then
        raise exception using errcode = 'GNW01',
          message = format('%s %s is not in Scope %s',
                           case when p_is_edge then 'Edge' else 'Node' end, p_id, p_scope);
      end if;
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
          message = format('a later transaction wrote %s %s first; retry the write',
                           case when p_is_edge then 'Edge' else 'Node' end, p_id);
      end if;
    end if;
    select coalesce(array_agg(st.id), '{}') into ignored_statements
      from gnomon.statement st
      where st.tenant_id = p_tenant_id and st.scope = v_scope and st.versioned_id = p_id
        and not st.retracts_episode and st.recorded_at <= t
        and exists (select from gnomon.episode_retraction r
                    where r.tenant_id = st.tenant_id and r.episode_id = st.episode_id
                      and r.recorded_at <= t);
    select coalesce(array_agg(b.id), '{}') into ignored_bounds
      from gnomon.bound_statement b
      where b.tenant_id = p_tenant_id and b.scope = v_scope and b.versioned_id = p_id
        and b.recorded_at <= t
        and exists (select from gnomon.episode_retraction r
                    where r.tenant_id = b.tenant_id and r.episode_id = b.episode_id
                      and r.recorded_at <= t);

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
               coalesce(u.bound_at, c.valid_to) as vt,
               coalesce(u.bound_precision, c.valid_to_precision) as vtp
        from (select 1) one
        left join lateral (
          select s.bound_at, s.bound_precision from gnomon.bound_statement s
          join gnomon.episode se on se.tenant_id = s.tenant_id and se.id = s.episode_id
          where s.tenant_id = c.tenant_id and s.contribution_id = c.id and s.side = 'from'
            and s.recorded_at <= t
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
            and s.recorded_at <= t
            and s.id <> all (ignored_bounds)
            and (not v_ranked or coalesce(se.authority, 0) >= coalesce(ce.authority, 0))
          order by case when v_ranked then coalesce(se.authority, 0) else 0 end desc,
                   case when v_ranked then s.episode_reference_time end desc nulls last,
                   case when v_ranked then s.recorded_at end desc nulls last,
                   s.id desc
          limit 1
        ) u on true
      ) e
      where c.tenant_id = p_tenant_id and c.scope = v_scope and c.versioned_id = p_id
        and c.recorded_at <= t
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
      -- whether the step recorded it.
      select w.sf, w.property, c.episode_id, c.place, c.ref, c.episode_rec,
             bool_or(c.rec = t) as fresh
      from winners w
      join contributions c
        on c.property = w.property and c.value = w.value and c.vf <= w.sf and c.vt >= w.st
      where w.value <> 'null'::jsonb
      group by w.sf, w.property, c.episode_id, c.place, c.ref, c.episode_rec
    ), segment_values as (
      select sf, max(st) as st,
             coalesce(
               jsonb_object_agg(property, value)
                 filter (where value <> 'null'::jsonb and not (p_is_edge and property = 'holds')),
               '{}'::jsonb) as props,
             case when p_is_edge then coalesce(bool_or(property = 'holds' and value <> 'null'::jsonb), false)
                  else bool_or(value <> 'null'::jsonb) end as held
      from winners group by sf
    ), islands as (
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

    -- 4. The stale Versions. One the step wrote is deleted with its
    -- Citations, so no empty Recorded time is left; every other is closed at
    -- the step and loses the Citations the step added to it.
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
      where x.tenant_id = p_tenant_id and x.version_id = any(stale)
        and case when p_replay then x.recorded_at = t else x.written_in = me end;
    delete from gnomon.version v
      where v.tenant_id = p_tenant_id and v.id = any(stale)
        and case when p_replay then v.recorded_from = t else v.written_in = me end;
    update gnomon.version v set recorded_to = t
      where v.tenant_id = p_tenant_id and v.id = any(stale);

    -- 5. Reopen what the step closed and now restores, and insert the rest.
    -- The new Versions are those no open Version, and none the step closed,
    -- already holds, found before the reopening.
    select coalesce(jsonb_agg(cand.c), '[]') into v_fresh
      from jsonb_array_elements(candidates) cand(c)
      where not exists (
        select from gnomon.version v
        where v.tenant_id = p_tenant_id and v.scope = v_scope and v.versioned_id = p_id
          and v.valid_from = (cand.c ->> 'vf')::timestamptz
          and v.valid_to = (cand.c ->> 'vt')::timestamptz
          and v.props = cand.c -> 'props'
          and v.valid_from_precision::text = cand.c ->> 'vfp'
          and v.valid_to_precision::text = cand.c ->> 'vtp'
          and (v.recorded_to = 'infinity'
               or (v.recorded_to = t and (p_replay or v.closed_in = me)))
      );
    with reopened as (
      update gnomon.version v set recorded_to = 'infinity'
        from jsonb_to_recordset(candidates)
          cand(vf timestamptz, vt timestamptz, vfp text, vtp text, props jsonb)
        where v.tenant_id = p_tenant_id and v.scope = v_scope and v.versioned_id = p_id
          and v.recorded_to = t and (p_replay or v.closed_in = me)
          and v.valid_from = cand.vf and v.valid_to = cand.vt and v.props = cand.props
          and v.valid_from_precision::text = cand.vfp and v.valid_to_precision::text = cand.vtp
        returning v.id
    )
    select coalesce(array_agg(id), '{}') into opened from reopened;
    if p_replay then
      with inserted as (
        insert into gnomon.version (
          tenant_id, scope, node_id, edge_id, props, valid_from, valid_from_precision,
          valid_to, valid_to_precision, merge_rule, recorded_from, recorded_to, written_in
        )
        select p_tenant_id, v_scope,
               case when not p_is_edge then p_id end, case when p_is_edge then p_id end,
               f.props, f.vf, f.vfp::gnomon.time_precision, f.vt, f.vtp::gnomon.time_precision,
               p_rule::gnomon.merge_rule, t, 'infinity', '0'::xid8
        from jsonb_to_recordset(v_fresh)
          f(vf timestamptz, vt timestamptz, vfp text, vtp text, props jsonb)
        returning id
      )
      select opened || coalesce(array_agg(id), '{}') into opened from inserted;
    else
      -- A caller may not name a Recorded time or a transaction mark, so the
      -- write path leaves them to the defaults.
      with inserted as (
        insert into gnomon.version (
          tenant_id, scope, node_id, edge_id, props, valid_from, valid_from_precision,
          valid_to, valid_to_precision, merge_rule
        )
        select p_tenant_id, v_scope,
               case when not p_is_edge then p_id end, case when p_is_edge then p_id end,
               f.props, f.vf, f.vfp::gnomon.time_precision, f.vt, f.vtp::gnomon.time_precision,
               p_rule::gnomon.merge_rule
        from jsonb_to_recordset(v_fresh)
          f(vf timestamptz, vt timestamptz, vfp text, vtp text, props jsonb)
        returning id
      )
      select opened || coalesce(array_agg(id), '{}') into opened from inserted;
    end if;
    changed := changed or cardinality(opened) > 0;

    -- 6. The Citations.
    if p_replay then
      insert into gnomon.citation (
        tenant_id, scope, version_id, episode_id, property, place,
        episode_reference_time, episode_recorded_at, recorded_at, written_in
      )
        select p_tenant_id, v_scope, v.id, (cc ->> 1)::uuid, cc ->> 0, cc ->> 2,
               (cc ->> 4)::timestamptz, (cc ->> 5)::timestamptz, t, '0'::xid8
        from jsonb_to_recordset(candidates) cand(vf timestamptz, vt timestamptz, cites jsonb)
        join gnomon.version v
          on v.tenant_id = p_tenant_id and v.scope = v_scope and v.versioned_id = p_id
         and v.recorded_to = 'infinity'
         and v.valid_from = cand.vf and v.valid_to = cand.vt
        cross join lateral jsonb_array_elements(cand.cites) cc
        where v.id = any(opened) or (cc ->> 3)::boolean
        on conflict do nothing;
    else
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
    end if;
  end
  $$;

-- The write path's recompute, as migration 0010 left it: it merges at the
-- start of its transaction under the rule the Kind has now, and the merge
-- locks the Node or Edge and refuses a write that a later transaction
-- overtook.
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
    v_scope text;
    v_kind text;
    step record;
  begin
    select l.scope, l.kind into v_scope, v_kind
      from gnomon.lock_for_write(p_tenant_id, p_id, p_is_edge) l;
    select * into step from gnomon.merge_versions(
      p_tenant_id, v_scope, p_id, p_is_edge, p_from, p_to, transaction_timestamp(),
      gnomon.kind_merge_rule(p_tenant_id, v_kind), false);
    changed := step.changed;
    opened := step.opened;
  end
  $$;

-- Deletes every row derived from the text of the Versions an Erasure deletes
-- or recomputes. The Sentence of an Edge Version is today's (ADR-0013); a
-- deleted Version's would go by its foreign key too, and a recomputed one's
-- would not. Tickets #17 and #18 add their token rows, BM25 counts and
-- embeddings here.
create function gnomon.erase_version_text(p_tenant_id uuid, p_versions uuid[])
  returns void
  language sql volatile security invoker set search_path = ''
  as $$
    delete from gnomon.sentence s
    where s.tenant_id = p_tenant_id and s.version_id = any(p_versions);
  $$;

-- Deletes every row that names an Episode or is about what it alone said,
-- and then the Episode. It runs before the replay, while the Citations still
-- show what the Episode alone supplied:
--
--   1. Every other Episode's Retraction of a Version that only this Episode
--      cites. Its Valid time is that Version's, so it would keep a value of
--      this Episode's, and as if the Episode never existed the Version it
--      retracts was never there. An End of validity stays: the instant it
--      states is its own Episode's, and once this Episode's values are gone
--      it takes effect (ADR-0007).
--   2. The bound statements: this Episode's, and every other Episode's about
--      this Episode's Contributions, which name them.
--   3. Its Contributions, its statements (whose Contributions would
--      cascade), its Citations and its Retraction marker.
--
-- Tickets #24 and #16 add their stage records, usage counts and Summary and
-- Community Citations here.
create function gnomon.erase_episode_rows(p_tenant_id uuid, p_episode_id uuid)
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
    delete from gnomon.episode e
      where e.tenant_id = p_tenant_id and e.id = p_episode_id;
  end
  $$;

-- Replays one Node's or Edge's history from T0 (see the header). `p_rules`
-- maps each Recorded time at which the Node or Edge's Versions were opened
-- to the rule they recorded, read before the Erasure deleted them.
create function gnomon.replay_versions(
  p_tenant_id uuid,
  p_scope text,
  p_id uuid,
  p_is_edge boolean,
  p_kind text,
  p_t0 timestamptz,
  p_rules jsonb
) returns void
  language plpgsql volatile security invoker set search_path = ''
  as $$
  declare
    s record;
    w record;
    r record;
    v record;
    v_rule text;
    v_whole boolean;
    v_statement bigint;
  begin
    if not gnomon.running_as_definer() then
      raise exception using errcode = 'insufficient_privilege',
        message = 'only Erasure replays history';
    end if;
    for s in
      select x.at from (
        select c.recorded_at as at from gnomon.contribution c
          where c.tenant_id = p_tenant_id and c.scope = p_scope and c.versioned_id = p_id
            and c.recorded_at >= p_t0
        union
        select b.recorded_at from gnomon.bound_statement b
          where b.tenant_id = p_tenant_id and b.scope = p_scope and b.versioned_id = p_id
            and b.recorded_at >= p_t0
        union
        select er.recorded_at from gnomon.episode_retraction er
          where er.tenant_id = p_tenant_id and er.recorded_at >= p_t0
            and (exists (select from gnomon.contribution c
                         where c.tenant_id = er.tenant_id and c.episode_id = er.episode_id
                           and c.scope = p_scope and c.versioned_id = p_id)
                 or exists (select from gnomon.bound_statement b
                            where b.tenant_id = er.tenant_id and b.episode_id = er.episode_id
                              and b.scope = p_scope and b.versioned_id = p_id))
      ) x
      order by x.at
    loop
      v_rule := coalesce(p_rules ->> s.at::text, gnomon.kind_merge_rule_at(p_tenant_id, p_kind, s.at));

      -- A bound statement can move a bound of the Contribution it names
      -- anywhere, and an Episode Retraction stops statements counting
      -- wherever they are, so such a step recomputes the whole Valid time.
      -- Every other step recomputes each Valid time its writes named, in the
      -- order they were written.
      v_whole := exists (
          select from gnomon.bound_statement b
          where b.tenant_id = p_tenant_id and b.scope = p_scope and b.versioned_id = p_id
            and b.recorded_at = s.at)
        or exists (
          select from gnomon.episode_retraction er
          where er.tenant_id = p_tenant_id and er.recorded_at = s.at
            and (exists (select from gnomon.contribution c
                         where c.tenant_id = er.tenant_id and c.episode_id = er.episode_id
                           and c.scope = p_scope and c.versioned_id = p_id)
                 or exists (select from gnomon.bound_statement b
                            where b.tenant_id = er.tenant_id and b.episode_id = er.episode_id
                              and b.scope = p_scope and b.versioned_id = p_id)));
      for w in
        select '-infinity'::timestamptz as vf, 'infinity'::timestamptz as vt, 0::bigint as first
          where v_whole
        union all
        select c.valid_from, c.valid_to, min(c.id)
          from gnomon.contribution c
          where not v_whole
            and c.tenant_id = p_tenant_id and c.scope = p_scope and c.versioned_id = p_id
            and c.recorded_at = s.at
          group by c.valid_from, c.valid_to
        order by 3
      loop
        perform gnomon.merge_versions(
          p_tenant_id, p_scope, p_id, p_is_edge, w.vf, w.vt, s.at, v_rule, true);
      end loop;

      -- Retract Episode's choice, made again for each Episode retracted at
      -- this step: each current Version that only it cites gets the
      -- Retraction Retract Episode makes (migration 0010), recorded at the
      -- step. One that Retract Episode already made and that lost under
      -- ranked merge is not made twice.
      for r in
        select er.episode_id, e.reference_time, e.recorded_at
          from gnomon.episode_retraction er
          join gnomon.episode e on e.tenant_id = er.tenant_id and e.id = er.episode_id
          where er.tenant_id = p_tenant_id and er.recorded_at = s.at
            and exists (select from gnomon.contribution c
                        where c.tenant_id = er.tenant_id and c.episode_id = er.episode_id
                          and c.scope = p_scope and c.versioned_id = p_id)
          order by er.episode_id
      loop
        for v in
          select x.id from gnomon.version x
          where x.tenant_id = p_tenant_id and x.scope = p_scope and x.versioned_id = p_id
            and x.recorded_to = 'infinity'
            and exists (select from gnomon.citation c
                        where c.tenant_id = x.tenant_id and c.version_id = x.id
                          and c.episode_id = r.episode_id)
            and not exists (select from gnomon.citation c
                            where c.tenant_id = x.tenant_id and c.version_id = x.id
                              and c.episode_id <> r.episode_id)
          order by x.valid_from, x.id
        loop
          select * into v from gnomon.version x
            where x.tenant_id = p_tenant_id and x.id = v.id and x.recorded_to = 'infinity';
          if not found or exists (
            select from gnomon.statement st
            where st.tenant_id = p_tenant_id and st.scope = p_scope and st.versioned_id = p_id
              and st.episode_id = r.episode_id and st.retracts_episode and st.recorded_at = s.at
              and st.valid_from <= v.valid_from and st.valid_to >= v.valid_to
          ) then
            continue;
          end if;
          insert into gnomon.statement (
            tenant_id, scope, node_id, edge_id, episode_id, kind, version_id,
            valid_from, valid_from_precision, valid_to, valid_to_precision,
            episode_reference_time, episode_recorded_at, retracts_episode, recorded_at
          ) values (
            p_tenant_id, p_scope, case when not p_is_edge then p_id end,
            case when p_is_edge then p_id end, r.episode_id, 'retraction', v.id,
            v.valid_from, v.valid_from_precision, v.valid_to, v.valid_to_precision,
            r.reference_time, r.recorded_at, true, s.at
          ) returning id into v_statement;
          insert into gnomon.contribution (
            tenant_id, scope, node_id, edge_id, episode_id, property, value,
            valid_from, valid_from_precision, valid_to, valid_to_precision,
            episode_reference_time, episode_recorded_at, statement_id, recorded_at
          )
          select p_tenant_id, p_scope,
                 case when not p_is_edge then p_id end, case when p_is_edge then p_id end,
                 r.episode_id, x.property, 'null'::jsonb,
                 v.valid_from, v.valid_from_precision, v.valid_to, v.valid_to_precision,
                 r.reference_time, r.recorded_at, v_statement, s.at
          from (
            select k as property from jsonb_object_keys(v.props) k
            union
            select b.property from gnomon.edge_builtin_rules() b where p_is_edge
          ) x
          order by x.property;
          perform gnomon.merge_versions(
            p_tenant_id, p_scope, p_id, p_is_edge, v.valid_from, v.valid_to, s.at, v_rule, true);
        end loop;
      end loop;
    end loop;
  end
  $$;

-- Erase Episode. It returns when the Erasure ran, the Nodes and Edges it
-- recomputed, and the Nodes and Edges it erased because nothing was left of
-- them, each list in order of ID. The header says what it deletes, which
-- locks it takes and how it replays.
create function gnomon.erase_episode(p_tenant_id uuid, p_episode_id uuid)
  returns table (
    erased_at timestamptz,
    recomputed_nodes uuid[],
    recomputed_edges uuid[],
    erased_nodes uuid[],
    erased_edges uuid[]
  )
  language plpgsql volatile security definer set search_path = ''
  as $$
  declare
    t constant timestamptz := transaction_timestamp();
    v_scope text;
    v_touched uuid[];
    v_plan jsonb := '[]';
    it record;
    v_text uuid[];
    v_edges uuid[] := '{}';
    v_nodes uuid[] := '{}';
    v_lock uuid[];
    v_locked uuid[] := '{}';
    v_row_locked boolean := false;
    v_candidates uuid[];
    v_id uuid;
  begin
    if p_tenant_id is null or p_tenant_id is distinct from gnomon.tenant_id() then
      raise exception using errcode = 'insufficient_privilege',
        message = 'Erasure may erase only in the caller''s own Tenant';
    end if;
    perform pg_catalog.pg_advisory_xact_lock(gnomon.node_lock_key(p_tenant_id, p_episode_id));
    select e.scope into v_scope
      from gnomon.episode e where e.tenant_id = p_tenant_id and e.id = p_episode_id;
    if not found then
      raise exception using errcode = 'GNW01',
        message = format('Episode %s is not visible to the caller', p_episode_id);
    end if;

    -- The locks of every Node and Edge the Episode touched and of their
    -- Edges' endpoints, in order of ID, then the Episode's row, as a write
    -- takes its Node's lock and then, through a foreign key, a lock on the
    -- Episode's row. Locking the row waits for every write citing the
    -- Episode that is still running, and what those wrote is locked next.
    -- After that no write can cite the Episode, so the set is final.
    loop
      select coalesce(array_agg(distinct x.id order by x.id), '{}') into v_touched
        from (
          select c.versioned_id as id from gnomon.contribution c
            where c.tenant_id = p_tenant_id and c.episode_id = p_episode_id
          union
          select s.versioned_id from gnomon.statement s
            where s.tenant_id = p_tenant_id and s.episode_id = p_episode_id
          union
          select b.versioned_id from gnomon.bound_statement b
            where b.tenant_id = p_tenant_id and b.episode_id = p_episode_id
          union
          select v.versioned_id from gnomon.citation c
            join gnomon.version v on v.tenant_id = c.tenant_id and v.id = c.version_id
            where c.tenant_id = p_tenant_id and c.episode_id = p_episode_id
        ) x;
      v_lock := array(
        select distinct l.id from (
          select unnest(v_touched) as id
          union
          select e.source_id from gnomon.edge e
            where e.tenant_id = p_tenant_id and e.id = any(v_touched)
          union
          select e.target_id from gnomon.edge e
            where e.tenant_id = p_tenant_id and e.id = any(v_touched)
        ) l
        where l.id <> all (v_locked)
        order by l.id);
      exit when cardinality(v_lock) = 0 and v_row_locked;
      foreach v_id in array v_lock loop
        perform pg_catalog.pg_advisory_xact_lock(gnomon.node_lock_key(p_tenant_id, v_id));
      end loop;
      v_locked := v_locked || v_lock;
      if not v_row_locked then
        perform from gnomon.episode e
          where e.tenant_id = p_tenant_id and e.id = p_episode_id
          for update;
        v_row_locked := true;
      end if;
    end loop;

    -- History is rewritten up to the Erasure's own start, and no further.
    if exists (
      select from gnomon.contribution c
      where c.tenant_id = p_tenant_id and c.versioned_id = any(v_touched) and c.recorded_at > t
    ) or exists (
      select from gnomon.version v
      where v.tenant_id = p_tenant_id and v.versioned_id = any(v_touched)
        and (v.recorded_from > t or (v.recorded_to > t and v.recorded_to <> 'infinity'))
    ) or exists (
      select from gnomon.bound_statement b
      where b.tenant_id = p_tenant_id and b.versioned_id = any(v_touched) and b.recorded_at > t
    ) then
      raise exception using errcode = 'serialization_failure',
        message = format('a later transaction wrote what Episode %s touched first; retry the Erasure',
                         p_episode_id);
    end if;
    if exists (
      select from gnomon.contribution c
      where c.tenant_id = p_tenant_id and c.versioned_id = any(v_touched) and c.recorded_at = t
    ) or exists (
      select from gnomon.version v
      where v.tenant_id = p_tenant_id and v.versioned_id = any(v_touched)
        and (v.recorded_from = t or v.recorded_to = t)
    ) or exists (
      select from gnomon.bound_statement b
      where b.tenant_id = p_tenant_id and b.versioned_id = any(v_touched) and b.recorded_at = t
    ) or exists (
      select from gnomon.episode_retraction r
      where r.tenant_id = p_tenant_id and r.episode_id = p_episode_id and r.recorded_at = t
    ) then
      raise exception using errcode = 'GNW01',
        message = 'an Erasure runs in a transaction of its own, and this one already wrote '
          'what the Episode touched';
    end if;

    -- The plan: each Node and Edge, its Kind, its T0 and the rule each of
    -- its steps recorded, read before anything is deleted.
    select coalesce(jsonb_agg(jsonb_build_object(
             'id', p.id, 'is_edge', p.is_edge, 'kind', p.kind, 't0', p.t0,
             'rules', coalesce((
               select jsonb_object_agg(r.at::text, r.rule)
               from (
                 select distinct on (v.recorded_from) v.recorded_from as at, v.merge_rule::text as rule
                 from gnomon.version v
                 where v.tenant_id = p_tenant_id and v.versioned_id = p.id and v.recorded_from >= p.t0
                 order by v.recorded_from, v.id desc
               ) r), '{}'::jsonb))
           order by p.id), '[]')
      into v_plan
      from (
        select x.id, e.id is not null as is_edge, coalesce(n.kind, e.kind) as kind,
               (select min(a.at) from (
                  select c.recorded_at as at from gnomon.contribution c
                    where c.tenant_id = p_tenant_id and c.episode_id = p_episode_id
                      and c.versioned_id = x.id
                  union all
                  select s.recorded_at from gnomon.statement s
                    where s.tenant_id = p_tenant_id and s.episode_id = p_episode_id
                      and s.versioned_id = x.id
                  union all
                  select b.recorded_at from gnomon.bound_statement b
                    where b.tenant_id = p_tenant_id and b.episode_id = p_episode_id
                      and b.versioned_id = x.id
                  union all
                  select c.recorded_at from gnomon.citation c
                    join gnomon.version v on v.tenant_id = c.tenant_id and v.id = c.version_id
                    where c.tenant_id = p_tenant_id and c.episode_id = p_episode_id
                      and v.versioned_id = x.id
                ) a) as t0
        from unnest(v_touched) x(id)
        left join gnomon.node n on n.tenant_id = p_tenant_id and n.id = x.id
        left join gnomon.edge e on e.tenant_id = p_tenant_id and e.id = x.id
      ) p;

    -- The Episode's rows, while the Citations still show what it alone
    -- supplied.
    perform gnomon.erase_episode_rows(p_tenant_id, p_episode_id);

    -- Each Node and Edge back to where it stood just before T0, losing the
    -- text derived from every Version held at or after T0.
    for it in
      select (p ->> 'id')::uuid as id, (p ->> 't0')::timestamptz as t0
      from jsonb_array_elements(v_plan) p
    loop
      select coalesce(array_agg(v.id), '{}') into v_text
        from gnomon.version v
        where v.tenant_id = p_tenant_id and v.versioned_id = it.id and v.recorded_to >= it.t0;
      perform gnomon.erase_version_text(p_tenant_id, v_text);
      delete from gnomon.citation c
        using gnomon.version v
        where c.tenant_id = p_tenant_id and v.tenant_id = p_tenant_id and v.id = c.version_id
          and v.versioned_id = it.id
          and (v.recorded_from >= it.t0 or (v.recorded_to >= it.t0 and c.recorded_at >= it.t0));
      delete from gnomon.version v
        where v.tenant_id = p_tenant_id and v.versioned_id = it.id and v.recorded_from >= it.t0;
      update gnomon.version v set recorded_to = 'infinity'
        where v.tenant_id = p_tenant_id and v.versioned_id = it.id
          and v.recorded_to >= it.t0 and v.recorded_to <> 'infinity';
    end loop;

    for it in
      select (p ->> 'id')::uuid as id, (p ->> 'is_edge')::boolean as is_edge, p ->> 'kind' as kind,
             (p ->> 't0')::timestamptz as t0, p -> 'rules' as rules
      from jsonb_array_elements(v_plan) p
    loop
      perform gnomon.replay_versions(
        p_tenant_id, v_scope, it.id, it.is_edge, it.kind, it.t0, it.rules);
    end loop;

    -- What is left of nothing: each Edge the Episode touched that no row
    -- names any more, then each such Node, and each endpoint of an erased
    -- Edge, that no row names and no Edge joins.
    with gone as (
      delete from gnomon.edge e
        where e.tenant_id = p_tenant_id and e.id = any(v_touched)
          and not exists (select from gnomon.contribution c
                          where c.tenant_id = e.tenant_id and c.edge_id = e.id)
          and not exists (select from gnomon.statement s
                          where s.tenant_id = e.tenant_id and s.edge_id = e.id)
          and not exists (select from gnomon.bound_statement b
                          where b.tenant_id = e.tenant_id and b.edge_id = e.id)
          and not exists (select from gnomon.version v
                          where v.tenant_id = e.tenant_id and v.edge_id = e.id)
        returning e.id, e.source_id, e.target_id
    )
    select coalesce(array_agg(g.id), '{}'),
           coalesce(array_agg(g.source_id) || array_agg(g.target_id), '{}')
      into v_edges, v_nodes
      from gone g;
    -- A Node is erased one at a time, so that one joined by an Edge in a Scope
    -- the caller cannot see, which RLS hides from the check above the delete
    -- but not from the foreign key, stays as a bare identity and the rest of
    -- the Erasure goes on.
    v_candidates := array(select distinct u.id from unnest(v_touched || v_nodes) u(id)
                          order by u.id);
    v_nodes := '{}';
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
          v_nodes := v_nodes || v_id;
        end if;
      exception when foreign_key_violation then
        null;
      end;
    end loop;

    erased_at := t;
    erased_nodes := array(select u.id from unnest(v_nodes) u(id) order by u.id);
    erased_edges := array(select u.id from unnest(v_edges) u(id) order by u.id);
    recomputed_nodes := array(
      select (p ->> 'id')::uuid from jsonb_array_elements(v_plan) p
      where not (p ->> 'is_edge')::boolean and (p ->> 'id')::uuid <> all (v_nodes)
      order by 1);
    recomputed_edges := array(
      select (p ->> 'id')::uuid from jsonb_array_elements(v_plan) p
      where (p ->> 'is_edge')::boolean and (p ->> 'id')::uuid <> all (v_edges)
      order by 1);
    return next;
  end
  $$;

-- The definer role reads and writes the graph tables under the same Tenant
-- and Scope policies as the callers, so Erasure reaches only what the caller
-- could see.
alter policy episode_tenant_scope on gnomon.episode
  to authenticated, gnomon_worker, gnomon_definer;
alter policy node_tenant_scope on gnomon.node to authenticated, gnomon_worker, gnomon_definer;
alter policy edge_tenant_scope on gnomon.edge to authenticated, gnomon_worker, gnomon_definer;
alter policy contribution_tenant_scope on gnomon.contribution
  to authenticated, gnomon_worker, gnomon_definer;
alter policy version_tenant_scope on gnomon.version
  to authenticated, gnomon_worker, gnomon_definer;
alter policy citation_tenant_scope on gnomon.citation
  to authenticated, gnomon_worker, gnomon_definer;
alter policy bound_statement_tenant_scope on gnomon.bound_statement
  to authenticated, gnomon_worker, gnomon_definer;
alter policy statement_tenant_scope on gnomon.statement
  to authenticated, gnomon_worker, gnomon_definer;
alter policy episode_retraction_tenant_scope on gnomon.episode_retraction
  to authenticated, gnomon_worker, gnomon_definer;
alter policy sentence_tenant_scope on gnomon.sentence
  to authenticated, gnomon_worker, gnomon_definer;

-- Grants. Every grant a default privilege made on this migration's new
-- functions is revoked first, so each holds only the grants below.
do $$
declare
  g record;
begin
  for g in
    select format('function %s', p.oid::regprocedure) as object, a.grantee
    from pg_proc p, aclexplode(p.proacl) a
    where p.oid in (
      'gnomon.running_as_definer()'::regprocedure,
      'gnomon.version_erasure_update()'::regprocedure,
      'gnomon.version_check_cited()'::regprocedure,
      'gnomon.edge_await_endpoints()'::regprocedure,
      'gnomon.kind_merge_rule_at(uuid, text, timestamptz)'::regprocedure,
      'gnomon.merge_versions(uuid, text, uuid, boolean, timestamptz, timestamptz, timestamptz, text, boolean)'::regprocedure,
      'gnomon.erase_version_text(uuid, uuid[])'::regprocedure,
      'gnomon.erase_episode_rows(uuid, uuid)'::regprocedure,
      'gnomon.replay_versions(uuid, text, uuid, boolean, text, timestamptz, jsonb)'::regprocedure,
      'gnomon.erase_episode(uuid, uuid)'::regprocedure
    ) and a.grantee <> p.proowner
  loop
    execute format(
      'revoke all on %s from %s', g.object,
      case when g.grantee = 0 then 'public' else g.grantee::regrole::text end);
  end loop;
  revoke all on function gnomon.running_as_definer() from public;
  revoke all on function gnomon.version_erasure_update() from public;
  revoke all on function gnomon.version_check_cited() from public;
  revoke all on function gnomon.edge_await_endpoints() from public;
  revoke all on function gnomon.kind_merge_rule_at(uuid, text, timestamptz) from public;
  revoke all on function gnomon.merge_versions(
    uuid, text, uuid, boolean, timestamptz, timestamptz, timestamptz, text, boolean) from public;
  revoke all on function gnomon.erase_version_text(uuid, uuid[]) from public;
  revoke all on function gnomon.erase_episode_rows(uuid, uuid) from public;
  revoke all on function gnomon.replay_versions(
    uuid, text, uuid, boolean, text, timestamptz, jsonb) from public;
  revoke all on function gnomon.erase_episode(uuid, uuid) from public;
end
$$;

-- Hand Erasure to the definer role, as migrations 0003 and 0007 hand it Kind
-- registration and the merge-rule setter.
do $$
declare
  joined boolean := not pg_has_role(current_user, 'gnomon_definer', 'SET');
begin
  if joined then
    execute format('grant gnomon_definer to %I', current_user);
  end if;
  grant create on schema gnomon to gnomon_definer;
  alter function gnomon.erase_episode(uuid, uuid) owner to gnomon_definer;
  revoke create on schema gnomon from gnomon_definer;
  if joined then
    execute format('revoke gnomon_definer from %I', current_user);
  end if;
end
$$;

-- Every caller fires the guard triggers, whose conditions call
-- `gnomon.running_as_definer()`, and the write path calls the merge.
grant execute on function gnomon.running_as_definer() to authenticated, gnomon_worker, gnomon_definer;
grant execute on function gnomon.merge_versions(
  uuid, text, uuid, boolean, timestamptz, timestamptz, timestamptz, text, boolean)
  to authenticated, gnomon_worker, gnomon_definer;

-- What Erasure needs as gnomon_definer: to read the graph and the Ontology,
-- to delete and rewrite history, and the functions it calls. The definer
-- gets UPDATE of an Episode's `content` only so that Erasure can lock the
-- Episode's row FOR UPDATE; no definer function updates an Episode. Callers
-- get none of this.
grant select, delete on gnomon.episode, gnomon.node, gnomon.edge, gnomon.contribution,
  gnomon.version, gnomon.citation, gnomon.bound_statement, gnomon.statement,
  gnomon.episode_retraction, gnomon.sentence
  to gnomon_definer;
grant update (content) on gnomon.episode to gnomon_definer;
grant insert, update (recorded_to) on gnomon.version to gnomon_definer;
grant insert on gnomon.citation, gnomon.statement, gnomon.contribution to gnomon_definer;
grant execute on function gnomon.kind_merge_rule_at(uuid, text, timestamptz) to gnomon_definer;
grant execute on function gnomon.erase_version_text(uuid, uuid[]) to gnomon_definer;
grant execute on function gnomon.erase_episode_rows(uuid, uuid) to gnomon_definer;
grant execute on function gnomon.replay_versions(
  uuid, text, uuid, boolean, text, timestamptz, jsonb) to gnomon_definer;
grant execute on function gnomon.node_lock_key(uuid, uuid) to gnomon_definer;
grant execute on function gnomon.allowed_scopes() to gnomon_definer;
grant execute on function gnomon.instant_or_unknown(timestamptz) to gnomon_definer;
grant execute on function gnomon.edge_builtin_rules() to gnomon_definer;
grant execute on function gnomon.uuid_range(uuid, uuid, text) to gnomon_definer;
grant execute on function gnomon.uuid_v7() to gnomon_definer;

insert into gnomon.schema_version (version, name) values (11, '0011_erase_episode');
