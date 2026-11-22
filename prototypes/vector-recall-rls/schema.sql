-- PROTOTYPE, wipe me. Scratch schema for the recall prototype.
drop schema if exists gnomon cascade;
create extension if not exists vector;
create schema gnomon;
do $$ begin
  if not exists (select 1 from pg_roles where rolname = 'authenticated') then
    create role authenticated nologin;
  end if;
end $$;
grant usage on schema gnomon to authenticated;

-- ADR-0005: deployment-supplied functions that read request.jwt.claims.
create function gnomon.tenant_id() returns uuid language sql stable as $$
  select (nullif(current_setting('request.jwt.claims', true), '')::jsonb ->> 'tenant_id')::uuid
$$;
create function gnomon.scope_allowed(s text) returns boolean language sql stable as $$
  select case when nullif(current_setting('request.jwt.claims', true), '')::jsonb ? 'scopes'
              then (nullif(current_setting('request.jwt.claims', true), '')::jsonb -> 'scopes') ? s
              else true end
$$;
grant execute on function gnomon.tenant_id(), gnomon.scope_allowed(text) to authenticated;

-- One embedding table; every row has a Tenant and a Citation (episode_id).
create unlogged table gnomon.embedding (
  id          bigint generated always as identity,
  tenant_id   uuid not null,
  scope       text not null default 'default',
  episode_id  uuid not null,
  embedding   vector(1536) not null
);
alter table gnomon.embedding enable row level security;
grant select on gnomon.embedding to authenticated;
create policy tenant_isolation on gnomon.embedding for select to authenticated
  using (tenant_id = gnomon.tenant_id() and gnomon.scope_allowed(scope));
