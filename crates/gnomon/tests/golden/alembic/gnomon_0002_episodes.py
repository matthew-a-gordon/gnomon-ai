"""gnomon migration 0002_episodes.

Exported by `gnomon migrations export --target alembic`. Released gnomon
migrations never change, so leave this revision as it is: a later gnomon
release adds new revisions after it.
"""

from alembic import op

revision = "gnomon_0002"
down_revision = "gnomon_0001"
branch_labels = None
depends_on = None

SQL = """-- gnomon migration 0002: Episodes, and the Store functions that record and
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

-- Grants. First every grant a default privilege made on this migration's
-- objects is revoked, so each holds only the grants below. The policy covers
-- every command, so the grants alone keep callers from updating or deleting
-- an Episode.
do $$
declare
  g record;
begin
  for g in
    select format('function %s', p.oid::regprocedure) as object, a.grantee
    from pg_proc p, aclexplode(p.proacl) a
    where p.oid in (
      'gnomon.record_episode(uuid, text, text, timestamptz, integer)'::regprocedure,
      'gnomon.read_episode(uuid, uuid)'::regprocedure
    ) and a.grantee <> p.proowner
    union
    select format('table %s', c.oid::regclass), a.grantee
    from pg_class c, aclexplode(c.relacl) a
    where c.oid = 'gnomon.episode'::regclass and a.grantee <> c.relowner
  loop
    execute format(
      'revoke all on %s from %s', g.object,
      case when g.grantee = 0 then 'public' else g.grantee::regrole::text end);
  end loop;
end
$$;
revoke all on function gnomon.record_episode(uuid, text, text, timestamptz, integer) from public;
revoke all on function gnomon.read_episode(uuid, uuid) from public;
grant select, insert on gnomon.episode to authenticated, gnomon_worker;
grant execute on function gnomon.record_episode(uuid, text, text, timestamptz, integer)
  to authenticated, gnomon_worker;
grant execute on function gnomon.read_episode(uuid, uuid) to authenticated, gnomon_worker;

insert into gnomon.schema_version (version, name) values (2, '0002_episodes');
"""


def upgrade() -> None:
    op.get_bind().exec_driver_sql(SQL, execution_options={"no_parameters": True})


def downgrade() -> None:
    raise NotImplementedError("gnomon migrations have no downgrade")
