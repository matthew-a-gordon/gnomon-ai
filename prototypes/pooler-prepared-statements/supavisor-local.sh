#!/usr/bin/env bash
# PROTOTYPE, throwaway. Starts local Supavisor on host port 56543 (transaction mode).
set -eu
cd "$(dirname "$0")"
# Fresh metadata every start: the vault key is random per run, so old encrypted tenant rows would not decrypt.
docker rm -f gnomon-proto-pooler-supavisor >/dev/null 2>&1 || true
docker exec gnomon-proto-pooler-pg psql -U postgres -c "drop database if exists _supabase with (force)"
docker exec gnomon-proto-pooler-pg psql -U postgres -c "create database _supabase"
docker exec gnomon-proto-pooler-pg psql -U postgres -d _supabase -c "create schema _supavisor"
docker rm -f gnomon-proto-pooler-supavisor >/dev/null 2>&1 || true
docker run -d --name gnomon-proto-pooler-supavisor --network gnomon-proto-pooler-net -p 56543:6543 \
  -v "$PWD/supavisor-pooler.exs:/etc/pooler/pooler.exs:ro" \
  -e PORT=4000 -e POSTGRES_PORT=5432 -e POSTGRES_DB=_supabase -e POSTGRES_PASSWORD=proto \
  -e DATABASE_URL="ecto://postgres:proto@gnomon-proto-pooler-pg:5432/_supabase" \
  -e CLUSTER_POSTGRES=true -e REGION=local -e ERL_AFLAGS="-proto_dist inet_tcp" \
  -e SECRET_KEY_BASE="$(openssl rand -hex 32)" -e VAULT_ENC_KEY="$(openssl rand -hex 16)" \
  -e API_JWT_SECRET="$(openssl rand -hex 16)" -e METRICS_JWT_SECRET="$(openssl rand -hex 16)" \
  -e DB_POOL_SIZE=5 ${EXTRA_ENV:-} \
  supabase/supavisor:2.10.1 \
  /bin/sh -c '/app/bin/migrate && /app/bin/supavisor eval "$(cat /etc/pooler/pooler.exs)" && /app/bin/server' >/dev/null
