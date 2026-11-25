-- gnomon migration 0014: stage records.
--
-- The Pipeline's `add_episode` is one resumable call (ADR-0011). Each stage
-- commits its output to the Store together with its own stage record, so a
-- retry resumes at the first unfinished stage. The optional server queue is
-- the same table: a worker claims a pending or retry-due record with a lease,
-- runs the stage, and finishes the record in the stage's own transaction.
--
-- A stage record holds the Episode, the stage name, a status, an attempt
-- count, the last error, a retry time and a lease time. Its usage counts,
-- one row per model, hold the model name, the call count, the input and
-- output tokens and the provider's reported cost when given. Neither table
-- has a column for model output or any other text (ADR-0011): anything
-- derived from an Episode must be reachable by Erasure (ADR-0003), and model
-- output would be one more copy of the Episode to delete. So every text
-- column but the Scope is an identifier, refused by a check constraint unless
-- it matches a fixed pattern with no spaces and a fixed maximum length. The
-- Scope is the Episode's own, which the foreign key to the Episode checks.
-- The last error is a code, such as `rate_limited` or `output.invalid_json`,
-- and an optional number, such as the provider's HTTP status. It has no
-- message, because a provider's message, or a parser's, often quotes its
-- input or the model's output.
--
-- Status. A record is
--
--   pending    written, not yet claimed;
--   running    claimed, with a lease until `lease_until`;
--   retry      failed, and due again at `retry_at`;
--   succeeded  finished, in the transaction that committed the stage's output;
--   failed     failed for the last time. It stays visible as failed.
--
-- A claim (`gnomon.claim_stage`) takes one record that is pending, retry-due
-- or running with an expired lease, sets it running with a lease, counts the
-- attempt and returns it. The Store commits the claim as its own call. The
-- claim locks the record with FOR UPDATE SKIP LOCKED, so two concurrent
-- claimers never get the same record: the second skips the record the first
-- holds and takes another, or none. Every time is the database's clock,
-- `now()`, so workers on hosts whose clocks differ agree on when a lease
-- expires and when a retry is due. A lease of zero expires at once.
--
-- A finish (`gnomon.finish_stage`) names the attempt its claim returned. It
-- finishes the record only while the record is still running at that
-- attempt, and otherwise fails with SQLSTATE GNS01. A worker whose lease
-- expired and whose record another worker claimed since cannot finish it, and
-- the stage output in the same transaction rolls back with it. A worker whose
-- lease expired but whom no one replaced still finishes.
--
-- Isolation. Both tables carry the Tenant and the Episode's Scope, and RLS
-- checks both for every caller (ADR-0005). The functions are invokers under
-- RLS with an empty search path (ADR-0009). No new definer is added. Callers
-- may insert and select, and update only the columns a claim or a finish
-- changes. They may not delete. Only Erasure deletes a stage record.
--
-- Erasure. `gnomon.erase_episode_rows` (migration 0011) now deletes the
-- Episode's usage counts and stage records. Erase Scope calls it for each of
-- its Episodes (migration 0012), so a Scope's Erasure removes them too. Erase
-- Node and Erase Edge leave Episodes, and their stage records, alone. The
-- definer role gets the Tenant and Scope policies and SELECT and DELETE on
-- both tables. Retract Episode changes neither table.

create type gnomon.stage_status as enum ('pending', 'running', 'retry', 'succeeded', 'failed');

-- The stage tables name the Episode with its Scope, so that the foreign key
-- checks the Scope they copy.
alter table gnomon.episode add constraint episode_scoped unique (tenant_id, scope, id);

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
create policy stage_record_tenant_scope on gnomon.stage_record
  for all to authenticated, gnomon_worker, gnomon_definer
  using (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()))
  with check (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()));

alter table gnomon.stage_usage enable row level security;
create policy stage_usage_tenant_scope on gnomon.stage_usage
  for all to authenticated, gnomon_worker, gnomon_definer
  using (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()))
  with check (tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes()));

-- Writes a pending record of a stage of an Episode the caller sees, unless a
-- record of that stage exists already, which it leaves as it is.
create function gnomon.stage_pending(p_tenant_id uuid, p_episode_id uuid, p_stage text)
  returns void
  language plpgsql volatile security invoker set search_path = ''
  as $$
  declare
    v_scope text;
  begin
    select e.scope into v_scope
      from gnomon.episode e
      where e.tenant_id = p_tenant_id and e.id = p_episode_id;
    if v_scope is null then
      raise exception using errcode = 'GNW01',
        message = pg_catalog.format('Episode %s is not visible to the caller', p_episode_id);
    end if;
    insert into gnomon.stage_record (tenant_id, scope, episode_id, stage)
      values (p_tenant_id, v_scope, p_episode_id, p_stage)
      on conflict (tenant_id, episode_id, stage) do nothing;
  end
  $$;

-- Claims one due record in the caller's Scopes, or of one Episode when
-- `p_episode_id` is not null: pending, retry-due, or running with an expired
-- lease, the one due longest first. It returns the claimed record, or no row.
create function gnomon.claim_stage(p_tenant_id uuid, p_episode_id uuid, p_lease interval)
  returns setof gnomon.stage_record
  language plpgsql volatile security invoker set search_path = ''
  as $$
  begin
    if p_lease is null or p_lease < interval '0' then
      raise exception using errcode = 'GNW01', message = 'a lease is zero or longer';
    end if;
    return query
      update gnomon.stage_record r
        set status = 'running',
            attempts = r.attempts + 1,
            lease_until = pg_catalog.now() + p_lease,
            retry_at = null,
            updated_at = pg_catalog.now()
        where (r.tenant_id, r.episode_id, r.stage) = (
          select d.tenant_id, d.episode_id, d.stage
          from gnomon.stage_record d
          where d.tenant_id = p_tenant_id
            and (p_episode_id is null or d.episode_id = p_episode_id)
            and (d.status = 'pending'
                 or (d.status = 'retry' and d.retry_at <= pg_catalog.now())
                 or (d.status = 'running' and d.lease_until <= pg_catalog.now()))
          order by case d.status
                     when 'pending' then d.created_at
                     when 'retry' then d.retry_at
                     else d.lease_until
                   end,
                   d.episode_id, d.stage
          limit 1
          for update skip locked)
        returning r.*;
  end
  $$;

-- Finishes a claimed record as succeeded, retry or failed. `p_attempt` is the
-- attempt count its claim returned. A retry is due `p_retry_after` from now.
-- A success keeps the last error an earlier attempt recorded.
create function gnomon.finish_stage(
  p_tenant_id uuid,
  p_episode_id uuid,
  p_stage text,
  p_attempt integer,
  p_status gnomon.stage_status,
  p_error_code text,
  p_error_status integer,
  p_retry_after interval
) returns void
  language plpgsql volatile security invoker set search_path = ''
  as $$
  begin
    if p_status is null or p_status not in ('succeeded', 'retry', 'failed') then
      raise exception using errcode = 'GNW01',
        message = 'a claimed stage finishes as succeeded, retry or failed';
    end if;
    if (p_status = 'succeeded') <> (p_error_code is null)
       or (p_error_code is null and p_error_status is not null) then
      raise exception using errcode = 'GNW01',
        message = 'a stage that fails records an error code, and one that succeeds records no error';
    end if;
    if (p_status = 'retry') <> (p_retry_after is not null) or p_retry_after < interval '0' then
      raise exception using errcode = 'GNW01',
        message = 'a retry, and only a retry, is due after an interval of zero or longer';
    end if;
    update gnomon.stage_record r
      set status = p_status,
          lease_until = null,
          retry_at = pg_catalog.now() + p_retry_after,
          last_error_code = coalesce(p_error_code, r.last_error_code),
          last_error_status = case when p_error_code is null then r.last_error_status
                                   else p_error_status end,
          updated_at = pg_catalog.now()
      where r.tenant_id = p_tenant_id and r.episode_id = p_episode_id and r.stage = p_stage
        and r.status = 'running' and r.attempts = p_attempt;
    if not found then
      raise exception using errcode = 'GNS01',
        message = pg_catalog.format(
          'the claim of stage %s of Episode %s at attempt %s is no longer held',
          p_stage, p_episode_id, p_attempt);
    end if;
  end
  $$;

-- Adds a model's usage to a stage record of the caller's.
create function gnomon.add_stage_usage(
  p_tenant_id uuid,
  p_episode_id uuid,
  p_stage text,
  p_model text,
  p_calls integer,
  p_input_tokens bigint,
  p_output_tokens bigint,
  p_cost_usd numeric
) returns void
  language plpgsql volatile security invoker set search_path = ''
  as $$
  begin
    insert into gnomon.stage_usage as u
        (tenant_id, scope, episode_id, stage, model, calls, input_tokens, output_tokens, cost_usd)
      select r.tenant_id, r.scope, r.episode_id, r.stage, p_model, p_calls, p_input_tokens,
             p_output_tokens, p_cost_usd
      from gnomon.stage_record r
      where r.tenant_id = p_tenant_id and r.episode_id = p_episode_id and r.stage = p_stage
      on conflict (tenant_id, episode_id, stage, model) do update
        set calls = u.calls + excluded.calls,
            input_tokens = u.input_tokens + excluded.input_tokens,
            output_tokens = u.output_tokens + excluded.output_tokens,
            cost_usd = case when u.cost_usd is null and excluded.cost_usd is null then null
                            else coalesce(u.cost_usd, 0) + coalesce(excluded.cost_usd, 0) end;
    if not found then
      raise exception using errcode = 'GNW01',
        message = pg_catalog.format(
          'no stage record of stage %s of Episode %s is visible to the caller',
          p_stage, p_episode_id);
    end if;
  end
  $$;

-- Every stage record of an Episode the caller sees, in the order written.
create function gnomon.read_stage_records(p_tenant_id uuid, p_episode_id uuid)
  returns table (
    tenant_id uuid,
    scope text,
    episode_id uuid,
    stage text,
    status gnomon.stage_status,
    attempts integer,
    last_error_code text,
    last_error_status integer,
    retry_at timestamptz,
    lease_until timestamptz,
    created_at timestamptz,
    updated_at timestamptz
  )
  language sql stable security invoker set search_path = ''
  as $$
    select r.tenant_id, r.scope, r.episode_id, r.stage, r.status, r.attempts,
           r.last_error_code, r.last_error_status,
           r.retry_at, r.lease_until, r.created_at, r.updated_at
    from gnomon.stage_record r
    where r.tenant_id = p_tenant_id and r.episode_id = p_episode_id
    order by r.created_at, r.stage
  $$;

-- The usage counts of an Episode's stage records, by stage and model.
create function gnomon.read_stage_usage(p_tenant_id uuid, p_episode_id uuid)
  returns table (
    stage text,
    model text,
    calls integer,
    input_tokens bigint,
    output_tokens bigint,
    cost_usd numeric
  )
  language sql stable security invoker set search_path = ''
  as $$
    select u.stage, u.model, u.calls, u.input_tokens, u.output_tokens, u.cost_usd
    from gnomon.stage_usage u
    where u.tenant_id = p_tenant_id and u.episode_id = p_episode_id
    order by u.stage, u.model
  $$;

-- Migration 0011's function with the stage tables' deletes added before the
-- Episode's. Everything else is as migration 0011 wrote it.
create or replace function gnomon.erase_episode_rows(p_tenant_id uuid, p_episode_id uuid)
  returns void
  language plpgsql volatile security invoker set search_path = ''
  as $$
  begin
    delete from gnomon.statement s
      where s.tenant_id = p_tenant_id and s.episode_id <> p_episode_id
        and s.kind = 'retraction'
        and exists (select from gnomon.citation c
                    where c.tenant_id = s.tenant_id and c.version_id = s.version_id
                      and c.episode_id = p_episode_id)
        and not exists (select from gnomon.citation c
                        where c.tenant_id = s.tenant_id and c.version_id = s.version_id
                          and c.episode_id <> p_episode_id);
    delete from gnomon.bound_statement b
      where b.tenant_id = p_tenant_id
        and (b.episode_id = p_episode_id
             or b.contribution_id in (select c.id from gnomon.contribution c
                                      where c.tenant_id = p_tenant_id
                                        and c.episode_id = p_episode_id));
    delete from gnomon.contribution c
      where c.tenant_id = p_tenant_id and c.episode_id = p_episode_id;
    delete from gnomon.statement s
      where s.tenant_id = p_tenant_id and s.episode_id = p_episode_id;
    delete from gnomon.citation c
      where c.tenant_id = p_tenant_id and c.episode_id = p_episode_id;
    delete from gnomon.episode_retraction r
      where r.tenant_id = p_tenant_id and r.episode_id = p_episode_id;
    delete from gnomon.stage_usage u
      where u.tenant_id = p_tenant_id and u.episode_id = p_episode_id;
    delete from gnomon.stage_record r
      where r.tenant_id = p_tenant_id and r.episode_id = p_episode_id;
    delete from gnomon.episode e
      where e.tenant_id = p_tenant_id and e.id = p_episode_id;
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
      'gnomon.stage_pending(uuid, uuid, text)'::regprocedure,
      'gnomon.claim_stage(uuid, uuid, interval)'::regprocedure,
      'gnomon.finish_stage(uuid, uuid, text, integer, gnomon.stage_status, text, integer, interval)'::regprocedure,
      'gnomon.add_stage_usage(uuid, uuid, text, text, integer, bigint, bigint, numeric)'::regprocedure,
      'gnomon.read_stage_records(uuid, uuid)'::regprocedure,
      'gnomon.read_stage_usage(uuid, uuid)'::regprocedure
    ) and a.grantee <> p.proowner
    union all
    select format('table %s', c.oid::regclass), a.grantee
    from pg_class c, aclexplode(c.relacl) a
    where c.oid in ('gnomon.stage_record'::regclass, 'gnomon.stage_usage'::regclass)
      and a.grantee <> c.relowner
  loop
    execute format(
      'revoke all on %s from %s', g.object,
      case when g.grantee = 0 then 'public' else g.grantee::regrole::text end);
  end loop;
  revoke all on function gnomon.stage_pending(uuid, uuid, text) from public;
  revoke all on function gnomon.claim_stage(uuid, uuid, interval) from public;
  revoke all on function gnomon.finish_stage(
    uuid, uuid, text, integer, gnomon.stage_status, text, integer, interval) from public;
  revoke all on function gnomon.add_stage_usage(
    uuid, uuid, text, text, integer, bigint, bigint, numeric) from public;
  revoke all on function gnomon.read_stage_records(uuid, uuid) from public;
  revoke all on function gnomon.read_stage_usage(uuid, uuid) from public;
  revoke all on gnomon.stage_record, gnomon.stage_usage from public;
end
$$;

-- Callers write and read stage records, and change only what a claim and a
-- finish change. No caller deletes one.
grant select, insert on gnomon.stage_record, gnomon.stage_usage to authenticated, gnomon_worker;
grant update (status, attempts, last_error_code, last_error_status, retry_at, lease_until,
              updated_at)
  on gnomon.stage_record to authenticated, gnomon_worker;
grant update (calls, input_tokens, output_tokens, cost_usd)
  on gnomon.stage_usage to authenticated, gnomon_worker;
grant execute on function gnomon.stage_pending(uuid, uuid, text) to authenticated, gnomon_worker;
grant execute on function gnomon.claim_stage(uuid, uuid, interval) to authenticated, gnomon_worker;
grant execute on function gnomon.finish_stage(
  uuid, uuid, text, integer, gnomon.stage_status, text, integer, interval)
  to authenticated, gnomon_worker;
grant execute on function gnomon.add_stage_usage(
  uuid, uuid, text, text, integer, bigint, bigint, numeric)
  to authenticated, gnomon_worker;
grant execute on function gnomon.read_stage_records(uuid, uuid) to authenticated, gnomon_worker;
grant execute on function gnomon.read_stage_usage(uuid, uuid) to authenticated, gnomon_worker;

-- Erasure deletes an Episode's stage records and usage counts as
-- gnomon_definer, under the callers' policies above.
grant select, delete on gnomon.stage_record, gnomon.stage_usage to gnomon_definer;

insert into gnomon.schema_version (version, name) values (14, '0014_stage_records');
