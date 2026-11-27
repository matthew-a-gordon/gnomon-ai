-- gnomon 0001: the schema. The floor checks, the extensions, the caller
-- roles and the definer role, the gnomon schema and its types, the
-- schema_version ledger, UUID v7, the tenancy mode, the reference Tenant and
-- Scope functions, and every table with its indexes and its Tenant and
-- Scope policies.
--
-- The six files define every object once. A consumer copies them into its
-- own migration chain, unchanged but for the embedding dimension (see
-- `gnomon.embedding_dimension()` below), and its tooling applies them in
-- order, each in one transaction (ADR-0006). Once released, a file never
-- changes: a later release adds new files.
--
-- The floor is PostgreSQL 17, pgvector 0.7.0 and btree_gist (ADR-0023). Nothing
-- here uses a PG 18-only feature.
--
-- Search paths. Every gnomon function sets its `search_path`, and every path
-- ends with `pg_temp` (ADR-0009). Postgres searches the temporary schema
-- first for tables and types unless the path names it, so with an empty path
-- a caller's temporary type could stand in for a built-in one inside a
-- definer function. A function that uses pgvector has `pg_catalog,
-- <pgvector's schema>, pg_temp`, and every other `pg_catalog, pg_temp`.
--
-- Tenancy modes (ADR-0005). Every row carries a Tenant, and every policy
-- checks it. The mode decides how a call that names no Tenant is treated:
--
--   single-tenant  the call uses the default Tenant, which this file
--                  creates. A caller whose claims bind no Tenant acts in
--                  the default Tenant too.
--   multi-tenant   the call fails, and never falls back to the default.
--
-- The files install single-tenant mode. A deployment that serves several
-- Tenants switches once, at install or later, by running
-- `select gnomon.switch_to_multi_tenant()` as the role that applied the
-- files. The default Tenant then becomes its first Tenant: no row moves, so
-- every row stays readable under that Tenant. The switch cannot be undone.
-- The mode is a row, not DDL, so gnomon applies no DDL at runtime.

do $$
begin
  if current_setting('server_version_num')::int < 170000 then
    raise exception 'gnomon needs PostgreSQL 17 or later, found %', current_setting('server_version');
  end if;
end
$$;

-- The extensions: pgvector for Search and btree_gist for the exclusion
-- constraint on Versions (ADR-0023). Each is created when it is missing, in
-- `extensions` when that schema exists, as on Supabase, and in the
-- migrating role's default schema otherwise. One already installed stays
-- where it is. btree_gist is trusted, so the migrating role needs only
-- CREATE on the database for it. On Azure Flexible Server both must be in
-- `azure.extensions` first.
do $$
begin
  if exists (select from pg_catalog.pg_namespace where nspname = 'extensions') then
    create extension if not exists vector with schema extensions;
    create extension if not exists btree_gist with schema extensions;
  else
    create extension if not exists vector;
    create extension if not exists btree_gist;
  end if;
end
$$;

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

-- The definer roles. Each owns definer functions and nothing else, never
-- logs in, and stays under RLS, so the policies below still apply inside
-- the definer functions: a role that owns no table and cannot bypass RLS is
-- held to the policies that name it (ADR-0009).
--
--   gnomon_writer   every function a caller writes through (#81). Its
--                   policies read the caller's Scopes and write only those
--                   that are writable as well.
--   gnomon_definer  Kind registration, the merge-rule setter and the search
--                   index trigger. Its policies read the caller's Scopes,
--                   apart from the BM25 totals of its own transaction.
--   gnomon_eraser   Erasure (#83). Its policies check the Tenant only, so an
--                   Erasure removes what depends on what it erases in every
--                   Scope of the Tenant. The Erasure functions check the
--                   caller's Scopes themselves.
--
-- A role of any of these names that already exists must hold no more than
-- this. No caller role is a member of any of them.
do $$
declare
  r text;
begin
  foreach r in array array['gnomon_definer', 'gnomon_writer', 'gnomon_eraser'] loop
    if not exists (select from pg_roles where rolname = r) then
      execute format('create role %I nologin noinherit nobypassrls', r);
    end if;
    if exists (
      select from pg_roles
      where rolname = r
        and (rolcanlogin or rolinherit or rolsuper or rolbypassrls or rolcreaterole
             or rolcreatedb or rolreplication)
    ) then
      raise exception 'the gnomon role % must be NOLOGIN and NOINHERIT, must not bypass RLS, '
        'and must not create roles or databases or replicate', r;
    end if;
    if exists (
      select from pg_auth_members m join pg_roles x on x.oid = m.member
      where x.rolname = r
    ) then
      raise exception 'the gnomon role % must not be a member of another role', r;
    end if;
    if exists (
      select from pg_auth_members m
      join pg_roles g on g.oid = m.roleid
      join pg_roles x on x.oid = m.member
      where g.rolname = r and x.rolname in ('authenticated', 'gnomon_worker')
    ) then
      raise exception 'the caller roles must not be members of the gnomon role %', r;
    end if;
  end loop;
end
$$;

-- pgvector's schema. The Store's read functions run as the caller, so the
-- caller roles need to reach pgvector's types and functions, the writer role
-- needs them to store embeddings, the definer role needs them for the
-- search index trigger, and the eraser role for the embedding rows an
-- Erasure deletes. `public` and Supabase's
-- `extensions` already let them. A role that neither owns the schema nor
-- holds the grant option is only warned and grants nothing, so the check
-- after each grant fails the migration instead.
do $$
declare
  ext text := (
    select n.nspname
    from pg_catalog.pg_extension e
    join pg_catalog.pg_namespace n on n.oid = e.extnamespace
    where e.extname = 'vector');
  r text;
begin
  if not (has_schema_privilege('authenticated', ext, 'USAGE')
          and has_schema_privilege('gnomon_worker', ext, 'USAGE')) then
    execute format('grant usage on schema %I to authenticated, gnomon_worker', ext);
    if not (has_schema_privilege('authenticated', ext, 'USAGE')
            and has_schema_privilege('gnomon_worker', ext, 'USAGE')) then
      raise exception 'the caller roles authenticated and gnomon_worker need USAGE on schema %, '
        'where pgvector is; grant it as the schema''s owner and apply this migration again', ext;
    end if;
  end if;
  -- A definer function resolves pgvector's names in its schema, so a caller
  -- that could create objects there could put its own code in their place.
  if has_schema_privilege('authenticated', ext, 'CREATE')
     or has_schema_privilege('gnomon_worker', ext, 'CREATE') then
    raise exception 'the caller roles authenticated and gnomon_worker must not hold CREATE on '
      'schema %, where pgvector is; revoke it and apply this migration again', ext;
  end if;
  foreach r in array array['gnomon_definer', 'gnomon_writer', 'gnomon_eraser'] loop
    if not has_schema_privilege(r, ext, 'USAGE') then
      execute format('grant usage on schema %I to %I', ext, r);
      if not has_schema_privilege(r, ext, 'USAGE') then
        raise exception 'the role % needs USAGE on schema %, where pgvector is; '
          'grant it as the schema''s owner and apply this migration again', r, ext;
      end if;
    end if;
  end loop;
end
$$;

-- pgvector's names. The SQL in this file names pgvector's types, functions
-- and operators without a schema, so the file sets `search_path` to
-- `pg_catalog, <pgvector's schema>, pg_temp` for its own statements, and
-- each function that uses pgvector keeps that path with `set search_path
-- from current`. No caller's path can then change what a name means, as
-- long as no untrusted role may CREATE in pgvector's schema. Moving the
-- extension to another schema later breaks those functions. The runner's
-- own path is saved first and restored at the end of the file, because a
-- runner may apply several files in one session or one transaction.
do $$
begin
  perform pg_catalog.set_config('gnomon.saved_search_path',
                                pg_catalog.current_setting('search_path'), false);
  perform pg_catalog.set_config('search_path', (
    select pg_catalog.format('pg_catalog, %I, pg_temp', n.nspname)
    from pg_catalog.pg_extension e
    join pg_catalog.pg_namespace n on n.oid = e.extnamespace
    where e.extname = 'vector'), false);
end
$$;

-- Every gnomon object lives here. PostgREST does not expose this schema.
create schema gnomon;

-- The ledger the Store reads to refuse a schema it does not expect.
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

-- The embedding dimension: the width of every embedding the Store keeps,
-- and of their quantized bits. This line is the one place that states it. A
-- consumer whose model has another width edits the number before applying
-- the files. The vector and bit columns are untyped by width and checked
-- against this function.
create function gnomon.embedding_dimension() returns integer
  language sql immutable parallel safe security invoker set search_path = pg_catalog, pg_temp
  as $$ select 1536 $$;

-- UUID v7 (RFC 9562): a 48-bit Unix millisecond timestamp, then random bits,
-- with the version and variant set. PG 18's uuidv7() is not used, because
-- the floor is PG 17. gen_random_uuid() is core since PG 13.
create function gnomon.uuid_v7() returns uuid
  language sql volatile parallel safe security invoker set search_path = pg_catalog, pg_temp
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

-- The deployment's tenancy: one row. Like the schema_version ledger it has
-- no Tenant of its own, because it describes the deployment, not a graph.
create table gnomon.tenancy (
  singleton boolean primary key default true check (singleton),
  mode text not null check (mode in ('single', 'multi')),
  -- The default Tenant. In multi-tenant mode, the first Tenant.
  default_tenant_id uuid not null,
  -- When the deployment switched to multi-tenant mode, or null.
  switched_at timestamptz,
  check ((mode = 'multi') = (switched_at is not null))
);

insert into gnomon.tenancy (mode, default_tenant_id) values ('single', gnomon.uuid_v7());

-- Grants decide who reads and who switches; the policies apply to every
-- role but the owner. The callers and the definer roles get SELECT, because
-- `gnomon.tenant_id()` reads the row as whichever of them is calling. No
-- caller role gets UPDATE. The update policy admits only the switch, from
-- single-tenant to multi-tenant mode, so a role a deployment grants UPDATE
-- can do nothing else to the row, and the check constraint holds the time
-- of the switch to the mode. The row changes once and never back. Only the
-- schema's owner could change it otherwise, and gnomon offers no defence
-- against its owner (ADR-0024).
--
-- The row holds no secret. The first Tenant's ID is readable to every
-- caller in multi-tenant mode, as any Tenant's ID is to whoever holds its
-- claims.
alter table gnomon.tenancy enable row level security;

create policy tenancy_read on gnomon.tenancy for select using (true);

create policy tenancy_switch on gnomon.tenancy for update
  using (mode = 'single') with check (mode = 'multi');

-- The Tenant, the Scopes and the user of the current call (ADR-0005). Every
-- read policy is
--
--   tenant_id = (select gnomon.tenant_id())
--   and scope = any ((select gnomon.allowed_scopes())::text[])
--
-- and every write policy also needs `scope = any ((select
-- gnomon.writable_scopes())::text[])`. Each call is a sub-select of its own,
-- so PostgreSQL runs it once per statement as an InitPlan and not once for
-- each row the policy checks, and the conditions stay index conditions. The
-- cast keeps the sub-select one array, which `any` compares against.
--
--   gnomon.tenant_id()        the Tenant
--   gnomon.allowed_scopes()   the Scope keys the caller may read
--   gnomon.writable_scopes()  the Scope keys the caller may write. A write
--                             needs its Scope in both lists.
--   gnomon.user_id()          the user who records an Episode or its
--                             Retraction, kept in `recorded_by`, or null
--
-- These are reference implementations. A deployment replaces them with
-- `create or replace function` to resolve them its own way, and keeps the
-- same signatures. They read `request.jwt.claims`, which the Rust core sets
-- for each transaction:
--
--   as_user(jwt)                     the end user's verified claims, unchanged
--   as_worker(tenant, scope, user)   {"role": "gnomon_worker", "sub": <user, when given>,
--                                     "app_metadata": {"tenant_id": <tenant>, "scopes": [<scope>]}}
--
-- so a replacement must also answer for the worker's claims. It tells a
-- worker call by the `role` claim, because a worker call may carry `sub`.
-- With no claims the Tenant is null and the Scope sets are empty, so every
-- policy denies. In single-tenant mode claims that bind no Tenant resolve to
-- the default Tenant, and a replacement must return `coalesce(<its Tenant>,
-- gnomon.default_tenant())`.
--
-- The four run as the caller during a read, as `gnomon_writer` during a
-- write, as `gnomon_eraser` during an Erasure, and as `gnomon_definer`
-- during Kind registration, the merge-rule setter and the search index
-- trigger (ADR-0009). So a
-- replacement reads only the claims, or reaches other data through a
-- `SECURITY DEFINER` function of its own, such as one that reads a
-- membership table. It does not depend on `current_user`. Anything outside
-- gnomon that it calls must be usable by every one of these roles. Its
-- `search_path`, and that of every function it calls, ends in `pg_temp`
-- (see Search paths above).
--
-- They are PL/pgSQL (ADR-0005). A SQL function with a `SET` clause is never
-- inlined, and PostgreSQL 17 plans its body again for each statement that
-- calls it, while PL/pgSQL keeps its plans for the session. One write runs
-- hundreds of statements that call these functions, so a replacement is
-- best written in PL/pgSQL too, and called in a sub-select from a policy.
create function gnomon.tenant_id() returns uuid
  language plpgsql stable parallel safe security invoker set search_path = pg_catalog, pg_temp
  as $fn$
  begin
    return coalesce(
      nullif(
        nullif(pg_catalog.current_setting('request.jwt.claims', true), '')::jsonb
          -> 'app_metadata' ->> 'tenant_id',
        ''
      )::uuid,
      (select t.default_tenant_id from gnomon.tenancy t where t.mode = 'single')
    );
  end
  $fn$;

create function gnomon.allowed_scopes() returns text[]
  language plpgsql stable parallel safe security invoker set search_path = pg_catalog, pg_temp
  as $$
  declare
    v_scopes jsonb := nullif(pg_catalog.current_setting('request.jwt.claims', true), '')::jsonb
                        -> 'app_metadata' -> 'scopes';
  begin
    if v_scopes is null or pg_catalog.jsonb_typeof(v_scopes) <> 'array' then
      return '{}';
    end if;
    return array(select pg_catalog.jsonb_array_elements_text(v_scopes));
  end
  $$;

-- By default a caller may write every Scope it may read.
create function gnomon.writable_scopes() returns text[]
  language plpgsql stable parallel safe security invoker set search_path = pg_catalog, pg_temp
  as $$
  begin
    return gnomon.allowed_scopes();
  end
  $$;

-- By default the `sub` claim, or null when it is missing or empty.
create function gnomon.user_id() returns text
  language plpgsql stable parallel safe security invoker set search_path = pg_catalog, pg_temp
  as $$
  begin
    return nullif(
      nullif(pg_catalog.current_setting('request.jwt.claims', true), '')::jsonb ->> 'sub',
      '');
  end
  $$;

-- The default Tenant in single-tenant mode, and null in multi-tenant mode,
-- so a caller whose claims bind no Tenant never reaches it once the
-- deployment serves several Tenants.
create function gnomon.default_tenant() returns uuid
  language sql stable parallel safe security invoker set search_path = pg_catalog, pg_temp
  as $$
    select t.default_tenant_id from gnomon.tenancy t where t.mode = 'single'
  $$;

-- The Tenant of one Store call, checked. Every call of the Store's
-- Store API runs this first, in the call's transaction, with the Tenant the
-- call names or null:
--
--   - a call that names no Tenant uses the default Tenant in single-tenant
--     mode, and fails with GNT01 in multi-tenant mode;
--   - the call's Tenant must then be the Tenant the claims bind, as
--     `gnomon.tenant_id()` returns it, or the call fails with GNT02.
--
-- The Store functions themselves still take the Tenant and check it under
-- RLS, so a direct SQL caller that names another Tenant still reaches
-- nothing (ADR-0009).
create function gnomon.call_tenant(p_tenant_id uuid) returns uuid
  language plpgsql stable security invoker set search_path = pg_catalog, pg_temp
  as $$
  declare
    v_mode text;
    v_default uuid;
    v_claims uuid := gnomon.tenant_id();
  begin
    select t.mode, t.default_tenant_id into strict v_mode, v_default from gnomon.tenancy t;
    if p_tenant_id is null then
      if v_mode <> 'single' then
        raise exception using errcode = 'GNT01',
          message = 'this deployment is multi-tenant, so every call names its Tenant';
      end if;
      p_tenant_id := v_default;
    end if;
    if v_claims is null then
      raise exception using errcode = 'GNT02',
        message = format('the call names Tenant %s, but the caller''s claims bind no Tenant',
                         p_tenant_id);
    end if;
    if v_claims <> p_tenant_id then
      raise exception using errcode = 'GNT02',
        message = format('the call names Tenant %s, but the caller''s claims bind Tenant %s',
                         p_tenant_id, v_claims);
    end if;
    return p_tenant_id;
  end
  $$;

-- Switches the deployment to multi-tenant mode and returns its first
-- Tenant, the former default Tenant. Every row stays where it is, so every
-- row stays readable under that Tenant. Switching a multi-tenant deployment
-- again changes nothing and returns the same Tenant.
--
-- This is an invoker function granted to no caller role, not a definer
-- (ADR-0009). It runs with the privileges of whoever calls it, and sets the
-- time of the switch itself. The role that applied the migrations owns the
-- tenancy row and may always switch. A deployment that wants another role
-- to switch grants it EXECUTE on this function, SELECT on gnomon.tenancy and
-- UPDATE (mode, switched_at) on gnomon.tenancy. A role that the update
-- cannot reach gets an error, never a silent single-tenant deployment.
create function gnomon.switch_to_multi_tenant() returns uuid
  language plpgsql volatile security invoker set search_path = pg_catalog, pg_temp
  as $$
  declare
    v_mode text;
    v_first uuid;
  begin
    update gnomon.tenancy set mode = 'multi', switched_at = transaction_timestamp()
      where mode = 'single';
    select t.mode, t.default_tenant_id into v_mode, v_first from gnomon.tenancy t;
    if v_mode is distinct from 'multi' then
      raise exception using errcode = 'insufficient_privilege',
        message = 'the switch to multi-tenant mode could not update gnomon.tenancy';
    end if;
    return v_first;
  end
  $$;

create table gnomon.episode (
  tenant_id uuid not null,
  scope text not null,
  id uuid not null,
  content text not null,
  reference_time timestamptz not null,
  authority integer,
  recorded_at timestamptz not null default transaction_timestamp(),
  -- Who recorded the Episode, as `gnomon.user_id()` names them, or null. No
  -- role but the owner may insert a value, so it is always the default.
  recorded_by text default gnomon.user_id(),
  -- The Tenant leads the key, so a later table's foreign key to an Episode
  -- names the Tenant too and cannot cite another Tenant's Episode.
  primary key (tenant_id, id),
  constraint episode_cited unique (tenant_id, scope, id, reference_time, recorded_at),
  constraint episode_scoped unique (tenant_id, scope, id)
);

-- Serves the policy's (tenant_id, scope) conditions (ADR-0005, ADR-0010).
create index episode_tenant_scope on gnomon.episode (tenant_id, scope, reference_time);

alter table gnomon.episode enable row level security;

-- One row per registered Kind, flattened with its ancestors.
create table gnomon.kind (
  tenant_id uuid not null,
  name text not null
    check (name ~ '^[A-Za-z][A-Za-z0-9_]{0,62}$' and name not in ('node', 'edge')),
  kind_of text not null check (kind_of in ('node', 'edge')),
  -- The parent Kind: a registered Kind of the same kind_of, or that root Kind.
  parent text not null,
  -- Every ancestor, the root first and the parent last.
  ancestors text[] not null check (cardinality(ancestors) > 0),
  -- The flattened property schema: {"type": "object", "properties": {...},
  -- "required": [...]}, the Kind's own properties with its ancestors'.
  properties jsonb not null,
  -- What feeds Search, inherited and own: property names, or `name` and
  -- `aliases` (Node Kinds) or `sentence` (Edge Kinds).
  search text[] not null,
  -- The Node Kinds an Edge Kind may join, at its source and at its target.
  -- A Node of one of these Kinds, or of any Kind below one, qualifies.
  source_kinds text[],
  target_kinds text[],
  -- The arguments the Kind was registered with, so that registering the same
  -- Kind again is recognised as a repeat.
  definition jsonb not null,
  registered_at timestamptz not null default transaction_timestamp(),
  primary key (tenant_id, name),
  check (
    (kind_of = 'edge') = (source_kinds is not null and target_kinds is not null)
  )
);

-- The compiled rules: one jsonpath predicate per property of the flattened
-- Kind. `jsonb_path_match(value, rule)` is true exactly when the value
-- satisfies the property's schema, and is never null. A null value ends a
-- property, so the write path checks only non-null values against the rule.
create table gnomon.kind_rule (
  tenant_id uuid not null,
  kind text not null,
  property text not null,
  rule jsonpath not null,
  primary key (tenant_id, kind, property),
  foreign key (tenant_id, kind) references gnomon.kind (tenant_id, name)
);

-- Every Scope of a Tenant shares its Ontology, so these policies check the
-- Tenant alone. The primary keys lead with the Tenant and serve them.
alter table gnomon.kind enable row level security;

alter table gnomon.kind_rule enable row level security;

create policy kind_tenant on gnomon.kind
  for select to authenticated, gnomon_worker, gnomon_writer, gnomon_definer, gnomon_eraser
  using (tenant_id = (select gnomon.tenant_id()));

create policy kind_tenant_register on gnomon.kind
  for insert to gnomon_definer
  with check (tenant_id = (select gnomon.tenant_id()));

create policy kind_rule_tenant on gnomon.kind_rule
  for select to authenticated, gnomon_worker, gnomon_writer, gnomon_definer, gnomon_eraser
  using (tenant_id = (select gnomon.tenant_id()));

create policy kind_rule_tenant_register on gnomon.kind_rule
  for insert to gnomon_definer
  with check (tenant_id = (select gnomon.tenant_id()));

-- Every Version and Contribution has a precision on each Valid-time bound
-- (ADR-0008). The coarsest is `unknown`, and an unknown bound is stored as
-- -infinity at the start and infinity at the end.
create type gnomon.time_precision as enum ('unknown', 'year', 'month', 'day', 'instant');

-- The rule that chose a Version's values (ADR-0007). `recency` orders by
-- Reference time, then Recorded time.
create type gnomon.merge_rule as enum ('recency', 'ranked');


-- A Node: a stable identity of a Kind, in one Scope. What is known about it
-- is held in its Versions.
create table gnomon.node (
  tenant_id uuid not null,
  scope text not null,
  id uuid not null default gnomon.uuid_v7(),
  -- The root Kind `node` or a registered Node Kind of the Tenant. Root Kinds
  -- are not rows, so a trigger checks this, not a foreign key.
  kind text not null,
  created_at timestamptz not null default transaction_timestamp(),
  primary key (tenant_id, id),
  unique (tenant_id, scope, id)
);

create index node_tenant_scope on gnomon.node (tenant_id, scope, kind);

-- An Edge: a stable identity of an Edge Kind, in one Scope, from a source
-- Node to a target Node.
create table gnomon.edge (
  tenant_id uuid not null,
  scope text not null,
  id uuid not null default gnomon.uuid_v7(),
  -- The root Kind `edge` or an Edge Kind the Tenant registered. Root Kinds
  -- are not rows, so a trigger checks this, not a foreign key.
  kind text not null,
  source_id uuid not null,
  target_id uuid not null,
  created_at timestamptz not null default transaction_timestamp(),
  primary key (tenant_id, id),
  unique (tenant_id, scope, id),
  -- The Tenant leads each key, so an endpoint is always a Node of the Edge's
  -- own Tenant, in any of its Scopes.
  foreign key (tenant_id, source_id) references gnomon.node (tenant_id, id),
  foreign key (tenant_id, target_id) references gnomon.node (tenant_id, id)
);

-- Traversal from a Node, and the foreign keys' checks when a Node is
-- erased. The policy's Tenant and Scope conditions are still index
-- conditions, after the endpoint (ADR-0005).
create index edge_source on gnomon.edge (tenant_id, source_id, scope);

create index edge_target on gnomon.edge (tenant_id, target_id, scope);

alter table gnomon.edge enable row level security;

-- A statement of an Episode about a Node or an Edge.
create table gnomon.statement (
  tenant_id uuid not null,
  scope text not null,
  id bigint generated always as identity,
  node_id uuid,
  edge_id uuid,
  versioned_id uuid generated always as (coalesce(node_id, edge_id)) stored,
  episode_id uuid not null,
  kind text not null check (kind in ('correction', 'end_of_validity', 'retraction')),
  -- The Version an End of validity ended or a Retraction retracted, as it
  -- stood. No foreign key: a Version its own transaction replaces is deleted.
  version_id uuid,
  valid_from timestamptz not null,
  valid_from_precision gnomon.time_precision not null,
  valid_to timestamptz not null,
  valid_to_precision gnomon.time_precision not null,
  recorded_at timestamptz not null default transaction_timestamp(),
  episode_reference_time timestamptz not null,
  episode_recorded_at timestamptz not null,
  retracts_episode boolean not null default false,
  primary key (tenant_id, id),
  unique (tenant_id, scope, id),
  foreign key (tenant_id, scope, node_id) references gnomon.node (tenant_id, scope, id),
  foreign key (tenant_id, scope, edge_id) references gnomon.edge (tenant_id, scope, id),
  foreign key (tenant_id, scope, episode_id, episode_reference_time, episode_recorded_at)
    references gnomon.episode (tenant_id, scope, id, reference_time, recorded_at),
  check (num_nonnulls(node_id, edge_id) = 1),
  check ((kind = 'correction') = (version_id is null)),
  check (valid_from < valid_to),
  check ((valid_from = '-infinity') = (valid_from_precision = 'unknown')),
  check ((valid_to = 'infinity') = (valid_to_precision = 'unknown')),
  constraint statement_retracts_episode_is_a_retraction
    check (not retracts_episode or kind = 'retraction')
);

create index statement_versioned on gnomon.statement (tenant_id, scope, versioned_id, id);

create index statement_episode on gnomon.statement (tenant_id, episode_id);

alter table gnomon.statement enable row level security;

-- A Contribution: one Episode's value for one property of a Node over a Valid
-- time. A JSON null ends the property over that Valid time. Contributions are
-- never updated, and only Erasure deletes them.
create table gnomon.contribution (
  tenant_id uuid not null,
  scope text not null,
  -- The order Contributions were recorded in, which breaks a tie between two
  -- with the same Reference time and Recorded time: the later one wins.
  id bigint generated always as identity,
  node_id uuid,
  episode_id uuid not null,
  property text not null,
  value jsonb not null,
  -- Where in the Episode the value was stated, in the consumer's own terms.
  place text,
  valid_from timestamptz not null,
  valid_from_precision gnomon.time_precision not null,
  valid_to timestamptz not null,
  valid_to_precision gnomon.time_precision not null,
  recorded_at timestamptz not null default transaction_timestamp(),
  -- The Episode's Reference time and Recorded time.
  episode_reference_time timestamptz not null,
  episode_recorded_at timestamptz not null,
  edge_id uuid,
  versioned_id uuid generated always as (coalesce(node_id, edge_id)) stored,
  statement_id bigint,
  primary key (tenant_id, id),
  foreign key (tenant_id, scope, node_id) references gnomon.node (tenant_id, scope, id),
  foreign key (tenant_id, scope, episode_id, episode_reference_time, episode_recorded_at)
    references gnomon.episode (tenant_id, scope, id, reference_time, recorded_at),
  check (valid_from < valid_to),
  check ((valid_from = '-infinity') = (valid_from_precision = 'unknown')),
  check ((valid_to = 'infinity') = (valid_to_precision = 'unknown')),
  constraint contribution_scoped unique (tenant_id, scope, id),
  constraint contribution_edge foreign key (tenant_id, scope, edge_id)
    references gnomon.edge (tenant_id, scope, id),
  constraint contribution_node_or_edge check (num_nonnulls(node_id, edge_id) = 1),
  constraint contribution_statement foreign key (tenant_id, scope, statement_id)
    references gnomon.statement (tenant_id, scope, id) on delete cascade
);

-- Every index a read or a write scans leads with (tenant_id, scope), so the
-- policy's conditions are index conditions, evaluated once per scan and not
-- once per row (ADR-0005).
create index contribution_node on gnomon.contribution (tenant_id, scope, node_id, valid_from);

create index contribution_episode on gnomon.contribution (tenant_id, episode_id);

-- A Version: one state of a Node, its values over a Valid time, held as the
-- current belief over a Recorded time.
create table gnomon.version (
  tenant_id uuid not null,
  scope text not null,
  id uuid not null default gnomon.uuid_v7(),
  node_id uuid,
  -- The property values, {property: value}, with no nulls.
  props jsonb not null,
  valid_from timestamptz not null,
  valid_from_precision gnomon.time_precision not null,
  valid_to timestamptz not null,
  valid_to_precision gnomon.time_precision not null,
  -- Recorded time is assigned only by gnomon: it starts when the transaction
  -- of the write that opened the Version started, and ends when the
  -- transaction of the write that closed it started.
  recorded_from timestamptz not null default transaction_timestamp(),
  recorded_to timestamptz not null default 'infinity',
  merge_rule gnomon.merge_rule not null,
  -- The transaction that wrote the row, and the one that closed it. Only
  -- that transaction may delete the row, or reopen it.
  written_in xid8 not null default pg_current_xact_id(),
  closed_in xid8,
  text_hash text generated always as (encode(sha256(jsonb_send(props)), 'hex')) stored,
  edge_id uuid,
  versioned_id uuid generated always as (coalesce(node_id, edge_id)) stored,
  primary key (tenant_id, id),
  unique (tenant_id, scope, id),
  foreign key (tenant_id, scope, node_id) references gnomon.node (tenant_id, scope, id),
  check (jsonb_typeof(props) = 'object'),
  check (valid_from < valid_to),
  check (recorded_from < recorded_to),
  check ((valid_from = '-infinity') = (valid_from_precision = 'unknown')),
  check ((valid_to = 'infinity') = (valid_to_precision = 'unknown')),
  check ((recorded_to = 'infinity') = (closed_in is null)),
  -- No two Versions of a Node or an Edge overlap in both Valid time and
  -- Recorded time. btree_gist gives uuid its GiST equality.
  constraint version_no_overlap exclude using gist (
    tenant_id with =,
    versioned_id with =,
    tstzrange(valid_from, valid_to) with &&,
    tstzrange(recorded_from, recorded_to) with &&
  ),
  constraint version_edge foreign key (tenant_id, scope, edge_id)
    references gnomon.edge (tenant_id, scope, id),
  constraint version_node_or_edge check (num_nonnulls(node_id, edge_id) = 1)
);

-- As-of reads: the Node, then the four time columns (ADR-0004).
create index version_as_of on gnomon.version
  (tenant_id, scope, node_id, valid_from, recorded_from) include (valid_to, recorded_to);

create index version_open on gnomon.version (tenant_id, scope, versioned_id, valid_from)
  where recorded_to = 'infinity';

-- The merge also finds the Versions of one Node or Edge by `versioned_id`
-- when they are closed: those recorded at the step, and those the step
-- closed. Without this index each such statement read every Version of the
-- Scope.
create index version_versioned on gnomon.version
  (tenant_id, scope, versioned_id, recorded_from);

-- A Citation: the link from a Version to an Episode it was derived from,
-- naming the property and, if the write gave one, the place.
create table gnomon.citation (
  tenant_id uuid not null,
  scope text not null,
  version_id uuid not null,
  episode_id uuid not null,
  property text not null,
  place text,
  -- When the Citation was recorded, the start of its write's transaction.
  recorded_at timestamptz not null default transaction_timestamp(),
  -- The Episode's Reference time and Recorded time.
  episode_reference_time timestamptz not null,
  episode_recorded_at timestamptz not null,
  written_in xid8 not null default pg_current_xact_id(),
  foreign key (tenant_id, scope, version_id) references gnomon.version (tenant_id, scope, id),
  foreign key (tenant_id, scope, episode_id, episode_reference_time, episode_recorded_at)
    references gnomon.episode (tenant_id, scope, id, reference_time, recorded_at),
  constraint citation_once unique nulls not distinct
    (tenant_id, version_id, episode_id, property, place)
);

create index citation_episode on gnomon.citation (tenant_id, episode_id);

create index citation_tenant_scope on gnomon.citation (tenant_id, scope, version_id);

-- RLS: the Tenant and the Scopes of the call, exactly as on Episodes
-- (ADR-0005). The policies are at the end of this file.
alter table gnomon.node enable row level security;

alter table gnomon.contribution enable row level security;

alter table gnomon.version enable row level security;

alter table gnomon.citation enable row level security;

-- A bound statement: an Episode's statement of one Valid-time bound of one
-- Contribution.
create table gnomon.bound_statement (
  tenant_id uuid not null,
  scope text not null,
  -- The order statements were recorded in: the later statement about a bound
  -- is the one that holds.
  id bigint generated always as identity,
  node_id uuid,
  episode_id uuid not null,
  contribution_id bigint not null,
  -- The bound the statement is about: `from` or `to`.
  side text not null check (side in ('from', 'to')),
  -- Decided by the insert trigger, never by the caller.
  kind text not null check (kind in ('refinement', 'correction')),
  bound_at timestamptz not null check (bound_at not in ('-infinity', 'infinity')),
  bound_precision gnomon.time_precision not null check (bound_precision <> 'unknown'),
  recorded_at timestamptz not null default transaction_timestamp(),
  episode_reference_time timestamptz not null,
  episode_recorded_at timestamptz not null,
  edge_id uuid,
  versioned_id uuid generated always as (coalesce(node_id, edge_id)) stored,
  primary key (tenant_id, id),
  foreign key (tenant_id, scope, node_id) references gnomon.node (tenant_id, scope, id),
  foreign key (tenant_id, scope, contribution_id)
    references gnomon.contribution (tenant_id, scope, id),
  foreign key (tenant_id, scope, episode_id, episode_reference_time, episode_recorded_at)
    references gnomon.episode (tenant_id, scope, id, reference_time, recorded_at),
  constraint bound_statement_edge foreign key (tenant_id, scope, edge_id)
    references gnomon.edge (tenant_id, scope, id),
  constraint bound_statement_node_or_edge check (num_nonnulls(node_id, edge_id) = 1)
);

create index bound_statement_contribution
  on gnomon.bound_statement (tenant_id, contribution_id, side, id);

create index bound_statement_episode on gnomon.bound_statement (tenant_id, episode_id);

alter table gnomon.bound_statement enable row level security;

-- Each change of a Kind's own merge rule. The latest row of a Kind holds.
create table gnomon.kind_merge (
  tenant_id uuid not null,
  id bigint generated always as identity,
  kind text not null,
  merge_rule text not null check (merge_rule in ('recency', 'ranked')),
  set_at timestamptz not null default transaction_timestamp(),
  primary key (tenant_id, id),
  foreign key (tenant_id, kind) references gnomon.kind (tenant_id, name)
);

create index kind_merge_kind on gnomon.kind_merge (tenant_id, kind, id desc);

alter table gnomon.kind_merge enable row level security;

create policy kind_merge_tenant on gnomon.kind_merge
  for select to authenticated, gnomon_worker, gnomon_writer, gnomon_definer, gnomon_eraser
  using (tenant_id = (select gnomon.tenant_id()));

create policy kind_merge_tenant_set on gnomon.kind_merge
  for insert to gnomon_definer
  with check (tenant_id = (select gnomon.tenant_id()));

-- The merge reads a Node's or an Edge's Contributions by `versioned_id`.
create index contribution_versioned on gnomon.contribution
  (tenant_id, scope, versioned_id, valid_from);

create index contribution_edge on gnomon.contribution (tenant_id, scope, edge_id)
  where edge_id is not null;

-- As-of reads of an Edge, as `version_as_of` serves a Node's.
create index version_edge_as_of on gnomon.version
  (tenant_id, scope, edge_id, valid_from, recorded_from) include (valid_to, recorded_to)
  where edge_id is not null;

-- The Sentence of an Edge Version (ADR-0013). Closed Versions keep theirs. A
-- Version deleted by the transaction that wrote it takes its Sentence with
-- it, and so will an Erasure that deletes a Version.
create table gnomon.sentence (
  tenant_id uuid not null,
  scope text not null,
  version_id uuid not null,
  sentence text not null check (sentence <> ''),
  primary key (tenant_id, scope, version_id),
  foreign key (tenant_id, scope, version_id)
    references gnomon.version (tenant_id, scope, id) on delete cascade,
  constraint sentence_has_words check (sentence ~ '[^[:space:]]')
);

alter table gnomon.sentence enable row level security;

create index contribution_statement on gnomon.contribution (tenant_id, statement_id)
  where statement_id is not null;

create table gnomon.episode_retraction (
  tenant_id uuid not null,
  scope text not null,
  episode_id uuid not null,
  recorded_at timestamptz not null default transaction_timestamp(),
  -- Who retracted the Episode, as for an Episode's `recorded_by`.
  recorded_by text default gnomon.user_id(),
  primary key (tenant_id, episode_id),
  foreign key (tenant_id, episode_id) references gnomon.episode (tenant_id, id)
    on delete cascade
);

alter table gnomon.episode_retraction enable row level security;

create type gnomon.stage_status as enum ('pending', 'running', 'retry', 'succeeded', 'failed');

create table gnomon.stage_record (
  tenant_id uuid not null,
  scope text not null,
  episode_id uuid not null,
  stage text not null
    constraint stage_record_stage_shape check (stage ~ '^[a-z][a-z0-9_]{0,62}$'),
  status gnomon.stage_status not null default 'pending',
  attempts integer not null default 0 check (attempts >= 0),
  last_error_code text
    constraint stage_record_last_error_code_shape
    check (last_error_code ~ '^[a-z][a-z0-9_.]{0,62}$'),
  last_error_status integer,
  retry_at timestamptz,
  lease_until timestamptz,
  created_at timestamptz not null default pg_catalog.now(),
  updated_at timestamptz not null default pg_catalog.now(),
  primary key (tenant_id, episode_id, stage),
  foreign key (tenant_id, scope, episode_id) references gnomon.episode (tenant_id, scope, id),
  constraint stage_record_retry_at check ((status = 'retry') = (retry_at is not null)),
  constraint stage_record_lease check ((status = 'running') = (lease_until is not null)),
  constraint stage_record_error check (
    (last_error_status is null or last_error_code is not null)
    and (status not in ('retry', 'failed') or last_error_code is not null))
);

-- Serves the policy's (tenant_id, scope) conditions and a claim's search for
-- a due record (ADR-0005).
create index stage_record_due on gnomon.stage_record (tenant_id, scope, status)
  where status in ('pending', 'running', 'retry');

create table gnomon.stage_usage (
  tenant_id uuid not null,
  scope text not null,
  episode_id uuid not null,
  stage text not null
    constraint stage_usage_stage_shape check (stage ~ '^[a-z][a-z0-9_]{0,62}$'),
  model text not null
    constraint stage_usage_model_shape check (model ~ '^[A-Za-z0-9][A-Za-z0-9._:/@-]{0,127}$'),
  calls integer not null check (calls >= 0),
  input_tokens bigint not null check (input_tokens >= 0),
  output_tokens bigint not null check (output_tokens >= 0),
  -- The sum of the costs in US dollars the provider reported, or null when no
  -- call reported one. A call without a reported cost adds nothing to it.
  cost_usd numeric(20, 10) check (cost_usd >= 0),
  primary key (tenant_id, episode_id, stage, model),
  foreign key (tenant_id, episode_id, stage)
    references gnomon.stage_record (tenant_id, episode_id, stage),
  foreign key (tenant_id, scope, episode_id) references gnomon.episode (tenant_id, scope, id)
);

create index stage_usage_tenant_scope on gnomon.stage_usage (tenant_id, scope, episode_id);

alter table gnomon.stage_record enable row level security;

alter table gnomon.stage_usage enable row level security;

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

-- The BM25 totals of a Scope: how many texts it holds and how many words
-- they hold together. Search counts how many texts hold a word from the
-- token rows, when it runs (#85, ADR-0010), so no table stores a word's
-- frequency.
create table gnomon.search_scope_length (
  tenant_id uuid not null,
  scope text not null,
  versions bigint not null check (versions > 0),
  words bigint not null check (words > 0),
  primary key (tenant_id, scope)
);

-- The changes one transaction made to a Scope's totals, one row for each
-- Scope, which its commit applies and deletes. Each transaction writes only
-- rows of its own, so no other transaction ever waits on them.
create table gnomon.search_count_delta (
  tenant_id uuid not null,
  scope text not null,
  written_in xid8 not null default pg_current_xact_id(),
  versions bigint not null,
  words bigint not null,
  constraint search_count_delta_once unique (tenant_id, written_in, scope)
);

alter table gnomon.search_token enable row level security;

alter table gnomon.search_scope_length enable row level security;

alter table gnomon.search_count_delta enable row level security;

-- Callers read the rows of their own Tenant and Scopes. The changes another
-- transaction recorded are never visible to a caller: they are uncommitted,
-- and the commit deletes them.
create policy search_scope_length_tenant_scope on gnomon.search_scope_length
  for select to authenticated, gnomon_worker
  using (tenant_id = (select gnomon.tenant_id()) and scope = any ((select gnomon.allowed_scopes())::text[]));

create policy search_count_delta_tenant_scope on gnomon.search_count_delta
  for select to authenticated, gnomon_worker
  using (tenant_id = (select gnomon.tenant_id()) and scope = any ((select gnomon.allowed_scopes())::text[]));

-- The definer records the changes of its own transaction, in any Scope of
-- the Tenant, because they come from the token rows that transaction wrote
-- or deleted, an erased Edge's of a Scope the caller cannot see among them.
create policy search_count_delta_own on gnomon.search_count_delta
  for all to gnomon_definer
  using (tenant_id = (select gnomon.tenant_id()) and written_in = pg_current_xact_id_if_assigned())
  with check (tenant_id = (select gnomon.tenant_id()) and written_in = pg_current_xact_id());

-- The definer changes the totals only of a Scope whose changes its own
-- transaction recorded. The condition is a subquery, not a function call,
-- because a function with a pinned search path costs a setting change on
-- every row the policy checks.
create policy search_scope_length_pending on gnomon.search_scope_length
  for all to gnomon_definer
  using (tenant_id = (select gnomon.tenant_id())
         and exists (select from gnomon.search_count_delta d
                     where d.tenant_id = search_scope_length.tenant_id
                       and d.written_in = pg_current_xact_id_if_assigned()
                       and d.scope = search_scope_length.scope))
  with check (tenant_id = (select gnomon.tenant_id())
              and exists (select from gnomon.search_count_delta d
                          where d.tenant_id = search_scope_length.tenant_id
                            and d.written_in = pg_current_xact_id_if_assigned()
                            and d.scope = search_scope_length.scope));

-- The key of a quantized row's foreign key to its Version.
create unique index version_embedding_key on gnomon.version (tenant_id, scope, id, recorded_to);

-- The full vector of one text in one Scope.
create table gnomon.embedding (
  tenant_id uuid not null,
  scope text not null,
  text_hash text not null check (text_hash ~ '^[0-9a-f]{64}$'),
  embedding vector not null
    check (vector_dims(embedding) = gnomon.embedding_dimension())
    check (vector_norm(embedding) > 0),
  primary key (tenant_id, scope, text_hash)
);

-- The quantized row of one Version.
create table gnomon.embedding_bit (
  tenant_id uuid not null,
  scope text not null,
  version_id uuid not null,
  text_hash text not null,
  q bit varying not null check (length(q) = gnomon.embedding_dimension()),
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
);
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

alter table gnomon.embedding enable row level security;

alter table gnomon.embedding_bit enable row level security;

create table gnomon.summary (
  tenant_id uuid not null,
  scope text not null,
  id uuid not null default gnomon.uuid_v7(),
  node_id uuid not null,
  -- The generated prose.
  content text not null check (content <> ''),
  -- The valid-at instant the Pipeline wrote it for (ADR-0015).
  as_of_valid timestamptz not null check (isfinite(as_of_valid)),
  recorded_from timestamptz not null default transaction_timestamp(),
  recorded_to timestamptz not null default 'infinity',
  -- The transaction that wrote the row. Only that transaction may delete it.
  written_in xid8 not null default pg_current_xact_id(),
  primary key (tenant_id, id),
  unique (tenant_id, scope, id),
  -- The Node may be in any Scope of the Tenant the writer can see, which a
  -- trigger checks.
  foreign key (tenant_id, node_id) references gnomon.node (tenant_id, id),
  check (recorded_from < recorded_to)
);

-- At most one current Summary of a Node in each Scope.
create unique index summary_current on gnomon.summary (tenant_id, scope, node_id)
  where recorded_to = 'infinity';

-- As-of reads: the policy's (tenant_id, scope), the Node, then Recorded time
-- (ADR-0004, ADR-0005).
create index summary_as_of on gnomon.summary
  (tenant_id, scope, node_id, recorded_from) include (recorded_to, as_of_valid);

-- Erasure finds every Summary of a Node, whatever its Scope.
create index summary_node on gnomon.summary (tenant_id, node_id);

-- A Citation of a Summary: an Episode of the Summary's Scope it was derived
-- from, optionally naming a property and a place in the Episode.
create table gnomon.summary_citation (
  tenant_id uuid not null,
  scope text not null,
  summary_id uuid not null,
  episode_id uuid not null,
  property text,
  place text,
  -- The Episode's Reference time and Recorded time, as a Version's Citation
  -- carries them.
  episode_reference_time timestamptz not null,
  episode_recorded_at timestamptz not null,
  written_in xid8 not null default pg_current_xact_id(),
  foreign key (tenant_id, scope, summary_id) references gnomon.summary (tenant_id, scope, id),
  foreign key (tenant_id, scope, episode_id, episode_reference_time, episode_recorded_at)
    references gnomon.episode (tenant_id, scope, id, reference_time, recorded_at),
  constraint summary_citation_once unique nulls not distinct
    (tenant_id, summary_id, episode_id, property, place)
);

create index summary_citation_episode on gnomon.summary_citation (tenant_id, episode_id);

create index summary_citation_tenant_scope on gnomon.summary_citation
  (tenant_id, scope, summary_id);

create table gnomon.community (
  tenant_id uuid not null,
  scope text not null,
  id uuid not null default gnomon.uuid_v7(),
  -- The generated Summary of the group.
  content text not null check (content <> ''),
  as_of_valid timestamptz not null check (isfinite(as_of_valid)),
  recorded_from timestamptz not null default transaction_timestamp(),
  recorded_to timestamptz not null default 'infinity',
  written_in xid8 not null default pg_current_xact_id(),
  primary key (tenant_id, id),
  unique (tenant_id, scope, id),
  check (recorded_from < recorded_to)
);

create index community_as_of on gnomon.community
  (tenant_id, scope, recorded_from) include (recorded_to, as_of_valid);

-- A Node a Community holds. The foreign key to the Node carries the
-- Community's Scope (ADR-0014).
create table gnomon.community_member (
  tenant_id uuid not null,
  scope text not null,
  community_id uuid not null,
  node_id uuid not null,
  written_in xid8 not null default pg_current_xact_id(),
  primary key (tenant_id, community_id, node_id),
  foreign key (tenant_id, scope, community_id) references gnomon.community (tenant_id, scope, id),
  foreign key (tenant_id, scope, node_id) references gnomon.node (tenant_id, scope, id)
);

create index community_member_node on gnomon.community_member (tenant_id, scope, node_id);

create table gnomon.community_citation (
  tenant_id uuid not null,
  scope text not null,
  community_id uuid not null,
  episode_id uuid not null,
  property text,
  place text,
  episode_reference_time timestamptz not null,
  episode_recorded_at timestamptz not null,
  written_in xid8 not null default pg_current_xact_id(),
  foreign key (tenant_id, scope, community_id) references gnomon.community (tenant_id, scope, id),
  foreign key (tenant_id, scope, episode_id, episode_reference_time, episode_recorded_at)
    references gnomon.episode (tenant_id, scope, id, reference_time, recorded_at),
  constraint community_citation_once unique nulls not distinct
    (tenant_id, community_id, episode_id, property, place)
);

create index community_citation_episode on gnomon.community_citation (tenant_id, episode_id);

create index community_citation_tenant_scope on gnomon.community_citation
  (tenant_id, scope, community_id);

-- RLS, with the policies at the end of this file.
do $$
declare
  t text;
begin
  foreach t in array array['summary', 'summary_citation', 'community', 'community_member',
                           'community_citation'] loop
    execute format('alter table gnomon.%I enable row level security', t);
  end loop;
end
$$;

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
  q bit varying check (length(q) = gnomon.embedding_dimension()),
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

-- The keys of the embedding rows' foreign keys, which carry the Recorded-time
-- end.
create unique index summary_embedding_key on gnomon.summary (tenant_id, scope, id, recorded_to);

create unique index community_embedding_key on gnomon.community
  (tenant_id, scope, id, recorded_to);

-- The embedding of one Summary's or Community's text.
create table gnomon.derived_embedding (
  tenant_id uuid not null,
  scope text not null,
  summary_id uuid,
  community_id uuid,
  derived_id uuid generated always as (coalesce(summary_id, community_id)) stored,
  embedding vector not null
    check (vector_dims(embedding) = gnomon.embedding_dimension())
    check (vector_norm(embedding) > 0),
  -- The bits of `embedding`, and the row's `as_of_valid` and Recorded time,
  -- which a trigger sets.
  q bit varying not null check (length(q) = gnomon.embedding_dimension()),
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
);
-- A full vector is read by a re-rank probe from one heap page, as
-- `gnomon.embedding` stores it.
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

alter table gnomon.derived_embedding enable row level security;

-- The Tenant and Scope policies of the graph tables (ADR-0005, ADR-0009).
--
--   <table>_tenant_scope   callers, and `gnomon_writer` on the tables it
--                          writes, read the rows of the caller's Tenant and
--                          allowed Scopes. Callers hold no write privilege
--                          on any table (#81).
--   <table>_definer        `gnomon_definer` reads, and on the search tables
--                          writes what its grants allow, in the same rows,
--                          as the search index trigger needs. It has no
--                          policy on a table the trigger never reads.
--   <table>_eraser         `gnomon_eraser` reads and writes what its grants
--                          allow in every row of the caller's Tenant, as
--                          Erasure needs (#83). The Erasure functions check
--                          the Scope of what they are asked to erase, and
--                          what they return, themselves.
--   <table>_writer_insert  `gnomon_writer` inserts rows of a Scope the
--                          caller may both read and write.
--   <table>_writer_update  `gnomon_writer` changes such rows, only in the
--                          ways listed below.
--   <table>_writer_delete  `gnomon_writer` deletes such rows only when its
--                          own transaction wrote them, which is how a second
--                          write in one transaction replaces the first
--                          (ADR-0008). Committed history goes only by Erasure.
--
-- The writer's grants decide which columns it may set (0006). Its update
-- policies decide how a row may change:
--
--   version     closing an open row at the start of the current
--               transaction, or reopening a row this transaction closed. A
--               trigger records the closing transaction in `closed_in`.
--   summary,    closing an open row at the start of the current
--   community   transaction.
--   the others  any change to the columns its grants allow.
--
-- The writer reads none of the token tables of Search and none of the BM25
-- totals tables (the latter have policies of their own above): only the
-- search index trigger writes them, as `gnomon_definer`. It reads and writes
-- the embedding tables, and updates the bits of a name row, as the table
-- above says.
--
-- The definer reads the tables the search index trigger derives text from,
-- and reads and writes the tables it writes:
--
--   select  version, node, edge, sentence, summary, community
--   all     search_token, embedding, embedding_bit, node_name,
--           derived_search_token
do $$
declare
  t record;
  v_read constant text :=
    'tenant_id = (select gnomon.tenant_id())'
    ' and scope = any ((select gnomon.allowed_scopes())::text[])';
  v_write constant text :=
    'tenant_id = (select gnomon.tenant_id())'
    ' and scope = any ((select gnomon.allowed_scopes())::text[])'
    ' and scope = any ((select gnomon.writable_scopes())::text[])';
begin
  for t in
    select * from (values
      -- table, the writer reads it, inserts, update (old row), update (new row), deletes,
      -- the definer's policy
      ('episode',              true,  true,  null, null, false, null),
      ('node',                 true,  true,  null, null, false, 'select'),
      ('edge',                 true,  true,  null, null, false, 'select'),
      ('statement',            true,  true,  null, null, false, null),
      ('contribution',         true,  true,  null, null, false, null),
      ('version',              true,  true,
         'recorded_to = ''infinity'' or closed_in = pg_current_xact_id()',
         '(recorded_to = transaction_timestamp() and closed_in = pg_current_xact_id())'
         ' or (recorded_to = ''infinity'' and closed_in is null)',
         true, 'select'),
      ('citation',             true,  true,  null, null, true,  null),
      ('bound_statement',      true,  true,  null, null, false, null),
      ('sentence',             true,  true,  'true', 'true', false, 'select'),
      ('episode_retraction',   true,  true,  null, null, false, null),
      ('stage_record',         true,  true,  'true', 'true', false, null),
      ('stage_usage',          true,  true,  'true', 'true', false, null),
      ('search_token',         false, false, null, null, false, 'all'),
      ('embedding',            true,  true,  null, null, false, 'all'),
      ('embedding_bit',        true,  true,  'true', 'true', false, 'all'),
      ('summary',              true,  true,
         'recorded_to = ''infinity''', 'recorded_to = transaction_timestamp()', true, 'select'),
      ('summary_citation',     true,  true,  null, null, true,  null),
      ('community',            true,  true,
         'recorded_to = ''infinity''', 'recorded_to = transaction_timestamp()', true, 'select'),
      ('community_member',     true,  true,  null, null, true,  null),
      ('community_citation',   true,  true,  null, null, true,  null),
      ('node_name',            true,  false, 'true', 'true', false, 'all'),
      ('derived_search_token', false, false, null, null, false, 'all'),
      ('derived_embedding',    true,  true,  null, null, false, null)
    ) x(name, reads, inserts, update_using, update_check, deletes, definer)
  loop
    execute format(
      'create policy %1$s_tenant_scope on gnomon.%1$I for select to %2$s using (%3$s)',
      t.name,
      case when t.reads then 'authenticated, gnomon_worker, gnomon_writer'
           else 'authenticated, gnomon_worker' end,
      v_read);
    if t.definer = 'select' then
      execute format(
        'create policy %1$s_definer on gnomon.%1$I for select to gnomon_definer
           using (%2$s)', t.name, v_read);
    elsif t.definer = 'all' then
      execute format(
        'create policy %1$s_definer on gnomon.%1$I for all to gnomon_definer
           using (%2$s) with check (%2$s)', t.name, v_read);
    end if;
    execute format(
      'create policy %1$s_eraser on gnomon.%1$I for all to gnomon_eraser
         using (tenant_id = (select gnomon.tenant_id()))
         with check (tenant_id = (select gnomon.tenant_id()))',
      t.name);
    if t.inserts then
      execute format(
        'create policy %1$s_writer_insert on gnomon.%1$I for insert to gnomon_writer
           with check (%2$s)', t.name, v_write);
    end if;
    if t.update_using is not null then
      execute format(
        'create policy %1$s_writer_update on gnomon.%1$I for update to gnomon_writer
           using (%2$s and (%3$s)) with check (%2$s and (%4$s))',
        t.name, v_write, t.update_using, t.update_check);
    end if;
    if t.deletes then
      execute format(
        'create policy %1$s_writer_delete on gnomon.%1$I for delete to gnomon_writer
           using (%2$s and written_in = pg_current_xact_id())', t.name, v_write);
    end if;
  end loop;
end
$$;

-- The runner's own search path again (see pgvector's names above).
do $$
begin
  perform pg_catalog.set_config('search_path',
                                pg_catalog.current_setting('gnomon.saved_search_path'), false);
  perform pg_catalog.set_config('gnomon.saved_search_path', '', false);
end
$$;

insert into gnomon.schema_version (version, name) values (1, '0001_schema');
