-- gnomon 0006: grants. What each role may do on the schema, its tables and
-- its functions, and the hand-over of the definer functions to
-- `gnomon_writer`, `gnomon_definer` and `gnomon_eraser` (ADR-0009).

-- Grants. First every grant a default privilege made on the schema and on
-- its functions, tables and sequences is revoked, so each holds only the
-- grants below. The sequences are those of the identity columns, and no
-- caller holds any right on them.
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
    select format(case when c.relkind = 'S' then 'sequence %s' else 'table %s' end,
                  c.oid::regclass), a.grantee
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

-- The schema. PostgREST does not expose it, and only these five roles may
-- reach it.
grant usage on schema gnomon
  to authenticated, gnomon_worker, gnomon_writer, gnomon_definer, gnomon_eraser;

-- The tables. Callers read their own Tenant's and Scopes' rows under RLS
-- and write nothing: they hold no INSERT, UPDATE, DELETE or TRUNCATE
-- privilege on any table or column (#81). Every write goes through a
-- definer function owned by `gnomon_writer`, which reads what the callers
-- read on the tables it writes, inserts only the columns a write may
-- choose, so an ID, a Recorded time, a transaction mark or `recorded_by`
-- always takes its default, updates only the few columns a write changes,
-- and deletes only rows its policies let a transaction delete: those it
-- wrote itself. The definer role reads and writes what Kind registration,
-- the merge-rule setter and the search index trigger need, and holds no
-- write privilege on the Episodes, the Versions, the Citations, the
-- Contributions or the statement tables. The eraser role reads and deletes
-- what Erasure needs, and inserts and closes the rows its replay writes
-- (ADR-0009, #83). No role holds a right on the identity sequences.

grant select on gnomon.schema_version to authenticated, gnomon_worker;

grant select on gnomon.tenancy
  to authenticated, gnomon_worker, gnomon_writer, gnomon_definer, gnomon_eraser;

grant select on gnomon.episode to authenticated, gnomon_worker, gnomon_writer;
grant insert (tenant_id, scope, id, content, reference_time, authority) on gnomon.episode to gnomon_writer;
grant select, delete on gnomon.episode to gnomon_eraser;
-- `select ... for update` needs an UPDATE right on some column, and an
-- Erasure locks the rows of the Episodes it erases. No Erasure updates one.
grant update (content) on gnomon.episode to gnomon_eraser;

grant select on gnomon.kind to authenticated, gnomon_worker, gnomon_writer, gnomon_eraser;
grant select, insert on gnomon.kind to gnomon_definer;

grant select on gnomon.kind_rule to authenticated, gnomon_worker, gnomon_writer, gnomon_eraser;
grant select, insert on gnomon.kind_rule to gnomon_definer;

grant select on gnomon.node to authenticated, gnomon_worker, gnomon_writer, gnomon_definer;
grant select, delete on gnomon.node to gnomon_eraser;
grant insert (tenant_id, scope, id, kind) on gnomon.node to gnomon_writer;

grant select on gnomon.edge to authenticated, gnomon_worker, gnomon_writer, gnomon_definer;
grant select, delete on gnomon.edge to gnomon_eraser;
grant insert (tenant_id, scope, id, kind, source_id, target_id) on gnomon.edge to gnomon_writer;

grant select on gnomon.statement to authenticated, gnomon_worker, gnomon_writer;
grant select, insert, delete on gnomon.statement to gnomon_eraser;
grant insert (tenant_id, scope, node_id, edge_id, episode_id, kind, version_id, valid_from, valid_from_precision, valid_to, valid_to_precision, episode_reference_time, episode_recorded_at, retracts_episode) on gnomon.statement to gnomon_writer;

grant select on gnomon.contribution to authenticated, gnomon_worker, gnomon_writer;
grant select, insert, delete on gnomon.contribution to gnomon_eraser;
grant insert (tenant_id, scope, node_id, episode_id, property, value, place, valid_from, valid_from_precision, valid_to, valid_to_precision, episode_reference_time, episode_recorded_at, edge_id, statement_id) on gnomon.contribution to gnomon_writer;

grant select on gnomon.version to authenticated, gnomon_worker, gnomon_definer;
grant select, delete on gnomon.version to gnomon_writer;
grant select, insert, delete on gnomon.version to gnomon_eraser;
grant insert (tenant_id, scope, id, node_id, props, valid_from, valid_from_precision, valid_to, valid_to_precision, merge_rule, edge_id) on gnomon.version to gnomon_writer;
grant update (recorded_to) on gnomon.version to gnomon_writer, gnomon_eraser;

grant select on gnomon.citation to authenticated, gnomon_worker;
grant select, delete on gnomon.citation to gnomon_writer;
grant select, insert, delete on gnomon.citation to gnomon_eraser;
grant insert (tenant_id, scope, version_id, episode_id, property, place, episode_reference_time, episode_recorded_at) on gnomon.citation to gnomon_writer;

grant select on gnomon.bound_statement to authenticated, gnomon_worker, gnomon_writer;
grant select, delete on gnomon.bound_statement to gnomon_eraser;
grant insert (tenant_id, scope, node_id, episode_id, contribution_id, side, bound_at, bound_precision, episode_reference_time, episode_recorded_at, edge_id) on gnomon.bound_statement to gnomon_writer;

grant select on gnomon.kind_merge to authenticated, gnomon_worker, gnomon_writer, gnomon_eraser;
grant select, insert on gnomon.kind_merge to gnomon_definer;

grant select on gnomon.sentence to authenticated, gnomon_worker, gnomon_writer, gnomon_definer;
grant select, delete on gnomon.sentence to gnomon_eraser;
grant insert (tenant_id, scope, version_id, sentence) on gnomon.sentence to gnomon_writer;
grant update (sentence) on gnomon.sentence to gnomon_writer;

grant select on gnomon.episode_retraction to authenticated, gnomon_worker, gnomon_writer;
grant select, delete on gnomon.episode_retraction to gnomon_eraser;
grant insert (tenant_id, episode_id) on gnomon.episode_retraction to gnomon_writer;

grant select on gnomon.stage_record to authenticated, gnomon_worker;
grant select, insert on gnomon.stage_record to gnomon_writer;
grant select, delete on gnomon.stage_record to gnomon_eraser;
grant update (status, attempts, last_error_code, last_error_status, retry_at, lease_until, updated_at) on gnomon.stage_record to gnomon_writer;

grant select on gnomon.stage_usage to authenticated, gnomon_worker;
grant select, insert on gnomon.stage_usage to gnomon_writer;
grant select, delete on gnomon.stage_usage to gnomon_eraser;
grant update (calls, input_tokens, output_tokens, cost_usd) on gnomon.stage_usage to gnomon_writer;

grant select on gnomon.search_token to authenticated, gnomon_worker;
grant select, insert, delete on gnomon.search_token to gnomon_definer;
grant select, delete on gnomon.search_token to gnomon_eraser;

grant select on gnomon.search_token_frequency to authenticated, gnomon_worker;
grant select, insert, delete, update on gnomon.search_token_frequency to gnomon_definer;

grant select on gnomon.search_scope_length to authenticated, gnomon_worker;
grant select, insert, delete, update on gnomon.search_scope_length to gnomon_definer;

grant select on gnomon.search_count_delta to authenticated, gnomon_worker;
grant select, insert, delete, update on gnomon.search_count_delta to gnomon_definer;

grant select on gnomon.embedding to authenticated, gnomon_worker;
grant select, insert on gnomon.embedding to gnomon_writer;
grant select, delete on gnomon.embedding to gnomon_definer, gnomon_eraser;

grant select on gnomon.embedding_bit to authenticated, gnomon_worker;
grant select, insert on gnomon.embedding_bit to gnomon_writer;
grant select, delete on gnomon.embedding_bit to gnomon_definer, gnomon_eraser;
grant update (text_hash) on gnomon.embedding_bit to gnomon_writer;

grant select on gnomon.summary to authenticated, gnomon_worker, gnomon_definer;
grant select, delete on gnomon.summary to gnomon_writer, gnomon_eraser;
grant insert (tenant_id, scope, node_id, content, as_of_valid) on gnomon.summary to gnomon_writer;
grant update (recorded_to) on gnomon.summary to gnomon_writer;

grant select on gnomon.summary_citation to authenticated, gnomon_worker;
grant select, delete on gnomon.summary_citation to gnomon_writer, gnomon_eraser;
grant insert (tenant_id, scope, summary_id, episode_id, property, place, episode_reference_time, episode_recorded_at) on gnomon.summary_citation to gnomon_writer;

grant select on gnomon.community to authenticated, gnomon_worker, gnomon_definer;
grant select, delete on gnomon.community to gnomon_writer, gnomon_eraser;
grant insert (tenant_id, scope, content, as_of_valid) on gnomon.community to gnomon_writer;
grant update (recorded_to) on gnomon.community to gnomon_writer;

grant select on gnomon.community_member to authenticated, gnomon_worker;
grant select, delete on gnomon.community_member to gnomon_writer, gnomon_eraser;
grant insert (tenant_id, scope, community_id, node_id) on gnomon.community_member to gnomon_writer;

grant select on gnomon.community_citation to authenticated, gnomon_worker;
grant select, delete on gnomon.community_citation to gnomon_writer, gnomon_eraser;
grant insert (tenant_id, scope, community_id, episode_id, property, place, episode_reference_time, episode_recorded_at) on gnomon.community_citation to gnomon_writer;

grant select on gnomon.node_name to authenticated, gnomon_worker, gnomon_writer;
grant select, insert, delete on gnomon.node_name to gnomon_definer;
grant select, delete on gnomon.node_name to gnomon_eraser;
grant update (q) on gnomon.node_name to gnomon_writer;

grant select on gnomon.derived_search_token to authenticated, gnomon_worker;
grant select, insert, delete on gnomon.derived_search_token to gnomon_definer;
grant select, delete on gnomon.derived_search_token to gnomon_eraser;

grant select on gnomon.derived_embedding to authenticated, gnomon_worker;
grant select, insert on gnomon.derived_embedding to gnomon_writer;
grant select, delete on gnomon.derived_embedding to gnomon_eraser;

-- The functions. Callers execute the read functions and the helpers those
-- call as the invoker, and the write functions, which run as
-- `gnomon_writer`. Each definer role executes the helpers its own functions
-- call, the internal write functions among them, which no caller may run.
-- Kind registration, the merge-rule setter, Erasure and the search index
-- trigger are granted to no role: a deployment grants the first three to
-- the roles it chooses, and the trigger to none. The functions a deployment
-- may replace, `gnomon.tenant_id()` and the others of 0001, are granted to
-- each role they run as.
grant execute on function gnomon.embedding_dimension()
  to authenticated, gnomon_worker, gnomon_writer, gnomon_definer;
grant execute on function gnomon.uuid_v7()
  to gnomon_writer, gnomon_eraser;
grant execute on function gnomon.tenant_id()
  to authenticated, gnomon_worker, gnomon_writer, gnomon_definer, gnomon_eraser;
grant execute on function gnomon.allowed_scopes()
  to authenticated, gnomon_worker, gnomon_writer, gnomon_definer, gnomon_eraser;
grant execute on function gnomon.writable_scopes()
  to gnomon_writer, gnomon_eraser;
grant execute on function gnomon.user_id()
  to gnomon_writer;
grant execute on function gnomon.refuse_read_only_scope(text)
  to gnomon_writer, gnomon_eraser;
grant execute on function gnomon.default_tenant()
  to authenticated, gnomon_worker, gnomon_writer, gnomon_definer, gnomon_eraser;
grant execute on function gnomon.call_tenant(uuid)
  to authenticated, gnomon_worker;
grant execute on function gnomon.root_kinds()
  to authenticated, gnomon_worker, gnomon_writer, gnomon_definer;
grant execute on function gnomon.kind_profile_keywords()
  to gnomon_definer;
grant execute on function gnomon.json_pointer_token(text)
  to gnomon_definer;
grant execute on function gnomon.compile_kind_schema(jsonb, text, text)
  to gnomon_definer;
grant execute on function gnomon.compile_kind_property(jsonb, text)
  to gnomon_definer;
grant execute on function gnomon.kind_endpoints(uuid, text[], text[], text)
  to gnomon_definer;
grant execute on function gnomon.node_builtin_rules()
  to gnomon_writer;
grant execute on function gnomon.kind_merge_rule(uuid, text)
  to authenticated, gnomon_worker, gnomon_writer;
grant execute on function gnomon.edge_builtin_rules()
  to gnomon_writer, gnomon_eraser;
grant execute on function gnomon.kind_merge_rule_at(uuid, text, timestamptz)
  to gnomon_eraser;
grant execute on function gnomon.reserved_kinds()
  to authenticated, gnomon_worker, gnomon_writer, gnomon_definer;
grant execute on function gnomon.kind_lineage(uuid, text)
  to authenticated, gnomon_worker, gnomon_writer;
grant execute on function gnomon.read_kind(uuid, text)
  to authenticated, gnomon_worker;
grant execute on function gnomon.is_symmetric_kind(text)
  to authenticated, gnomon_worker, gnomon_writer;
grant execute on function gnomon.running_as_eraser()
  to gnomon_writer, gnomon_eraser;
grant execute on function gnomon.record_episode(uuid, text, text, timestamptz, integer)
  to authenticated, gnomon_worker;
grant execute on function gnomon.read_episode(uuid, uuid)
  to authenticated, gnomon_worker;
grant execute on function gnomon.check_node_value(uuid, text, text, jsonb)
  to gnomon_writer;
grant execute on function gnomon.node_lock_key(uuid, uuid)
  to gnomon_writer, gnomon_eraser;
grant execute on function gnomon.create_node(uuid, text, text)
  to authenticated, gnomon_worker;
grant execute on function gnomon.list_citations(uuid, uuid)
  to authenticated, gnomon_worker;
grant execute on function gnomon.period_start(timestamptz, gnomon.time_precision)
  to gnomon_writer;
grant execute on function gnomon.period_end(timestamptz, gnomon.time_precision)
  to authenticated, gnomon_worker, gnomon_writer;
grant execute on function gnomon.read_node(uuid, uuid, timestamptz, timestamptz)
  to authenticated, gnomon_worker;
grant execute on function gnomon.classify_bound(timestamptz, gnomon.time_precision, timestamptz, gnomon.time_precision)
  to gnomon_writer;
grant execute on function gnomon.check_edge_value(uuid, text, text, jsonb)
  to gnomon_writer;
grant execute on function gnomon.lock_for_write(uuid, uuid, boolean)
  to gnomon_writer;
grant execute on function gnomon.contribution_kind(uuid, uuid, uuid)
  to gnomon_writer;
grant execute on function gnomon.contribution_bound(uuid, bigint, text)
  to gnomon_writer;
grant execute on function gnomon.recompute_versions(uuid, uuid, boolean, timestamptz, timestamptz)
  to gnomon_writer;
grant execute on function gnomon.create_edge(uuid, text, text, uuid, uuid)
  to authenticated, gnomon_worker;
grant execute on function gnomon.state_bound_of(uuid, uuid, boolean, uuid, uuid, text, timestamptz, text)
  to gnomon_writer;
grant execute on function gnomon.state_bound(uuid, uuid, uuid, uuid, text, timestamptz, text)
  to authenticated, gnomon_worker;
grant execute on function gnomon.state_edge_bound(uuid, uuid, uuid, uuid, text, timestamptz, text)
  to authenticated, gnomon_worker;
grant execute on function gnomon.uncertain_at(timestamptz, timestamptz, gnomon.time_precision)
  to authenticated, gnomon_worker;
grant execute on function gnomon.read_edge(uuid, uuid, timestamptz, timestamptz)
  to authenticated, gnomon_worker;
grant execute on function gnomon.read_nodes(uuid, uuid[], timestamptz, timestamptz)
  to authenticated, gnomon_worker;
grant execute on function gnomon.traverse(uuid, uuid, timestamptz, timestamptz, text, text[])
  to authenticated, gnomon_worker;
grant execute on function gnomon.write_patch_as(uuid, uuid, boolean, uuid, timestamptz, text, timestamptz, text, jsonb, jsonb, text)
  to gnomon_writer;
grant execute on function gnomon.write_patch(uuid, uuid, boolean, uuid, timestamptz, text, timestamptz, text, jsonb, jsonb)
  to gnomon_writer;
grant execute on function gnomon.write_edge(uuid, uuid, uuid, timestamptz, text, timestamptz, text, jsonb, jsonb)
  to authenticated, gnomon_worker;
grant execute on function gnomon.write_node(uuid, uuid, uuid, timestamptz, text, timestamptz, text, jsonb, jsonb)
  to authenticated, gnomon_worker;
grant execute on function gnomon.correct_node(uuid, uuid, uuid, timestamptz, text, timestamptz, text, jsonb, jsonb)
  to authenticated, gnomon_worker;
grant execute on function gnomon.correct_edge(uuid, uuid, uuid, timestamptz, text, timestamptz, text, jsonb, jsonb)
  to authenticated, gnomon_worker;
grant execute on function gnomon.list_statements(uuid, uuid)
  to authenticated, gnomon_worker;
grant execute on function gnomon.write_ending_as(uuid, uuid, boolean, uuid, text, uuid, timestamptz, text, boolean)
  to gnomon_writer;
grant execute on function gnomon.write_ending(uuid, uuid, boolean, uuid, text, uuid, timestamptz, text)
  to gnomon_writer;
grant execute on function gnomon.retract_version(uuid, uuid, boolean, uuid, uuid)
  to authenticated, gnomon_worker;
grant execute on function gnomon.end_validity(uuid, uuid, boolean, uuid, timestamptz, text)
  to authenticated, gnomon_worker;
grant execute on function gnomon.retract_episode(uuid, uuid)
  to authenticated, gnomon_worker;
grant execute on function gnomon.merge_versions(uuid, text, uuid, boolean, timestamptz, timestamptz, timestamptz, text, boolean)
  to gnomon_writer, gnomon_eraser;
grant execute on function gnomon.stage_pending(uuid, uuid, text)
  to authenticated, gnomon_worker;
grant execute on function gnomon.claim_stage(uuid, uuid, interval)
  to authenticated, gnomon_worker;
grant execute on function gnomon.finish_stage(uuid, uuid, text, integer, gnomon.stage_status, text, integer, interval)
  to authenticated, gnomon_worker;
grant execute on function gnomon.add_stage_usage(uuid, uuid, text, text, integer, bigint, bigint, numeric)
  to authenticated, gnomon_worker;
grant execute on function gnomon.read_stage_records(uuid, uuid)
  to authenticated, gnomon_worker;
grant execute on function gnomon.read_stage_usage(uuid, uuid)
  to authenticated, gnomon_worker;
grant execute on function gnomon.derived_citations(uuid, text, uuid[], text[], text[])
  to gnomon_writer;
grant execute on function gnomon.check_write_scope(uuid, text)
  to gnomon_writer;
grant execute on function gnomon.write_summary(uuid, text, uuid, text, timestamptz, uuid[], text[], text[])
  to authenticated, gnomon_worker;
grant execute on function gnomon.write_community(uuid, text, uuid[], uuid[], text, timestamptz, uuid[], text[], text[])
  to authenticated, gnomon_worker;
grant execute on function gnomon.read_summaries(uuid, uuid, timestamptz, timestamptz)
  to authenticated, gnomon_worker;
grant execute on function gnomon.read_communities(uuid, uuid, timestamptz, timestamptz)
  to authenticated, gnomon_worker;
grant execute on function gnomon.kind_search(uuid, text)
  to gnomon_definer;
grant execute on function gnomon.search_text_tokens(uuid, uuid[])
  to gnomon_definer;
grant execute on function gnomon.index_search_text(uuid, uuid[], boolean)
  to gnomon_definer;
grant execute on function gnomon.search_full_text(uuid, text, timestamptz, timestamptz, integer)
  to authenticated, gnomon_worker;
grant execute on function gnomon.sentence_text_hash(text)
  to authenticated, gnomon_worker, gnomon_writer, gnomon_definer, gnomon_eraser;
grant execute on function gnomon.embedding_vector(real[])
  to authenticated, gnomon_worker, gnomon_writer;
grant execute on function gnomon.store_embedding(uuid, uuid, text, real[])
  to authenticated, gnomon_worker;
grant execute on function gnomon.search_vector(uuid, real[], timestamptz, timestamptz, integer)
  to authenticated, gnomon_worker;
grant execute on function gnomon.node_names(uuid, uuid[])
  to gnomon_definer;
grant execute on function gnomon.index_node_names(uuid, uuid[])
  to gnomon_definer;
grant execute on function gnomon.resolve_full_text(uuid, text, timestamptz, integer)
  to authenticated, gnomon_worker;
grant execute on function gnomon.store_name_embedding(uuid, uuid, text, real[])
  to authenticated, gnomon_worker;
grant execute on function gnomon.resolve_vector(uuid, real[], timestamptz, integer)
  to authenticated, gnomon_worker;
grant execute on function gnomon.resolve(uuid, text, real[], timestamptz, integer)
  to authenticated, gnomon_worker;
grant execute on function gnomon.derived_text_tokens(uuid, text, uuid[])
  to gnomon_definer;
grant execute on function gnomon.search_query_words(uuid, text)
  to authenticated, gnomon_worker;
grant execute on function gnomon.search_full_text_derived(uuid, text, timestamptz, timestamptz, integer)
  to authenticated, gnomon_worker;
grant execute on function gnomon.search_full_text_branch(uuid, text, timestamptz, timestamptz, integer)
  to authenticated, gnomon_worker;
grant execute on function gnomon.store_derived_embedding(uuid, text, uuid, real[])
  to gnomon_writer;
grant execute on function gnomon.store_summary_embedding(uuid, uuid, real[])
  to authenticated, gnomon_worker;
grant execute on function gnomon.store_community_embedding(uuid, uuid, real[])
  to authenticated, gnomon_worker;
grant execute on function gnomon.search_vector_derived(uuid, real[], timestamptz, timestamptz, integer)
  to authenticated, gnomon_worker;
grant execute on function gnomon.search_vector_branch(uuid, real[], timestamptz, timestamptz, integer)
  to authenticated, gnomon_worker;
grant execute on function gnomon.search_mentions(uuid, uuid[], text[], uuid[], text[], timestamptz, timestamptz)
  to authenticated, gnomon_worker;
grant execute on function gnomon.search_connectedness(uuid, uuid[], uuid[], uuid[], timestamptz, timestamptz)
  to authenticated, gnomon_worker;
grant execute on function gnomon.neighbourhood_edges(uuid, uuid[], timestamptz, timestamptz)
  to authenticated, gnomon_worker;
grant execute on function gnomon.search_neighbourhood(uuid, uuid[], integer, timestamptz, timestamptz, integer)
  to authenticated, gnomon_worker;
grant execute on function gnomon.search(uuid, text, real[], timestamptz, timestamptz, uuid[], integer, double precision, double precision, double precision, double precision, double precision, integer)
  to authenticated, gnomon_worker;
grant execute on function gnomon.store_sentence(uuid, uuid, text)
  to authenticated, gnomon_worker;
grant execute on function gnomon.list_gaps(uuid, text, text, uuid, text, integer)
  to authenticated, gnomon_worker;
grant execute on function gnomon.erase_version_text(uuid, uuid[])
  to gnomon_eraser;
grant execute on function gnomon.erase_episode_rows(uuid, uuid)
  to gnomon_eraser;
grant execute on function gnomon.replay_versions(uuid, text, uuid, boolean, text, timestamptz, jsonb)
  to gnomon_eraser;
grant execute on function gnomon.erase_empty_nodes(uuid, uuid[])
  to gnomon_eraser;
grant execute on function gnomon.erase_versioned_rows(uuid, uuid[])
  to gnomon_eraser;
grant execute on function gnomon.erase_graph(uuid, text, uuid[], uuid[])
  to gnomon_eraser;
grant execute on function gnomon.erasure_check_tenant(uuid)
  to gnomon_eraser;
grant execute on function gnomon.erase_summaries_and_communities(uuid, uuid[], uuid[])
  to gnomon_eraser;
grant execute on function gnomon.list_node_episodes(uuid, uuid)
  to authenticated, gnomon_worker;

-- Hand the definer functions to their roles, after their grants are
-- revoked: the migration role may revoke them only while it owns the
-- function. Owning a function needs SET ROLE on the new owner, which a
-- CREATEROLE migration role may grant itself on a role it created. The owner
-- also needs CREATE on the schema. Both are held only for this step.
--
--   gnomon_writer   every function a caller writes through.
--   gnomon_definer  Kind registration, the merge-rule setter and the search
--                   index trigger.
--   gnomon_eraser   Erasure.
--
-- Every other function stays an invoker, and the last check refuses a
-- definer function none of the three roles owns.
do $$
declare
  r text;
  joined text[] := '{}';
  f regprocedure;
begin
  foreach r in array array['gnomon_definer', 'gnomon_writer', 'gnomon_eraser'] loop
    if not pg_has_role(current_user, r, 'SET') then
      execute format('grant %I to %I', r, current_user);
      joined := joined || r;
    end if;
    execute format('grant create on schema gnomon to %I', r);
  end loop;
  alter function gnomon.register_kind(uuid, text, text, text, jsonb, text[], text[], text[])
    owner to gnomon_definer;
  alter function gnomon.set_merge_rule(uuid, text, text) owner to gnomon_definer;
  alter function gnomon.erase_episode(uuid, uuid) owner to gnomon_eraser;
  alter function gnomon.erase_node(uuid, uuid) owner to gnomon_eraser;
  alter function gnomon.erase_edge(uuid, uuid) owner to gnomon_eraser;
  alter function gnomon.erase_scope(uuid, text) owner to gnomon_eraser;
  alter function gnomon.maintain_search_index() owner to gnomon_definer;
  for f in
    select p.oid::regprocedure from pg_catalog.pg_proc p
    where p.pronamespace = 'gnomon'::regnamespace
      and p.proname = any(array[
        'record_episode', 'create_node', 'create_edge', 'write_node', 'write_edge',
        'correct_node', 'correct_edge', 'state_bound', 'state_edge_bound', 'end_validity',
        'retract_version', 'retract_episode', 'stage_pending', 'claim_stage', 'finish_stage',
        'add_stage_usage', 'write_summary', 'write_community', 'store_embedding',
        'store_name_embedding', 'store_summary_embedding', 'store_community_embedding',
        'store_sentence'])
  loop
    execute format('alter function %s owner to gnomon_writer', f);
  end loop;
  foreach r in array array['gnomon_definer', 'gnomon_writer', 'gnomon_eraser'] loop
    execute format('revoke create on schema gnomon from %I', r);
  end loop;
  foreach r in array joined loop
    execute format('revoke %I from %I', r, current_user);
  end loop;
  if exists (
    select from pg_catalog.pg_proc p
    where p.pronamespace = 'gnomon'::regnamespace and p.prosecdef
      and p.proowner not in ('gnomon_definer'::regrole, 'gnomon_writer'::regrole,
                             'gnomon_eraser'::regrole)
  ) then
    raise exception 'a gnomon definer function is owned by none of gnomon_writer, '
      'gnomon_definer and gnomon_eraser';
  end if;
end
$$;

insert into gnomon.schema_version (version, name) values (6, '0006_grants');
