-- gnomon migration 0001: the gnomon schema, its caller roles, the
-- schema_version ledger, UUID v7, and the reference Tenant and Scope
-- functions.
--
-- Released migrations never change (ADR-0006). A later change is a new
-- numbered file.
--
-- Floor: PostgreSQL 17 and pgvector 0.7.0. Nothing here uses a PG 18-only
-- feature, and no other extension is created.

do $$
begin
  if current_setting('server_version_num')::int < 170000 then
    raise exception 'gnomon needs PostgreSQL 17 or later, found %', current_setting('server_version');
  end if;
end
$$;

create extension if not exists vector;

do $$
declare
  v text := (select extversion from pg_extension where extname = 'vector');
begin
  if string_to_array(split_part(v, '-', 1), '.')::int[] < array[0, 7, 0] then
    raise exception 'gnomon needs pgvector 0.7.0 or later, found %', v;
  end if;
end
$$;

-- Caller roles. `authenticated` is the end user's role, as on Supabase, and
-- `gnomon_worker` runs background work for one Tenant and Scope (ADR-0001).
-- Neither may bypass RLS, because RLS is the barrier between Tenants. A
-- deployment grants both to the role it logs in as.
do $$
begin
  if not exists (select from pg_roles where rolname = 'authenticated') then
    create role authenticated nologin noinherit;
  end if;
  if not exists (select from pg_roles where rolname = 'gnomon_worker') then
    create role gnomon_worker nologin noinherit nobypassrls;
  end if;
  if exists (
    select from pg_roles
    where rolname in ('authenticated', 'gnomon_worker') and (rolsuper or rolbypassrls)
  ) then
    raise exception 'the gnomon caller roles authenticated and gnomon_worker must not bypass RLS';
  end if;
end
$$;

-- Every gnomon object lives here. PostgREST does not expose this schema.
create schema gnomon;

-- The ledger the Rust core reads to refuse a schema it does not expect.
-- It is the one table with no Tenant: it describes the schema, not a graph.
-- Callers may read it and nothing else.
create table gnomon.schema_version (
  version integer primary key check (version > 0),
  name text not null,
  applied_at timestamptz not null default transaction_timestamp()
);
alter table gnomon.schema_version enable row level security;
create policy schema_version_read on gnomon.schema_version
  for select to authenticated, gnomon_worker using (true);

-- The embedding dimension this chain was exported with. Vector columns in
-- later migrations are typed with the same value.
create function gnomon.embedding_dimension() returns integer
  language sql immutable parallel safe security invoker set search_path = ''
  as $$ select {{embedding_dimension}} $$;

-- UUID v7 (RFC 9562): a 48-bit Unix millisecond timestamp, then random bits,
-- with the version and variant set. PG 18's uuidv7() is not used, because
-- the floor is PG 17. gen_random_uuid() is core since PG 13.
create function gnomon.uuid_v7() returns uuid
  language sql volatile parallel safe security invoker set search_path = ''
  as $$
    select encode(
      set_bit(
        set_bit(
          overlay(
            pg_catalog.uuid_send(pg_catalog.gen_random_uuid())
            placing substring(pg_catalog.int8send((extract(epoch from pg_catalog.clock_timestamp()) * 1000)::bigint) from 3)
            from 1 for 6
          ),
          52, 1
        ),
        53, 1
      ),
      'hex'
    )::uuid
  $$;

-- The Tenant and the Scopes of the current call (ADR-0005). Every RLS policy
-- is `tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes())`.
--
-- These are reference implementations. A deployment replaces them with
-- `create or replace function` to resolve Tenant and Scope its own way, and
-- keeps the same signatures. They read `request.jwt.claims`, which the Rust
-- core sets for each transaction:
--
--   as_user(jwt)               the end user's verified claims, unchanged
--   as_worker(tenant, scope)   {"role": "gnomon_worker",
--                               "app_metadata": {"tenant_id": <tenant>, "scopes": [<scope>]}}
--
-- so a replacement must also resolve the worker's claims. With no claims the
-- Tenant is null and the Scope set is empty, so every policy denies.
create function gnomon.tenant_id() returns uuid
  language sql stable parallel safe security invoker set search_path = ''
  as $$
    select nullif(
      nullif(pg_catalog.current_setting('request.jwt.claims', true), '')::jsonb
        -> 'app_metadata' ->> 'tenant_id',
      ''
    )::uuid
  $$;

create function gnomon.allowed_scopes() returns text[]
  language sql stable parallel safe security invoker set search_path = ''
  as $$
    select coalesce(
      array(
        select pg_catalog.jsonb_array_elements_text(s)
        from (
          select nullif(pg_catalog.current_setting('request.jwt.claims', true), '')::jsonb
            -> 'app_metadata' -> 'scopes' as s
        ) c
        where pg_catalog.jsonb_typeof(s) = 'array'
      ),
      '{}'
    )
  $$;

-- Grants. First every grant a default privilege made on the schema and on
-- this migration's objects is revoked, so each holds only the grants below.
-- Every object in the schema is this migration's.
do $$
declare
  g record;
begin
  for g in
    select format('schema %s', n.oid::regnamespace) as object, a.grantee
    from pg_namespace n, aclexplode(n.nspacl) a
    where n.oid = 'gnomon'::regnamespace and a.grantee <> n.nspowner
    union
    select format('function %s', p.oid::regprocedure), a.grantee
    from pg_proc p, aclexplode(p.proacl) a
    where p.pronamespace = 'gnomon'::regnamespace and a.grantee <> p.proowner
    union
    select format('table %s', c.oid::regclass), a.grantee
    from pg_class c, aclexplode(c.relacl) a
    where c.relnamespace = 'gnomon'::regnamespace and a.grantee <> c.relowner
  loop
    execute format(
      'revoke all on %s from %s', g.object,
      case when g.grantee = 0 then 'public' else g.grantee::regrole::text end);
  end loop;
end
$$;
revoke all on schema gnomon from public;
revoke all on all functions in schema gnomon from public;

grant usage on schema gnomon to authenticated, gnomon_worker;
grant select on gnomon.schema_version to authenticated, gnomon_worker;
grant execute on all functions in schema gnomon to authenticated, gnomon_worker;

insert into gnomon.schema_version (version, name) values (1, '0001_gnomon_schema');
