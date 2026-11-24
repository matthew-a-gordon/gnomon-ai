-- gnomon migration 0020: neighbourhood search and fusion.
--
-- A consumer runs one Search call, `gnomon.search`, and gets one fused,
-- ranked list of the Versions, Summaries and Communities that match its
-- query at a valid-at and a known-at instant (ADR-0010, GLOSSARY: Search).
-- Every part is an invoker function under RLS with an empty search path
-- (ADR-0009), so everything it counts is a row the caller can see.
--
-- Three branches find candidates:
--
--   vector          the Versions nearest the query embedding
--                   (`gnomon.search_vector`, migration 0017), and the
--                   Summaries and Communities nearest it (below);
--   full text       the Versions whose searchable text matches the query
--                   by BM25 (`gnomon.search_full_text`, migration 0016),
--                   and the Summaries and Communities whose text does;
--   neighbourhood   the Edges that join the seed Nodes, and the Nodes at
--                   their other ends, at depth 1 by default and at most 2.
--
-- Each branch hands fusion its best 100, or the call's limit when that is
-- more, as prototype 5 fused 100 from each branch and as Resolution fuses
-- its halves (migration 0019). Reciprocal rank fusion with k = 60 adds
-- `weight / (60 + rank)` for each branch that found a candidate. The fused
-- best 100, or the limit when that is more, then get two more ranked lists,
-- the graph signals:
--
--   mentions        how many Episodes cite the candidate: the Citations of
--                   every Version of a Node or an Edge, or a Summary's or a
--                   Community's own Citations;
--   connectedness   how many Edges join a Node the candidate is about: the
--                   Node of a Node Version or a Summary, either end of an
--                   Edge Version (the Edge itself left out), or any Node a
--                   Community holds.
--
-- A signal ranks the candidates whose count is above zero, most first, and
-- equal counts share a rank. Its weight adds as a branch's does. Both
-- signals are counted at search time over the rows the caller can see at
-- the As-of instants (ADR-0010): stored counters would leak across Scopes
-- and be wrong for a past read. They rank only candidates the branches
-- found, so a hub never enters on popularity alone. Every weight is set per
-- query: the branches' default to 1 and the signals' to 0.5, a lower weight
-- that leaves them on. A branch weighted 0 adds no candidate.
--
-- What is visible at the As-of instants `(v, k)`, where a null `k` reads the
-- current belief:
--
--   A Version       its Valid time contains `v` and its Recorded time `k`.
--   A Summary or    its Recorded time contains `k` and `v >= as_of_valid`
--   a Community     (ADR-0015), so a Search at a past valid-at finds none.
--   An Edge         it holds a Version visible at `(v, k)`, and the caller
--                   sees both of its Nodes.
--   A mention       an Episode the caller sees, recorded at or before `k`,
--                   cited by a Citation recorded at or before `k`, of a
--                   Version whose Valid time contains `v`, or of the
--                   Summary or Community itself. A
--                   retracted Episode counts until its Retraction is
--                   recorded: at a known-at before that it is a mention, and
--                   at or after it, or in a read of the current belief, it
--                   is not. A retracted Episode was found to state what was
--                   never true, so it no longer makes a Node look mentioned.
--                   A past read still sees what was believed then. Erasure
--                   still counts a retracted Episode as another Episode
--                   (migration 0010).
--
-- The neighbourhood. The caller names its seed Nodes, or names none and the
-- branch takes the ten Nodes the vector and full-text branches rank best,
-- by reciprocal rank fusion of their Node Versions. From each seed it
-- follows the visible Edges that join it, in either direction, keeping at
-- most 50 per Node: the most-mentioned at the As-of instants, Edge ID
-- breaking a tie, so the same call follows the same Edges. At depth 2 it
-- does the same from each Node it reached that is not a seed, and adds the
-- Edges depth 1 did not follow. Each Edge followed is a candidate through
-- its Version at `(v, k)`, and so is each Node it reached, apart from the
-- seeds, through its Version then. They rank by depth, then by how many
-- seeds or followed Edges reached them, most first, then by ID. A `same_as` or
-- `different_from` Edge is followed as any other Edge (ADR-0012): Search
-- collapses no two Nodes. A seed the caller cannot see expands to nothing.
--
-- Summaries and Communities searched alongside Versions (ADR-0014). Their
-- text is indexed as a Version's is, in tables of their own:
--
--   `gnomon.derived_search_token`  one row per word of each Summary's or
--                                  Community's text, with `tf` and `dl`, as
--                                  `gnomon.search_token` holds a Version's.
--                                  The search index trigger writes the rows
--                                  when the row is inserted, in the write's
--                                  transaction, and they go with the row.
--                                  Their words join the Scope's BM25 counts,
--                                  which now count every text Search scores,
--                                  so a Version's score and a Summary's are
--                                  on one scale. The counts' `versions`
--                                  columns count those texts.
--   `gnomon.derived_embedding`     the full vector of each Summary's or
--                                  Community's text, stored by the Pipeline
--                                  with `gnomon.store_summary_embedding` or
--                                  `gnomon.store_community_embedding`, with
--                                  the `bit` `binary_quantize` gives and the
--                                  row's `as_of_valid` and Recorded time
--                                  behind a covering B-tree on
--                                  (tenant_id, scope). Search scans the bit
--                                  distance over that index, keeps the 400
--                                  nearest and re-ranks them at full
--                                  precision through one pinned primary-key
--                                  probe each, as migration 0017 does. A
--                                  Summary's text is its own, so its vector
--                                  is not shared with any other row.
--
-- Who writes them (ADR-0009). No definer is added. The search index trigger
-- of migration 0016, `gnomon.maintain_search_index`, now also fires when a
-- Summary or a Community is inserted and when its token rows are written or
-- deleted, and writes those rows and their count changes as it writes a
-- Version's. Callers hold SELECT on the token rows and no write privilege.
-- The embedding functions are invokers that check the call's Tenant and
-- Scope. Callers may insert an embedding row and nothing more, and a trigger
-- sets its bits and time columns from the stored row, whatever the caller
-- supplied. Closing a Summary or Community reaches its embedding row's
-- Recorded-time end through the foreign key
-- `(tenant_id, scope, ..., recorded_to)` with ON UPDATE CASCADE.
--
-- Erasure (ADR-0003). `gnomon.erase_summaries_and_communities`, which every
-- Erasure that removes a Summary or Community calls, now deletes their token
-- rows and embedding rows first, and the counts go down at commit. The
-- definer reads and deletes the token and embedding rows of a Summary in a
-- Scope the caller cannot see when the running Erasure reaches that Summary
-- (migration 0018). A row the transaction that wrote it deletes takes them
-- with it through the foreign keys. The vacuum call vacuums both tables.

-- The token table of Summaries and Communities.
create table gnomon.derived_search_token (
  tenant_id uuid not null,
  scope text not null,
  token text not null check (token <> ''),
  summary_id uuid,
  community_id uuid,
  derived_id uuid generated always as (coalesce(summary_id, community_id)) stored,
  tf integer not null check (tf > 0),
  dl integer not null check (dl >= tf),
  primary key (tenant_id, scope, token, derived_id) include (tf, dl, summary_id, community_id),
  foreign key (tenant_id, scope, summary_id)
    references gnomon.summary (tenant_id, scope, id) on delete cascade,
  foreign key (tenant_id, scope, community_id)
    references gnomon.community (tenant_id, scope, id) on delete cascade,
  check (num_nonnulls(summary_id, community_id) = 1)
);
-- Erasure, the foreign keys, and writing a row's tokens.
create index derived_search_token_summary on gnomon.derived_search_token
  (tenant_id, scope, summary_id) where summary_id is not null;
create index derived_search_token_community on gnomon.derived_search_token
  (tenant_id, scope, community_id) where community_id is not null;

alter table gnomon.derived_search_token enable row level security;
create policy derived_search_token_tenant_scope on gnomon.derived_search_token
  for all to authenticated, gnomon_worker, gnomon_definer
  using (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()))
  with check (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()));
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

-- The keys of the embedding rows' foreign keys, which carry the Recorded-time
-- end.
create unique index summary_embedding_key on gnomon.summary (tenant_id, scope, id, recorded_to);
create unique index community_embedding_key on gnomon.community
  (tenant_id, scope, id, recorded_to);

-- The token rows of these Summaries or Communities, from their text. Each
-- row has its Summary's or Community's Tenant and Scope.
create function gnomon.derived_text_tokens(p_tenant_id uuid, p_table text, p_ids uuid[])
  returns table (
    tenant_id uuid,
    scope text,
    token text,
    summary_id uuid,
    community_id uuid,
    tf integer,
    dl integer
  )
  language sql stable security invoker set search_path = ''
  as $$
    select d.tenant_id, d.scope, t.lexeme, d.summary_id, d.community_id,
           pg_catalog.cardinality(t.positions),
           (pg_catalog.sum(pg_catalog.cardinality(t.positions)) over (partition by d.id))::integer
    from (
      select s.tenant_id, s.scope, s.id, s.id as summary_id, null::uuid as community_id,
             pg_catalog.to_tsvector('simple'::pg_catalog.regconfig, s.content) as words
      from gnomon.summary s
      where p_table = 'summary' and s.tenant_id = p_tenant_id and s.id = any(p_ids)
      union all
      select c.tenant_id, c.scope, c.id, null, c.id,
             pg_catalog.to_tsvector('simple'::pg_catalog.regconfig, c.content)
      from gnomon.community c
      where p_table = 'community' and c.tenant_id = p_tenant_id and c.id = any(p_ids)
    ) d
    cross join lateral pg_catalog.unnest(d.words) t
  $$;

-- The search index trigger of migration 0016, which now also writes the
-- token rows of an inserted Summary or Community and records the count
-- changes of their token rows. Everything else is as migration 0016 wrote
-- it. It stays a definer owned by `gnomon_definer` and granted to no role
-- (ADR-0009).
-- The function is owned by `gnomon_definer`, so the role applying this
-- migration joins that role, with its privileges, to replace it and to
-- attach it to the new tables, and leaves it again, as migration 0016 joined
-- it to hand the function over.
do $do$
declare
  joined boolean := not pg_has_role(current_user, 'gnomon_definer', 'USAGE');
begin
  if joined then
    execute format('grant gnomon_definer to %I with inherit true, set false', current_user);
  end if;
  execute $ddl$
create or replace function gnomon.maintain_search_index() returns trigger
  language plpgsql security definer set search_path = ''
  as $$
  declare
    t uuid;
    v_sign bigint;
    v_versions uuid[];
    v_rows uuid[];
    d record;
  begin
    -- At commit: the transaction's changes to the counts of the Tenant that
    -- fired it, one row at a time, every word's row in key order and then
    -- every Scope's row. Each statement reaches one row by its key. A change
    -- that empties a row deletes it. Then the changes go.
    if tg_level = 'ROW' then
      for d in
        select x.* from gnomon.search_count_delta x
        where x.tenant_id = new.tenant_id and x.written_in = new.written_in
        order by x.token is null, x.scope, x.token
      loop
        if d.token is not null and d.versions > 0 then
          insert into gnomon.search_token_frequency as f (tenant_id, scope, token, versions)
            values (d.tenant_id, d.scope, d.token, d.versions)
            on conflict (tenant_id, scope, token)
              do update set versions = f.versions + excluded.versions;
        elsif d.token is not null and d.versions < 0 then
          update gnomon.search_token_frequency f set versions = f.versions + d.versions
            where f.tenant_id = d.tenant_id and f.scope = d.scope and f.token = d.token
              and f.versions + d.versions > 0;
          if not found then
            delete from gnomon.search_token_frequency f
              where f.tenant_id = d.tenant_id and f.scope = d.scope and f.token = d.token;
          end if;
        elsif d.token is null and d.versions > 0 then
          insert into gnomon.search_scope_length as l (tenant_id, scope, versions, words)
            values (d.tenant_id, d.scope, d.versions, d.words)
            on conflict (tenant_id, scope)
              do update set versions = l.versions + excluded.versions,
                            words = l.words + excluded.words;
        elsif d.token is null and (d.versions < 0 or d.words <> 0) then
          update gnomon.search_scope_length l
            set versions = l.versions + d.versions, words = l.words + d.words
            where l.tenant_id = d.tenant_id and l.scope = d.scope
              and l.versions + d.versions > 0;
          if not found then
            delete from gnomon.search_scope_length l
              where l.tenant_id = d.tenant_id and l.scope = d.scope;
          end if;
        end if;
      end loop;
      delete from gnomon.search_count_delta x
        where x.tenant_id = new.tenant_id and x.written_in = new.written_in;
      return null;
    end if;

    -- Token rows written or deleted: the changes to their Scopes' counts.
    -- A text's rows are always written and deleted together.
    if tg_table_name = 'search_token' then
      v_sign := case when tg_op = 'INSERT' then 1 else -1 end;
      insert into gnomon.search_count_delta as x (tenant_id, scope, token, versions, words)
        select c.tenant_id, c.scope, c.token, v_sign * pg_catalog.count(*), 0
        from changed c group by c.tenant_id, c.scope, c.token
        union all
        select c.tenant_id, c.scope, null, v_sign * pg_catalog.count(distinct c.version_id),
               v_sign * pg_catalog.sum(c.tf)
        from changed c group by c.tenant_id, c.scope
        on conflict (tenant_id, written_in, scope, token)
          do update set versions = x.versions + excluded.versions,
                        words = x.words + excluded.words;
      return null;
    end if;
    if tg_table_name = 'derived_search_token' then
      v_sign := case when tg_op = 'INSERT' then 1 else -1 end;
      insert into gnomon.search_count_delta as x (tenant_id, scope, token, versions, words)
        select c.tenant_id, c.scope, c.token, v_sign * pg_catalog.count(*), 0
        from changed c group by c.tenant_id, c.scope, c.token
        union all
        select c.tenant_id, c.scope, null, v_sign * pg_catalog.count(distinct c.derived_id),
               v_sign * pg_catalog.sum(c.tf)
        from changed c group by c.tenant_id, c.scope
        on conflict (tenant_id, written_in, scope, token)
          do update set versions = x.versions + excluded.versions,
                        words = x.words + excluded.words;
      return null;
    end if;

    -- A Summary or a Community inserted: its token rows, from its text. It
    -- is read under the caller's claims, as a Version is below.
    if tg_table_name in ('summary', 'community') then
      for t in select distinct c.tenant_id from changed c loop
        v_rows := array(select c.id from changed c where c.tenant_id = t);
        if tg_table_name = 'summary' and exists (
             select from unnest(v_rows) x(id)
             where not exists (select from gnomon.summary s
                               where s.tenant_id = t and s.id = x.id))
           or tg_table_name = 'community' and exists (
             select from unnest(v_rows) x(id)
             where not exists (select from gnomon.community k
                               where k.tenant_id = t and k.id = x.id)) then
          raise exception using errcode = 'insufficient_privilege',
            message = 'a Summary or Community is indexed for Search under the claims of a '
              'caller who can see it, and these claims do not show it';
        end if;
        insert into gnomon.derived_search_token
            (tenant_id, scope, token, summary_id, community_id, tf, dl)
          select * from gnomon.derived_text_tokens(t, tg_table_name, v_rows);
      end loop;
      return null;
    end if;

    -- A Version inserted, a Sentence stored, or a Version an Erasure
    -- reopened: their token rows, from their text. The function reads them
    -- under the caller's claims, so it refuses a Version they do not let it
    -- see rather than index nothing.
    for t in select distinct c.tenant_id from changed c loop
      if tg_table_name = 'sentence' then
        v_versions := array(select c.version_id from changed c where c.tenant_id = t);
      else
        v_versions := array(select c.id from changed c where c.tenant_id = t);
      end if;
      if exists (select from unnest(v_versions) x(id)
                 where not exists (select from gnomon.version v
                                   where v.tenant_id = t and v.id = x.id)) then
        raise exception using errcode = 'insufficient_privilege',
          message = 'a Version is indexed for Search under the claims of a caller who can '
            'see it, and these claims do not show it';
      end if;
      perform gnomon.index_search_text(t, v_versions, tg_table_name = 'sentence');
    end loop;
    return null;
  end
  $$
  $ddl$;
  -- Creating a trigger needs EXECUTE on its function, which only its owner
  -- holds, so the triggers are created here too.
  execute $ddl$
create trigger summary_index_search_text after insert on gnomon.summary
  referencing new table as changed
  for each statement execute function gnomon.maintain_search_index();
create trigger community_index_search_text after insert on gnomon.community
  referencing new table as changed
  for each statement execute function gnomon.maintain_search_index();
create trigger derived_search_token_count_inserted after insert on gnomon.derived_search_token
  referencing new table as changed
  for each statement execute function gnomon.maintain_search_index();
create trigger derived_search_token_count_deleted after delete on gnomon.derived_search_token
  referencing old table as changed
  for each statement execute function gnomon.maintain_search_index();
  $ddl$;
  if joined then
    execute format('revoke gnomon_definer from %I', current_user);
  end if;
end
$do$;


-- The words of `p_query`, each with its BM25 inverse document frequency,
-- and the mean length of a text, from the counts of the caller's allowed
-- Scopes and the changes the caller's own transaction recorded, as
-- `gnomon.search_full_text` computes them (migration 0016).
create function gnomon.search_query_words(p_tenant_id uuid, p_query text)
  returns table (token text, idf double precision, mean_length double precision)
  language sql stable security invoker set search_path = ''
  as $$
    with query as materialized (
      select distinct w.lexeme as token
      from pg_catalog.unnest(pg_catalog.to_tsvector(
             'simple'::pg_catalog.regconfig, coalesce(p_query, ''))) w
    ),
    totals as materialized (
      select pg_catalog.sum(x.versions)::float8 as versions,
             pg_catalog.sum(x.words)::float8 as words
      from (
        select s.versions, s.words from gnomon.search_scope_length s
        where s.tenant_id = p_tenant_id
        union all
        select d.versions, d.words from gnomon.search_count_delta d
        where d.tenant_id = p_tenant_id and d.token is null
      ) x
    )
    select q.token,
           pg_catalog.ln(1 + (t.versions - f.versions + 0.5) / (f.versions + 0.5)),
           t.words / t.versions
    from query q
    cross join totals t
    cross join lateral (
      select pg_catalog.sum(x.versions)::float8 as versions
      from (
        select c.versions from gnomon.search_token_frequency c
        where c.tenant_id = p_tenant_id and c.token = q.token
        union all
        select d.versions from gnomon.search_count_delta d
        where d.tenant_id = p_tenant_id and d.token = q.token
      ) x
    ) f
    where f.versions > 0
  $$;

-- The full-text branch over Summaries and Communities: those whose text
-- holds a word of `p_query`, visible at the As-of instants, best first by
-- BM25 with k1 = 1.2 and b = 0.75, at most `p_limit`. Every row of the
-- caller's Scopes is scored from its token rows, and the As-of conditions
-- are then checked on each, so the result is exact. Ties go to the lower ID.
create function gnomon.search_full_text_derived(
  p_tenant_id uuid,
  p_query text,
  p_valid_at timestamptz,
  p_known_at timestamptz,
  p_limit integer
)
  returns table (
    summary_id uuid,
    community_id uuid,
    node_id uuid,
    scope text,
    score double precision
  )
  language sql stable security invoker set search_path = ''
  as $$
    with words as materialized (
      select * from gnomon.search_query_words(p_tenant_id, p_query)
    ),
    scored as materialized (
      select p.scope, p.summary_id, p.community_id, p.derived_id,
             pg_catalog.sum(w.idf * p.tf * 2.2
               / (p.tf + 1.2 * (0.25 + 0.75 * p.dl / w.mean_length))) as score
      from words w
      join gnomon.derived_search_token p on p.tenant_id = p_tenant_id and p.token = w.token
      group by p.scope, p.summary_id, p.community_id, p.derived_id
    )
    select s.summary_id, s.community_id, h.node_id, s.scope, s.score
    from scored s
    -- One probe by key of the row each scored text belongs to.
    cross join lateral (
      select x.node_id, x.as_of_valid, x.recorded_from, x.recorded_to
      from gnomon.summary x
      where s.summary_id is not null
        and x.tenant_id = p_tenant_id and x.scope = s.scope and x.id = s.summary_id
      union all
      select null::uuid, x.as_of_valid, x.recorded_from, x.recorded_to
      from gnomon.community x
      where s.community_id is not null
        and x.tenant_id = p_tenant_id and x.scope = s.scope and x.id = s.community_id
      limit 1
    ) h
    where h.as_of_valid <= p_valid_at
      and case
            when p_known_at is null then h.recorded_to = 'infinity'
            else h.recorded_from <= p_known_at and h.recorded_to > p_known_at
          end
    order by s.score desc, s.derived_id
    limit p_limit
  $$;

-- The full-text branch of Search: the Versions, Summaries and Communities
-- whose text holds a word of `p_query`, best first, at most `p_limit`, each
-- with its rank. One BM25 scale covers all three (see the header). Ties go
-- to the lower ID.
create function gnomon.search_full_text_branch(
  p_tenant_id uuid,
  p_query text,
  p_valid_at timestamptz,
  p_known_at timestamptz,
  p_limit integer
)
  returns table (
    kind text,
    id uuid,
    node_id uuid,
    edge_id uuid,
    scope text,
    score double precision,
    rank bigint
  )
  language sql stable security invoker set search_path = ''
  as $$
    select x.kind, x.id, x.node_id, x.edge_id, x.scope, x.score,
           pg_catalog.row_number() over (order by x.score desc, x.id)
    from (
      select 'version'::text as kind, f.version_id as id, f.node_id, f.edge_id, f.scope, f.score
      from gnomon.search_full_text(p_tenant_id, p_query, p_valid_at, p_known_at, p_limit) f
      union all
      select case when d.summary_id is not null then 'summary' else 'community' end,
             coalesce(d.summary_id, d.community_id), d.node_id, null::uuid, d.scope, d.score
      from gnomon.search_full_text_derived(p_tenant_id, p_query, p_valid_at, p_known_at,
                                           p_limit) d
    ) x
    order by x.score desc, x.id
    limit p_limit
  $$;

do $migration$
declare
  ext text := (
    select n.nspname
    from pg_catalog.pg_extension e
    join pg_catalog.pg_namespace n on n.oid = e.extnamespace
    where e.extname = 'vector');
  run text;
begin
  if ext is null then
    raise exception 'gnomon needs pgvector, which migration 0001 creates';
  end if;
  foreach run in array array[
$sql$
-- The embedding of one Summary's or Community's text.
create table gnomon.derived_embedding (
  tenant_id uuid not null,
  scope text not null,
  summary_id uuid,
  community_id uuid,
  derived_id uuid generated always as (coalesce(summary_id, community_id)) stored,
  embedding @extschema@.vector({{embedding_dimension}}) not null
    check (@extschema@.vector_norm(embedding) > 0),
  -- The bits of `embedding`, and the row's `as_of_valid` and Recorded time,
  -- which a trigger sets.
  q bit({{embedding_dimension}}) not null,
  as_of_valid timestamptz not null,
  recorded_from timestamptz not null,
  recorded_to timestamptz not null,
  primary key (tenant_id, scope, derived_id),
  foreign key (tenant_id, scope, summary_id, recorded_to)
    references gnomon.summary (tenant_id, scope, id, recorded_to)
    on update cascade on delete cascade,
  foreign key (tenant_id, scope, community_id, recorded_to)
    references gnomon.community (tenant_id, scope, id, recorded_to)
    on update cascade on delete cascade,
  check (num_nonnulls(summary_id, community_id) = 1)
)
$sql$,
$sql$
-- An embedding row as the Store derives it: of a Summary or Community the
-- caller can see in that Scope, with its bits and the row's time columns,
-- whatever the caller supplied. It runs as the caller, under RLS.
create function gnomon.derived_embedding_derive() returns trigger
  language plpgsql security invoker set search_path = ''
  as $fn$
  declare
    r record;
  begin
    if new.summary_id is not null then
      select s.as_of_valid, s.recorded_from, s.recorded_to into r
        from gnomon.summary s
        where s.tenant_id = new.tenant_id and s.scope = new.scope and s.id = new.summary_id;
    else
      select c.as_of_valid, c.recorded_from, c.recorded_to into r
        from gnomon.community c
        where c.tenant_id = new.tenant_id and c.scope = new.scope and c.id = new.community_id;
    end if;
    if not found then
      raise exception using errcode = 'GNW01',
        message = pg_catalog.format('%s %s is not visible to the caller in Scope %s',
                                    case when new.summary_id is not null then 'Summary'
                                         else 'Community' end,
                                    coalesce(new.summary_id, new.community_id), new.scope);
    end if;
    new.q := @extschema@.binary_quantize(new.embedding)::bit({{embedding_dimension}});
    new.as_of_valid := r.as_of_valid;
    new.recorded_from := r.recorded_from;
    new.recorded_to := r.recorded_to;
    return new;
  end
  $fn$
$sql$,
$sql$
-- Stores the embedding of a Summary's or a Community's text, after the write
-- that made it committed. `p_table` is 'summary' or 'community'. The call's
-- Tenant and the row's Scope are the caller's. A row's text never changes,
-- so its first embedding stays, and a second call does not use
-- `p_embedding`.
create function gnomon.store_derived_embedding(
  p_tenant_id uuid,
  p_table text,
  p_id uuid,
  p_embedding real[]
)
  returns void
  language plpgsql volatile security invoker set search_path = ''
  as $fn$
  declare
    v_vector @extschema@.vector := gnomon.embedding_vector(p_embedding);
    v_scope text;
  begin
    if p_table = 'summary' then
      select s.scope into v_scope from gnomon.summary s
        where s.tenant_id = p_tenant_id and s.id = p_id;
    elsif p_table = 'community' then
      select c.scope into v_scope from gnomon.community c
        where c.tenant_id = p_tenant_id and c.id = p_id;
    else
      raise exception using errcode = 'invalid_parameter_value',
        message = pg_catalog.format('a Summary or Community is embedded, not a %s', p_table);
    end if;
    if v_scope is null then
      raise exception using errcode = 'GNW01',
        message = pg_catalog.format('%s %s is not visible to the caller',
                                    case when p_table = 'summary' then 'Summary'
                                         else 'Community' end, p_id);
    end if;
    perform gnomon.check_write_scope(p_tenant_id, v_scope);
    insert into gnomon.derived_embedding (tenant_id, scope, summary_id, community_id, embedding,
                                          q, as_of_valid, recorded_from, recorded_to)
      values (p_tenant_id, v_scope,
              case when p_table = 'summary' then p_id end,
              case when p_table = 'community' then p_id end,
              v_vector,
              -- The trigger sets these four from the stored row.
              @extschema@.binary_quantize(v_vector)::bit({{embedding_dimension}}),
              '-infinity', '-infinity', 'infinity')
      on conflict (tenant_id, scope, derived_id) do nothing;
  end
  $fn$
$sql$,
$sql$
create function gnomon.store_summary_embedding(
  p_tenant_id uuid,
  p_summary_id uuid,
  p_embedding real[]
)
  returns void
  language sql volatile security invoker set search_path = ''
  as $fn$
    select gnomon.store_derived_embedding(p_tenant_id, 'summary', p_summary_id, p_embedding)
  $fn$
$sql$,
$sql$
create function gnomon.store_community_embedding(
  p_tenant_id uuid,
  p_community_id uuid,
  p_embedding real[]
)
  returns void
  language sql volatile security invoker set search_path = ''
  as $fn$
    select gnomon.store_derived_embedding(p_tenant_id, 'community', p_community_id, p_embedding)
  $fn$
$sql$,
$sql$
-- The vector branch over Summaries and Communities: those nearest `p_query`
-- by cosine distance at full precision, visible at the As-of instants,
-- nearest first, at most `p_limit`. Ties go to the lower ID. The plan is
-- migration 0017's: the 400 nearest by bits over the covering index, then
-- one pinned primary-key probe per candidate.
create function gnomon.search_vector_derived(
  p_tenant_id uuid,
  p_query real[],
  p_valid_at timestamptz,
  p_known_at timestamptz,
  p_limit integer
)
  returns table (
    summary_id uuid,
    community_id uuid,
    node_id uuid,
    scope text,
    distance double precision
  )
  language plpgsql stable security invoker set search_path = ''
  as $fn$
  declare
    v_query @extschema@.vector := gnomon.embedding_vector(p_query);
    v_bits bit({{embedding_dimension}}) :=
      @extschema@.binary_quantize(v_query)::bit({{embedding_dimension}});
  begin
    return query
      with candidates as materialized (
        select b.scope as cscope, b.derived_id as cid, b.summary_id as csummary,
               b.community_id as ccommunity
        from gnomon.derived_embedding b
        where b.tenant_id = p_tenant_id and b.as_of_valid <= p_valid_at
          and case
                when p_known_at is null then b.recorded_to = 'infinity'
                else b.recorded_from <= p_known_at and p_known_at < b.recorded_to
              end
        order by b.q operator(@extschema@.<~>) v_bits, b.derived_id
        limit greatest(400, p_limit)
      ),
      reranked as materialized (
        select c.cscope, c.cid, c.csummary, c.ccommunity,
               (f.embedding operator(@extschema@.<=>) v_query)::double precision as dist
        from candidates c
        -- The pin: one primary-key probe per candidate (migration 0017).
        cross join lateral (
          select e.embedding from gnomon.derived_embedding e
          where e.tenant_id = p_tenant_id and e.scope = c.cscope and e.derived_id = c.cid
          limit 1
        ) f
        order by 5, c.cid
        limit p_limit
      )
      select r.csummary, r.ccommunity, s.node_id, r.cscope, r.dist
      from reranked r
      left join lateral (
        select x.node_id from gnomon.summary x
        where x.tenant_id = p_tenant_id and x.scope = r.cscope and x.id = r.csummary
        limit 1
      ) s on true
      order by r.dist, r.cid;
  end
  $fn$
$sql$
  ] loop
    execute replace(run, '@extschema@', quote_ident(ext));
  end loop;
end
$migration$;

-- A full vector is read by a re-rank probe from one heap page, as migration
-- 0017 stores `gnomon.embedding`.
do $$
begin
  if gnomon.embedding_dimension() <= 1900 then
    alter table gnomon.derived_embedding alter column embedding set storage plain;
  else
    alter table gnomon.derived_embedding alter column embedding set storage external;
  end if;
end
$$;

-- The bit scan reads only this index: the policy's conditions are its index
-- conditions and the As-of conditions are checked on its tuples.
create index derived_embedding_scan on gnomon.derived_embedding (tenant_id, scope)
  include (derived_id, summary_id, community_id, q, as_of_valid, recorded_from, recorded_to);
-- Erasure and the foreign keys.
create index derived_embedding_summary on gnomon.derived_embedding
  (tenant_id, scope, summary_id, recorded_to) where summary_id is not null;
create index derived_embedding_community on gnomon.derived_embedding
  (tenant_id, scope, community_id, recorded_to) where community_id is not null;

create trigger derived_embedding_derive before insert on gnomon.derived_embedding
  for each row execute function gnomon.derived_embedding_derive();

alter table gnomon.derived_embedding enable row level security;
create policy derived_embedding_tenant_scope on gnomon.derived_embedding
  for all to authenticated, gnomon_worker, gnomon_definer
  using (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()))
  with check (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()));
create policy derived_embedding_erasure_reach_read on gnomon.derived_embedding
  for select to gnomon_definer
  using (tenant_id = gnomon.tenant_id()
         and gnomon.erasure_reaches_summary(tenant_id, summary_id));
create policy derived_embedding_erasure_reach_delete on gnomon.derived_embedding
  for delete to gnomon_definer
  using (tenant_id = gnomon.tenant_id()
         and gnomon.erasure_reaches_summary(tenant_id, summary_id));

-- The vector branch of Search: the Versions, Summaries and Communities
-- nearest `p_embedding`, nearest first, at most `p_limit`, each with its
-- rank. A null embedding finds nothing. Ties go to the lower ID.
create function gnomon.search_vector_branch(
  p_tenant_id uuid,
  p_embedding real[],
  p_valid_at timestamptz,
  p_known_at timestamptz,
  p_limit integer
)
  returns table (
    kind text,
    id uuid,
    node_id uuid,
    edge_id uuid,
    scope text,
    distance double precision,
    rank bigint
  )
  language plpgsql stable security invoker set search_path = ''
  as $$
  begin
    if p_embedding is null then
      return;
    end if;
    return query
      select x.kind, x.id, x.node_id, x.edge_id, x.scope, x.distance,
             pg_catalog.row_number() over (order by x.distance, x.id)
      from (
        select 'version'::text as kind, v.version_id as id, v.node_id, v.edge_id, v.scope,
               v.distance
        from gnomon.search_vector(p_tenant_id, p_embedding, p_valid_at, p_known_at, p_limit) v
        union all
        select case when d.summary_id is not null then 'summary' else 'community' end,
               coalesce(d.summary_id, d.community_id), d.node_id, null::uuid, d.scope,
               d.distance
        from gnomon.search_vector_derived(p_tenant_id, p_embedding, p_valid_at, p_known_at,
                                          p_limit) d
      ) x
      order by x.distance, x.id
      limit p_limit;
  end
  $$;

-- The mentions of a set of things at the As-of instants (see the header):
-- for each `p_keys[i]`, how many Episodes the caller sees cite `p_ids[i]`,
-- a Node or an Edge through the Citations of its Versions valid at
-- `p_valid_at` and recorded by `p_known_at`, or a Summary or a Community
-- through its own, as `p_kinds[i]`
-- says, in Scope `p_scopes[i]`. One query counts them all, so the cost is
-- one plan, not one per thing.
create function gnomon.search_mentions(
  p_tenant_id uuid,
  p_keys uuid[],
  p_kinds text[],
  p_ids uuid[],
  p_scopes text[],
  p_valid_at timestamptz,
  p_known_at timestamptz
)
  returns table (key uuid, mentions bigint)
  language sql stable security invoker set search_path = ''
  as $$
    with things as materialized (
      select t.key, t.kind, t.id, t.scope
      from unnest(p_keys, p_kinds, p_ids, p_scopes) t(key, kind, id, scope)
    ),
    cited as (
      select t.key, x.episode_id, x.recorded_at
      from things t
      join gnomon.version v
        on v.tenant_id = p_tenant_id and v.scope = t.scope and v.node_id = t.id
       and v.valid_from <= p_valid_at and p_valid_at < v.valid_to
       and (p_known_at is null or v.recorded_from <= p_known_at)
      join gnomon.citation x
        on x.tenant_id = v.tenant_id and x.scope = v.scope and x.version_id = v.id
      where t.kind = 'node'
      union all
      select t.key, x.episode_id, x.recorded_at
      from things t
      join gnomon.version v
        on v.tenant_id = p_tenant_id and v.scope = t.scope and v.edge_id = t.id
       and v.valid_from <= p_valid_at and p_valid_at < v.valid_to
       and (p_known_at is null or v.recorded_from <= p_known_at)
      join gnomon.citation x
        on x.tenant_id = v.tenant_id and x.scope = v.scope and x.version_id = v.id
      where t.kind = 'edge'
      union all
      select t.key, x.episode_id, null
      from things t
      join gnomon.summary_citation x
        on x.tenant_id = p_tenant_id and x.scope = t.scope and x.summary_id = t.id
      where t.kind = 'summary'
      union all
      select t.key, x.episode_id, null
      from things t
      join gnomon.community_citation x
        on x.tenant_id = p_tenant_id and x.scope = t.scope and x.community_id = t.id
      where t.kind = 'community'
    )
    select c.key, pg_catalog.count(distinct c.episode_id)
    from cited c
    join gnomon.episode e on e.tenant_id = p_tenant_id and e.id = c.episode_id
    where (p_known_at is null or (e.recorded_at <= p_known_at
                                  and (c.recorded_at is null or c.recorded_at <= p_known_at)))
      and not exists (
        select from gnomon.episode_retraction r
        where r.tenant_id = p_tenant_id and r.episode_id = c.episode_id
          and (p_known_at is null or r.recorded_at <= p_known_at))
    group by c.key
  $$;

-- The connectedness of a set of things at the As-of instants: for each key
-- of `p_keys`, how many Edges the caller sees join one of the Nodes paired
-- with it in `p_nodes`, other than the Edge paired with it in `p_skip`.
-- An Edge counts when it holds a Version visible then and the caller sees
-- both of its Nodes. One query counts them all.
create function gnomon.search_connectedness(
  p_tenant_id uuid,
  p_keys uuid[],
  p_nodes uuid[],
  p_skip uuid[],
  p_valid_at timestamptz,
  p_known_at timestamptz
)
  returns table (key uuid, connectedness bigint)
  language sql stable security invoker set search_path = ''
  as $$
    with about as materialized (
      select distinct a.key, a.node, a.skip
      from unnest(p_keys, p_nodes, p_skip) a(key, node, skip)
      where a.node is not null
    ),
    joining as materialized (
      select a.key, x.id, x.scope, x.source_id, x.target_id
      from about a
      join gnomon.edge x on x.tenant_id = p_tenant_id and x.source_id = a.node
      where x.id is distinct from a.skip
      union
      select a.key, x.id, x.scope, x.source_id, x.target_id
      from about a
      join gnomon.edge x on x.tenant_id = p_tenant_id and x.target_id = a.node
      where x.id is distinct from a.skip
    ),
    -- Each Edge is checked once, whichever things it joins.
    held as materialized (
      select e.id
      from (select distinct j.id, j.scope, j.source_id, j.target_id from joining j) e
      where exists (
          select from gnomon.version y
          where y.tenant_id = p_tenant_id and y.scope = e.scope and y.edge_id = e.id
            and y.valid_from <= p_valid_at and p_valid_at < y.valid_to
            and case
                  when p_known_at is null then y.recorded_to = 'infinity'
                  else y.recorded_from <= p_known_at and p_known_at < y.recorded_to
                end)
        and exists (select from gnomon.node a
                    where a.tenant_id = p_tenant_id and a.id = e.source_id)
        and exists (select from gnomon.node b
                    where b.tenant_id = p_tenant_id and b.id = e.target_id)
    )
    select j.key, pg_catalog.count(distinct j.id)
    from joining j
    join held h on h.id = j.id
    group by j.key
  $$;

-- The Edges the neighbourhood follows from each of `p_nodes`: the visible
-- Edges that join it to a Node the caller sees, in either direction, holding
-- a Version at the As-of instants, at most 50 per Node, the most-mentioned
-- first and Edge ID breaking a tie (see the header). Each comes with the
-- Node it was followed from, its Version then and the Node at its other end.
create function gnomon.neighbourhood_edges(
  p_tenant_id uuid,
  p_nodes uuid[],
  p_valid_at timestamptz,
  p_known_at timestamptz
)
  returns table (
    from_node uuid,
    edge_id uuid,
    scope text,
    version_id uuid,
    neighbour_id uuid,
    mentions bigint
  )
  language sql stable security invoker set search_path = ''
  as $$
    with joining as materialized (
      select n.id as from_node, x.id, x.scope, x.target_id as neighbour
      from (select distinct u as id from pg_catalog.unnest(p_nodes) u) n
      join gnomon.edge x on x.tenant_id = p_tenant_id and x.source_id = n.id
      where x.target_id <> n.id
      union all
      select n.id, x.id, x.scope, x.source_id
      from (select distinct u as id from pg_catalog.unnest(p_nodes) u) n
      join gnomon.edge x on x.tenant_id = p_tenant_id and x.target_id = n.id
      where x.source_id <> n.id
    ),
    held as materialized (
      select j.from_node, j.id, j.scope, v.id as version_id, j.neighbour
      from joining j
      cross join lateral (
        select y.id from gnomon.version y
        where y.tenant_id = p_tenant_id and y.scope = j.scope and y.edge_id = j.id
          and y.valid_from <= p_valid_at and p_valid_at < y.valid_to
          and case
                when p_known_at is null then y.recorded_to = 'infinity'
                else y.recorded_from <= p_known_at and p_known_at < y.recorded_to
              end
        limit 1
      ) v
      where exists (select from gnomon.node n
                    where n.tenant_id = p_tenant_id and n.id = j.neighbour)
    ),
    edges as materialized (
      select distinct h.id, h.scope from held h
    ),
    mentioned as materialized (
      select m.key, m.mentions
      from (select pg_catalog.array_agg(e.id) as ids, pg_catalog.array_agg(e.scope) as scopes
            from edges e) a
      cross join lateral gnomon.search_mentions(
        p_tenant_id, a.ids, pg_catalog.array_fill('edge'::text, array[coalesce(pg_catalog.cardinality(a.ids), 0)]),
        a.ids, a.scopes, p_valid_at, p_known_at) m
    ),
    ranked as (
      select h.from_node, h.id, h.scope, h.version_id, h.neighbour,
             coalesce(m.mentions, 0) as mentions,
             pg_catalog.row_number() over (
               partition by h.from_node order by coalesce(m.mentions, 0) desc, h.id) as place
      from held h
      left join mentioned m on m.key = h.id
    )
    select r.from_node, r.id, r.scope, r.version_id, r.neighbour, r.mentions
    from ranked r
    where r.place <= 50
  $$;

-- The neighbourhood branch of Search: from the seed Nodes the caller sees,
-- the Edges it follows to depth `p_depth` and the Nodes they reach, each
-- through its Version at the As-of instants, best first, at most `p_limit`,
-- each with its depth and rank (see the header).
create function gnomon.search_neighbourhood(
  p_tenant_id uuid,
  p_seeds uuid[],
  p_depth integer,
  p_valid_at timestamptz,
  p_known_at timestamptz,
  p_limit integer
)
  returns table (
    kind text,
    id uuid,
    node_id uuid,
    edge_id uuid,
    scope text,
    depth integer,
    rank bigint
  )
  language sql stable security invoker set search_path = ''
  as $$
    with seeds as materialized (
      select distinct n.id
      from pg_catalog.unnest(p_seeds) s(id)
      join gnomon.node n on n.tenant_id = p_tenant_id and n.id = s.id
    ),
    depth_one as materialized (
      select 1 as depth, e.*
      from gnomon.neighbourhood_edges(p_tenant_id, array(select s.id from seeds s),
                                      p_valid_at, p_known_at) e
    ),
    -- From each Node depth 1 reached that is not a seed, the Edges it keeps
    -- that depth 1 did not follow already.
    depth_two as materialized (
      select 2 as depth, e.*
      from gnomon.neighbourhood_edges(
             p_tenant_id,
             array(select distinct f.neighbour_id from depth_one f
                   where p_depth >= 2
                     and not exists (select from seeds s where s.id = f.neighbour_id)),
             p_valid_at, p_known_at) e
      where not exists (select from depth_one f where f.edge_id = e.edge_id)
    ),
    reached as materialized (
      select * from depth_one
      union all
      select * from depth_two
    ),
    edge_hits as (
      select 'version'::text as kind, r.version_id as id, null::uuid as node_id, r.edge_id,
             r.scope, pg_catalog.min(r.depth) as depth, pg_catalog.count(*) as times
      from reached r
      group by r.version_id, r.edge_id, r.scope
    ),
    -- The Nodes reached, apart from the seeds, which the branch expands
    -- from and does not return.
    nodes as materialized (
      select r.neighbour_id, pg_catalog.min(r.depth) as depth,
             pg_catalog.count(distinct r.edge_id) as times
      from reached r
      where not exists (select from seeds s where s.id = r.neighbour_id)
      group by r.neighbour_id
    ),
    node_hits as (
      select 'version'::text as kind, nv.id, n.neighbour_id as node_id, null::uuid as edge_id,
             nv.scope, n.depth, n.times
      from nodes n
      cross join lateral (
        select y.id, y.scope
        from gnomon.node x
        join gnomon.version y
          on y.tenant_id = x.tenant_id and y.scope = x.scope and y.node_id = x.id
        where x.tenant_id = p_tenant_id and x.id = n.neighbour_id
          and y.valid_from <= p_valid_at and p_valid_at < y.valid_to
          and case
                when p_known_at is null then y.recorded_to = 'infinity'
                else y.recorded_from <= p_known_at and p_known_at < y.recorded_to
              end
        limit 1
      ) nv
    )
    select h.kind, h.id, h.node_id, h.edge_id, h.scope, h.depth,
           pg_catalog.row_number() over (order by h.depth, h.times desc, h.id)
    from (select * from edge_hits union all select * from node_hits) h
    order by h.depth, h.times desc, h.id
    limit p_limit
  $$;

-- Search (ADR-0010): one fused, ranked list of the Versions, Summaries and
-- Communities that match `p_query` and `p_embedding` at `p_valid_at` and
-- `p_known_at` (a null known-at reads the current belief), at most
-- `p_limit`, best first. `p_seeds` names the neighbourhood's seed Nodes; null
-- or empty takes the top Node hits. `p_depth` is 1 or 2, and null is 1. A
-- null weight is its default: 1 for each branch, 0.5 for each signal. Each
-- result reports its rank in each branch and signal that ranked it, and its
-- distance, score, depth or count there. See the header. Ties go to the
-- lower ID.
create function gnomon.search(
  p_tenant_id uuid,
  p_query text,
  p_embedding real[],
  p_valid_at timestamptz,
  p_known_at timestamptz,
  p_seeds uuid[],
  p_depth integer,
  p_vector_weight double precision,
  p_full_text_weight double precision,
  p_neighbourhood_weight double precision,
  p_mentions_weight double precision,
  p_connectedness_weight double precision,
  p_limit integer
)
  returns table (
    kind text,
    id uuid,
    node_id uuid,
    edge_id uuid,
    scope text,
    score double precision,
    rank bigint,
    vector_distance double precision,
    vector_rank bigint,
    full_text_score double precision,
    full_text_rank bigint,
    neighbourhood_depth integer,
    neighbourhood_rank bigint,
    mentions bigint,
    mentions_rank bigint,
    connectedness bigint,
    connectedness_rank bigint
  )
  language plpgsql stable security invoker set search_path = ''
  as $$
  declare
    n integer;
    w_vector double precision := coalesce(p_vector_weight, 1);
    w_full_text double precision := coalesce(p_full_text_weight, 1);
    w_neighbourhood double precision := coalesce(p_neighbourhood_weight, 1);
    w_mentions double precision := coalesce(p_mentions_weight, 0.5);
    w_connectedness double precision := coalesce(p_connectedness_weight, 0.5);
    v_depth integer := coalesce(p_depth, 1);
  begin
    if p_limit is null or p_limit < 1 then
      raise exception using errcode = 'invalid_parameter_value',
        message = pg_catalog.format('a Search returns at least one candidate, not %s', p_limit);
    end if;
    if p_valid_at is null then
      raise exception using errcode = 'invalid_parameter_value',
        message = 'a Search needs a valid-at instant';
    end if;
    if v_depth not in (1, 2) then
      raise exception using errcode = 'invalid_parameter_value',
        message = pg_catalog.format(
          'a neighbourhood expands to depth 1 or 2, not %s', v_depth);
    end if;
    if exists (select from pg_catalog.unnest(array[w_vector, w_full_text, w_neighbourhood,
                                                   w_mentions, w_connectedness]) x
               where x < 0 or x = 'NaN'::float8 or x = 'Infinity'::float8) then
      raise exception using errcode = 'invalid_parameter_value',
        message = 'a Search weight is a finite number of at least 0';
    end if;
    n := greatest(100, p_limit);
    return query
      with vec as materialized (
        select * from gnomon.search_vector_branch(p_tenant_id, p_embedding, p_valid_at,
                                                  p_known_at, n)
      ),
      ft as materialized (
        select * from gnomon.search_full_text_branch(p_tenant_id, p_query, p_valid_at,
                                                     p_known_at, n)
      ),
      -- The seeds the caller named, or the ten Nodes the vector and
      -- full-text branches rank best together.
      seeds as materialized (
        select case
                 when coalesce(pg_catalog.cardinality(p_seeds), 0) > 0 then p_seeds
                 else array(
                   select h.node_id
                   from (select x.node_id, x.rank from vec x
                         where x.kind = 'version' and x.node_id is not null
                         union all
                         select x.node_id, x.rank from ft x
                         where x.kind = 'version' and x.node_id is not null) h
                   group by h.node_id
                   order by pg_catalog.sum(1.0::float8 / (60 + h.rank)) desc, h.node_id
                   limit 10)
               end as ids
      ),
      nb as materialized (
        select x.*
        from seeds s
        cross join lateral gnomon.search_neighbourhood(p_tenant_id, s.ids, v_depth, p_valid_at,
                                                       p_known_at, n) x
      ),
      -- A branch weighted 0 adds no candidate, though the vector and
      -- full-text branches still give the seeds.
      found as (
        select x.kind, x.id, x.node_id, x.edge_id, x.scope from vec x where w_vector > 0
        union
        select x.kind, x.id, x.node_id, x.edge_id, x.scope from ft x where w_full_text > 0
        union
        select x.kind, x.id, x.node_id, x.edge_id, x.scope from nb x where w_neighbourhood > 0
      ),
      fused as materialized (
        select f.kind, f.id, f.node_id, f.edge_id, f.scope,
               v.distance as v_distance, v.rank as v_rank,
               t.score as t_score, t.rank as t_rank,
               b.depth as b_depth, b.rank as b_rank,
               coalesce(w_vector / (60 + v.rank), 0)
                 + coalesce(w_full_text / (60 + t.rank), 0)
                 + coalesce(w_neighbourhood / (60 + b.rank), 0) as base
        from found f
        left join vec v on w_vector > 0 and v.kind = f.kind and v.id = f.id
        left join ft t on w_full_text > 0 and t.kind = f.kind and t.id = f.id
        left join nb b on w_neighbourhood > 0 and b.kind = f.kind and b.id = f.id
        order by base desc, f.id
        limit n
      ),
      -- The graph signals, counted over the fused candidates only, in one
      -- query each.
      mentioned as materialized (
        select m.key, m.mentions
        from (select pg_catalog.array_agg(f.id) as keys,
                     pg_catalog.array_agg(case when f.kind <> 'version' then f.kind
                                               when f.edge_id is not null then 'edge'
                                               else 'node' end) as kinds,
                     pg_catalog.array_agg(case when f.kind = 'version'
                                               then coalesce(f.node_id, f.edge_id)
                                               else f.id end) as ids,
                     pg_catalog.array_agg(f.scope) as scopes
              from fused f) a
        cross join lateral gnomon.search_mentions(p_tenant_id, a.keys, a.kinds, a.ids, a.scopes,
                                                  p_valid_at, p_known_at) m
      ),
      -- The Nodes each candidate is about: a Node Version's or a Summary's
      -- Node, both ends of an Edge Version, and every Node a Community holds.
      about as materialized (
        select f.id as key, f.node_id as node, null::uuid as skip
        from fused f where f.node_id is not null
        union all
        select f.id, u, f.edge_id
        from fused f
        join gnomon.edge e on e.tenant_id = p_tenant_id and e.scope = f.scope and e.id = f.edge_id
        cross join lateral pg_catalog.unnest(array[e.source_id, e.target_id]) u
        union all
        select f.id, m.node_id, null
        from fused f
        join gnomon.community_member m
          on m.tenant_id = p_tenant_id and m.scope = f.scope and m.community_id = f.id
        where f.kind = 'community'
      ),
      connected as materialized (
        select c.key, c.connectedness
        from (select pg_catalog.array_agg(a.key) as keys, pg_catalog.array_agg(a.node) as nodes,
                     pg_catalog.array_agg(a.skip) as skips
              from about a) a
        cross join lateral gnomon.search_connectedness(p_tenant_id, a.keys, a.nodes, a.skips,
                                                       p_valid_at, p_known_at) c
      ),
      counted as (
        select f.*, coalesce(m.mentions, 0) as m_count, coalesce(c.connectedness, 0) as c_count
        from fused f
        left join mentioned m on m.key = f.id
        left join connected c on c.key = f.id
      ),
      ranked as (
        select c.*,
               case when c.m_count > 0
                    then pg_catalog.rank() over (order by c.m_count desc) end as m_rank,
               case when c.c_count > 0
                    then pg_catalog.rank() over (order by c.c_count desc) end as c_rank
        from counted c
      ),
      scored as (
        select r.*,
               r.base + coalesce(w_mentions / (60 + r.m_rank), 0)
                 + coalesce(w_connectedness / (60 + r.c_rank), 0) as total
        from ranked r
      )
      select s.kind, s.id, s.node_id, s.edge_id, s.scope, s.total,
             pg_catalog.row_number() over (order by s.total desc, s.id),
             s.v_distance, s.v_rank, s.t_score, s.t_rank, s.b_depth, s.b_rank,
             s.m_count, s.m_rank, s.c_count, s.c_rank
      from scored s
      order by s.total desc, s.id
      limit p_limit;
  end
  $$;

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
      'gnomon.derived_text_tokens(uuid, text, uuid[])'::regprocedure,
      'gnomon.search_query_words(uuid, text)'::regprocedure,
      'gnomon.search_full_text_derived(uuid, text, timestamptz, timestamptz, integer)'::regprocedure,
      'gnomon.search_full_text_branch(uuid, text, timestamptz, timestamptz, integer)'::regprocedure,
      'gnomon.derived_embedding_derive()'::regprocedure,
      'gnomon.store_derived_embedding(uuid, text, uuid, real[])'::regprocedure,
      'gnomon.store_summary_embedding(uuid, uuid, real[])'::regprocedure,
      'gnomon.store_community_embedding(uuid, uuid, real[])'::regprocedure,
      'gnomon.search_vector_derived(uuid, real[], timestamptz, timestamptz, integer)'::regprocedure,
      'gnomon.search_vector_branch(uuid, real[], timestamptz, timestamptz, integer)'::regprocedure,
      'gnomon.search_mentions(uuid, uuid[], text[], uuid[], text[], timestamptz, timestamptz)'::regprocedure,
      'gnomon.search_connectedness(uuid, uuid[], uuid[], uuid[], timestamptz, timestamptz)'::regprocedure,
      'gnomon.neighbourhood_edges(uuid, uuid[], timestamptz, timestamptz)'::regprocedure,
      'gnomon.search_neighbourhood(uuid, uuid[], integer, timestamptz, timestamptz, integer)'::regprocedure,
      'gnomon.search(uuid, text, real[], timestamptz, timestamptz, uuid[], integer, double precision, double precision, double precision, double precision, double precision, integer)'::regprocedure
    ) and a.grantee <> p.proowner
    union all
    select format('table %s', c.oid::regclass), a.grantee
    from pg_class c, aclexplode(c.relacl) a
    where c.oid in ('gnomon.derived_search_token'::regclass, 'gnomon.derived_embedding'::regclass)
      and a.grantee <> c.relowner
  loop
    execute format(
      'revoke all on %s from %s', g.object,
      case when g.grantee = 0 then 'public' else g.grantee::regrole::text end);
  end loop;
  revoke all on function gnomon.derived_text_tokens(uuid, text, uuid[]) from public;
  revoke all on function gnomon.search_query_words(uuid, text) from public;
  revoke all on function gnomon.search_full_text_derived(
    uuid, text, timestamptz, timestamptz, integer) from public;
  revoke all on function gnomon.search_full_text_branch(
    uuid, text, timestamptz, timestamptz, integer) from public;
  revoke all on function gnomon.derived_embedding_derive() from public;
  revoke all on function gnomon.store_derived_embedding(uuid, text, uuid, real[]) from public;
  revoke all on function gnomon.store_summary_embedding(uuid, uuid, real[]) from public;
  revoke all on function gnomon.store_community_embedding(uuid, uuid, real[]) from public;
  revoke all on function gnomon.search_vector_derived(
    uuid, real[], timestamptz, timestamptz, integer) from public;
  revoke all on function gnomon.search_vector_branch(
    uuid, real[], timestamptz, timestamptz, integer) from public;
  revoke all on function gnomon.search_mentions(
    uuid, uuid[], text[], uuid[], text[], timestamptz, timestamptz) from public;
  revoke all on function gnomon.search_connectedness(
    uuid, uuid[], uuid[], uuid[], timestamptz, timestamptz) from public;
  revoke all on function gnomon.neighbourhood_edges(uuid, uuid[], timestamptz, timestamptz)
    from public;
  revoke all on function gnomon.search_neighbourhood(
    uuid, uuid[], integer, timestamptz, timestamptz, integer) from public;
  revoke all on function gnomon.search(
    uuid, text, real[], timestamptz, timestamptz, uuid[], integer, double precision,
    double precision, double precision, double precision, double precision, integer) from public;
end
$$;

-- Callers search, and store the embeddings of their own Summaries and
-- Communities. They read the token rows and write none, and insert an
-- embedding row, which the trigger derives, and nothing more.
grant select on gnomon.derived_search_token to authenticated, gnomon_worker;
grant select, insert on gnomon.derived_embedding to authenticated, gnomon_worker;
grant execute on function gnomon.search_query_words(uuid, text) to authenticated, gnomon_worker;
grant execute on function gnomon.search_full_text_derived(
  uuid, text, timestamptz, timestamptz, integer) to authenticated, gnomon_worker;
grant execute on function gnomon.search_full_text_branch(
  uuid, text, timestamptz, timestamptz, integer) to authenticated, gnomon_worker;
grant execute on function gnomon.store_derived_embedding(uuid, text, uuid, real[])
  to authenticated, gnomon_worker;
grant execute on function gnomon.store_summary_embedding(uuid, uuid, real[])
  to authenticated, gnomon_worker;
grant execute on function gnomon.store_community_embedding(uuid, uuid, real[])
  to authenticated, gnomon_worker;
grant execute on function gnomon.search_vector_derived(
  uuid, real[], timestamptz, timestamptz, integer) to authenticated, gnomon_worker;
grant execute on function gnomon.search_vector_branch(
  uuid, real[], timestamptz, timestamptz, integer) to authenticated, gnomon_worker;
grant execute on function gnomon.search_mentions(
  uuid, uuid[], text[], uuid[], text[], timestamptz, timestamptz) to authenticated, gnomon_worker;
grant execute on function gnomon.search_connectedness(
  uuid, uuid[], uuid[], uuid[], timestamptz, timestamptz) to authenticated, gnomon_worker;
grant execute on function gnomon.neighbourhood_edges(uuid, uuid[], timestamptz, timestamptz)
  to authenticated, gnomon_worker;
grant execute on function gnomon.search_neighbourhood(
  uuid, uuid[], integer, timestamptz, timestamptz, integer) to authenticated, gnomon_worker;
grant execute on function gnomon.search(
  uuid, text, real[], timestamptz, timestamptz, uuid[], integer, double precision,
  double precision, double precision, double precision, double precision, integer)
  to authenticated, gnomon_worker;

-- What the search index trigger needs to write the token rows, and Erasure
-- to delete them and the embedding rows. The vacuum call vacuums both
-- tables (migration 0013), which `gnomon.erasure_vacuum_tables()` now lists,
-- since the definer may delete from them.
grant select, insert, delete on gnomon.derived_search_token to gnomon_definer;
grant select, delete on gnomon.derived_embedding to gnomon_definer;
grant execute on function gnomon.derived_text_tokens(uuid, text, uuid[]) to gnomon_definer;
grant maintain on gnomon.derived_search_token, gnomon.derived_embedding to gnomon_maintainer;

insert into gnomon.schema_version (version, name) values (20, '0020_neighbourhood_search_and_fusion');
