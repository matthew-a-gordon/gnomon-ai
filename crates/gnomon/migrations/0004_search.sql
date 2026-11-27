-- gnomon 0004: Search. The full-text and vector branches, Resolution, the
-- neighbourhood and fusion, Sentences, and the gap listing (ADR-0010,
-- ADR-0013).

-- The Kind marks of `p_kind` in a Tenant: a root Kind's, a reserved Kind's,
-- or a registered Kind's, flattened with its ancestors'.
create function gnomon.kind_search(p_tenant_id uuid, p_kind text)
  returns text[]
  language sql stable security invoker set search_path = ''
  as $$
    select coalesce(
      (select r.search from gnomon.root_kinds() r where r.name = p_kind),
      (select r.search from gnomon.reserved_kinds() r where r.name = p_kind),
      (select k.search from gnomon.kind k where k.tenant_id = p_tenant_id and k.name = p_kind),
      '{}')
  $$;

-- The token rows of these Versions, from their text as it stands now. Each
-- row has its Version's Tenant and Scope.
create function gnomon.search_text_tokens(p_tenant_id uuid, p_versions uuid[])
  returns table (
    tenant_id uuid,
    scope text,
    token text,
    version_id uuid,
    tf integer,
    dl integer
  )
  language sql stable security invoker set search_path = ''
  as $$
    select d.tenant_id, d.scope, t.lexeme, d.id, pg_catalog.cardinality(t.positions),
           (pg_catalog.sum(pg_catalog.cardinality(t.positions)) over (partition by d.id))::integer
    from (
      select v.tenant_id, v.scope, v.id,
             pg_catalog.jsonb_to_tsvector(
               'simple'::pg_catalog.regconfig,
               coalesce((select pg_catalog.jsonb_agg(v.props -> m.mark order by m.ord)
                         from unnest(k.marks) with ordinality m(mark, ord)
                         where v.props ? m.mark), '[]'::jsonb),
               '["string", "numeric"]'::jsonb)
             || case when v.edge_id is not null and 'sentence' = any(k.marks)
                     then pg_catalog.to_tsvector('simple'::pg_catalog.regconfig,
                                                 coalesce(s.sentence, ''))
                     else ''::pg_catalog.tsvector end as words
      from gnomon.version v
      left join gnomon.node n on n.tenant_id = v.tenant_id and n.id = v.node_id
      left join gnomon.edge e on e.tenant_id = v.tenant_id and e.id = v.edge_id
      left join gnomon.sentence s
        on s.tenant_id = v.tenant_id and s.scope = v.scope and s.version_id = v.id
      cross join lateral (
        select gnomon.kind_search(v.tenant_id, coalesce(n.kind, e.kind)) as marks) k
      where v.tenant_id = p_tenant_id and v.id = any(p_versions)
    ) d
    cross join lateral pg_catalog.unnest(d.words) t
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

create or replace function gnomon.maintain_search_index() returns trigger
  language plpgsql security definer set search_path = ''
  as $$
  declare
    t uuid;
    v_sign bigint;
    v_versions uuid[];
    v_rows uuid[];
    d record;
    v_scopes text[];
    v_hashes text[];
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
      -- A Sentence stored or replaced: each of its Versions' quantized rows
      -- of a text the Version no longer has, neither its values nor its
      -- Sentence, and the full vector of that text unless a quantized row
      -- or a name row of the Scope still uses it. Only rows of the stored
      -- Sentences' own Tenant and Scope are touched.
      if tg_table_name = 'sentence' then
        with stale as (
          delete from gnomon.embedding_bit b
            using changed c
            join gnomon.version v
              on v.tenant_id = c.tenant_id and v.scope = c.scope and v.id = c.version_id
            where c.tenant_id = t
              and b.tenant_id = c.tenant_id and b.scope = c.scope and b.version_id = c.version_id
              and b.text_hash <> v.text_hash
              and b.text_hash <> gnomon.sentence_text_hash(c.sentence)
            returning b.scope, b.text_hash
        )
        select coalesce(pg_catalog.array_agg(x.scope), '{}'),
               coalesce(pg_catalog.array_agg(x.text_hash), '{}')
          into v_scopes, v_hashes
          from (select distinct s.scope, s.text_hash from stale s) x;
        delete from gnomon.embedding e
          using unnest(v_scopes, v_hashes) x(scope, text_hash)
          where e.tenant_id = t and e.scope = x.scope and e.text_hash = x.text_hash
            and not exists (select from gnomon.embedding_bit b
                            where b.tenant_id = e.tenant_id and b.scope = e.scope
                              and b.text_hash = e.text_hash)
            and not exists (select from gnomon.node_name n
                            where n.tenant_id = e.tenant_id and n.scope = e.scope
                              and n.embedded_hash = e.text_hash);
      end if;
    end loop;
    return null;
  end
  $$;

create trigger version_index_search_text after insert on gnomon.version
  referencing new table as changed
  for each statement execute function gnomon.maintain_search_index();

-- Only an Erasure rewrites a Version's Recorded time so that it lacks token
-- rows: `gnomon.erase_version_text` deleted those of a Version it then
-- reopens. A caller's update closes a Version, whose rows stay.
create trigger version_reindex_search_text after update on gnomon.version
  referencing new table as changed
  for each statement when (gnomon.running_as_definer())
  execute function gnomon.maintain_search_index();

create trigger sentence_index_search_text_insert after insert on gnomon.sentence
  referencing new table as changed
  for each statement execute function gnomon.maintain_search_index();

create trigger sentence_index_search_text_update after update on gnomon.sentence
  referencing new table as changed
  for each statement execute function gnomon.maintain_search_index();

create trigger search_token_count_inserted after insert on gnomon.search_token
  referencing new table as changed
  for each statement execute function gnomon.maintain_search_index();

create trigger search_token_count_deleted after delete on gnomon.search_token
  referencing old table as changed
  for each statement execute function gnomon.maintain_search_index();

-- Once per Scope a transaction changed, at commit. The first firing applies
-- every change of the transaction in its Tenant, and the rest find none left.
create constraint trigger search_count_delta_apply after insert on gnomon.search_count_delta
  deferrable initially deferred
  for each row when (new.token is null)
  execute function gnomon.maintain_search_index();

-- The full-text branch of Search: the Versions whose searchable text holds
-- a word of `p_query`, As-of the valid-at and known-at instants (a null
-- known-at reads the current belief), best first, at most `p_limit`.
--
-- BM25 with k1 = 1.2 and b = 0.75. The number of Versions, their mean length
-- and each word's frequency are the sums of the counts of the caller's
-- allowed Scopes, which RLS selects, with the changes the caller's own
-- transaction has recorded. Every Version of those Scopes is scored from its
-- token rows, and the As-of instants are then checked on each scored
-- Version, so the result is exact. Ties go to the lower Version ID.
create function gnomon.search_full_text(
  p_tenant_id uuid,
  p_query text,
  p_valid_at timestamptz,
  p_known_at timestamptz,
  p_limit integer
)
  returns table (
    version_id uuid,
    node_id uuid,
    edge_id uuid,
    scope text,
    score double precision,
    rank bigint
  )
  language plpgsql stable security invoker set search_path = ''
  as $$
  begin
    if p_limit is null or p_limit < 1 then
      raise exception using errcode = 'invalid_parameter_value',
        message = format('a Search returns at least one candidate, not %s', p_limit);
    end if;
    if p_valid_at is null then
      raise exception using errcode = 'invalid_parameter_value',
        message = 'a Search needs a valid-at instant';
    end if;
    return query
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
      scored as materialized (
        select p.version_id,
               pg_catalog.sum(i.idf * p.tf * 2.2
                 / (p.tf + 1.2 * (0.25 + 0.75 * p.dl / (t.words / t.versions)))) as score
        from idf i
        cross join totals t
        join gnomon.search_token p on p.tenant_id = p_tenant_id and p.token = i.token
        group by p.version_id
      )
      select v.id, v.node_id, v.edge_id, v.scope, s.score,
             pg_catalog.row_number() over (order by s.score desc, v.id)
      from scored s
      join gnomon.version v on v.tenant_id = p_tenant_id and v.id = s.version_id
      where v.valid_from <= p_valid_at and v.valid_to > p_valid_at
        and case
              when p_known_at is null then v.recorded_to = 'infinity'
              else v.recorded_from <= p_known_at and v.recorded_to > p_known_at
            end
      order by s.score desc, v.id
      limit p_limit;
  end
  $$;

-- The hash of a Sentence's text, as a quantized row of its Version names it.
create function gnomon.sentence_text_hash(p_sentence text) returns text
  language sql immutable parallel safe security invoker set search_path = ''
  as $$
    select pg_catalog.encode(pg_catalog.sha256(pg_catalog.convert_to(p_sentence, 'UTF8')), 'hex')
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
  foreach run in array array[
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
create trigger embedding_bit_derive before insert or update of text_hash, q, valid_from,
  valid_to, recorded_from on gnomon.embedding_bit
  for each row execute function gnomon.embedding_bit_derive();

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

create constraint trigger embedding_check_used after insert on gnomon.embedding
  deferrable initially deferred
  for each row execute function gnomon.embedding_check_used();

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

-- Resolution: the Nodes whose current names and Aliases match `p_mention`,
-- fused from the full-text and vector halves by reciprocal rank fusion with
-- k = 60, as prototype 5 fused Search, best first, at most `p_limit`. Each
-- half hands fusion its best 100, or `p_limit` when that is more, as
-- prototype 5 fused 100 from each branch, so a Node both halves rank just
-- below the top still outranks one a single half ranks first. Each
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
    full_text_version_id uuid,
    full_text_name text,
    full_text_alias boolean,
    full_text_score double precision,
    full_text_rank bigint,
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
        message = pg_catalog.format('a Resolution returns at least one candidate, not %s',
                                    p_limit);
    end if;
    return query
      with t as materialized (
        select * from gnomon.resolve_full_text(p_tenant_id, p_mention, p_valid_at,
                                               greatest(100, p_limit))
      ),
      vec as materialized (
        select * from gnomon.resolve_vector(p_tenant_id, p_embedding, p_valid_at,
                                            greatest(100, p_limit))
      ),
      fused as (
        select coalesce(t.node_id, vec.node_id) as node_id,
               coalesce(t.scope, vec.scope) as scope,
               t.version_id as t_version, t.name as t_name, t.alias as t_alias,
               t.score as t_score, t.rank as t_rank,
               vec.version_id as v_version, vec.name as v_name, vec.alias as v_alias,
               vec.distance as v_distance, vec.rank as v_rank,
               coalesce(1.0::float8 / (60 + t.rank), 0)
                 + coalesce(1.0::float8 / (60 + vec.rank), 0) as score
        from t full join vec on vec.node_id = t.node_id
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
  foreach run in array array[
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
$sql$
  ] loop
    execute replace(run, '@extschema@', quote_ident(ext));
  end loop;
end
$migration$;
create function gnomon.store_summary_embedding(
  p_tenant_id uuid,
  p_summary_id uuid,
  p_embedding real[]
)
  returns void
  language sql volatile security invoker set search_path = ''
  as $fn$
    select gnomon.store_derived_embedding(p_tenant_id, 'summary', p_summary_id, p_embedding)
  $fn$;

create function gnomon.store_community_embedding(
  p_tenant_id uuid,
  p_community_id uuid,
  p_embedding real[]
)
  returns void
  language sql volatile security invoker set search_path = ''
  as $fn$
    select gnomon.store_derived_embedding(p_tenant_id, 'community', p_community_id, p_embedding)
  $fn$;

do $migration$
declare
  ext text := (
    select n.nspname
    from pg_catalog.pg_extension e
    join pg_catalog.pg_namespace n on n.oid = e.extnamespace
    where e.extname = 'vector');
  run text;
begin
  foreach run in array array[
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
create trigger derived_embedding_derive before insert on gnomon.derived_embedding
  for each row execute function gnomon.derived_embedding_derive();

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

-- Stores the Sentence of Edge Version `p_version_id`, replacing any stored
-- before. It runs as the caller, under RLS.
create function gnomon.store_sentence(
  p_tenant_id uuid,
  p_version_id uuid,
  p_sentence text
)
  returns void
  language plpgsql volatile security invoker set search_path = ''
  as $$
  declare
    v_scope text;
  begin
    if p_sentence is null or p_sentence !~ '[^[:space:]]' then
      raise exception using errcode = 'invalid_parameter_value',
        message = 'a Sentence holds at least one word';
    end if;
    if p_tenant_id is null or p_tenant_id is distinct from gnomon.tenant_id() then
      raise exception using errcode = 'insufficient_privilege',
        message = 'a Sentence is stored only in the caller''s own Tenant';
    end if;
    select v.scope into v_scope
      from gnomon.version v
      where v.tenant_id = p_tenant_id and v.id = p_version_id and v.edge_id is not null;
    if not found then
      raise exception using errcode = 'GNW01',
        message = pg_catalog.format(
          'a Sentence states what an Edge Version says, and %s is not an Edge Version the '
          'caller can see', p_version_id);
    end if;
    insert into gnomon.sentence as s (tenant_id, scope, version_id, sentence)
      values (p_tenant_id, v_scope, p_version_id, p_sentence)
      on conflict (tenant_id, scope, version_id)
        do update set sentence = excluded.sentence
        where s.sentence is distinct from excluded.sentence;
  end
  $$;

-- The gaps of Scope `p_scope` the caller sees, in the order the header
-- gives, after the page token `(p_after_kind, p_after_id, p_after_hash)`,
-- at most `p_limit`. A null token starts at the first gap. `text_hash` is
-- the hash of the text to embed, and `search_text` that text, for a Sentence
-- or a name; both are null where they do not apply.
create function gnomon.list_gaps(
  p_tenant_id uuid,
  p_scope text,
  p_after_kind text,
  p_after_id uuid,
  p_after_hash text,
  p_limit integer
)
  returns table (
    kind text,
    id uuid,
    text_hash text,
    node_id uuid,
    edge_id uuid,
    search_text text
  )
  language plpgsql stable security invoker set search_path = ''
  as $$
  #variable_conflict use_column
  declare
    v_kinds constant text[] := array['sentence', 'sentence_embedding', 'node_embedding',
      'name_embedding', 'summary_embedding', 'community_embedding', 'summary', 'community'];
    v_after integer := 0;
    v_id uuid := coalesce(p_after_id, '00000000-0000-0000-0000-000000000000');
    v_hash text := coalesce(p_after_hash, '');
  begin
    if p_limit is null or p_limit < 1 then
      raise exception using errcode = 'invalid_parameter_value',
        message = pg_catalog.format('a page holds at least one gap, not %s', p_limit);
    end if;
    if p_after_kind is not null or p_after_id is not null or p_after_hash is not null then
      v_after := pg_catalog.array_position(v_kinds, p_after_kind);
      -- The kinds of an embedding of a text carry its hash, and the others
      -- none, so a token names one gap.
      if v_after is null or p_after_id is null
         or (p_after_hash is null) = (v_after in (2, 3, 4)) then
        raise exception using errcode = 'invalid_parameter_value',
          message = pg_catalog.format('%s %s %s is not a page token of the gap listing',
                                      p_after_kind, p_after_id, p_after_hash);
      end if;
    end if;
    if p_tenant_id is null or p_tenant_id is distinct from gnomon.tenant_id() then
      raise exception using errcode = 'insufficient_privilege',
        message = 'the gaps are listed only in the caller''s own Tenant';
    end if;
    if p_scope is null or not (p_scope = any(gnomon.allowed_scopes())) then
      raise exception using errcode = 'GNW01',
        message = pg_catalog.format('Scope %s is not visible to the caller', p_scope);
    end if;
    return query
      select g.kind, g.id, nullif(g.hash, ''), g.node_id, g.edge_id, g.search_text
      from (
        (select 1 as ord, 'sentence'::text as kind, v.id, ''::text as hash,
                null::uuid as node_id, v.edge_id, null::text as search_text
         from gnomon.version v
         where v.tenant_id = p_tenant_id and v.scope = p_scope and v.edge_id is not null
           and not exists (select from gnomon.sentence s
                           where s.tenant_id = v.tenant_id and s.scope = v.scope
                             and s.version_id = v.id)
           and (v_after < 1 or v_after = 1 and v.id > v_id)
         order by v.id
         limit p_limit)
        union all
        (select 2, 'sentence_embedding', v.id, h.hash, null, v.edge_id, s.sentence
         from gnomon.version v
         join gnomon.sentence s
           on s.tenant_id = v.tenant_id and s.scope = v.scope and s.version_id = v.id
         cross join lateral (select gnomon.sentence_text_hash(s.sentence) as hash) h
         where v.tenant_id = p_tenant_id and v.scope = p_scope and v.edge_id is not null
           and not exists (select from gnomon.embedding_bit b
                           where b.tenant_id = v.tenant_id and b.scope = v.scope
                             and b.version_id = v.id and b.text_hash = h.hash)
           and (v_after < 2
                or v_after = 2 and (v.id, h.hash collate "C") > (v_id, v_hash collate "C"))
         order by v.id, h.hash collate "C"
         limit p_limit)
        union all
        (select 3, 'node_embedding', v.id, v.text_hash, v.node_id, null, null
         from gnomon.version v
         where v.tenant_id = p_tenant_id and v.scope = p_scope and v.node_id is not null
           and not exists (select from gnomon.embedding_bit b
                           where b.tenant_id = v.tenant_id and b.scope = v.scope
                             and b.version_id = v.id and b.text_hash = v.text_hash)
           and (v_after < 3
                or v_after = 3 and (v.id, v.text_hash collate "C") > (v_id, v_hash collate "C"))
         order by v.id, v.text_hash collate "C"
         limit p_limit)
        union all
        (select 4, 'name_embedding', n.node_id, n.text_hash, n.node_id, null,
                pg_catalog.min(n.name)
         from gnomon.node_name n
         where n.tenant_id = p_tenant_id and n.scope = p_scope
           and n.recorded_to = 'infinity' and n.q is null
           and (v_after < 4
                or v_after = 4 and (n.node_id, n.text_hash collate "C") > (v_id, v_hash collate "C"))
         group by n.node_id, n.text_hash
         order by n.node_id, n.text_hash collate "C"
         limit p_limit)
        union all
        (select 5, 'summary_embedding', s.id, '', s.node_id, null, null
         from gnomon.summary s
         where s.tenant_id = p_tenant_id and s.scope = p_scope
           and not exists (select from gnomon.derived_embedding d
                           where d.tenant_id = s.tenant_id and d.scope = s.scope
                             and d.summary_id = s.id)
           and (v_after < 5 or v_after = 5 and s.id > v_id)
         order by s.id
         limit p_limit)
        union all
        (select 6, 'community_embedding', c.id, '', null, null, null
         from gnomon.community c
         where c.tenant_id = p_tenant_id and c.scope = p_scope
           and not exists (select from gnomon.derived_embedding d
                           where d.tenant_id = c.tenant_id and d.scope = c.scope
                             and d.community_id = c.id)
           and (v_after < 6 or v_after = 6 and c.id > v_id)
         order by c.id
         limit p_limit)
        union all
        (select 7, 'summary', n.id, '', n.id, null, null
         from gnomon.node n
         where n.tenant_id = p_tenant_id and n.scope = p_scope
           and exists (select from gnomon.version v
                       where v.tenant_id = n.tenant_id and v.scope = n.scope
                         and v.node_id = n.id and v.recorded_to = 'infinity')
           and not exists (select from gnomon.summary s
                           where s.tenant_id = n.tenant_id and s.scope = p_scope
                             and s.node_id = n.id and s.recorded_to = 'infinity')
           and (v_after < 7 or v_after = 7 and n.id > v_id)
         order by n.id
         limit p_limit)
        union all
        (select 8, 'community', c.id, '', null, null, null
         from gnomon.community c
         where c.tenant_id = p_tenant_id and c.scope = p_scope
           and c.recorded_to <> 'infinity'
           and not exists (
             select from gnomon.community r
             where r.tenant_id = c.tenant_id and r.scope = c.scope
               and r.recorded_from = c.recorded_to)
           and not exists (
             select from gnomon.community_member cm
             join gnomon.community_member rm
               on rm.tenant_id = cm.tenant_id and rm.scope = cm.scope and rm.node_id = cm.node_id
             join gnomon.community r
               on r.tenant_id = rm.tenant_id and r.scope = rm.scope and r.id = rm.community_id
             where cm.tenant_id = c.tenant_id and cm.scope = c.scope and cm.community_id = c.id
               and r.recorded_to = 'infinity')
           and (v_after < 8 or v_after = 8 and c.id > v_id)
         order by c.id
         limit p_limit)
      ) g
      order by g.ord, g.id, g.hash collate "C"
      limit p_limit;
  end
  $$;

insert into gnomon.schema_version (version, name) values (4, '0004_search');
