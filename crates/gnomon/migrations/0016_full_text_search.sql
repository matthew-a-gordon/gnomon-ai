-- gnomon migration 0016: the full-text branch of Search.
--
-- A consumer searches with query text at a valid-at and a known-at instant,
-- and gets the Versions whose searchable text matches, ranked by BM25 and
-- filtered by Tenant, Scope and the As-of instants (ADR-0010).
--
-- GIN on `tsvector` cannot serve this under RLS, because `@@` is not
-- leakproof, so it can never be an index condition and every visible row
-- would be scanned (prototype 5: 253 ms at 200,000 visible rows). Instead a
-- token table holds one row per word of each Version's searchable text, and
-- a lookup is an equality on the word, `texteq`, which is leakproof, through
-- a B-tree that leads with the Tenant and the Scope, so the policy's
-- conditions are index conditions too (ADR-0004, ADR-0005). Prototype 5
-- measured 4.7 ms at 200,000 visible rows.
--
-- The searchable text of a Version is what its Kind marks as searchable
-- (ADR-0002): the properties the Kind or an ancestor names, a Node's `name`
-- and `aliases`, and, for an Edge Version, its Sentence once the Pipeline has
-- stored one (ADR-0013). No other property is indexed. String and number
-- values are indexed, inside arrays and objects too, and keys are not. Words
-- are split and lower-cased by the `simple` text search configuration, with
-- no stemming and no stop words, so every image splits a text the same way.
-- Postgres keeps at most 256 positions of one word in a text, so a word's
-- count in one Version stops at 256.
--
-- Each Version's text is one document. Its token rows hold each word, how
-- often the word occurs in it (`tf`) and the document's length in words
-- (`dl`). Closed Versions keep theirs, so a past As-of read finds them. The
-- BM25 counts are kept per Scope: how many documents hold each word, and how
-- many documents and words each Scope holds. Search sums them over the
-- caller's allowed Scopes, which RLS decides, so the ranking reveals nothing
-- about a Scope the caller cannot read (ADR-0010). The counts cover every
-- Version of those Scopes whatever its time; the As-of instants decide only
-- which Versions are returned.
--
-- The token rows are written by statement-level triggers in the write's own
-- transaction, so a Version is findable when the write commits, before its
-- embedding exists (ADR-0010). A batch of Versions inserted by one statement
-- is indexed by one statement. The triggers run as the invoker, the caller
-- or the Erasure's definer, under RLS (ADR-0009); this migration adds no
-- definer function.
--
--   A Version inserted       its token rows, from its text.
--   A Version deleted        its token rows go by the foreign key, which
--                            only the transaction that wrote the Version, or
--                            an Erasure, can do.
--   A Sentence stored        the Version's token rows again, from its text
--                            with the Sentence. Ticket #26 adds the function
--                            that stores one, which needs nothing more.
--   A token row written      the BM25 counts of its Scope, up or down, in
--   or deleted               the same statement. A count that reaches zero is
--                            deleted, so an erased Scope leaves no count row.
--
-- No caller writes the token rows or the counts directly. Each table's guard
-- refuses any write that does not come from one of these triggers (it runs
-- at a trigger depth of 1 when the statement is the caller's own) or from
-- an Erasure.
--
-- Erasure (ADR-0003). `gnomon.erase_version_text` now deletes the token rows
-- of every Version an Erasure deletes or recomputes, besides its Sentence,
-- and the counts go down with them. The Versions an Erase Episode replays
-- are inserted and so indexed from their recomputed text. A Version it
-- reopens, the one that stood just before the Episode first touched its Node
-- or Edge, is indexed again from its text as it stands, which owes nothing
-- to the erased Episode. The definer may read and delete the token rows of an
-- Edge the running Erasure reaches in a Scope the caller cannot see, and
-- change that Scope's counts (migration 0012).

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

-- The BM25 counts of a Scope: how many of its documents hold each word, and
-- how many documents and words it holds.
create table gnomon.search_token_frequency (
  tenant_id uuid not null,
  scope text not null,
  token text not null,
  documents bigint not null check (documents >= 0),
  primary key (tenant_id, scope, token)
);
create table gnomon.search_scope_length (
  tenant_id uuid not null,
  scope text not null,
  documents bigint not null check (documents >= 0),
  length bigint not null check (length >= 0),
  primary key (tenant_id, scope)
);

alter table gnomon.search_token enable row level security;
alter table gnomon.search_token_frequency enable row level security;
alter table gnomon.search_scope_length enable row level security;

create policy search_token_tenant_scope on gnomon.search_token
  for all to authenticated, gnomon_worker, gnomon_definer
  using (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()))
  with check (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()));
create policy search_token_frequency_tenant_scope on gnomon.search_token_frequency
  for all to authenticated, gnomon_worker, gnomon_definer
  using (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()))
  with check (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()));
create policy search_scope_length_tenant_scope on gnomon.search_scope_length
  for all to authenticated, gnomon_worker, gnomon_definer
  using (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()))
  with check (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()));

-- The guard: a write to the token table or the counts comes from one of
-- this migration's triggers, which puts it at a trigger depth of 2 or more
-- here, or from an Erasure. A caller's own statement on one of the tables
-- runs this at depth 1 and is refused.
create function gnomon.search_guard() returns trigger
  language plpgsql security invoker set search_path = ''
  as $$
  begin
    if pg_catalog.pg_trigger_depth() < 2 and not gnomon.running_as_definer() then
      raise exception using errcode = 'insufficient_privilege',
        message = format('gnomon.%s is written only by the Store, from the text of each Version',
                         tg_table_name);
    end if;
    return null;
  end
  $$;
create trigger search_token_guard before insert or update or delete on gnomon.search_token
  for each statement execute function gnomon.search_guard();
create trigger search_token_frequency_guard
  before insert or update or delete on gnomon.search_token_frequency
  for each statement execute function gnomon.search_guard();
create trigger search_scope_length_guard
  before insert or update or delete on gnomon.search_scope_length
  for each statement execute function gnomon.search_guard();

-- The token rows of these Versions, from their text as it stands now.
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

-- Writes the token rows of these Versions anew, from their text as it
-- stands now.
create function gnomon.index_search_text(p_tenant_id uuid, p_versions uuid[])
  returns void
  language plpgsql volatile security invoker set search_path = ''
  as $$
  begin
    delete from gnomon.search_token t
      using gnomon.version v
      where v.tenant_id = p_tenant_id and v.id = any(p_versions)
        and t.tenant_id = v.tenant_id and t.scope = v.scope and t.version_id = v.id;
    insert into gnomon.search_token (tenant_id, scope, token, version_id, tf, dl)
      select * from gnomon.search_text_tokens(p_tenant_id, p_versions);
  end
  $$;

-- A Version inserted: its token rows, in the same statement's transaction.
create function gnomon.version_index_search_text() returns trigger
  language plpgsql security invoker set search_path = ''
  as $$
  declare
    t uuid;
  begin
    for t in select distinct n.tenant_id from inserted n loop
      insert into gnomon.search_token (tenant_id, scope, token, version_id, tf, dl)
        select * from gnomon.search_text_tokens(
          t, array(select n.id from inserted n where n.tenant_id = t));
    end loop;
    return null;
  end
  $$;
create trigger version_index_search_text after insert on gnomon.version
  referencing new table as inserted
  for each statement execute function gnomon.version_index_search_text();

-- A Sentence stored or changed: its Version's token rows again.
create function gnomon.sentence_index_search_text() returns trigger
  language plpgsql security invoker set search_path = ''
  as $$
  declare
    t uuid;
  begin
    for t in select distinct n.tenant_id from changed n loop
      perform gnomon.index_search_text(
        t, array(select n.version_id from changed n where n.tenant_id = t));
    end loop;
    return null;
  end
  $$;
create trigger sentence_index_search_text_insert after insert on gnomon.sentence
  referencing new table as changed
  for each statement execute function gnomon.sentence_index_search_text();
create trigger sentence_index_search_text_update after update on gnomon.sentence
  referencing new table as changed
  for each statement execute function gnomon.sentence_index_search_text();

-- Token rows written: the counts of their Scopes go up. Rows are locked in
-- the order of their keys, the frequencies before the Scope's length, as
-- the delete below locks them, so two writers in one Scope wait for each
-- other instead of deadlocking.
create function gnomon.search_count_inserted() returns trigger
  language plpgsql security invoker set search_path = ''
  as $$
  begin
    insert into gnomon.search_token_frequency as f (tenant_id, scope, token, documents)
      select a.tenant_id, a.scope, a.token, pg_catalog.count(*)
      from added a
      group by a.tenant_id, a.scope, a.token
      order by a.tenant_id, a.scope, a.token
      on conflict (tenant_id, scope, token)
        do update set documents = f.documents + excluded.documents;
    insert into gnomon.search_scope_length as s (tenant_id, scope, documents, length)
      select a.tenant_id, a.scope, pg_catalog.count(distinct a.version_id), pg_catalog.sum(a.tf)
      from added a
      group by a.tenant_id, a.scope
      order by a.tenant_id, a.scope
      on conflict (tenant_id, scope)
        do update set documents = s.documents + excluded.documents,
                      length = s.length + excluded.length;
    return null;
  end
  $$;
create trigger search_token_count_inserted after insert on gnomon.search_token
  referencing new table as added
  for each statement execute function gnomon.search_count_inserted();

-- Token rows deleted: the counts of their Scopes go down, and a count that
-- reaches zero goes. A Version's rows are always written and deleted
-- together, so each Version among them is one document.
create function gnomon.search_count_deleted() returns trigger
  language plpgsql security invoker set search_path = ''
  as $$
  begin
    perform from gnomon.search_token_frequency f
      join (select distinct r.tenant_id, r.scope, r.token from removed r) r
        on f.tenant_id = r.tenant_id and f.scope = r.scope and f.token = r.token
      order by f.tenant_id, f.scope, f.token
      for update of f;
    update gnomon.search_token_frequency f set documents = f.documents - r.n
      from (select r.tenant_id, r.scope, r.token, pg_catalog.count(*) as n
            from removed r group by r.tenant_id, r.scope, r.token) r
      where f.tenant_id = r.tenant_id and f.scope = r.scope and f.token = r.token;
    delete from gnomon.search_token_frequency f
      using (select distinct r.tenant_id, r.scope, r.token from removed r) r
      where f.tenant_id = r.tenant_id and f.scope = r.scope and f.token = r.token
        and f.documents = 0;
    perform from gnomon.search_scope_length s
      join (select distinct r.tenant_id, r.scope from removed r) r
        on s.tenant_id = r.tenant_id and s.scope = r.scope
      order by s.tenant_id, s.scope
      for update of s;
    update gnomon.search_scope_length s
      set documents = s.documents - r.documents, length = s.length - r.length
      from (select r.tenant_id, r.scope, pg_catalog.count(distinct r.version_id) as documents,
                   pg_catalog.sum(r.tf) as length
            from removed r group by r.tenant_id, r.scope) r
      where s.tenant_id = r.tenant_id and s.scope = r.scope;
    delete from gnomon.search_scope_length s
      using (select distinct r.tenant_id, r.scope from removed r) r
      where s.tenant_id = r.tenant_id and s.scope = r.scope and s.documents = 0;
    return null;
  end
  $$;
create trigger search_token_count_deleted after delete on gnomon.search_token
  referencing old table as removed
  for each statement execute function gnomon.search_count_deleted();

-- The full-text branch of Search: the Versions whose searchable text holds
-- a word of `p_query`, As-of the valid-at and known-at instants (a null
-- known-at reads the current belief), best first, at most `p_limit`.
--
-- BM25 with k1 = 1.2 and b = 0.75. The document count, the mean length and
-- each word's document frequency are the sums of the counts of the caller's
-- allowed Scopes, which RLS selects. Every Version of those Scopes is scored
-- from its token rows, and the As-of instants are then checked on each
-- scored Version, so the result is exact. Ties go to the lower Version ID.
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
        select pg_catalog.sum(s.documents)::float8 as documents,
               pg_catalog.sum(s.length)::float8 as length
        from gnomon.search_scope_length s
        where s.tenant_id = p_tenant_id
      ),
      weight as materialized (
        select q.token,
               pg_catalog.ln(1 + (t.documents - f.documents + 0.5) / (f.documents + 0.5)) as idf
        from query q
        cross join totals t
        cross join lateral (
          select pg_catalog.sum(x.documents)::float8 as documents
          from gnomon.search_token_frequency x
          where x.tenant_id = p_tenant_id and x.token = q.token
        ) f
        where f.documents > 0
      ),
      scored as materialized (
        select p.version_id,
               pg_catalog.sum(w.idf * p.tf * 2.2
                 / (p.tf + 1.2 * (0.25 + 0.75 * p.dl / (t.length / t.documents)))) as score
        from weight w
        cross join totals t
        join gnomon.search_token p on p.tenant_id = p_tenant_id and p.token = w.token
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

-- Erasure (ADR-0003). Every row derived from the text of a Version an
-- Erasure deletes or recomputes goes: its Sentence, as migration 0011 wrote,
-- and its token rows, whose counts go down by the trigger above. The token
-- rows of a deleted Version would go by the foreign key too, and those of a
-- recomputed one would not.
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

-- A Version an Erasure reopens is the one that stood just before the erased
-- Episode first touched its Node or Edge (migration 0011), so its text owes
-- nothing to the Episode. `gnomon.erase_version_text` deleted its token rows
-- with those of every Version held at or after that instant, and this
-- writes them again from its text as it stands, with no Sentence. Only an
-- Erasure rewrites a Version's Recorded time this way, so the trigger fires
-- for the definer alone; a caller's update closes a Version, whose token
-- rows stay.
create function gnomon.version_reindex_search_text() returns trigger
  language plpgsql security invoker set search_path = ''
  as $$
  declare
    t uuid;
  begin
    for t in select distinct n.tenant_id from changed n loop
      insert into gnomon.search_token (tenant_id, scope, token, version_id, tf, dl)
        select * from gnomon.search_text_tokens(t, array(
          select n.id from changed n
          where n.tenant_id = t
            and not exists (select from gnomon.search_token x
                            where x.tenant_id = n.tenant_id and x.scope = n.scope
                              and x.version_id = n.id)));
    end loop;
    return null;
  end
  $$;
create trigger version_reindex_search_text after update on gnomon.version
  referencing new table as changed
  for each statement when (gnomon.running_as_definer())
  execute function gnomon.version_reindex_search_text();

-- Whether the running Erasure reaches an Edge of `p_scope`, which it may
-- do in a Scope the caller cannot see (migration 0012).
create function gnomon.erasure_reaches_scope(p_tenant_id uuid, p_scope text) returns boolean
  language sql stable security invoker set search_path = ''
  as $$
    select exists (
      select from gnomon.erasure_reach r
      join gnomon.edge e on e.tenant_id = r.tenant_id and e.id = r.id
      where r.tenant_id = p_tenant_id and r.written_in = pg_current_xact_id_if_assigned()
        and e.scope = p_scope
    )
  $$;

-- The definer's reach into Scopes the caller cannot see, as migration 0012
-- gives it on the other tables of an Edge: it reads and deletes the token
-- rows of a listed Edge's Versions, and reads, lowers and deletes the counts
-- of that Edge's Scope.
create policy search_token_erasure_reach_read on gnomon.search_token
  for select to gnomon_definer
  using (tenant_id = gnomon.tenant_id()
         and gnomon.erasure_reaches_version(tenant_id, version_id));
create policy search_token_erasure_reach_delete on gnomon.search_token
  for delete to gnomon_definer
  using (tenant_id = gnomon.tenant_id()
         and gnomon.erasure_reaches_version(tenant_id, version_id));
do $$
declare
  t text;
begin
  foreach t in array array['search_token_frequency', 'search_scope_length'] loop
    execute format(
      'create policy %1$s_erasure_reach_read on gnomon.%1$I for select to gnomon_definer
         using (tenant_id = gnomon.tenant_id()
                and gnomon.erasure_reaches_scope(tenant_id, scope))', t);
    execute format(
      'create policy %1$s_erasure_reach_update on gnomon.%1$I for update to gnomon_definer
         using (tenant_id = gnomon.tenant_id()
                and gnomon.erasure_reaches_scope(tenant_id, scope))
         with check (tenant_id = gnomon.tenant_id()
                     and gnomon.erasure_reaches_scope(tenant_id, scope))', t);
    execute format(
      'create policy %1$s_erasure_reach_delete on gnomon.%1$I for delete to gnomon_definer
         using (tenant_id = gnomon.tenant_id()
                and gnomon.erasure_reaches_scope(tenant_id, scope))', t);
  end loop;
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
      'gnomon.search_guard()'::regprocedure,
      'gnomon.search_text_tokens(uuid, uuid[])'::regprocedure,
      'gnomon.index_search_text(uuid, uuid[])'::regprocedure,
      'gnomon.version_index_search_text()'::regprocedure,
      'gnomon.sentence_index_search_text()'::regprocedure,
      'gnomon.search_count_inserted()'::regprocedure,
      'gnomon.search_count_deleted()'::regprocedure,
      'gnomon.version_reindex_search_text()'::regprocedure,
      'gnomon.erasure_reaches_scope(uuid, text)'::regprocedure,
      'gnomon.search_full_text(uuid, text, timestamptz, timestamptz, integer)'::regprocedure
    ) and a.grantee <> p.proowner
    union all
    select format('table %s', c.oid::regclass), a.grantee
    from pg_class c, aclexplode(c.relacl) a
    where c.oid in ('gnomon.search_token'::regclass, 'gnomon.search_token_frequency'::regclass,
                    'gnomon.search_scope_length'::regclass)
      and a.grantee <> c.relowner
  loop
    execute format(
      'revoke all on %s from %s', g.object,
      case when g.grantee = 0 then 'public' else g.grantee::regrole::text end);
  end loop;
  revoke all on function gnomon.kind_search(uuid, text) from public;
  revoke all on function gnomon.search_guard() from public;
  revoke all on function gnomon.search_text_tokens(uuid, uuid[]) from public;
  revoke all on function gnomon.index_search_text(uuid, uuid[]) from public;
  revoke all on function gnomon.version_index_search_text() from public;
  revoke all on function gnomon.sentence_index_search_text() from public;
  revoke all on function gnomon.search_count_inserted() from public;
  revoke all on function gnomon.search_count_deleted() from public;
  revoke all on function gnomon.version_reindex_search_text() from public;
  revoke all on function gnomon.erasure_reaches_scope(uuid, text) from public;
  revoke all on function gnomon.search_full_text(uuid, text, timestamptz, timestamptz, integer)
    from public;
end
$$;

-- Callers read the token rows and the counts of their own Scopes, through
-- Search. The writes they are granted are the triggers', which run as the
-- caller; the guard refuses them as a caller's own statement.
grant select, insert, delete on gnomon.search_token to authenticated, gnomon_worker;
grant select, insert, update, delete on gnomon.search_token_frequency, gnomon.search_scope_length
  to authenticated, gnomon_worker;
grant execute on function gnomon.kind_search(uuid, text) to authenticated, gnomon_worker;
grant execute on function gnomon.search_text_tokens(uuid, uuid[]) to authenticated, gnomon_worker;
grant execute on function gnomon.index_search_text(uuid, uuid[]) to authenticated, gnomon_worker;
grant execute on function gnomon.search_full_text(uuid, text, timestamptz, timestamptz, integer)
  to authenticated, gnomon_worker;

-- What Erasure needs as gnomon_definer: to read and delete the token rows
-- and to change the counts, beside writing both as a replayed Version's
-- trigger does. The vacuum call vacuums all three tables (migration 0013),
-- which `gnomon.erasure_vacuum_tables()` now lists, since the definer may
-- delete from them.
grant select, insert, delete on gnomon.search_token to gnomon_definer;
grant select, insert, update, delete on gnomon.search_token_frequency, gnomon.search_scope_length
  to gnomon_definer;
grant execute on function gnomon.kind_search(uuid, text) to gnomon_definer;
grant execute on function gnomon.search_text_tokens(uuid, uuid[]) to gnomon_definer;
grant execute on function gnomon.index_search_text(uuid, uuid[]) to gnomon_definer;
grant execute on function gnomon.erasure_reaches_scope(uuid, text) to gnomon_definer;
grant maintain on gnomon.search_token, gnomon.search_token_frequency, gnomon.search_scope_length
  to gnomon_maintainer;

insert into gnomon.schema_version (version, name) values (16, '0016_full_text_search');
