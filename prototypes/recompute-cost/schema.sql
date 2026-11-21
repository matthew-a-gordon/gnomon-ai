-- PROTOTYPE, wipe me. Scratch schema for the recompute-cost prototype.
do $$ begin
  if not exists (select from pg_roles where rolname = 'gnomon_owner') then create role gnomon_owner nologin; end if;
  if not exists (select from pg_roles where rolname = 'authenticated') then create role authenticated nologin; end if;
end $$;

create extension if not exists btree_gist;
create schema gnomon authorization gnomon_owner;
grant usage on schema gnomon to authenticated;
set role gnomon_owner;

create function gnomon.tenant_id() returns uuid language sql stable set search_path = '' as
$$ select nullif(nullif(current_setting('request.jwt.claims', true), '')::jsonb ->> 'tenant_id', '')::uuid $$;

create function gnomon.uuid_v7() returns uuid language sql volatile set search_path = '' as
$$ select encode(set_bit(set_bit(overlay(uuid_send(gen_random_uuid()) placing
     substring(int8send(floor(extract(epoch from clock_timestamp()) * 1000)::bigint) from 3) from 1 for 6), 52, 1), 53, 1), 'hex')::uuid $$;

create table gnomon.tenant (id uuid primary key);
create table gnomon.kind (
  tenant_id uuid not null references gnomon.tenant, name text not null, ranked_merge boolean not null,
  primary key (tenant_id, name));
create table gnomon.node (
  tenant_id uuid not null, id uuid primary key, kind text not null,
  foreign key (tenant_id, kind) references gnomon.kind);
create table gnomon.episode (
  tenant_id uuid not null, id uuid primary key, authority int not null default 0,
  reference_time timestamptz not null, recorded_at timestamptz not null);
create table gnomon.contribution (
  id bigint generated always as identity primary key,
  tenant_id uuid not null, episode_id uuid not null references gnomon.episode, node_id uuid not null references gnomon.node,
  property text not null, value jsonb not null,   -- jsonb 'null' is a statement (end of validity, retraction): the property has no value
  valid_from timestamptz not null, valid_to timestamptz not null,
  valid_from_prec smallint not null, valid_to_prec smallint not null,
  recorded_at timestamptz not null,
  check (valid_from < valid_to));
create index contribution_node on gnomon.contribution (tenant_id, node_id, valid_from);
create index contribution_episode on gnomon.contribution (episode_id);
create table gnomon.version (
  id uuid primary key, tenant_id uuid not null, node_id uuid not null references gnomon.node,
  props jsonb not null,
  valid_from timestamptz not null, valid_to timestamptz not null,
  valid_from_prec smallint not null, valid_to_prec smallint not null,
  recorded_from timestamptz not null, recorded_to timestamptz not null default 'infinity',
  rule text not null,
  check (valid_from < valid_to), check (recorded_from < recorded_to),
  exclude using gist (tenant_id with =, node_id with =,
                      tstzrange(valid_from, valid_to) with &&, tstzrange(recorded_from, recorded_to) with &&));
create index version_node on gnomon.version (tenant_id, node_id, valid_from, recorded_from);
create table gnomon.citation (
  tenant_id uuid not null,
  version_id uuid not null references gnomon.version on delete cascade,
  episode_id uuid not null references gnomon.episode on delete cascade,
  property text not null,
  primary key (version_id, episode_id, property));
create index citation_episode on gnomon.citation (episode_id);

-- RLS on every table, as the invoker role.
do $$ declare t text; begin
  foreach t in array array['kind','node','episode','contribution','version','citation'] loop
    execute format('alter table gnomon.%I enable row level security', t);
    execute format('create policy tenant_isolation on gnomon.%I for all to authenticated using (tenant_id = gnomon.tenant_id()) with check (tenant_id = gnomon.tenant_id())', t);
  end loop;
  alter table gnomon.tenant enable row level security;
  create policy tenant_isolation on gnomon.tenant for all to authenticated using (id = gnomon.tenant_id());
end $$;
grant select on gnomon.tenant, gnomon.kind, gnomon.node to authenticated;
grant select, insert on gnomon.episode, gnomon.contribution, gnomon.citation, gnomon.version to authenticated;
grant update (recorded_to) on gnomon.version to authenticated;
grant usage on sequence gnomon.contribution_id_seq to authenticated;

-- ADR-0009: callers may only close an open Version at the transaction time. The definer role (Erasure) may rewrite.
create function gnomon.version_guard() returns trigger language plpgsql set search_path = '' as $$
begin
  if current_user = 'gnomon_owner' then return new; end if;
  if (new.id, new.tenant_id, new.node_id, new.props, new.valid_from, new.valid_to, new.recorded_from, new.rule)
     is distinct from (old.id, old.tenant_id, old.node_id, old.props, old.valid_from, old.valid_to, old.recorded_from, old.rule) then
    raise exception 'version rows are immutable apart from recorded_to'; end if;
  if old.recorded_to <> 'infinity' or new.recorded_to <> transaction_timestamp() then
    raise exception 'a Version may only be closed at the transaction time'; end if;
  return new;
end $$;
create trigger version_guard before update on gnomon.version for each row execute function gnomon.version_guard();
