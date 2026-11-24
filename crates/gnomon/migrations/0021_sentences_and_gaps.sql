-- gnomon migration 0021: storing a Sentence, and the gaps `maintain`
-- repairs.
--
-- Storing a Sentence (ADR-0013). The Pipeline writes the Sentence of an Edge
-- Version in its own stage, from the Version as the Store committed it, and
-- stores it with `gnomon.store_sentence`, an invoker under RLS with an empty
-- search path (ADR-0009). It inserts the row into `gnomon.sentence`, keyed by
-- the Edge Version (migration 0008), or replaces the text of the one stored
-- already. It writes no Version and changes no Recorded time: the Sentence is
-- held outside the Version's values, so a Version stays immutable. A Sentence
-- is stored for any Edge Version the caller sees, closed ones too, since a
-- Search at a past known-at instant reads them.
--
-- Callers may now insert a Sentence and change its text, and nothing else:
-- no delete, and no change to its Tenant, Scope or Version. Every row a
-- caller writes, through the function or by its own statement, passes the
-- same checks. RLS keeps it in the caller's Tenant and Scopes, the insert
-- trigger of migration 0008 refuses a Version that is not an Edge Version
-- the caller can see, and a check refuses a text with no word.
--
-- Its words. The search index trigger (migration 0016) writes the Version's
-- token rows again on the insert or the update, with the Sentence's words, in
-- the store's own transaction, and its BM25 count changes are applied at
-- commit. So full-text Search finds the Edge Version by its Sentence when the
-- store commits, before any embedding exists (ADR-0010).
--
-- Its embedding. The Pipeline then calls `gnomon.store_embedding` with
-- `gnomon.sentence_text_hash(sentence)` (migration 0017), a separate call. A
-- replaced Sentence's old embedding must not stay: vector Search would find
-- the Version by a text it no longer has, and once the Version was embedded
-- again no row would use the old full vector, so no Erasure would ever find
-- it (ADR-0003). Callers hold no DELETE on the embedding tables, so the
-- search index trigger, the fourth definer (ADR-0009), now also deletes them
-- when a Sentence is stored. That is the Version's quantized row of a text it
-- no longer has, and that text's full vector unless a quantized row or a name
-- row of the Scope still uses it. It touches only rows of the stored
-- Sentence's own Tenant and Scope. No definer and no grant are added.
--
-- A Sentence is not carried to a new Version. A Refinement, a Correction or
-- an Erasure that recomputes an Edge Version leaves that Version with no
-- Sentence (ADR-0013, migration 0011), and the gap listing below names it.
--
-- The gaps (`gnomon.list_gaps`). `maintain` finds what is missing in one
-- Scope and repairs it. The listing is an invoker under RLS, so it reads only
-- the rows the caller sees, and names one Scope of the caller's. Each gap has
-- a kind, an ID and, for an embedding of a text, the text's hash. The kinds,
-- in the order they are listed:
--
--   sentence             An Edge Version, open or closed, with no Sentence.
--                        A Search at a past known-at instant reaches closed
--                        Versions, so they are repaired too.
--   sentence_embedding   An Edge Version with a Sentence, and no quantized
--                        row of the Sentence's hash. An embedding of its
--                        values does not count: once the Sentence exists the
--                        Version is searched by it. An Edge Version with no
--                        Sentence is listed for the Sentence only, since its
--                        embedding follows the Sentence.
--   node_embedding       A Node Version, open or closed, with no quantized
--                        row of its text's hash.
--   name_embedding       A current name or Alias of a Node, one per Node and
--                        text, whose name rows have no bits (migration 0019).
--                        Resolution reads only current names, so a replaced
--                        name is not a gap.
--   summary_embedding    A Summary, open or closed, with no embedding.
--   community_embedding  A Community, open or closed, with no embedding.
--   summary              A Node of the Scope that has a current Version and
--                        no current Summary in the Scope. "Current" means an
--                        open Recorded time (ADR-0015). A Node with no
--                        current Version, bare or retracted, has nothing to
--                        summarise.
--   community            A closed Community of the Scope whose group has no
--                        current Summary: no Community of the Scope that
--                        opened at the instant it closed, and no current
--                        Community of the Scope, holds any of its Nodes.
--
-- Why a Community that lost its replacement is a gap. A Community's text is
-- the Summary of its group. Regenerating it closes the old row and adds the
-- new one at the same instant (migration 0018), so a closed Community always
-- has a successor that opened when it closed, unless an Erasure deleted that
-- successor for citing an erased Episode (ADR-0003). The closed row then
-- stands for a group the graph still holds with no current Summary, which
-- the Pipeline regenerates from the Episodes that remain. A row whose
-- successor stands, closed or open, is ordinary history and is not listed,
-- and nor is one whose Nodes a current Community holds again. An Erasure
-- that removed every row of a Community leaves nothing to list.
--
-- Paging. The order is fixed: by kind as above, then by ID, then by hash in
-- byte order. A page holds at most the limit, and the next page starts after
-- the last gap's key, the page token: its kind, ID and hash. The key of a
-- gap never changes while it is listed, so a gap that is there on every call
-- is listed exactly once however the set changes between pages, and a gap
-- added behind the token is listed on the next pass. The same set lists the
-- same way on every call.

-- A Sentence holds at least one word, whoever writes it.
alter table gnomon.sentence
  add constraint sentence_has_words check (sentence ~ '[^[:space:]]');

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
      if v_after is null or p_after_id is null then
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
             select from gnomon.community_member cm
             join gnomon.community_member rm
               on rm.tenant_id = cm.tenant_id and rm.scope = cm.scope and rm.node_id = cm.node_id
             join gnomon.community r
               on r.tenant_id = rm.tenant_id and r.scope = rm.scope and r.id = rm.community_id
             where cm.tenant_id = c.tenant_id and cm.scope = c.scope and cm.community_id = c.id
               and (r.recorded_from = c.recorded_to or r.recorded_to = 'infinity'))
           and (v_after < 8 or v_after = 8 and c.id > v_id)
         order by c.id
         limit p_limit)
      ) g
      order by g.ord, g.id, g.hash collate "C"
      limit p_limit;
  end
  $$;

-- The search index trigger of migration 0020, which now also deletes the
-- embedding of a text a stored Sentence replaced (see the header).
-- Everything else is as migration 0020 wrote it. It stays a definer owned by
-- `gnomon_definer` and granted to no role (ADR-0009). The role applying
-- this migration joins `gnomon_definer` to replace it, and leaves it again,
-- as migration 0020 did.
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
  $$
  $ddl$;
  if joined then
    execute format('revoke gnomon_definer from %I', current_user);
  end if;
end
$do$;

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
      'gnomon.store_sentence(uuid, uuid, text)'::regprocedure,
      'gnomon.list_gaps(uuid, text, text, uuid, text, integer)'::regprocedure
    ) and a.grantee <> p.proowner
  loop
    execute format(
      'revoke all on %s from %s', g.object,
      case when g.grantee = 0 then 'public' else g.grantee::regrole::text end);
  end loop;
  revoke all on function gnomon.store_sentence(uuid, uuid, text) from public;
  revoke all on function gnomon.list_gaps(uuid, text, text, uuid, text, integer) from public;
end
$$;

-- Callers store Sentences and list gaps in their own Scopes. On Sentences
-- they may insert a row and change its text, and nothing more (see the
-- header). The definer's grants on the embedding tables, SELECT and DELETE,
-- and on `gnomon.node_name`, SELECT, are those Erasure already holds
-- (migrations 0017 and 0019).
grant insert (tenant_id, scope, version_id, sentence) on gnomon.sentence
  to authenticated, gnomon_worker;
grant update (sentence) on gnomon.sentence to authenticated, gnomon_worker;
grant execute on function gnomon.store_sentence(uuid, uuid, text) to authenticated, gnomon_worker;
grant execute on function gnomon.list_gaps(uuid, text, text, uuid, text, integer)
  to authenticated, gnomon_worker;

insert into gnomon.schema_version (version, name) values (21, '0021_sentences_and_gaps');
