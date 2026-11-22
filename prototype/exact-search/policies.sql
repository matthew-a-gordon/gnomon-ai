-- PROTOTYPE, wipe me. Switchable RLS policies: select gnomon.proto_set_policy('any' | 'func' | 'inline').
create or replace function gnomon.proto_set_policy(mode text) returns void language plpgsql as $$
declare t text; cond text;
begin
  cond := case mode
    when 'any'    then 'tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes())'
    when 'func'   then 'tenant_id = gnomon.tenant_id() and gnomon.scope_allowed(scope)'
    when 'inline' then 'tenant_id = gnomon.tenant_id() and gnomon.scope_allowed_inl(scope)'
  end;
  foreach t in array array['emb','emb_half','emb_bit','ver','episode','citation','tok','name_item','name_bit'] loop
    execute format('alter table gnomon.%I enable row level security', t);
    execute format('drop policy if exists p_all on gnomon.%I', t);
    execute format('create policy p_all on gnomon.%I for all to gnomon_app using (%s)', t, cond);
  end loop;
  foreach t in array array['tok_df','tenant_stats'] loop
    execute format('alter table gnomon.%I enable row level security', t);
    execute format('drop policy if exists p_all on gnomon.%I', t);
    execute format('create policy p_all on gnomon.%I for all to gnomon_app using (tenant_id = gnomon.tenant_id())', t);
  end loop;
end $$;
grant select, insert, update on all tables in schema gnomon to gnomon_app;
grant execute on all functions in schema gnomon to gnomon_app;
select gnomon.proto_set_policy('any');
