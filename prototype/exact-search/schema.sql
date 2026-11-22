-- PROTOTYPE, wipe me. Tables and claim functions. Indexes and policies come after the load.
create extension if not exists vector with schema public;
drop schema if exists gnomon cascade;
create schema gnomon;
do $$ begin
  if not exists (select from pg_roles where rolname = 'gnomon_app') then create role gnomon_app nologin; end if;
end $$;
grant usage on schema public to gnomon_app;
grant usage on schema gnomon to gnomon_app;

-- Claims are read from session settings, as a consumer's deployment functions would.
create function gnomon.tenant_id() returns uuid language sql stable parallel safe set search_path = ''
as $$ select nullif(pg_catalog.current_setting('gnomon.tenant', true), '')::uuid $$;
create function gnomon.allowed_scopes() returns text[] language sql stable parallel safe set search_path = ''
as $$ select pg_catalog.string_to_array(pg_catalog.current_setting('gnomon.scopes', true), ',') $$;
-- Old form from ADR-0005, opaque to the planner (plpgsql is never inlined).
create function gnomon.scope_allowed(s text) returns boolean language plpgsql stable parallel safe set search_path = ''
as $$ begin return s = any(gnomon.allowed_scopes()); end $$;
-- Old form written as an inlinable SQL function (no SET clause), for comparison.
create function gnomon.scope_allowed_inl(s text) returns boolean language sql stable parallel safe
as $$ select s = any(string_to_array(current_setting('gnomon.scopes', true), ',')) $$;

-- One embedding per distinct text, keyed by a hash of the embedded text.
create table gnomon.emb (tenant_id uuid not null, scope text not null, text_hash bytea not null, embedding public.vector(1536) not null);
alter table gnomon.emb alter column embedding set storage plain;
create table gnomon.emb_half (tenant_id uuid not null, scope text not null, text_hash bytea not null, embedding public.halfvec(1536) not null);
alter table gnomon.emb_half alter column embedding set storage plain;
create table gnomon.emb_bit (tenant_id uuid not null, scope text not null, text_hash bytea not null, q bit(1536) not null);

-- Versions of Nodes and Edges, plus Summaries and Communities (kind n, e, s, c), all bi-temporal.
create table gnomon.ver (
  id bigint not null, tenant_id uuid not null, scope text not null, kind text not null,
  entity_id bigint not null, anchor_id bigint, src_id bigint, dst_id bigint,
  body text not null, text_hash bytea not null, doc_len int not null,
  valid_from timestamptz not null, valid_to timestamptz not null,
  recorded_from timestamptz not null, recorded_to timestamptz not null,
  fts tsvector generated always as (pg_catalog.to_tsvector('simple'::regconfig, body)) stored
);
create table gnomon.episode (id bigint not null, tenant_id uuid not null, scope text not null, recorded_from timestamptz not null);
create table gnomon.citation (tenant_id uuid not null, scope text not null, entity_id bigint not null, episode_id bigint not null);

-- Token table for full-text search, and per-Tenant BM25 statistics.
create table gnomon.tok (tenant_id uuid not null, scope text not null, token text not null, ver_id bigint not null, tf smallint not null, dl int not null);
create table gnomon.tok_df (tenant_id uuid not null, token text not null, df int not null);
create table gnomon.tenant_stats (tenant_id uuid not null, n_docs bigint not null, total_len bigint not null);

-- Node names and Aliases for Resolution. The embedding column is inline; name_bit is the narrow copy.
create table gnomon.name_item (id bigint not null, tenant_id uuid not null, scope text not null, entity_id bigint not null,
  is_alias boolean not null, recorded_from timestamptz not null, recorded_to timestamptz not null, embedding public.vector(1536) not null);
alter table gnomon.name_item alter column embedding set storage plain;
create table gnomon.name_bit (id bigint not null, tenant_id uuid not null, scope text not null, recorded_to timestamptz not null, q bit(1536) not null);
