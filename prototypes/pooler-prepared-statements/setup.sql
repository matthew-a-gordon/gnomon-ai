-- PROTOTYPE, throwaway. Scratch objects the harness expects (run as a superuser or database owner).
-- Rename "app" to the login role the pooler connects as (the harness reads PROTO_BASE_ROLE, default app).
create role authenticated nologin;   -- skip if it already exists
create role service_a nologin;
create role app login password 'proto';
grant authenticated, service_a to app;
create table public.proto_items(id int primary key, tenant text);
insert into public.proto_items select g, 't'||(g%10) from generate_series(1,100) g;
grant select on public.proto_items to authenticated, service_a, app;
