# PROTOTYPE, throwaway. Registers one tenant that pools the scratch database in transaction mode.
{:ok, _} = Application.ensure_all_started(:supavisor)
{:ok, version} =
  case Supavisor.Repo.query!("select version()") do
    %{rows: [[ver]]} -> Supavisor.Helpers.parse_pg_version(ver)
    _ -> {:ok, nil}
  end
params = %{
  "external_id" => "proto",
  "db_host" => "gnomon-proto-pooler-pg",
  "db_port" => 5432,
  "db_database" => "proto",
  "require_user" => true,
  "default_max_clients" => 500,
  "default_pool_size" => 10,
  "default_parameter_status" => %{"server_version" => version},
  "users" => [%{"db_user" => "app", "db_password" => "proto", "mode_type" => "transaction", "pool_size" => 10, "is_manager" => true}]
}
if !Supavisor.Tenants.get_tenant_by_external_id("proto") do
  {:ok, _} = Supavisor.Tenants.create_tenant(params)
end
