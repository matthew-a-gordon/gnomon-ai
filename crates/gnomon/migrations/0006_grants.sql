-- gnomon 0006: grants. What each role may do on the schema, its tables and
-- its functions, and the hand-over of the definer functions to
-- `gnomon_definer` (ADR-0009).

-- Grants. First every grant a default privilege made on the schema and on
-- its functions and tables is revoked, so each holds only the grants below.
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
    where c.relnamespace = 'gnomon'::regnamespace and c.relkind <> 'S' and a.grantee <> c.relowner
  loop
    execute format(
      'revoke all on %s from %s', g.object,
      case when g.grantee = 0 then 'public' else g.grantee::regrole::text end);
  end loop;
end
$$;
revoke all on schema gnomon from public;
revoke all on all functions in schema gnomon from public;

grant usage on schema gnomon to authenticated, gnomon_worker, gnomon_definer;

grant select on gnomon.schema_version to authenticated, gnomon_worker;

grant select on gnomon.tenancy to authenticated, gnomon_worker, gnomon_definer;

grant select, insert on gnomon.episode to authenticated, gnomon_worker;
grant select, delete on gnomon.episode to gnomon_definer;
grant update (content) on gnomon.episode to gnomon_definer;

grant select on gnomon.kind to authenticated, gnomon_worker;
grant select, insert on gnomon.kind to gnomon_definer;

grant select on gnomon.kind_rule to authenticated, gnomon_worker;
grant select, insert on gnomon.kind_rule to gnomon_definer;

grant select on gnomon.node to authenticated, gnomon_worker;
grant select, delete on gnomon.node to gnomon_definer;
grant insert (tenant_id, scope, id, kind) on gnomon.node to authenticated, gnomon_worker;

grant select on gnomon.edge to authenticated, gnomon_worker;
grant select, delete on gnomon.edge to gnomon_definer;
grant insert (tenant_id, scope, id, kind, source_id, target_id) on gnomon.edge to authenticated, gnomon_worker;

grant select on gnomon.statement to authenticated, gnomon_worker;
grant select, insert, delete on gnomon.statement to gnomon_definer;
grant insert (tenant_id, scope, node_id, edge_id, episode_id, kind, version_id, valid_from, valid_from_precision, valid_to, valid_to_precision, episode_reference_time, episode_recorded_at, retracts_episode) on gnomon.statement to authenticated, gnomon_worker;

grant select on gnomon.contribution to authenticated, gnomon_worker;
grant select, insert, delete on gnomon.contribution to gnomon_definer;
grant insert (tenant_id, scope, node_id, episode_id, property, value, place, valid_from, valid_from_precision, valid_to, valid_to_precision, episode_reference_time, episode_recorded_at, edge_id, statement_id) on gnomon.contribution to authenticated, gnomon_worker;

grant select, delete on gnomon.version to authenticated, gnomon_worker;
grant select, insert, delete on gnomon.version to gnomon_definer;
grant insert (tenant_id, scope, id, node_id, props, valid_from, valid_from_precision, valid_to, valid_to_precision, merge_rule, edge_id) on gnomon.version to authenticated, gnomon_worker;
grant update (recorded_to) on gnomon.version to authenticated, gnomon_worker, gnomon_definer;

grant select, delete on gnomon.citation to authenticated, gnomon_worker;
grant select, insert, delete on gnomon.citation to gnomon_definer;
grant insert (tenant_id, scope, version_id, episode_id, property, place, episode_reference_time, episode_recorded_at) on gnomon.citation to authenticated, gnomon_worker;

grant select on gnomon.bound_statement to authenticated, gnomon_worker;
grant select, delete on gnomon.bound_statement to gnomon_definer;
grant insert (tenant_id, scope, node_id, episode_id, contribution_id, side, bound_at, bound_precision, episode_reference_time, episode_recorded_at, edge_id) on gnomon.bound_statement to authenticated, gnomon_worker;

grant select on gnomon.kind_merge to authenticated, gnomon_worker;
grant select, insert on gnomon.kind_merge to gnomon_definer;

grant select on gnomon.sentence to authenticated, gnomon_worker;
grant select, delete on gnomon.sentence to gnomon_definer;
grant insert (tenant_id, scope, version_id, sentence) on gnomon.sentence to authenticated, gnomon_worker;
grant update (sentence) on gnomon.sentence to authenticated, gnomon_worker;

grant select on gnomon.episode_retraction to authenticated, gnomon_worker;
grant select, delete on gnomon.episode_retraction to gnomon_definer;
grant insert (tenant_id, episode_id) on gnomon.episode_retraction to authenticated, gnomon_worker;

grant select, insert, delete on gnomon.erasure_reach to gnomon_definer;

grant select, insert on gnomon.stage_record to authenticated, gnomon_worker;
grant select, delete on gnomon.stage_record to gnomon_definer;
grant update (status, attempts, last_error_code, last_error_status, retry_at, lease_until, updated_at) on gnomon.stage_record to authenticated, gnomon_worker;

grant select, insert on gnomon.stage_usage to authenticated, gnomon_worker;
grant select, delete on gnomon.stage_usage to gnomon_definer;
grant update (calls, input_tokens, output_tokens, cost_usd) on gnomon.stage_usage to authenticated, gnomon_worker;

grant select on gnomon.search_token to authenticated, gnomon_worker;
grant select, insert, delete on gnomon.search_token to gnomon_definer;

grant select on gnomon.search_token_frequency to authenticated, gnomon_worker;
grant select, insert, delete, update on gnomon.search_token_frequency to gnomon_definer;

grant select on gnomon.search_scope_length to authenticated, gnomon_worker;
grant select, insert, delete, update on gnomon.search_scope_length to gnomon_definer;

grant select on gnomon.search_count_delta to authenticated, gnomon_worker;
grant select, insert, delete, update on gnomon.search_count_delta to gnomon_definer;

grant select, insert on gnomon.embedding to authenticated, gnomon_worker;
grant select, delete on gnomon.embedding to gnomon_definer;

grant select, insert on gnomon.embedding_bit to authenticated, gnomon_worker;
grant select, delete on gnomon.embedding_bit to gnomon_definer;
grant update (text_hash) on gnomon.embedding_bit to authenticated, gnomon_worker;

grant select, delete on gnomon.summary to authenticated, gnomon_worker, gnomon_definer;
grant insert (tenant_id, scope, node_id, content, as_of_valid) on gnomon.summary to authenticated, gnomon_worker;
grant update (recorded_to) on gnomon.summary to authenticated, gnomon_worker;

grant select, delete on gnomon.summary_citation to authenticated, gnomon_worker, gnomon_definer;
grant insert (tenant_id, scope, summary_id, episode_id, property, place, episode_reference_time, episode_recorded_at) on gnomon.summary_citation to authenticated, gnomon_worker;

grant select, delete on gnomon.community to authenticated, gnomon_worker, gnomon_definer;
grant insert (tenant_id, scope, content, as_of_valid) on gnomon.community to authenticated, gnomon_worker;
grant update (recorded_to) on gnomon.community to authenticated, gnomon_worker;

grant select, delete on gnomon.community_member to authenticated, gnomon_worker, gnomon_definer;
grant insert (tenant_id, scope, community_id, node_id) on gnomon.community_member to authenticated, gnomon_worker;

grant select, delete on gnomon.community_citation to authenticated, gnomon_worker, gnomon_definer;
grant insert (tenant_id, scope, community_id, episode_id, property, place, episode_reference_time, episode_recorded_at) on gnomon.community_citation to authenticated, gnomon_worker;

grant select on gnomon.node_name to authenticated, gnomon_worker;
grant select, insert, delete on gnomon.node_name to gnomon_definer;
grant update (q) on gnomon.node_name to authenticated, gnomon_worker;

grant select on gnomon.derived_search_token to authenticated, gnomon_worker;
grant select, insert, delete on gnomon.derived_search_token to gnomon_definer;

grant select, insert on gnomon.derived_embedding to authenticated, gnomon_worker;
grant select, delete on gnomon.derived_embedding to gnomon_definer;

grant execute on function gnomon.embedding_dimension()
  to authenticated, gnomon_worker;
grant execute on function gnomon.uuid_v7()
  to authenticated, gnomon_worker, gnomon_definer;
grant execute on function gnomon.tenant_id()
  to authenticated, gnomon_worker, gnomon_definer;
grant execute on function gnomon.allowed_scopes()
  to authenticated, gnomon_worker, gnomon_definer;
grant execute on function gnomon.default_tenant()
  to authenticated, gnomon_worker, gnomon_definer;
grant execute on function gnomon.call_tenant(uuid)
  to authenticated, gnomon_worker;
grant execute on function gnomon.root_kinds()
  to authenticated, gnomon_worker, gnomon_definer;
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
  to authenticated, gnomon_worker;
grant execute on function gnomon.kind_merge_rule(uuid, text)
  to authenticated, gnomon_worker;
grant execute on function gnomon.edge_builtin_rules()
  to authenticated, gnomon_worker, gnomon_definer;
grant execute on function gnomon.kind_merge_rule_at(uuid, text, timestamptz)
  to gnomon_definer;
grant execute on function gnomon.reserved_kinds()
  to authenticated, gnomon_worker, gnomon_definer;
grant execute on function gnomon.kind_lineage(uuid, text)
  to authenticated, gnomon_worker;
grant execute on function gnomon.read_kind(uuid, text)
  to authenticated, gnomon_worker;
grant execute on function gnomon.is_symmetric_kind(text)
  to authenticated, gnomon_worker;
grant execute on function gnomon.running_as_definer()
  to authenticated, gnomon_worker, gnomon_definer;
grant execute on function gnomon.record_episode(uuid, text, text, timestamptz, integer)
  to authenticated, gnomon_worker;
grant execute on function gnomon.read_episode(uuid, uuid)
  to authenticated, gnomon_worker;
grant execute on function gnomon.check_node_value(uuid, text, text, jsonb)
  to authenticated, gnomon_worker;
grant execute on function gnomon.instant_or_unknown(timestamptz)
  to authenticated, gnomon_worker, gnomon_definer;
grant execute on function gnomon.node_lock_key(uuid, uuid)
  to authenticated, gnomon_worker, gnomon_definer;
grant execute on function gnomon.lock_node_for_write(uuid, uuid)
  to authenticated, gnomon_worker;
grant execute on function gnomon.create_node(uuid, text, text)
  to authenticated, gnomon_worker;
grant execute on function gnomon.list_citations(uuid, uuid)
  to authenticated, gnomon_worker;
grant execute on function gnomon.period_start(timestamptz, gnomon.time_precision)
  to authenticated, gnomon_worker;
grant execute on function gnomon.period_end(timestamptz, gnomon.time_precision)
  to authenticated, gnomon_worker;
grant execute on function gnomon.read_node(uuid, uuid, timestamptz, timestamptz)
  to authenticated, gnomon_worker;
grant execute on function gnomon.classify_bound(timestamptz, gnomon.time_precision, timestamptz, gnomon.time_precision)
  to authenticated, gnomon_worker;
grant execute on function gnomon.check_edge_value(uuid, text, text, jsonb)
  to authenticated, gnomon_worker;
grant execute on function gnomon.lock_for_write(uuid, uuid, boolean)
  to authenticated, gnomon_worker;
grant execute on function gnomon.contribution_kind(uuid, uuid, uuid)
  to authenticated, gnomon_worker;
grant execute on function gnomon.contribution_bound(uuid, bigint, text)
  to authenticated, gnomon_worker;
grant execute on function gnomon.recompute_versions(uuid, uuid, boolean, timestamptz, timestamptz)
  to authenticated, gnomon_worker;
grant execute on function gnomon.recompute_node(uuid, uuid, timestamptz, timestamptz)
  to authenticated, gnomon_worker;
grant execute on function gnomon.create_edge(uuid, text, text, uuid, uuid)
  to authenticated, gnomon_worker;
grant execute on function gnomon.state_bound_of(uuid, uuid, boolean, uuid, uuid, text, timestamptz, text)
  to authenticated, gnomon_worker;
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
  to authenticated, gnomon_worker;
grant execute on function gnomon.write_patch(uuid, uuid, boolean, uuid, timestamptz, text, timestamptz, text, jsonb, jsonb)
  to authenticated, gnomon_worker;
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
  to authenticated, gnomon_worker;
grant execute on function gnomon.write_ending(uuid, uuid, boolean, uuid, text, uuid, timestamptz, text)
  to authenticated, gnomon_worker;
grant execute on function gnomon.retract_version(uuid, uuid, boolean, uuid, uuid)
  to authenticated, gnomon_worker;
grant execute on function gnomon.end_validity(uuid, uuid, boolean, uuid, timestamptz, text)
  to authenticated, gnomon_worker;
grant execute on function gnomon.retract_episode(uuid, uuid)
  to authenticated, gnomon_worker;
grant execute on function gnomon.merge_versions(uuid, text, uuid, boolean, timestamptz, timestamptz, timestamptz, text, boolean)
  to authenticated, gnomon_worker, gnomon_definer;
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
  to authenticated, gnomon_worker;
grant execute on function gnomon.check_write_scope(uuid, text)
  to authenticated, gnomon_worker;
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
  to authenticated, gnomon_worker, gnomon_definer;
grant execute on function gnomon.embedding_vector(real[])
  to authenticated, gnomon_worker;
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
  to authenticated, gnomon_worker;
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
  to gnomon_definer;
grant execute on function gnomon.erase_episode_rows(uuid, uuid)
  to gnomon_definer;
grant execute on function gnomon.replay_versions(uuid, text, uuid, boolean, text, timestamptz, jsonb)
  to gnomon_definer;
grant execute on function gnomon.erasure_reaches(uuid, uuid[])
  to gnomon_definer;
grant execute on function gnomon.erasure_reaches_version(uuid, uuid)
  to gnomon_definer;
grant execute on function gnomon.erase_versioned_rows(uuid, uuid[])
  to gnomon_definer;
grant execute on function gnomon.erase_graph(uuid, text, uuid[], uuid[])
  to gnomon_definer;
grant execute on function gnomon.erasure_check_tenant(uuid)
  to gnomon_definer;
grant execute on function gnomon.erasure_reaches_text(uuid, text, text)
  to gnomon_definer;
grant execute on function gnomon.erasure_reaches_summary(uuid, uuid)
  to gnomon_definer;
grant execute on function gnomon.erase_summaries_and_communities(uuid, uuid[], uuid[])
  to gnomon_definer;

-- The range type's constructor, which the exclusion constraint on
-- `gnomon.version` calls with the caller's rights.
grant execute on function gnomon.uuid_range(uuid, uuid, text)
  to authenticated, gnomon_worker, gnomon_definer;

-- Hand the definer functions to the definer role, after their grants are
-- revoked: the migration role may revoke them only while it owns the
-- function. Owning a function needs SET ROLE on the new owner, which a
-- CREATEROLE migration role may grant itself on a role it created. The owner
-- also needs CREATE on the schema. Both are held only for this step.
do $$
declare
  joined boolean := not pg_has_role(current_user, 'gnomon_definer', 'SET');
begin
  if joined then
    execute format('grant gnomon_definer to %I', current_user);
  end if;
  grant create on schema gnomon to gnomon_definer;
  alter function gnomon.register_kind(uuid, text, text, text, jsonb, text[], text[], text[])
    owner to gnomon_definer;
  alter function gnomon.set_merge_rule(uuid, text, text) owner to gnomon_definer;
  alter function gnomon.erase_episode(uuid, uuid) owner to gnomon_definer;
  alter function gnomon.erase_node(uuid, uuid) owner to gnomon_definer;
  alter function gnomon.erase_edge(uuid, uuid) owner to gnomon_definer;
  alter function gnomon.erase_scope(uuid, text) owner to gnomon_definer;
  alter function gnomon.maintain_search_index() owner to gnomon_definer;
  revoke create on schema gnomon from gnomon_definer;
  if joined then
    execute format('revoke gnomon_definer from %I', current_user);
  end if;
end
$$;

insert into gnomon.schema_version (version, name) values (6, '0006_grants');
