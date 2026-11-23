-- gnomon migration 0002: Episodes, and the Store functions that record and
-- read one.
--
-- An Episode is an immutable source of its own, not a Node (ADR-0003).
-- Callers may insert and select Episodes and nothing else: no UPDATE and no
-- DELETE. Only Erasure, a definer function in a later migration, removes one.

create table gnomon.episode (
  tenant_id uuid not null,
  scope text not null,
  id uuid not null,
  content text not null,
  reference_time timestamptz not null,
  authority integer,
  recorded_at timestamptz not null default transaction_timestamp(),
  -- The Tenant leads the key, so a later table's foreign key to an Episode
  -- names the Tenant too and cannot cite another Tenant's Episode.
  primary key (tenant_id, id)
);

-- Serves the policy's (tenant_id, scope) conditions (ADR-0005, ADR-0010).
create index episode_tenant_scope on gnomon.episode (tenant_id, scope, reference_time);

alter table gnomon.episode enable row level security;
create policy episode_tenant_scope on gnomon.episode
  for all to authenticated, gnomon_worker
  using (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()))
  with check (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()));

grant select, insert on gnomon.episode to authenticated, gnomon_worker;

-- Store functions run as the invoker with an empty search path, so RLS
-- applies to every call and nothing can be resolved through a caller's
-- search path (ADR-0009).

create function gnomon.record_episode(
  p_tenant_id uuid,
  p_scope text,
  p_content text,
  p_reference_time timestamptz,
  p_authority integer
) returns uuid
  language sql volatile security invoker set search_path = ''
  as $$
    insert into gnomon.episode (tenant_id, scope, id, content, reference_time, authority)
    values (p_tenant_id, p_scope, gnomon.uuid_v7(), p_content, p_reference_time, p_authority)
    returning id
  $$;

create function gnomon.read_episode(p_tenant_id uuid, p_episode_id uuid)
  returns table (
    tenant_id uuid,
    scope text,
    id uuid,
    content text,
    reference_time timestamptz,
    authority integer,
    recorded_at timestamptz
  )
  language sql stable security invoker set search_path = ''
  as $$
    select e.tenant_id, e.scope, e.id, e.content, e.reference_time, e.authority, e.recorded_at
    from gnomon.episode e
    where e.tenant_id = p_tenant_id and e.id = p_episode_id
  $$;

revoke all on function gnomon.record_episode(uuid, text, text, timestamptz, integer) from public;
revoke all on function gnomon.read_episode(uuid, uuid) from public;
grant execute on function gnomon.record_episode(uuid, text, text, timestamptz, integer)
  to authenticated, gnomon_worker;
grant execute on function gnomon.read_episode(uuid, uuid) to authenticated, gnomon_worker;

insert into gnomon.schema_version (version, name) values (2, '0002_episodes');
