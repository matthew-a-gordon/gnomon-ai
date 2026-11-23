-- gnomon migration 0013: the vacuum call after Erasure.
--
-- An Erasure is complete at commit, when nothing derived from what it erased
-- is readable. Copies of the erased bytes stay in dead tuples and index pages
-- until the tables it touched are vacuumed (ADR-0003, GLOSSARY.md: Erasure),
-- and so do the dead rows of `gnomon.erasure_reach`, which hold the IDs of
-- what was erased (migration 0012). Prototype 4 found reads about 50% slower
-- after an Erasure until vacuum ran, so the vacuum is the performance fix as
-- well.
--
-- `VACUUM` cannot run inside a transaction block, and so not inside a
-- function. The call is therefore the Rust core's, not a function here: the
-- Store sends each `VACUUM` as a simple query of one statement, outside any
-- transaction, on its own connection (`Store::vacuum_erasure` and
-- `Store::vacuum_erasures`). Through a transaction-mode pooler that is a
-- transaction of its own on whichever server connection the pooler picks, and
-- it sets nothing that outlives it.
--
-- The call is not a definer (ADR-0009). It runs as the login role the
-- deployment connects it with, which holds the privilege through the NOLOGIN
-- role `gnomon_maintainer`. This migration creates that role and grants it
-- `MAINTAIN` (PostgreSQL 17) on every table an Erasure touches, USAGE on the
-- schema and EXECUTE on the function below, and nothing else: it reads no
-- row. The deployment grants it to the login role that runs the call:
--
--   grant gnomon_maintainer to <login role> with inherit true, set false;
--
-- INHERIT is needed because the call cannot `SET ROLE` on a pooled
-- connection, and SET FALSE keeps the login role from becoming it. No caller
-- role gets it. It is not `gnomon_definer`: a login role that inherited the
-- definer would hold Erasure's privileges and its policies outside Erasure.
--
-- Which tables. `gnomon.erasure_vacuum_tables()` reads them from the
-- catalog, so a table a later migration adds cannot be missed: every gnomon
-- table `gnomon_definer` may delete from or update, which are the tables an
-- Erasure deletes from or rewrites, and every gnomon table a foreign key
-- deletes from or updates when one of those changes (ON DELETE or ON UPDATE
-- CASCADE, SET NULL or SET DEFAULT), recursively. Today those are the
-- Episodes, Nodes, Edges, Contributions, Versions, Citations, statements,
-- bound statements, Retraction marks, Sentences and `erasure_reach`. A later
-- migration that adds a table an Erasure deletes from grants the definer
-- DELETE on it already (migration 0011); it grants `gnomon_maintainer`
-- MAINTAIN on it too. Until it does, the call refuses to run rather than
-- skip the table, which `VACUUM` would do with only a warning.
--
-- The tables a given Erasure touched come from the same transaction's
-- statistics (`pg_stat_xact_user_tables`), which count every row it deleted
-- or updated, by trigger or foreign key as well, and the Store returns them
-- with the Erasure's result.
--
-- What the call runs, per table: `VACUUM (INDEX_CLEANUP ON, ANALYZE)`.
-- INDEX_CLEANUP ON stops Postgres skipping the index pass when few pages hold
-- dead tuples, so the index entries of erased rows go too. ANALYZE replaces
-- the planner's statistics, whose most common values may hold erased values.
-- A plain VACUUM removes dead tuples and their index entries and takes no
-- lock that blocks reads or writes, but it does not overwrite the free space
-- it leaves on a page, so the bytes stay on the page until Postgres reuses
-- the space. `VACUUM (FULL, ANALYZE)` writes each table and its indexes anew
-- and drops the old files, so no page of the table holds them, but it locks
-- each table against reads and writes while it runs. The call does either.
--
-- VACUUM removes only dead tuples that no open transaction can still see. A
-- transaction of the same database that began before the Erasure committed
-- keeps them until it ends, and so do replication slots and a standby's
-- hot_standby_feedback.

-- The maintenance role. It never logs in, stays under RLS, and holds nothing
-- but the grants below. A role of that name that already exists must hold no
-- more than this.
do $$
begin
  if not exists (select from pg_roles where rolname = 'gnomon_maintainer') then
    create role gnomon_maintainer nologin noinherit nobypassrls;
  end if;
  if exists (
    select from pg_roles
    where rolname = 'gnomon_maintainer'
      and (rolcanlogin or rolsuper or rolbypassrls or rolcreaterole or rolcreatedb or rolreplication)
  ) then
    raise exception 'the gnomon role gnomon_maintainer must be NOLOGIN, must not bypass RLS, '
      'and must not create roles or databases or replicate';
  end if;
  if exists (
    select from pg_auth_members m join pg_roles r on r.oid = m.member
    where r.rolname = 'gnomon_maintainer'
  ) then
    raise exception 'the gnomon role gnomon_maintainer must not be a member of another role';
  end if;
end
$$;

-- The tables any Erasure may delete from or rewrite, schema-qualified and
-- quoted as `VACUUM` takes them, in order of name, and whether the current
-- role may vacuum each.
create function gnomon.erasure_vacuum_tables()
  returns table (table_name text, maintainable boolean)
  language sql stable security invoker set search_path = ''
  as $$
    with recursive erased (oid) as (
      select c.oid
      from pg_catalog.pg_class c
      where c.relnamespace = 'gnomon'::pg_catalog.regnamespace
        and c.relkind in ('r', 'p')
        and (pg_catalog.has_table_privilege('gnomon_definer', c.oid, 'DELETE')
             or pg_catalog.has_any_column_privilege('gnomon_definer', c.oid, 'UPDATE'))
      union
      select k.conrelid
      from pg_catalog.pg_constraint k
      join erased e on e.oid = k.confrelid
      join pg_catalog.pg_class c on c.oid = k.conrelid
      where k.contype = 'f'
        and c.relnamespace = 'gnomon'::pg_catalog.regnamespace
        and (k.confdeltype in ('c', 'n', 'd') or k.confupdtype in ('c', 'n', 'd'))
    )
    select pg_catalog.format('%I.%I', 'gnomon', c.relname),
           pg_catalog.has_table_privilege(c.oid, 'MAINTAIN')
    from erased e
    join pg_catalog.pg_class c on c.oid = e.oid
    order by c.relname
  $$;

revoke all on function gnomon.erasure_vacuum_tables() from public;
grant usage on schema gnomon to gnomon_maintainer;
grant execute on function gnomon.erasure_vacuum_tables() to gnomon_maintainer;

-- What the call vacuums: every table `gnomon.erasure_vacuum_tables()` lists
-- today. A later migration that adds one grants it here too.
grant maintain on gnomon.episode, gnomon.node, gnomon.edge, gnomon.contribution,
  gnomon.version, gnomon.citation, gnomon.bound_statement, gnomon.statement,
  gnomon.episode_retraction, gnomon.sentence, gnomon.erasure_reach
  to gnomon_maintainer;

insert into gnomon.schema_version (version, name) values (13, '0013_vacuum_after_erasure');
