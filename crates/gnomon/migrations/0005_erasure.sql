-- gnomon 0005: Erasure. Erase Episode and the Erasure of a Node, an Edge or
-- a Scope, and the policies through which an Erasure reaches rows in Scopes
-- the caller cannot see (ADR-0003, ADR-0009).

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

-- Erasure (ADR-0003). Every row derived from the text of a Version an
-- Erasure deletes or recomputes goes: its name rows, its quantized row, the
-- full vector of each of its texts that no other quantized row or name row
-- with bits uses, its Sentence and its token rows. Everything else is as
-- migration 0017 wrote it. The texts' hashes are read first, while the name
-- rows and the Sentences still give them. Then the name rows and quantized
-- rows go, and then the full vectors that nothing uses any more.
create or replace function gnomon.erase_version_text(p_tenant_id uuid, p_versions uuid[])
  returns void
  language plpgsql volatile security invoker set search_path = ''
  as $$
  declare
    v_scopes text[];
    v_hashes text[];
  begin
    select coalesce(pg_catalog.array_agg(t.scope), '{}'), coalesce(pg_catalog.array_agg(t.text_hash), '{}')
      into v_scopes, v_hashes
      from (
        select v.scope, v.text_hash
        from gnomon.version v
        where v.tenant_id = p_tenant_id and v.id = any(p_versions)
        union
        select s.scope, gnomon.sentence_text_hash(s.sentence)
        from gnomon.sentence s
        where s.tenant_id = p_tenant_id and s.version_id = any(p_versions)
        union
        select b.scope, b.text_hash
        from gnomon.embedding_bit b
        join gnomon.version v
          on v.tenant_id = b.tenant_id and v.scope = b.scope and v.id = b.version_id
        where v.tenant_id = p_tenant_id and v.id = any(p_versions)
        union
        select n.scope, n.text_hash
        from gnomon.node_name n
        join gnomon.version v
          on v.tenant_id = n.tenant_id and v.scope = n.scope and v.id = n.version_id
        where v.tenant_id = p_tenant_id and v.id = any(p_versions)
      ) t;
    delete from gnomon.node_name n
      using gnomon.version v
      where v.tenant_id = p_tenant_id and v.id = any(p_versions)
        and n.tenant_id = v.tenant_id and n.scope = v.scope and n.version_id = v.id;
    delete from gnomon.embedding_bit b
      using gnomon.version v
      where v.tenant_id = p_tenant_id and v.id = any(p_versions)
        and b.tenant_id = v.tenant_id and b.scope = v.scope and b.version_id = v.id;
    delete from gnomon.embedding e
      using unnest(v_scopes, v_hashes) t(scope, text_hash)
      where e.tenant_id = p_tenant_id and e.scope = t.scope and e.text_hash = t.text_hash
        and not exists (select from gnomon.embedding_bit b
                        where b.tenant_id = e.tenant_id and b.scope = e.scope
                          and b.text_hash = e.text_hash)
        and not exists (select from gnomon.node_name n
                        where n.tenant_id = e.tenant_id and n.scope = e.scope
                          and n.text_hash = e.text_hash and n.q is not null);
    delete from gnomon.sentence s
      where s.tenant_id = p_tenant_id and s.version_id = any(p_versions);
    delete from gnomon.search_token t
      using gnomon.version v
      where v.tenant_id = p_tenant_id and v.id = any(p_versions)
        and t.tenant_id = v.tenant_id and t.scope = v.scope and t.version_id = v.id;
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
    v_open uuid[] := '{}';
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
      v_open := v_open || array(
        select v.id from gnomon.version v
        where v.tenant_id = p_tenant_id and v.versioned_id = it.id
          and v.recorded_to = 'infinity' and v.recorded_from < it.t0);
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

    -- A Version current at T0 that was never closed lost its text-derived
    -- rows above, and the replay neither inserts nor reopens it, so nothing
    -- indexes it again. An update that changes nothing sends it through the
    -- search index trigger (migration 0016), which writes its token and name
    -- rows from its text as it stands, as for a Version the reset reopened.
    -- One the replay closed was indexed then, and the trigger writes nothing
    -- for it twice.
    update gnomon.version v set recorded_to = v.recorded_to
      where v.tenant_id = p_tenant_id and v.id = any(v_open);

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

-- The definer's reach into Scopes the caller cannot see, for reading and
-- deleting only: an Edge that joins a listed Node, and the rows of a listed
-- Edge.
create policy edge_erasure_reach_read on gnomon.edge for select to gnomon_definer
  using (tenant_id = gnomon.tenant_id()
         and gnomon.erasure_reaches(tenant_id, array[id, source_id, target_id]));

create policy edge_erasure_reach_delete on gnomon.edge for delete to gnomon_definer
  using (tenant_id = gnomon.tenant_id() and gnomon.erasure_reaches(tenant_id, array[id]));

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

-- The definer's reach into Scopes the caller cannot see, as migration 0012
-- gives it on the other tables of an Edge: it reads and deletes the token
-- rows of a listed Edge's Versions.
create policy search_token_erasure_reach_read on gnomon.search_token
  for select to gnomon_definer
  using (tenant_id = gnomon.tenant_id()
         and gnomon.erasure_reaches_version(tenant_id, version_id));

create policy search_token_erasure_reach_delete on gnomon.search_token
  for delete to gnomon_definer
  using (tenant_id = gnomon.tenant_id()
         and gnomon.erasure_reaches_version(tenant_id, version_id));

-- Whether a text of a Version of an Edge the running Erasure listed, its
-- properties' or its Sentence's, has hash `p_text_hash` in Scope `p_scope`.
create function gnomon.erasure_reaches_text(p_tenant_id uuid, p_scope text, p_text_hash text)
  returns boolean
  language sql stable security invoker set search_path = ''
  as $$
    select exists (
      select from gnomon.erasure_reach r
      join gnomon.version v
        on v.tenant_id = r.tenant_id and v.scope = p_scope and v.edge_id = r.id
      left join gnomon.sentence s
        on s.tenant_id = v.tenant_id and s.scope = v.scope and s.version_id = v.id
      where r.tenant_id = p_tenant_id and r.written_in = pg_current_xact_id_if_assigned()
        and (v.text_hash = p_text_hash or gnomon.sentence_text_hash(s.sentence) = p_text_hash)
    )
  $$;

-- The definer's reach into Scopes the caller cannot see, as migration 0012
-- gives it on the other tables of an Edge: it reads and deletes the
-- quantized rows of a listed Edge's Versions and the full vectors of their
-- texts, and reads the quantized rows that share those texts.
create policy embedding_bit_erasure_reach_read on gnomon.embedding_bit
  for select to gnomon_definer
  using (tenant_id = gnomon.tenant_id()
         and (gnomon.erasure_reaches_version(tenant_id, version_id)
              or gnomon.erasure_reaches_text(tenant_id, scope, text_hash)));

create policy embedding_bit_erasure_reach_delete on gnomon.embedding_bit
  for delete to gnomon_definer
  using (tenant_id = gnomon.tenant_id()
         and gnomon.erasure_reaches_version(tenant_id, version_id));

create policy embedding_erasure_reach_read on gnomon.embedding
  for select to gnomon_definer
  using (tenant_id = gnomon.tenant_id()
         and gnomon.erasure_reaches_text(tenant_id, scope, text_hash));

create policy embedding_erasure_reach_delete on gnomon.embedding
  for delete to gnomon_definer
  using (tenant_id = gnomon.tenant_id()
         and gnomon.erasure_reaches_text(tenant_id, scope, text_hash));

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

-- Migration 0018's function with the token rows and embedding rows of these
-- Summaries and Communities deleted first, while the Erasure's reach still
-- finds a Summary of another Scope. Everything else is as migration 0018
-- wrote it.
create or replace function gnomon.erase_summaries_and_communities(
  p_tenant_id uuid,
  p_summaries uuid[],
  p_communities uuid[]
) returns void
  language plpgsql volatile security invoker set search_path = ''
  as $$
  begin
    delete from gnomon.derived_search_token t
      where t.tenant_id = p_tenant_id and t.summary_id = any(p_summaries);
    delete from gnomon.derived_search_token t
      where t.tenant_id = p_tenant_id and t.community_id = any(p_communities);
    delete from gnomon.derived_embedding e
      where e.tenant_id = p_tenant_id and e.summary_id = any(p_summaries);
    delete from gnomon.derived_embedding e
      where e.tenant_id = p_tenant_id and e.community_id = any(p_communities);
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

-- The definer's reach into a Scope the caller cannot see (see the header).
create policy node_name_erasure_reach_read on gnomon.node_name
  for select to gnomon_definer
  using (tenant_id = gnomon.tenant_id()
         and gnomon.erasure_reaches_text(tenant_id, scope, text_hash));

-- The definer's reach (migration 0018): the token rows of a Summary of a
-- Node the running Erasure listed, in any Scope of the Tenant.
create policy derived_search_token_erasure_reach_read on gnomon.derived_search_token
  for select to gnomon_definer
  using (tenant_id = gnomon.tenant_id()
         and gnomon.erasure_reaches_summary(tenant_id, summary_id));

create policy derived_search_token_erasure_reach_delete on gnomon.derived_search_token
  for delete to gnomon_definer
  using (tenant_id = gnomon.tenant_id()
         and gnomon.erasure_reaches_summary(tenant_id, summary_id));

create policy derived_embedding_erasure_reach_read on gnomon.derived_embedding
  for select to gnomon_definer
  using (tenant_id = gnomon.tenant_id()
         and gnomon.erasure_reaches_summary(tenant_id, summary_id));

create policy derived_embedding_erasure_reach_delete on gnomon.derived_embedding
  for delete to gnomon_definer
  using (tenant_id = gnomon.tenant_id()
         and gnomon.erasure_reaches_summary(tenant_id, summary_id));

insert into gnomon.schema_version (version, name) values (5, '0005_erasure');
