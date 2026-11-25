-- gnomon migration 0016: the full-text branch of Search.
--
-- A consumer searches with query text at a valid-at and a known-at instant.
-- Search returns the Versions whose searchable text matches, ranked by BM25
-- and filtered by Tenant, Scope and the As-of instants (ADR-0010).
--
-- GIN on `tsvector` cannot serve this under RLS. Its operator `@@` is not
-- leakproof, so it can never be an index condition, and every visible row
-- would be scanned (prototype 5: 253 ms at 200,000 visible rows). Instead a
-- token table holds one row per word of each Version's searchable text. A
-- lookup is an equality on the word, `texteq`, which is leakproof. It runs
-- through a B-tree that leads with the Tenant and the Scope, so the policy's
-- conditions are index conditions too (ADR-0004, ADR-0005). Prototype 5
-- measured 4.7 ms at 200,000 visible rows.
--
-- What is searched. A Version's searchable text is what its Kind marks as
-- searchable (ADR-0002): the properties the Kind or an ancestor names, a
-- Node's `name` and `aliases`, and an Edge Version's Sentence once the
-- Pipeline has stored one (ADR-0013). No other property is indexed. String
-- and number values are indexed, inside arrays and objects too, and keys are
-- not. The `simple` text search configuration splits and lower-cases the
-- words, with no stemming and no stop words, so every image splits a text
-- the same way. Postgres keeps at most 256 positions of one word in a text,
-- so a word's count in one Version stops at 256.
--
-- The counts. Each Version's text is scored as a whole. Its token rows hold
-- each word, how often the word occurs in the text (`tf`), and the text's
-- length in words (`dl`). Closed Versions keep their rows, so a past As-of
-- read finds them. The BM25 counts are kept per Scope: how many Versions'
-- texts hold each word, and how many Versions and words the Scope holds.
-- Search sums them over the caller's allowed Scopes, which RLS selects, so
-- the ranking reveals nothing about a Scope the caller cannot read
-- (ADR-0010). The counts cover every Version of those Scopes whatever its
-- time. The As-of instants decide only which Versions are returned.
--
-- Who writes. One trigger function, `gnomon.maintain_search_index`, writes
-- the token rows and the counts. It is the fourth kind of definer ADR-0009
-- allows, owned by the NOLOGIN `gnomon_definer`, and it is granted to no
-- role. No caller can execute it or attach it to a table of its own, because
-- both need EXECUTE. Callers hold SELECT on the search tables and no write
-- privilege, so no caller's statement, nor a trigger of a caller's own, can
-- write them. The function runs under the caller's claims and the definer's
-- policies, and writes only what its triggering rows give:
--
--   A Version inserted     its token rows, from its text, in its Tenant and
--                          Scope. One statement indexes a whole batch.
--   A Sentence stored      its Version's token rows again, from its text with
--                          the Sentence. Ticket #26's function stores a
--                          Sentence with an insert into `gnomon.sentence`,
--                          and needs nothing more.
--   An Erasure reopening   the Version's token rows again (see Erasure).
--   a Version
--   Token rows written     the count changes of their Scopes, kept for the
--   or deleted             transaction in `gnomon.search_count_delta`.
--   Commit                 the transaction's count changes, applied to the
--                          counts in key order, and a count that reaches zero
--                          is deleted, so an erased Scope leaves no count row.
--
-- A deleted Version's token rows go by the foreign key, which only the
-- transaction that wrote the Version, or an Erasure, can set off. Every row is
-- written in the write's own transaction, so a Version is findable when the
-- write commits, before its embedding exists (ADR-0010). Within the
-- transaction, Search adds the transaction's own count changes to the counts.
--
-- Why the counts change at commit. Every write in a Scope changes the
-- Scope's row, and most change some word's row. A row changed statement by
-- statement stays locked until commit. Two transactions that each write
-- several Nodes of a Scope would then wait on each other's rows, a deadlock.
-- Each transaction instead records its changes in rows of its own, which no
-- other transaction touches. A deferred trigger applies them once at commit,
-- every word's row in key order and then every Scope's row, so two committing
-- transactions take the rows in one order and wait at most for each other's
-- commit.
--
-- Erasure (ADR-0003). `gnomon.erase_version_text` now deletes the token rows
-- of every Version an Erasure deletes or recomputes, besides its Sentence.
-- The counts go down at commit. Erase Episode inserts the Versions it
-- replays, so they are indexed from their recomputed text. A Version it
-- reopens is the one that stood just before the Episode first touched its
-- Node or Edge, so its text owes nothing to the erased Episode. It is indexed
-- again from its text as it stands, with no Sentence, and so is a Version
-- that was current then and that the Episode never closed. The definer may read
-- and delete the token rows of an Edge the running Erasure reaches in a
-- Scope the caller cannot see (migration 0012). It may change the counts
-- only of a Scope whose changes its own transaction recorded.

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

-- The token table: one row per word of each Version's searchable text.
create table gnomon.search_token (
  tenant_id uuid not null,
  scope text not null,
  token text not null check (token <> ''),
  version_id uuid not null,
  -- How often the word occurs in the Version's text, and the text's length
  -- in words.
  tf integer not null check (tf > 0),
  dl integer not null check (dl >= tf),
  primary key (tenant_id, scope, token, version_id) include (tf, dl),
  foreign key (tenant_id, scope, version_id)
    references gnomon.version (tenant_id, scope, id) on delete cascade
);
-- Writing, re-indexing and deleting a Version's rows, and the foreign key.
create index search_token_version on gnomon.search_token (tenant_id, scope, version_id);

-- The BM25 counts of a Scope: how many Versions' texts hold each word, and
-- how many Versions and words the Scope holds.
create table gnomon.search_token_frequency (
  tenant_id uuid not null,
  scope text not null,
  token text not null,
  versions bigint not null check (versions > 0),
  primary key (tenant_id, scope, token)
);
create table gnomon.search_scope_length (
  tenant_id uuid not null,
  scope text not null,
  versions bigint not null check (versions > 0),
  words bigint not null check (words > 0),
  primary key (tenant_id, scope)
);

-- The changes one transaction made to the counts, which its commit applies
-- and deletes. A row with a word changes that word's count, and the row with
-- no word changes the Scope's totals. Each transaction writes only rows of
-- its own, so no other transaction ever waits on them.
create table gnomon.search_count_delta (
  tenant_id uuid not null,
  scope text not null,
  token text,
  written_in xid8 not null default pg_current_xact_id(),
  versions bigint not null,
  words bigint not null,
  constraint search_count_delta_once unique nulls not distinct
    (tenant_id, written_in, scope, token)
);

alter table gnomon.search_token enable row level security;
alter table gnomon.search_token_frequency enable row level security;
alter table gnomon.search_scope_length enable row level security;
alter table gnomon.search_count_delta enable row level security;

-- Callers read the rows of their own Tenant and Scopes. The changes another
-- transaction recorded are never visible to a caller: they are uncommitted,
-- and the commit deletes them.
create policy search_token_tenant_scope on gnomon.search_token
  for all to authenticated, gnomon_worker, gnomon_definer
  using (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()))
  with check (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()));
create policy search_token_frequency_tenant_scope on gnomon.search_token_frequency
  for select to authenticated, gnomon_worker
  using (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()));
create policy search_scope_length_tenant_scope on gnomon.search_scope_length
  for select to authenticated, gnomon_worker
  using (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()));
create policy search_count_delta_tenant_scope on gnomon.search_count_delta
  for select to authenticated, gnomon_worker
  using (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()));

-- The definer records the changes of its own transaction, in any Scope of
-- the Tenant, because they come from the token rows that transaction wrote
-- or deleted, an erased Edge's of a Scope the caller cannot see among them.
create policy search_count_delta_own on gnomon.search_count_delta
  for all to gnomon_definer
  using (tenant_id = gnomon.tenant_id() and written_in = pg_current_xact_id_if_assigned())
  with check (tenant_id = gnomon.tenant_id() and written_in = pg_current_xact_id());

-- The definer changes the counts only of a Scope whose changes its own
-- transaction recorded. The condition is a subquery, not a function call,
-- because a function with a pinned search path costs a setting change on
-- every row the policy checks.
create policy search_token_frequency_pending on gnomon.search_token_frequency
  for all to gnomon_definer
  using (tenant_id = gnomon.tenant_id()
         and exists (select from gnomon.search_count_delta d
                     where d.tenant_id = search_token_frequency.tenant_id
                       and d.written_in = pg_current_xact_id_if_assigned()
                       and d.scope = search_token_frequency.scope))
  with check (tenant_id = gnomon.tenant_id()
              and exists (select from gnomon.search_count_delta d
                          where d.tenant_id = search_token_frequency.tenant_id
                            and d.written_in = pg_current_xact_id_if_assigned()
                            and d.scope = search_token_frequency.scope));
create policy search_scope_length_pending on gnomon.search_scope_length
  for all to gnomon_definer
  using (tenant_id = gnomon.tenant_id()
         and exists (select from gnomon.search_count_delta d
                     where d.tenant_id = search_scope_length.tenant_id
                       and d.written_in = pg_current_xact_id_if_assigned()
                       and d.scope = search_scope_length.scope))
  with check (tenant_id = gnomon.tenant_id()
              and exists (select from gnomon.search_count_delta d
                          where d.tenant_id = search_scope_length.tenant_id
                            and d.written_in = pg_current_xact_id_if_assigned()
                            and d.scope = search_scope_length.scope));

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

-- Writes the token rows of these Versions from their text as it stands
-- now. With `p_replace` it deletes their rows first; without it, it writes
-- only those of the Versions that have none.
create function gnomon.index_search_text(
  p_tenant_id uuid,
  p_versions uuid[],
  p_replace boolean
)
  returns void
  language plpgsql volatile security invoker set search_path = ''
  as $$
  begin
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

-- The one function that writes the token rows and the counts (see the
-- header). It is a definer, and each trigger below that runs it names its
-- transition table `changed`.
create function gnomon.maintain_search_index() returns trigger
  language plpgsql security definer set search_path = ''
  as $$
  declare
    t uuid;
    v_sign bigint;
    v_versions uuid[];
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
    -- A Version's rows are always written and deleted together.
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

-- Erasure (ADR-0003). Every row derived from the text of a Version an
-- Erasure deletes or recomputes goes: its Sentence, as migration 0011 wrote,
-- and its token rows, whose counts go down at commit. The token rows of a
-- deleted Version would go by the foreign key too, and those of a recomputed
-- one would not.
create or replace function gnomon.erase_version_text(p_tenant_id uuid, p_versions uuid[])
  returns void
  language sql volatile security invoker set search_path = ''
  as $$
    delete from gnomon.sentence s
    where s.tenant_id = p_tenant_id and s.version_id = any(p_versions);
    delete from gnomon.search_token t
    using gnomon.version v
    where v.tenant_id = p_tenant_id and v.id = any(p_versions)
      and t.tenant_id = v.tenant_id and t.scope = v.scope and t.version_id = v.id;
  $$;

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
      'gnomon.kind_search(uuid, text)'::regprocedure,
      'gnomon.search_text_tokens(uuid, uuid[])'::regprocedure,
      'gnomon.index_search_text(uuid, uuid[], boolean)'::regprocedure,
      'gnomon.maintain_search_index()'::regprocedure,
      'gnomon.search_full_text(uuid, text, timestamptz, timestamptz, integer)'::regprocedure
    ) and a.grantee <> p.proowner
    union all
    select format('table %s', c.oid::regclass), a.grantee
    from pg_class c, aclexplode(c.relacl) a
    where c.oid in ('gnomon.search_token'::regclass, 'gnomon.search_token_frequency'::regclass,
                    'gnomon.search_scope_length'::regclass, 'gnomon.search_count_delta'::regclass)
      and a.grantee <> c.relowner
  loop
    execute format(
      'revoke all on %s from %s', g.object,
      case when g.grantee = 0 then 'public' else g.grantee::regrole::text end);
  end loop;
  revoke all on function gnomon.kind_search(uuid, text) from public;
  revoke all on function gnomon.search_text_tokens(uuid, uuid[]) from public;
  revoke all on function gnomon.index_search_text(uuid, uuid[], boolean) from public;
  revoke all on function gnomon.maintain_search_index() from public;
  revoke all on function gnomon.search_full_text(uuid, text, timestamptz, timestamptz, integer)
    from public;
end
$$;

-- Hand the search index function to the definer role, as migration 0011
-- hands it Erasure. EXECUTE on it is granted to no role.
do $$
declare
  joined boolean := not pg_has_role(current_user, 'gnomon_definer', 'SET');
begin
  if joined then
    execute format('grant gnomon_definer to %I', current_user);
  end if;
  grant create on schema gnomon to gnomon_definer;
  alter function gnomon.maintain_search_index() owner to gnomon_definer;
  revoke create on schema gnomon from gnomon_definer;
  if joined then
    execute format('revoke gnomon_definer from %I', current_user);
  end if;
end
$$;

-- Callers read the token rows and the counts of their own Scopes, through
-- Search, and write none of them.
grant select on gnomon.search_token, gnomon.search_token_frequency, gnomon.search_scope_length,
  gnomon.search_count_delta
  to authenticated, gnomon_worker;
grant execute on function gnomon.search_full_text(uuid, text, timestamptz, timestamptz, integer)
  to authenticated, gnomon_worker;

-- What the definer needs, for the search index function and for Erasure:
-- to write and delete the token rows, the counts and the transaction's
-- changes to them, and the functions they call. The vacuum call vacuums all
-- four tables (migration 0013), which `gnomon.erasure_vacuum_tables()` now
-- lists, since the definer may delete from them.
grant select, insert, delete on gnomon.search_token to gnomon_definer;
grant select, insert, update, delete on gnomon.search_token_frequency,
  gnomon.search_scope_length, gnomon.search_count_delta
  to gnomon_definer;
grant execute on function gnomon.kind_search(uuid, text) to gnomon_definer;
grant execute on function gnomon.search_text_tokens(uuid, uuid[]) to gnomon_definer;
grant execute on function gnomon.index_search_text(uuid, uuid[], boolean) to gnomon_definer;
grant maintain on gnomon.search_token, gnomon.search_token_frequency, gnomon.search_scope_length,
  gnomon.search_count_delta
  to gnomon_maintainer;

insert into gnomon.schema_version (version, name) values (16, '0016_full_text_search');
