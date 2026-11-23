-- gnomon migration 0017: the vector branch of Search.
--
-- The Pipeline stores an embedding of a Version's search text after the
-- write commits. A consumer searches with a query embedding at a valid-at
-- and a known-at instant, and gets the nearest Versions at full precision,
-- exactly over the rows the caller can see, for every Tenant and Scope size
-- (ADR-0010).
--
-- No vector index is used. A vector index scan applies Tenant, Scope and
-- As-of only to the rows it returns, so a small Scope inside a large Tenant
-- loses its own rows: prototype 1 measured HNSW recall@10 of 0.13 for a
-- 100-row Tenant in a 1M-row table. Instead, as prototype 5 measured
-- (`prototype/exact-search`, FINDINGS.md):
--
--   `gnomon.embedding`      the full vector, once per distinct text, keyed by
--                           Tenant, Scope and the text's hash, with PLAIN
--                           storage so a re-rank probe reads one heap page.
--   `gnomon.embedding_bit`  one row per Version: the `bit` that
--                           `binary_quantize` gives, the text's hash and the
--                           Version's four time columns, behind a covering
--                           B-tree on (tenant_id, scope). The policy's
--                           conditions are its index conditions (ADR-0005),
--                           and the As-of conditions are checked on the index
--                           tuples, which are leakproof `timestamptz`
--                           comparisons (ADR-0004).
--
-- Search scans the bit distance over the covering index, keeps the 400
-- nearest, and re-ranks them at full precision. Prototype 5 found recall@10
-- of 0.99 at 200,000 visible rows with 400, against 0.83 with 200. Under RLS
-- the planner estimates one row for every scan, so it would join the
-- candidates to the full vectors by scanning every visible full vector (660
-- ms against 96 ms). The re-rank is pinned: each candidate's full vector is
-- read by one primary-key probe in a lateral subquery with `limit 1`, which
-- the planner cannot pull up into an ordinary join. There is no `halfvec`,
-- no HNSW and no partitioning. pgvector 0.7.0 has everything this uses.
--
-- Where pgvector lives. `create extension vector` put pgvector in whatever
-- schema it chose: `public` on most hosts, `extensions` on Supabase. Every
-- function here runs with `search_path = ''`, so each names pgvector's types,
-- operators and functions with that schema, which this migration reads from
-- `pg_extension` and writes into the SQL it runs in place of `@extschema@`.
-- No caller's search path can then change what a name means. The functions
-- run as the caller, so the caller roles get USAGE on that schema when they
-- lack it, and the migration fails when the role that applies it cannot
-- grant that.
--
-- Text hashes. A Version's embedding is of its search text: its properties,
-- whose hash is `version.text_hash`, or, for an Edge Version, its Sentence,
-- whose hash is `gnomon.sentence_text_hash(sentence)` (ADR-0013). Versions
-- with the same text share one full vector, so a Refinement, which changes
-- no text, costs no new embedding. A Sentence is stored after its Version
-- commits and is not carried to a Refinement Version, so ticket #26 stores
-- the Refinement's Sentence and then calls `gnomon.store_embedding` with the
-- Sentence's hash, which finds the full vector already stored.
--
-- Who writes. The Pipeline calls `gnomon.store_embedding`, an invoker, as
-- any caller would (ADR-0009). Callers may insert into both tables and
-- change a quantized row's text hash, and nothing else: no update of a full
-- vector, no delete. Every row a caller writes, by the function or by its
-- own statement, passes the same checks:
--
--   RLS          the row is in the caller's Tenant and one of its Scopes.
--   The trigger  a quantized row names a Version the caller can see in that
--                Scope, and a hash of that Version's text or Sentence. Its
--                bits come from the full vector stored for that hash in the
--                same Scope, and its time columns from the Version, whatever
--                the caller supplied. No full vector of another Scope can be
--                attached: the foreign key to the full vector carries the
--                Scope.
--   Commit       a full vector inserted in the transaction is the vector of
--                a quantized row by then, so none is stored for a hash no
--                Version has.
--
-- The write path gains no trigger function of gnomon's. The triggers above
-- run only when an embedding is stored. A Version is closed by an update of
-- its Recorded-time end, which reaches its quantized row through the foreign
-- key `(tenant_id, scope, version_id, recorded_to)` with ON UPDATE CASCADE.
-- That key costs the write path Postgres's own referential triggers on
-- `gnomon.version`, which look up a closed or deleted Version's quantized row
-- by its primary key, and the unique index on those columns of
-- `gnomon.version` the key needs. A Version deleted by the transaction that
-- wrote it, or by an Erasure, takes its quantized row with it.
--
-- Erasure (ADR-0003). `gnomon.erase_version_text` now deletes the quantized
-- rows of every Version an Erasure deletes or recomputes, then the full
-- vector of each text those Versions had, their properties' and their
-- Sentences', unless a quantized row of another Version still uses it. A
-- Version uses a full vector through its quantized row, so a full vector
-- shared with a surviving Version stays, and one a recomputed Version alone
-- used goes with its row. The Pipeline embeds the recomputed text afterwards.
-- A full vector is kept while a quantized row uses it, not while some
-- Version merely has its text: a surviving Version with no quantized row yet
-- gets its vector again when the Pipeline embeds it, and checking every
-- Version of a Scope by text would cost an Erasure a scan of the Scope or the
-- write path an index on the text hash. No embedding row names an Episode,
-- and an embedding reaches its Episodes through its Version's Citations, so
-- `gnomon.erase_episode_rows` needs nothing more. The definer may read and delete the embeddings of an Edge the
-- running Erasure reaches in a Scope the caller cannot see (migration 0012),
-- and read the quantized rows of that Scope that share their texts, so that
-- it keeps a full vector another Version there still uses.

-- The hash of a Sentence's text, as a quantized row of its Version names it.
create function gnomon.sentence_text_hash(p_sentence text) returns text
  language sql immutable parallel safe security invoker set search_path = ''
  as $$
    select pg_catalog.encode(pg_catalog.sha256(pg_catalog.convert_to(p_sentence, 'UTF8')), 'hex')
  $$;

-- The key of a quantized row's foreign key to its Version.
create unique index version_embedding_key on gnomon.version (tenant_id, scope, id, recorded_to);

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
  -- The Store's functions run as the caller, so the caller roles need to
  -- reach pgvector's types and functions. `public` and Supabase's
  -- `extensions` already let them.
  if not (has_schema_privilege('authenticated', ext, 'USAGE')
          and has_schema_privilege('gnomon_worker', ext, 'USAGE')) then
    execute format('grant usage on schema %I to authenticated, gnomon_worker', ext);
    -- A role that neither owns the schema nor holds the grant option is
    -- only warned, and grants nothing.
    if not (has_schema_privilege('authenticated', ext, 'USAGE')
            and has_schema_privilege('gnomon_worker', ext, 'USAGE')) then
      raise exception 'the caller roles authenticated and gnomon_worker need USAGE on schema %, '
        'where pgvector is; grant it as the schema''s owner and apply this migration again', ext;
    end if;
  end if;
  foreach run in array array[
$sql$
-- The full vector of one text in one Scope.
create table gnomon.embedding (
  tenant_id uuid not null,
  scope text not null,
  text_hash text not null check (text_hash ~ '^[0-9a-f]{64}$'),
  embedding @extschema@.vector({{embedding_dimension}}) not null
    check (@extschema@.vector_norm(embedding) > 0),
  primary key (tenant_id, scope, text_hash)
)
$sql$,
$sql$
-- The quantized row of one Version.
create table gnomon.embedding_bit (
  tenant_id uuid not null,
  scope text not null,
  version_id uuid not null,
  text_hash text not null,
  q bit({{embedding_dimension}}) not null,
  valid_from timestamptz not null,
  valid_to timestamptz not null,
  recorded_from timestamptz not null,
  recorded_to timestamptz not null,
  primary key (tenant_id, scope, version_id),
  foreign key (tenant_id, scope, version_id, recorded_to)
    references gnomon.version (tenant_id, scope, id, recorded_to)
    on update cascade on delete cascade,
  foreign key (tenant_id, scope, text_hash)
    references gnomon.embedding (tenant_id, scope, text_hash)
)
$sql$,
$sql$
-- A query or stored embedding as a vector of the exported dimension. It
-- refuses one of another length, one with a null, NaN or infinite element,
-- and one of length zero, which has no cosine distance.
create function gnomon.embedding_vector(p_embedding real[])
  returns @extschema@.vector
  language plpgsql immutable security invoker set search_path = ''
  as $fn$
  begin
    if p_embedding is null or pg_catalog.array_ndims(p_embedding) is distinct from 1
       or pg_catalog.cardinality(p_embedding) <> gnomon.embedding_dimension() then
      raise exception using errcode = 'invalid_parameter_value',
        message = pg_catalog.format('an embedding has %s dimensions, not %s',
                                    gnomon.embedding_dimension(),
                                    coalesce(pg_catalog.cardinality(p_embedding), 0));
    end if;
    if exists (select from pg_catalog.unnest(p_embedding) x
               where x is null or x = 'NaN'::real or x in ('Infinity'::real, '-Infinity'::real)) then
      raise exception using errcode = 'invalid_parameter_value',
        message = 'an embedding holds only finite numbers';
    end if;
    if not exists (select from pg_catalog.unnest(p_embedding) x where x <> 0) then
      raise exception using errcode = 'invalid_parameter_value',
        message = 'an embedding of length zero has no direction';
    end if;
    return p_embedding::@extschema@.vector({{embedding_dimension}});
  end
  $fn$
$sql$,
$sql$
-- A quantized row as the Store derives it (see the header). It runs as the
-- caller, under RLS.
create function gnomon.embedding_bit_derive() returns trigger
  language plpgsql security invoker set search_path = ''
  as $fn$
  declare
    v record;
  begin
    select x.valid_from, x.valid_to, x.recorded_from, x.recorded_to, x.text_hash into v
      from gnomon.version x
      where x.tenant_id = new.tenant_id and x.scope = new.scope and x.id = new.version_id;
    if not found then
      raise exception using errcode = 'GNW01',
        message = pg_catalog.format('Version %s is not visible to the caller in Scope %s',
                                    new.version_id, new.scope);
    end if;
    if new.text_hash is distinct from v.text_hash
       and not exists (select from gnomon.sentence s
                       where s.tenant_id = new.tenant_id and s.scope = new.scope
                         and s.version_id = new.version_id
                         and gnomon.sentence_text_hash(s.sentence) = new.text_hash) then
      raise exception using errcode = 'GNW01',
        message = pg_catalog.format(
          'an embedding of Version %s is of its text or its Sentence, and %s is the hash of neither',
          new.version_id, new.text_hash);
    end if;
    select @extschema@.binary_quantize(e.embedding)::bit({{embedding_dimension}}) into new.q
      from gnomon.embedding e
      where e.tenant_id = new.tenant_id and e.scope = new.scope and e.text_hash = new.text_hash;
    if not found then
      raise exception using errcode = 'GNW01',
        message = pg_catalog.format('no embedding of the text %s is stored in Scope %s',
                                    new.text_hash, new.scope);
    end if;
    new.valid_from := v.valid_from;
    new.valid_to := v.valid_to;
    new.recorded_from := v.recorded_from;
    new.recorded_to := v.recorded_to;
    return new;
  end
  $fn$
$sql$,
$sql$
-- Stores the embedding of Version `p_version_id`'s text, whose hash is
-- `p_text_hash`: the Version's `text_hash`, or, for an Edge Version, its
-- Sentence's `gnomon.sentence_text_hash`. The full vector is stored once
-- per text in the Version's Scope: when one is stored already, it stays and
-- `p_embedding` is not used. The Version's quantized row is written, or
-- moved to this text.
create function gnomon.store_embedding(
  p_tenant_id uuid,
  p_version_id uuid,
  p_text_hash text,
  p_embedding real[]
)
  returns void
  language plpgsql volatile security invoker set search_path = ''
  as $fn$
  declare
    v_vector @extschema@.vector := gnomon.embedding_vector(p_embedding);
    v_scope text;
  begin
    select x.scope into v_scope
      from gnomon.version x where x.tenant_id = p_tenant_id and x.id = p_version_id;
    if not found then
      raise exception using errcode = 'GNW01',
        message = pg_catalog.format('Version %s is not visible to the caller', p_version_id);
    end if;
    insert into gnomon.embedding (tenant_id, scope, text_hash, embedding)
      values (p_tenant_id, v_scope, p_text_hash, v_vector)
      on conflict (tenant_id, scope, text_hash) do nothing;
    insert into gnomon.embedding_bit as b (tenant_id, scope, version_id, text_hash)
      values (p_tenant_id, v_scope, p_version_id, p_text_hash)
      on conflict (tenant_id, scope, version_id)
        do update set text_hash = excluded.text_hash
        where b.text_hash <> excluded.text_hash;
  end
  $fn$
$sql$,
$sql$
-- The vector branch of Search: the Versions nearest `p_query` by cosine
-- distance at full precision, As-of the valid-at and known-at instants (a
-- null known-at reads the current belief), nearest first, at most `p_limit`.
-- Ties go to the lower Version ID. See the header for the plan.
create function gnomon.search_vector(
  p_tenant_id uuid,
  p_query real[],
  p_valid_at timestamptz,
  p_known_at timestamptz,
  p_limit integer
)
  returns table (
    version_id uuid,
    node_id uuid,
    edge_id uuid,
    scope text,
    distance double precision,
    rank bigint
  )
  language plpgsql stable security invoker set search_path = ''
  as $fn$
  declare
    v_query @extschema@.vector;
    v_bits bit({{embedding_dimension}});
  begin
    if p_limit is null or p_limit < 1 then
      raise exception using errcode = 'invalid_parameter_value',
        message = pg_catalog.format('a Search returns at least one candidate, not %s', p_limit);
    end if;
    if p_valid_at is null then
      raise exception using errcode = 'invalid_parameter_value',
        message = 'a Search needs a valid-at instant';
    end if;
    v_query := gnomon.embedding_vector(p_query);
    v_bits := @extschema@.binary_quantize(v_query)::bit({{embedding_dimension}});
    return query
      with candidates as materialized (
        select b.version_id as vid, b.scope as vscope, b.text_hash as vhash
        from gnomon.embedding_bit b
        where b.tenant_id = p_tenant_id
          and b.valid_from <= p_valid_at and p_valid_at < b.valid_to
          and case
                when p_known_at is null then b.recorded_to = 'infinity'
                else b.recorded_from <= p_known_at and p_known_at < b.recorded_to
              end
        order by b.q operator(@extschema@.<~>) v_bits, b.version_id
        limit greatest(400, p_limit)
      ),
      reranked as materialized (
        select c.vid, c.vscope,
               (f.embedding operator(@extschema@.<=>) v_query)::double precision as dist
        from candidates c
        -- The pin: one primary-key probe per candidate (see the header).
        cross join lateral (
          select e.embedding from gnomon.embedding e
          where e.tenant_id = p_tenant_id and e.scope = c.vscope and e.text_hash = c.vhash
          limit 1
        ) f
        order by 3, c.vid
        limit p_limit
      )
      select r.vid, x.node_id, x.edge_id, r.vscope, r.dist,
             pg_catalog.row_number() over (order by r.dist, r.vid)
      from reranked r
      cross join lateral (
        select y.node_id, y.edge_id from gnomon.version y
        where y.tenant_id = p_tenant_id and y.scope = r.vscope and y.id = r.vid
        limit 1
      ) x
      order by r.dist, r.vid;
  end
  $fn$
$sql$
  ] loop
    execute replace(run, '@extschema@', quote_ident(ext));
  end loop;
end
$migration$;

-- A full vector is read by a re-rank probe from one heap page. A row of
-- more than about 1,900 dimensions does not fit in a page, and is stored out
-- of line without compression instead.
do $$
begin
  if gnomon.embedding_dimension() <= 1900 then
    alter table gnomon.embedding alter column embedding set storage plain;
  else
    alter table gnomon.embedding alter column embedding set storage external;
  end if;
end
$$;

-- The bit scan reads only this index: the policy's conditions are its index
-- conditions and the As-of conditions are checked on its tuples.
create index embedding_bit_scan on gnomon.embedding_bit (tenant_id, scope)
  include (version_id, text_hash, q, valid_from, valid_to, recorded_from, recorded_to);
-- The quantized rows that use a full vector, for its foreign key and for
-- Erasure.
create index embedding_bit_text on gnomon.embedding_bit (tenant_id, scope, text_hash);

create trigger embedding_bit_derive before insert or update of text_hash, q, valid_from,
  valid_to, recorded_from on gnomon.embedding_bit
  for each row execute function gnomon.embedding_bit_derive();

-- At commit, every full vector the transaction stored is used by a
-- quantized row.
create function gnomon.embedding_check_used() returns trigger
  language plpgsql security invoker set search_path = ''
  as $$
  begin
    if exists (select from gnomon.embedding e
               where e.tenant_id = new.tenant_id and e.scope = new.scope
                 and e.text_hash = new.text_hash)
       and not exists (select from gnomon.embedding_bit b
                       where b.tenant_id = new.tenant_id and b.scope = new.scope
                         and b.text_hash = new.text_hash) then
      raise exception using errcode = 'GNW01',
        message = pg_catalog.format(
          'an embedding of the text %s is stored for a Version, and no Version''s embedding uses it',
          new.text_hash);
    end if;
    return null;
  end
  $$;
create constraint trigger embedding_check_used after insert on gnomon.embedding
  deferrable initially deferred
  for each row execute function gnomon.embedding_check_used();

alter table gnomon.embedding enable row level security;
alter table gnomon.embedding_bit enable row level security;

create policy embedding_tenant_scope on gnomon.embedding
  for all to authenticated, gnomon_worker, gnomon_definer
  using (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()))
  with check (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()));
create policy embedding_bit_tenant_scope on gnomon.embedding_bit
  for all to authenticated, gnomon_worker, gnomon_definer
  using (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()))
  with check (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()));

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

-- Erasure (ADR-0003). Every row derived from the text of a Version an
-- Erasure deletes or recomputes goes: its quantized row, the full vector of
-- each of its texts that no other Version's quantized row uses, its
-- Sentence, as migration 0011 wrote, and its token rows, as migration 0016
-- wrote. The embeddings go first, while the Sentences still give their
-- hashes.
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
      ) t;
    delete from gnomon.embedding_bit b
      using gnomon.version v
      where v.tenant_id = p_tenant_id and v.id = any(p_versions)
        and b.tenant_id = v.tenant_id and b.scope = v.scope and b.version_id = v.id;
    delete from gnomon.embedding e
      using unnest(v_scopes, v_hashes) t(scope, text_hash)
      where e.tenant_id = p_tenant_id and e.scope = t.scope and e.text_hash = t.text_hash
        and not exists (select from gnomon.embedding_bit b
                        where b.tenant_id = e.tenant_id and b.scope = e.scope
                          and b.text_hash = e.text_hash);
    delete from gnomon.sentence s
      where s.tenant_id = p_tenant_id and s.version_id = any(p_versions);
    delete from gnomon.search_token t
      using gnomon.version v
      where v.tenant_id = p_tenant_id and v.id = any(p_versions)
        and t.tenant_id = v.tenant_id and t.scope = v.scope and t.version_id = v.id;
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
      'gnomon.sentence_text_hash(text)'::regprocedure,
      'gnomon.embedding_vector(real[])'::regprocedure,
      'gnomon.embedding_bit_derive()'::regprocedure,
      'gnomon.embedding_check_used()'::regprocedure,
      'gnomon.store_embedding(uuid, uuid, text, real[])'::regprocedure,
      'gnomon.search_vector(uuid, real[], timestamptz, timestamptz, integer)'::regprocedure,
      'gnomon.erasure_reaches_text(uuid, text, text)'::regprocedure
    ) and a.grantee <> p.proowner
    union all
    select format('table %s', c.oid::regclass), a.grantee
    from pg_class c, aclexplode(c.relacl) a
    where c.oid in ('gnomon.embedding'::regclass, 'gnomon.embedding_bit'::regclass)
      and a.grantee <> c.relowner
  loop
    execute format(
      'revoke all on %s from %s', g.object,
      case when g.grantee = 0 then 'public' else g.grantee::regrole::text end);
  end loop;
  revoke all on function gnomon.sentence_text_hash(text) from public;
  revoke all on function gnomon.embedding_vector(real[]) from public;
  revoke all on function gnomon.embedding_bit_derive() from public;
  revoke all on function gnomon.embedding_check_used() from public;
  revoke all on function gnomon.store_embedding(uuid, uuid, text, real[]) from public;
  revoke all on function gnomon.search_vector(uuid, real[], timestamptz, timestamptz, integer)
    from public;
  revoke all on function gnomon.erasure_reaches_text(uuid, text, text) from public;
end
$$;

-- Callers store embeddings and search them, in their own Scopes. They may
-- insert, and move a quantized row to another text of its Version, and
-- nothing more (see the header).
grant select, insert on gnomon.embedding to authenticated, gnomon_worker;
grant select, insert on gnomon.embedding_bit to authenticated, gnomon_worker;
grant update (text_hash) on gnomon.embedding_bit to authenticated, gnomon_worker;
grant execute on function gnomon.sentence_text_hash(text) to authenticated, gnomon_worker;
grant execute on function gnomon.embedding_vector(real[]) to authenticated, gnomon_worker;
grant execute on function gnomon.store_embedding(uuid, uuid, text, real[])
  to authenticated, gnomon_worker;
grant execute on function gnomon.search_vector(uuid, real[], timestamptz, timestamptz, integer)
  to authenticated, gnomon_worker;

-- What Erasure needs: to read and delete both tables, and the functions its
-- policies and `gnomon.erase_version_text` call. The vacuum call vacuums
-- both (migration 0013), which `gnomon.erasure_vacuum_tables()` now lists,
-- since the definer may delete from them.
grant select, delete on gnomon.embedding, gnomon.embedding_bit to gnomon_definer;
grant execute on function gnomon.sentence_text_hash(text) to gnomon_definer;
grant execute on function gnomon.erasure_reaches_text(uuid, text, text) to gnomon_definer;
grant maintain on gnomon.embedding, gnomon.embedding_bit to gnomon_maintainer;

insert into gnomon.schema_version (version, name) values (17, '0017_vector_search');
