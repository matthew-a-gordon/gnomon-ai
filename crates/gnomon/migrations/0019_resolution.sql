-- gnomon migration 0019: Resolution.
--
-- The Pipeline or a consumer matches a mention against the Nodes the graph
-- already has (GLOSSARY: Resolution). The call searches the current names
-- and Aliases of the Nodes the caller can see, by full text and by vector,
-- and returns candidate Nodes, ranked, each with the name or Alias it
-- matched. It returns candidates only. It merges nothing and writes no
-- `same_as` or `different_from` Edge (ADR-0012).
--
-- The name table. `gnomon.node_name` holds one row per distinct name and
-- Alias of each Node Version: the text, whether it is an Alias, its hash,
-- and the Version's Valid time and Recorded-time end. A Version's name and an
-- Alias with the same text are one row, the name's. The rows are written in
-- the write's own transaction, so a Node is found by a new name as soon as
-- the write commits, before the Pipeline embeds it (ADR-0010). Closed
-- Versions keep their rows, which the Recorded-time end marks. Resolution
-- reads only the current ones, `recorded_to = 'infinity'`, through partial
-- indexes. "Current" is the Store's current belief: a name a Node had over
-- an earlier Valid time stays current until a Correction, a Retraction or an
-- Erasure replaces it, because a mention in an Episode may use it. A call may
-- also name a valid-at instant, and then matches only the names that held
-- then.
--
-- Who writes. The name rows are Search's rows, derived from a Version's
-- text, so the search index trigger of migration 0016,
-- `gnomon.maintain_search_index`, writes them, as it writes the token rows.
-- No definer is added (ADR-0009). It calls `gnomon.index_search_text`, which
-- now writes a Version's name rows with its token rows. Callers hold SELECT
-- on the table and no insert or delete. A Version is closed by an update of
-- its Recorded-time end, which reaches its name rows through the foreign key
-- `(tenant_id, scope, version_id, recorded_to)` with ON UPDATE CASCADE, as it
-- reaches its quantized row (migration 0017). A Version deleted by the
-- transaction that wrote it, or by an Erasure, takes its name rows with it.
--
-- The full-text half reuses the token table of migration 0016, with no
-- token table of its own. Every Node Kind marks its name and Aliases as
-- searchable (migration 0003), so the Versions whose token rows hold a word
-- of the mention include every Node whose name or Alias does. Each such
-- Version's current name rows are then read by key, and each name or Alias
-- is scored on its own: BM25 with k1 = 1.2 and b = 0.75, the word's
-- frequency from the counts of the caller's allowed Scopes and the
-- transaction's own changes to them, as `gnomon.search_full_text` reads
-- them, and the length of that name or Alias in words. A word that only
-- another searchable property holds matches no name, so it finds nothing.
-- A Node's best-scoring name is its match.
--
-- The vector half has its own quantized copy, as prototype 5 measured it
-- (`prototype/exact-search`, FINDINGS.md: 54 ms and recall@10 of 0.95 at
-- 200,000 visible rows). Each name row carries the `bit` that
-- `binary_quantize` gives for the full vector of its text, once one is
-- stored, behind a covering partial index on (tenant_id, scope) of the
-- current rows that have one. The search scans the bit distance over that
-- index, keeps the 400 nearest, and re-ranks them at full precision through
-- one pinned primary-key probe per candidate, as `gnomon.search_vector` does
-- (migration 0017). A Node's nearest name is its match.
--
-- Full vectors. A name's text is hashed as a Sentence's is,
-- `gnomon.sentence_text_hash`, and its full vector is the row of
-- `gnomon.embedding` for that hash in the name's Scope (migration 0017).
-- There is no second full-vector table: a name whose text some Version,
-- Sentence or other name in the Scope already has an embedding of uses that
-- full vector, and the Pipeline stores none. The Pipeline calls
-- `gnomon.store_name_embedding` with a Node, one of its current names or
-- Aliases and the embedding of that text. The full vector is stored once
-- per text in the Scope, and every current name row of the Scope with that
-- text gets its bits. A name row written later, for a new Version or another
-- Node with the same text, gets its bits from the stored full vector when
-- the row is written, so a Version that keeps its name needs no new
-- embedding.
--
-- What callers may write. Callers update the bits of a name row in their
-- own Scopes, and nothing else, and a trigger sets the bits from the stored
-- full vector of the row's text whatever the caller supplied, refusing a
-- text with none. A foreign key from the rows with bits to the full vector
-- of their text, carrying the Scope, keeps any full vector from going while
-- a name row uses it, and the deferred check of migration 0017 now accepts
-- a full vector that a name row uses.
--
-- Erasure (ADR-0003). `gnomon.erase_version_text` now deletes the name rows
-- of every Version an Erasure deletes or recomputes, and the full vector of
-- each of their texts unless a quantized row or a name row with bits still
-- uses it. A Version an Erasure reopens gets its name rows again from its
-- text, through the search index trigger, as it gets its token rows. No name
-- row names an Episode, so `gnomon.erase_episode_rows` needs nothing more.
-- The definer may read the name rows of a Scope the caller cannot see that
-- share a text with an Edge the running Erasure reaches there, so that it
-- keeps a full vector a name there still uses.

-- The name table: one row per distinct name and Alias of a Node Version.
create table gnomon.node_name (
  tenant_id uuid not null,
  scope text not null,
  version_id uuid not null,
  node_id uuid not null,
  -- `gnomon.sentence_text_hash` of the text.
  text_hash text not null check (text_hash ~ '^[0-9a-f]{64}$'),
  name text not null check (name <> ''),
  alias boolean not null,
  -- The Version's Valid time and Recorded-time end.
  valid_from timestamptz not null,
  valid_to timestamptz not null,
  recorded_to timestamptz not null,
  -- The bits of the stored full vector of the text, or null while none is
  -- stored.
  q bit({{embedding_dimension}}),
  -- The text's hash while the row has bits, for the foreign key to the full
  -- vector.
  embedded_hash text generated always as (case when q is not null then text_hash end) stored,
  primary key (tenant_id, scope, version_id, text_hash),
  foreign key (tenant_id, scope, version_id, recorded_to)
    references gnomon.version (tenant_id, scope, id, recorded_to)
    on update cascade on delete cascade,
  foreign key (tenant_id, scope, embedded_hash)
    references gnomon.embedding (tenant_id, scope, text_hash)
);
-- The vector half reads only this index: the policy's conditions are its
-- index conditions, and the current rows with bits are its rows.
create index node_name_current_bits on gnomon.node_name (tenant_id, scope)
  include (version_id, text_hash, q, valid_from, valid_to)
  where recorded_to = 'infinity' and q is not null;
-- The name rows of a text: storing its embedding, the foreign key from the
-- full vector, and Erasure.
create index node_name_text on gnomon.node_name (tenant_id, scope, text_hash);

alter table gnomon.node_name enable row level security;
create policy node_name_tenant_scope on gnomon.node_name
  for all to authenticated, gnomon_worker, gnomon_definer
  using (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()))
  with check (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()));
-- The definer's reach into a Scope the caller cannot see (see the header).
create policy node_name_erasure_reach_read on gnomon.node_name
  for select to gnomon_definer
  using (tenant_id = gnomon.tenant_id()
         and gnomon.erasure_reaches_text(tenant_id, scope, text_hash));

-- The name rows of these Versions, from their text as it stands now: the
-- `name` and each of the `aliases` of each Node Version, once per distinct
-- text, the name before an Alias with the same text.
create function gnomon.node_names(p_tenant_id uuid, p_versions uuid[])
  returns table (
    tenant_id uuid,
    scope text,
    version_id uuid,
    node_id uuid,
    text_hash text,
    name text,
    alias boolean,
    valid_from timestamptz,
    valid_to timestamptz,
    recorded_to timestamptz
  )
  language sql stable security invoker set search_path = ''
  as $$
    select distinct on (v.id, h.text_hash)
           v.tenant_id, v.scope, v.id, v.node_id, h.text_hash, x.name, x.alias,
           v.valid_from, v.valid_to, v.recorded_to
    from gnomon.version v
    cross join lateral (
      select v.props ->> 'name' as name, false as alias
      where pg_catalog.jsonb_typeof(v.props -> 'name') = 'string'
      union all
      select a.name, true
      from pg_catalog.jsonb_array_elements_text(
             case when pg_catalog.jsonb_typeof(v.props -> 'aliases') = 'array'
                  then v.props -> 'aliases' else '[]'::jsonb end) a(name)
    ) x
    cross join lateral (select gnomon.sentence_text_hash(x.name) as text_hash) h
    where v.tenant_id = p_tenant_id and v.id = any(p_versions) and v.node_id is not null
      and x.name <> ''
    order by v.id, h.text_hash, x.alias
  $$;

-- Writes the name rows of these Versions that have none.
create function gnomon.index_node_names(p_tenant_id uuid, p_versions uuid[])
  returns void
  language sql volatile security invoker set search_path = ''
  as $$
    insert into gnomon.node_name (tenant_id, scope, version_id, node_id, text_hash, name, alias,
                                  valid_from, valid_to, recorded_to)
      select * from gnomon.node_names(p_tenant_id, p_versions)
      on conflict (tenant_id, scope, version_id, text_hash) do nothing
  $$;

-- Migration 0016's function, which the search index trigger calls for a
-- Version inserted, a Sentence stored or a Version an Erasure reopened, now
-- writing the Versions' name rows too. Everything else is as migration 0016
-- wrote it.
create or replace function gnomon.index_search_text(
  p_tenant_id uuid,
  p_versions uuid[],
  p_replace boolean
)
  returns void
  language plpgsql volatile security invoker set search_path = ''
  as $$
  begin
    perform gnomon.index_node_names(p_tenant_id, p_versions);
    if p_replace then
      delete from gnomon.search_token t
        using gnomon.version v
        where v.tenant_id = p_tenant_id and v.id = any(p_versions)
          and t.tenant_id = v.tenant_id and t.scope = v.scope and t.version_id = v.id;
    else
      p_versions := array(
        select v.id from gnomon.version v
        where v.tenant_id = p_tenant_id and v.id = any(p_versions)
          and not exists (select from gnomon.search_token t
                          where t.tenant_id = v.tenant_id and t.scope = v.scope
                            and t.version_id = v.id));
    end if;
    insert into gnomon.search_token (tenant_id, scope, token, version_id, tf, dl)
      select * from gnomon.search_text_tokens(p_tenant_id, p_versions);
  end
  $$;

-- The full-text half of Resolution: the Nodes whose current names and
-- Aliases hold a word of `p_mention`, each with its best-scoring one, best
-- first, at most `p_limit`. A null `p_valid_at` matches the names of every
-- Valid time. Ties go to the lower Node ID.
create function gnomon.resolve_full_text(
  p_tenant_id uuid,
  p_mention text,
  p_valid_at timestamptz,
  p_limit integer
)
  returns table (
    node_id uuid,
    scope text,
    version_id uuid,
    name text,
    alias boolean,
    score double precision,
    rank bigint
  )
  language sql stable security invoker set search_path = ''
  as $$
    with query as materialized (
      select distinct w.lexeme as token
      from pg_catalog.unnest(pg_catalog.to_tsvector(
             'simple'::pg_catalog.regconfig, coalesce(p_mention, ''))) w
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
    ),
    idf as materialized (
      select q.token,
             pg_catalog.ln(1 + (t.versions - f.versions + 0.5) / (f.versions + 0.5)) as idf
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
    ),
    -- The Versions whose searchable text holds a word of the mention.
    hits as materialized (
      select distinct p.scope, p.version_id
      from idf i
      join gnomon.search_token p on p.tenant_id = p_tenant_id and p.token = i.token
    ),
    -- Their current names and Aliases, read by key. `offset 0` keeps the
    -- planner from turning the probe into a join that scans every name row
    -- (ADR-0010: under RLS it estimates one row for every scan).
    names as materialized (
      select n.node_id, n.scope, n.version_id, n.name, n.alias, n.words
      from hits h
      cross join lateral (
        select x.node_id, x.scope, x.version_id, x.name, x.alias,
               pg_catalog.to_tsvector('simple'::pg_catalog.regconfig, x.name) as words
        from gnomon.node_name x
        where x.tenant_id = p_tenant_id and x.scope = h.scope and x.version_id = h.version_id
          and x.recorded_to = 'infinity'
          and (p_valid_at is null or (x.valid_from <= p_valid_at and p_valid_at < x.valid_to))
        offset 0
      ) n
    ),
    scored as (
      select n.node_id, n.scope, n.version_id, n.name, n.alias,
             pg_catalog.sum(i.idf * pg_catalog.cardinality(w.positions) * 2.2
               / (pg_catalog.cardinality(w.positions)
                  + 1.2 * (0.25 + 0.75 * l.words / (t.words / t.versions)))) as score
      from names n
      cross join lateral (
        select pg_catalog.sum(pg_catalog.cardinality(y.positions))::float8 as words
        from pg_catalog.unnest(n.words) y
      ) l
      cross join lateral pg_catalog.unnest(n.words) w
      join idf i on i.token = w.lexeme
      cross join totals t
      group by n.node_id, n.scope, n.version_id, n.name, n.alias
    ),
    best as (
      select distinct on (s.node_id) s.*
      from scored s
      order by s.node_id, s.score desc, s.alias, s.version_id, s.name
    )
    select b.node_id, b.scope, b.version_id, b.name, b.alias, b.score,
           pg_catalog.row_number() over (order by b.score desc, b.node_id)
    from best b
    order by b.score desc, b.node_id
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
  -- The search index trigger writes name rows as the definer role, and the
  -- trigger below quantizes their full vectors with pgvector's functions, so
  -- that role needs to reach pgvector's schema as the caller roles do
  -- (migration 0017).
  if not has_schema_privilege('gnomon_definer', ext, 'USAGE') then
    execute format('grant usage on schema %I to gnomon_definer', ext);
    if not has_schema_privilege('gnomon_definer', ext, 'USAGE') then
      raise exception 'the role gnomon_definer needs USAGE on schema %, where pgvector is; '
        'grant it as the schema''s owner and apply this migration again', ext;
    end if;
  end if;
  foreach run in array array[
$sql$
-- A name row's bits, from the stored full vector of its text in its Scope.
-- A row written with no full vector stored has none, and an update of its
-- bits with none stored is refused. It runs as whoever writes the row.
create function gnomon.node_name_derive() returns trigger
  language plpgsql security invoker set search_path = ''
  as $fn$
  begin
    new.q := (select @extschema@.binary_quantize(e.embedding)::bit({{embedding_dimension}})
              from gnomon.embedding e
              where e.tenant_id = new.tenant_id and e.scope = new.scope
                and e.text_hash = new.text_hash);
    if new.q is null and tg_op = 'UPDATE' then
      raise exception using errcode = 'GNW01',
        message = pg_catalog.format('no embedding of the name %s is stored in Scope %s',
                                    new.text_hash, new.scope);
    end if;
    return new;
  end
  $fn$
$sql$,
$sql$
-- Stores the embedding of `p_name`, a current name or Alias of Node
-- `p_node_id`, in the Node's Scope (see the header). When a full vector of
-- that text is stored already, it stays and `p_embedding` is not used.
-- Every current name row of the Scope with that text gets its bits.
create function gnomon.store_name_embedding(
  p_tenant_id uuid,
  p_node_id uuid,
  p_name text,
  p_embedding real[]
)
  returns void
  language plpgsql volatile security invoker set search_path = ''
  as $fn$
  declare
    v_vector @extschema@.vector := gnomon.embedding_vector(p_embedding);
    v_hash text := gnomon.sentence_text_hash(p_name);
    v_scope text;
  begin
    select x.scope into v_scope
      from gnomon.node x where x.tenant_id = p_tenant_id and x.id = p_node_id;
    if not found then
      raise exception using errcode = 'GNW01',
        message = pg_catalog.format('Node %s is not visible to the caller', p_node_id);
    end if;
    if not exists (select from gnomon.node_name n
                   where n.tenant_id = p_tenant_id and n.scope = v_scope
                     and n.text_hash = v_hash and n.node_id = p_node_id
                     and n.recorded_to = 'infinity') then
      raise exception using errcode = 'GNW01',
        message = pg_catalog.format('Node %s has no current name or Alias %L',
                                    p_node_id, p_name);
    end if;
    insert into gnomon.embedding (tenant_id, scope, text_hash, embedding)
      values (p_tenant_id, v_scope, v_hash, v_vector)
      on conflict (tenant_id, scope, text_hash) do nothing;
    -- The trigger sets the bits from the stored full vector.
    update gnomon.node_name n set q = null
      where n.tenant_id = p_tenant_id and n.scope = v_scope and n.text_hash = v_hash
        and n.recorded_to = 'infinity' and n.q is null;
  end
  $fn$
$sql$,
$sql$
-- The vector half of Resolution: the Nodes whose current names and Aliases
-- have stored embeddings nearest `p_embedding` by cosine distance at full
-- precision, each with its nearest one, nearest first, at most `p_limit`. A
-- null `p_embedding` finds nothing, and a null `p_valid_at` matches the
-- names of every Valid time. Ties go to the lower Node ID. See the header
-- for the plan.
create function gnomon.resolve_vector(
  p_tenant_id uuid,
  p_embedding real[],
  p_valid_at timestamptz,
  p_limit integer
)
  returns table (
    node_id uuid,
    scope text,
    version_id uuid,
    name text,
    alias boolean,
    distance double precision,
    rank bigint
  )
  language plpgsql stable security invoker set search_path = ''
  as $fn$
  declare
    v_query @extschema@.vector;
    v_bits bit({{embedding_dimension}});
  begin
    if p_embedding is null then
      return;
    end if;
    v_query := gnomon.embedding_vector(p_embedding);
    v_bits := @extschema@.binary_quantize(v_query)::bit({{embedding_dimension}});
    return query
      with candidates as materialized (
        select n.scope as nscope, n.version_id as nversion, n.text_hash as nhash
        from gnomon.node_name n
        where n.tenant_id = p_tenant_id and n.recorded_to = 'infinity' and n.q is not null
          and (p_valid_at is null or (n.valid_from <= p_valid_at and p_valid_at < n.valid_to))
        order by n.q operator(@extschema@.<~>) v_bits, n.version_id, n.text_hash
        limit greatest(400, p_limit)
      ),
      reranked as materialized (
        select c.nscope, c.nversion, c.nhash,
               (f.embedding operator(@extschema@.<=>) v_query)::double precision as dist
        from candidates c
        -- The pin: one primary-key probe per candidate (migration 0017).
        cross join lateral (
          select e.embedding from gnomon.embedding e
          where e.tenant_id = p_tenant_id and e.scope = c.nscope and e.text_hash = c.nhash
          limit 1
        ) f
      ),
      named as (
        select x.node_id as nnode, r.nscope, r.nversion, x.name as nname, x.alias as nalias,
               r.dist
        from reranked r
        cross join lateral (
          select y.node_id, y.name, y.alias from gnomon.node_name y
          where y.tenant_id = p_tenant_id and y.scope = r.nscope and y.version_id = r.nversion
            and y.text_hash = r.nhash
          limit 1
        ) x
      ),
      best as (
        select distinct on (m.nnode) m.*
        from named m
        order by m.nnode, m.dist, m.nalias, m.nversion, m.nname
      )
      select b.nnode, b.nscope, b.nversion, b.nname, b.nalias, b.dist,
             pg_catalog.row_number() over (order by b.dist, b.nnode)
      from best b
      order by b.dist, b.nnode
      limit p_limit;
  end
  $fn$
$sql$
  ] loop
    execute replace(run, '@extschema@', quote_ident(ext));
  end loop;
end
$migration$;

create trigger node_name_derive before insert or update of q on gnomon.node_name
  for each row execute function gnomon.node_name_derive();

-- Migration 0017's check at commit that every full vector the transaction
-- stored is used, now by a quantized row or by a name row with bits.
create or replace function gnomon.embedding_check_used() returns trigger
  language plpgsql security invoker set search_path = ''
  as $$
  begin
    if exists (select from gnomon.embedding e
               where e.tenant_id = new.tenant_id and e.scope = new.scope
                 and e.text_hash = new.text_hash)
       and not exists (select from gnomon.embedding_bit b
                       where b.tenant_id = new.tenant_id and b.scope = new.scope
                         and b.text_hash = new.text_hash)
       and not exists (select from gnomon.node_name n
                       where n.tenant_id = new.tenant_id and n.scope = new.scope
                         and n.text_hash = new.text_hash and n.q is not null) then
      raise exception using errcode = 'GNW01',
        message = pg_catalog.format(
          'an embedding of the text %s is stored for a Version, and no Version''s embedding uses it',
          new.text_hash);
    end if;
    return null;
  end
  $$;

-- Erasure (ADR-0003). Every row derived from the text of a Version an
-- Erasure deletes or recomputes goes: its name rows, its quantized row, the
-- full vector of each of its texts that no other quantized row or name row
-- with bits uses, its Sentence and its token rows. Everything else is as
-- migration 0017 wrote it. The embeddings go first, while the name rows and
-- the Sentences still give their hashes.
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

-- Resolution: the Nodes whose current names and Aliases match `p_mention`,
-- fused from the full-text and vector halves by reciprocal rank fusion with
-- k = 60, as prototype 5 fused Search, best first, at most `p_limit`. Each
-- candidate reports what each half found: its matched name or Alias, its
-- score or distance and its rank, or nulls where that half did not find it.
-- Its own name and Alias are those of the half that ranked it higher, the
-- full-text half's on a tie. Ties go to the lower Node ID.
create function gnomon.resolve(
  p_tenant_id uuid,
  p_mention text,
  p_embedding real[],
  p_valid_at timestamptz,
  p_limit integer
)
  returns table (
    node_id uuid,
    scope text,
    name text,
    alias boolean,
    score double precision,
    rank bigint,
    text_version_id uuid,
    text_name text,
    text_alias boolean,
    text_score double precision,
    text_rank bigint,
    vector_version_id uuid,
    vector_name text,
    vector_alias boolean,
    vector_distance double precision,
    vector_rank bigint
  )
  language plpgsql stable security invoker set search_path = ''
  as $$
  begin
    if p_limit is null or p_limit < 1 then
      raise exception using errcode = 'invalid_parameter_value',
        message = format('a Resolution returns at least one candidate, not %s', p_limit);
    end if;
    return query
      with t as materialized (
        select * from gnomon.resolve_full_text(p_tenant_id, p_mention, p_valid_at, p_limit)
      ),
      v as materialized (
        select * from gnomon.resolve_vector(p_tenant_id, p_embedding, p_valid_at, p_limit)
      ),
      fused as (
        select coalesce(t.node_id, v.node_id) as node_id, coalesce(t.scope, v.scope) as scope,
               t.version_id as t_version, t.name as t_name, t.alias as t_alias,
               t.score as t_score, t.rank as t_rank,
               v.version_id as v_version, v.name as v_name, v.alias as v_alias,
               v.distance as v_distance, v.rank as v_rank,
               coalesce(1.0::float8 / (60 + t.rank), 0) + coalesce(1.0::float8 / (60 + v.rank), 0)
                 as score
        from t full join v on v.node_id = t.node_id
      )
      select f.node_id, f.scope,
             case when f.v_rank is null or f.t_rank <= f.v_rank then f.t_name else f.v_name end,
             case when f.v_rank is null or f.t_rank <= f.v_rank then f.t_alias else f.v_alias end,
             f.score,
             pg_catalog.row_number() over (order by f.score desc, f.node_id),
             f.t_version, f.t_name, f.t_alias, f.t_score, f.t_rank,
             f.v_version, f.v_name, f.v_alias, f.v_distance, f.v_rank
      from fused f
      order by f.score desc, f.node_id
      limit p_limit;
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
      'gnomon.node_names(uuid, uuid[])'::regprocedure,
      'gnomon.index_node_names(uuid, uuid[])'::regprocedure,
      'gnomon.resolve_full_text(uuid, text, timestamptz, integer)'::regprocedure,
      'gnomon.node_name_derive()'::regprocedure,
      'gnomon.store_name_embedding(uuid, uuid, text, real[])'::regprocedure,
      'gnomon.resolve_vector(uuid, real[], timestamptz, integer)'::regprocedure,
      'gnomon.resolve(uuid, text, real[], timestamptz, integer)'::regprocedure
    ) and a.grantee <> p.proowner
    union all
    select format('table %s', c.oid::regclass), a.grantee
    from pg_class c, aclexplode(c.relacl) a
    where c.oid = 'gnomon.node_name'::regclass and a.grantee <> c.relowner
  loop
    execute format(
      'revoke all on %s from %s', g.object,
      case when g.grantee = 0 then 'public' else g.grantee::regrole::text end);
  end loop;
  revoke all on function gnomon.node_names(uuid, uuid[]) from public;
  revoke all on function gnomon.index_node_names(uuid, uuid[]) from public;
  revoke all on function gnomon.resolve_full_text(uuid, text, timestamptz, integer) from public;
  revoke all on function gnomon.resolve(uuid, text, real[], timestamptz, integer) from public;
  revoke all on function gnomon.node_name_derive() from public;
  revoke all on function gnomon.store_name_embedding(uuid, uuid, text, real[]) from public;
  revoke all on function gnomon.resolve_vector(uuid, real[], timestamptz, integer) from public;
end
$$;

-- Callers read the name rows of their own Scopes, through Resolution, and
-- write only their bits, which the trigger derives (see the header).
grant select on gnomon.node_name to authenticated, gnomon_worker;
grant update (q) on gnomon.node_name to authenticated, gnomon_worker;
grant execute on function gnomon.store_name_embedding(uuid, uuid, text, real[])
  to authenticated, gnomon_worker;
grant execute on function gnomon.resolve_vector(uuid, real[], timestamptz, integer)
  to authenticated, gnomon_worker;
grant execute on function gnomon.resolve_full_text(uuid, text, timestamptz, integer)
  to authenticated, gnomon_worker;
grant execute on function gnomon.resolve(uuid, text, real[], timestamptz, integer)
  to authenticated, gnomon_worker;

-- What the search index trigger needs to write the name rows, and Erasure
-- to delete them. The vacuum call vacuums the table (migration 0013), which
-- `gnomon.erasure_vacuum_tables()` now lists, since the definer may delete
-- from it.
grant select, insert, delete on gnomon.node_name to gnomon_definer;
grant execute on function gnomon.node_names(uuid, uuid[]) to gnomon_definer;
grant execute on function gnomon.index_node_names(uuid, uuid[]) to gnomon_definer;
grant maintain on gnomon.node_name to gnomon_maintainer;

insert into gnomon.schema_version (version, name) values (19, '0019_resolution');
