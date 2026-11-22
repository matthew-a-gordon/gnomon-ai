-- gnomon migration 0005: single-tenant and multi-tenant modes (ADR-0005).
--
-- Every row already carries a Tenant, and every policy already checks it.
-- What the mode changes is how a call that names no Tenant is treated:
--
--   single-tenant  the call uses the default Tenant, which this migration
--                  creates. A caller whose claims bind no Tenant acts in
--                  the default Tenant too.
--   multi-tenant   the call fails, and never falls back to the default.
--
-- This migration installs single-tenant mode. A deployment that serves
-- several Tenants switches once, at install or later, by running
-- `select gnomon.switch_to_multi_tenant()` as the role that applied the
-- migrations. The default Tenant then becomes its first Tenant: no row
-- moves, so every row stays readable under that Tenant. The switch cannot
-- be undone.
--
-- The mode is a row, not DDL, so switching it needs no migration and gnomon
-- applies no DDL at runtime (ADR-0006).

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

-- The row changes once, from single-tenant to multi-tenant mode, and never
-- back. Guard triggers refuse every other update, every delete and a
-- truncate, whoever runs them, the table's owner included. The switch's
-- time is the transaction's, whatever the update says.
create function gnomon.tenancy_guard_update() returns trigger
  language plpgsql security invoker set search_path = ''
  as $$
  begin
    if old.mode = 'single' and new.mode = 'multi'
       and new.singleton = old.singleton
       and new.default_tenant_id = old.default_tenant_id then
      new.switched_at := transaction_timestamp();
      return new;
    end if;
    if old.mode = 'multi' and new.mode = 'single' then
      raise exception using errcode = 'GNT03',
        message = 'a multi-tenant deployment cannot switch back to single-tenant mode';
    end if;
    raise exception using errcode = 'GNT03',
      message = 'the tenancy row changes only by switching from single-tenant to multi-tenant mode';
  end
  $$;

create function gnomon.tenancy_guard_remove() returns trigger
  language plpgsql security invoker set search_path = ''
  as $$
  begin
    raise exception using errcode = 'GNT03',
      message = 'the tenancy row cannot be removed';
  end
  $$;

create trigger tenancy_guard_update before update on gnomon.tenancy
  for each row execute function gnomon.tenancy_guard_update();
create trigger tenancy_guard_delete before delete on gnomon.tenancy
  for each row execute function gnomon.tenancy_guard_remove();
create trigger tenancy_guard_truncate before truncate on gnomon.tenancy
  for each statement execute function gnomon.tenancy_guard_remove();

-- Grants decide who reads and who switches; the policies apply to every
-- role. The callers and the definer role get SELECT, because
-- `gnomon.tenant_id()` reads the row as whichever of them is calling. No
-- caller role gets UPDATE. The update policy admits only the switch, so a
-- role a deployment grants UPDATE can do nothing else to the row, and the
-- guard triggers refuse anything else from the owner too.
--
-- The row holds no secret. The first Tenant's ID is readable to every
-- caller in multi-tenant mode, as any Tenant's ID is to whoever holds its
-- claims.
alter table gnomon.tenancy enable row level security;
create policy tenancy_read on gnomon.tenancy for select using (true);
create policy tenancy_switch on gnomon.tenancy for update
  using (mode = 'single') with check (mode = 'multi');

-- The default Tenant in single-tenant mode, and null in multi-tenant mode,
-- so a caller whose claims bind no Tenant never reaches it once the
-- deployment serves several Tenants.
create function gnomon.default_tenant() returns uuid
  language sql stable parallel safe security invoker set search_path = ''
  as $$
    select t.default_tenant_id from gnomon.tenancy t where t.mode = 'single'
  $$;

-- The reference `gnomon.tenant_id()` of migration 0001, which now falls back
-- to the default Tenant when the claims bind none. Only claims with no
-- Tenant read the tenancy row, so a caller whose claims bind one pays
-- nothing more per policy check.
--
-- Policies evaluate this function once per row they check, and it is never
-- inlined, because it pins its search path. So the fallback is written out
-- here instead of calling `gnomon.default_tenant()`. On a bounded patch to a
-- Node with 100 Contributions, claims that bind a Tenant measured a 9 ms
-- p50. Claims with no Tenant added 1.6 ms to that, and the extra function
-- call had added another 0.8 ms.
--
-- A deployment that already replaced `gnomon.tenant_id()` keeps its own.
-- This migration replaces only the reference body, compared with its
-- whitespace removed. For single-tenant mode to reach the default Tenant,
-- such a function must return `coalesce(<its Tenant>,
-- gnomon.default_tenant())`. Migration 0001 says every replacement must
-- also read the worker's claims,
--
--   {"role": "gnomon_worker",
--    "app_metadata": {"tenant_id": <tenant>, "scopes": [<scope>]}},
--
-- and from this migration on those claims leave out "tenant_id" when a
-- single-tenant worker names no Tenant.
do $$
declare
  reference constant text := $ref$
    select nullif(
      nullif(pg_catalog.current_setting('request.jwt.claims', true), '')::jsonb
        -> 'app_metadata' ->> 'tenant_id',
      ''
    )::uuid
  $ref$;
begin
  if regexp_replace(
       (select p.prosrc from pg_proc p where p.oid = 'gnomon.tenant_id()'::regprocedure),
       '\s', '', 'g'
     ) = regexp_replace(reference, '\s', '', 'g') then
    create or replace function gnomon.tenant_id() returns uuid
      language sql stable parallel safe security invoker set search_path = ''
      as $fn$
        select coalesce(
          nullif(
            nullif(pg_catalog.current_setting('request.jwt.claims', true), '')::jsonb
              -> 'app_metadata' ->> 'tenant_id',
            ''
          )::uuid,
          (select t.default_tenant_id from gnomon.tenancy t where t.mode = 'single')
        )
      $fn$;
  else
    raise notice 'gnomon.tenant_id() was replaced by this deployment and is left as it is. '
      'For single-tenant mode it must return gnomon.default_tenant() when the claims bind no Tenant.';
  end if;
end
$$;

-- The Tenant of one Store call, checked. Every call of the Rust core's
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
  language plpgsql stable security invoker set search_path = ''
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
-- (ADR-0009). It runs with the privileges of whoever calls it. The role
-- that applied the migrations owns the tenancy row and may always switch.
-- A deployment that wants another role to switch grants it EXECUTE on this
-- function, SELECT on gnomon.tenancy and UPDATE (mode) on gnomon.tenancy.
-- A role that the update cannot reach gets an error, never a silent
-- single-tenant deployment.
create function gnomon.switch_to_multi_tenant() returns uuid
  language plpgsql volatile security invoker set search_path = ''
  as $$
  declare
    v_mode text;
    v_first uuid;
  begin
    update gnomon.tenancy set mode = 'multi' where mode = 'single';
    select t.mode, t.default_tenant_id into v_mode, v_first from gnomon.tenancy t;
    if v_mode is distinct from 'multi' then
      raise exception using errcode = 'insufficient_privilege',
        message = 'the switch to multi-tenant mode could not update gnomon.tenancy';
    end if;
    return v_first;
  end
  $$;

-- Grants. First every grant a default privilege made on this migration's
-- objects is revoked, so each object holds only the grants below.
do $$
declare
  g record;
begin
  for g in
    select format('function %s', p.oid::regprocedure) as object, a.grantee
    from pg_proc p, aclexplode(p.proacl) a
    where p.oid in (
      'gnomon.default_tenant()'::regprocedure,
      'gnomon.call_tenant(uuid)'::regprocedure,
      'gnomon.switch_to_multi_tenant()'::regprocedure,
      'gnomon.tenancy_guard_update()'::regprocedure,
      'gnomon.tenancy_guard_remove()'::regprocedure
    ) and a.grantee <> p.proowner
    union
    select format('table %s', c.oid::regclass), a.grantee
    from pg_class c, aclexplode(c.relacl) a
    where c.oid = 'gnomon.tenancy'::regclass and a.grantee <> c.relowner
  loop
    execute format(
      'revoke all on %s from %s', g.object,
      case when g.grantee = 0 then 'public' else g.grantee::regrole::text end);
  end loop;
  revoke all on function gnomon.default_tenant() from public;
  revoke all on function gnomon.call_tenant(uuid) from public;
  revoke all on function gnomon.switch_to_multi_tenant() from public;
  revoke all on function gnomon.tenancy_guard_update() from public;
  revoke all on function gnomon.tenancy_guard_remove() from public;
end
$$;

grant select on gnomon.tenancy to authenticated, gnomon_worker, gnomon_definer;
grant execute on function gnomon.default_tenant() to authenticated, gnomon_worker, gnomon_definer;
grant execute on function gnomon.call_tenant(uuid) to authenticated, gnomon_worker;

insert into gnomon.schema_version (version, name) values (5, '0005_tenancy_modes');
