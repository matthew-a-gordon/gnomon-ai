#!/usr/bin/env bash
# Test lane 4 (ADR-0020): the suite through PgBouncer and through Supavisor
# in transaction mode, at their default settings.
#
#   crates/gnomon/tests/support/pooler/run-lane-4.sh [extra cargo nextest arguments]
#
# LANE4_POOLERS picks the runs, in order (default: all three):
#
#   pgbouncer         PgBouncer in front of a plain pgvector/pgvector:pg17
#                     container, with unnamed statements, the Store's default.
#   pgbouncer-named   The same PgBouncer, with GNOMON_TEST_NAMED_STATEMENTS=1.
#                     PgBouncer 1.21 and later support named statements when
#                     max_prepared_statements is above 0. It has defaulted to
#                     200 since 1.24.
#   supavisor         The Supabase CLI local stack with only the database and
#                     Supavisor running, with unnamed statements.
#
# In every run GNOMON_TEST_DATABASE_URL points at the pooler, so the Store and
# every raw login-role connection go through it, and
# GNOMON_TEST_DIRECT_DATABASE_URL points at the server behind it, where the
# harness builds templates and creates each test's database. Each run prints
# the pooler's settings before the suite starts.
#
# The PgBouncer containers and their network are named with
# GNOMON_TEST_CONTAINER_PREFIX (default gnomon-t5), and publish on loopback
# with ports Docker picks. The CLI stack's project id is gnomon-t5, in
# supabase/config.toml beside this script. The CLI stack uses ports 56050 to 56059 only. The
# script removes what it started, and nothing else, whether the run passed or
# not. Needs Docker, npx, and the images edoburu/pgbouncer (pinned below),
# pgvector/pgvector:pg17 and supabase/supavisor:2.10.1, which the pinned CLI
# runs.
set -euo pipefail
cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"

name=${GNOMON_TEST_CONTAINER_PREFIX:-gnomon-t5}
project=gnomon-t5
workdir=crates/gnomon/tests/support/pooler
# PgBouncer 1.26.1. Pinned by digest, so a later pull cannot change the
# defaults a run uses.
pgbouncer_image=edoburu/pgbouncer@sha256:8638aa563607256d3c7440437534195f1066a92b68015d89ae9d6b935b444a6d
cli=(npx --yes supabase@3.2.0)
password=gnomon-test
poolers=${LANE4_POOLERS:-pgbouncer pgbouncer-named supavisor}

stop_pgbouncer() {
  docker rm -f "$name-pgbouncer" "$name-postgres" >/dev/null 2>&1 || true
  docker network rm "$name-net" >/dev/null 2>&1 || true
}
stop_supavisor() {
  "${cli[@]}" stop --workdir "$workdir" --project-id "$project" --no-backup >/dev/null 2>&1 || true
}
cleanup() {
  stop_pgbouncer
  stop_supavisor
}
trap cleanup EXIT

# psql from the pgvector image, on the lane's network, as a container of its
# own name.
psql_on() {
  local network=$1
  shift
  docker run --rm --name "$name-psql-$$" --network "$network" \
    -e PGPASSWORD="${PGPASSWORD:-$password}" pgvector/pgvector:pg17 psql -X "$@"
}

host_port() {
  docker port "$1" 5432/tcp | head -1 | sed 's/.*://'
}

wait_for() {
  local network=$1 conninfo=$2
  for _ in $(seq 1 120); do
    if psql_on "$network" "$conninfo" -tAc 'select 1' >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  echo "timed out waiting for $conninfo" >&2
  return 1
}

start_pgbouncer() {
  stop_pgbouncer
  docker network create "$name-net" >/dev/null
  docker run -d --name "$name-postgres" --network "$name-net" \
    -p 127.0.0.1::5432 --tmpfs /var/lib/postgresql/data \
    -e POSTGRES_PASSWORD="$password" pgvector/pgvector:pg17 >/dev/null
  wait_for "$name-net" "host=$name-postgres user=postgres dbname=postgres"
  # POOL_MODE is the one pooling setting the lane chooses. DB_* and
  # AUTH_TYPE only connect PgBouncer to the server: the image writes
  # `* = host=... auth_user=postgres`, so any database and any role with a
  # password reach the server through PgBouncer's default auth_query, and
  # scram-sha-256 stores the password in plain text in its userlist, which
  # the server's SCRAM authentication needs.
  docker run -d --name "$name-pgbouncer" --network "$name-net" \
    -p 127.0.0.1::5432 \
    -e DB_HOST="$name-postgres" -e DB_USER=postgres -e DB_PASSWORD="$password" \
    -e AUTH_TYPE=scram-sha-256 -e POOL_MODE=transaction \
    "$pgbouncer_image" >/dev/null
  wait_for "$name-net" "host=$name-pgbouncer user=postgres dbname=postgres"

  echo "== PgBouncer settings"
  psql_on "$name-net" "host=$name-pgbouncer user=postgres dbname=pgbouncer" -c 'show version'
  echo 'setting | value | default | changeable'
  psql_on "$name-net" "host=$name-pgbouncer user=postgres dbname=pgbouncer" -tA -F ' | ' -c 'show config' |
    grep -E '^(pool_mode|default_pool_size|min_pool_size|reserve_pool_size|max_client_conn|max_db_connections|max_user_connections|max_prepared_statements|server_reset_query|server_reset_query_always|server_check_query|server_idle_timeout|auth_type|auth_query|ignore_startup_parameters|track_extra_parameters) \| '
  psql_on "$name-net" "host=$name-postgres user=postgres dbname=postgres" -tAc 'select version()'

  export GNOMON_TEST_DATABASE_URL="postgres://postgres:$password@127.0.0.1:$(host_port "$name-pgbouncer")/postgres"
  export GNOMON_TEST_DIRECT_DATABASE_URL="postgres://postgres:$password@127.0.0.1:$(host_port "$name-postgres")/postgres"
  unset GNOMON_TEST_MIGRATION_ROLE GNOMON_TEST_MIGRATION_PASSWORD GNOMON_TEST_SUPAVISOR_TENANT
}

start_supavisor() {
  stop_supavisor
  local block='( sport >= :56050 and sport <= :56059 )'
  if ss -ltnH "$block" | grep -q .; then
    ss -ltnH "$block" >&2
    echo "a port in 56050 to 56059 is in use; lane 4's Supavisor needs the block free" >&2
    exit 1
  fi
  # The block is inside Linux's ephemeral port range, so a client connection
  # of the suite's last run can still hold one of its ports, in TIME_WAIT
  # for up to a minute. Docker cannot publish a port then.
  for _ in $(seq 1 90); do
    ss -tanH "$block" | grep -q . || break
    sleep 1
  done
  "${cli[@]}" start --workdir "$workdir"

  echo "== Supavisor settings"
  docker inspect "supabase_pooler_$project" --format '{{.Config.Image}}'
  # The tenant the CLI registers, without its password, and the pooler's
  # environment, without its secrets. No NAMED_PREPARED_STATEMENTS flag is
  # set, so named statements are off, Supavisor's default.
  docker exec "supabase_pooler_$project" cat /app/pooler_tenant.exs |
    sed -n '/^params = %{/,/^}/p' | sed -E 's/("db_password" => ")[^"]*/\1<redacted>/'
  docker inspect "supabase_pooler_$project" --format '{{range .Config.Env}}{{println .}}{{end}}' |
    grep -v -E 'KEY|SECRET|PASSWORD|DATABASE_URL|^PATH=|^LANG|^LC_' | grep .
  PGPASSWORD=postgres psql_on "supabase_network_$project" \
    "host=supabase_db_$project user=supabase_admin dbname=postgres" -tAc 'select version()'

  export GNOMON_TEST_DATABASE_URL="postgres://postgres:postgres@127.0.0.1:56053/postgres"
  export GNOMON_TEST_DIRECT_DATABASE_URL="postgres://supabase_admin:postgres@127.0.0.1:56052/postgres"
  export GNOMON_TEST_MIGRATION_ROLE=postgres
  export GNOMON_TEST_MIGRATION_PASSWORD=postgres
  # Supavisor reads its own tenant, not a gnomon Tenant, from the user name:
  # gnomon_test_login.pooler-dev.
  export GNOMON_TEST_SUPAVISOR_TENANT=pooler-dev
}

for run in $poolers; do
  case $run in
    pgbouncer)
      start_pgbouncer
      echo "== lane 4: the suite through PgBouncer, unnamed statements"
      GNOMON_TEST_NAMED_STATEMENTS=0 cargo nextest run --workspace "$@"
      ;;
    pgbouncer-named)
      start_pgbouncer
      echo "== lane 4: the suite through PgBouncer, named statements"
      GNOMON_TEST_NAMED_STATEMENTS=1 cargo nextest run --workspace "$@"
      ;;
    supavisor)
      stop_pgbouncer
      start_supavisor
      echo "== lane 4: the suite through Supavisor, unnamed statements"
      # The CLI stack's Postgres allows 100 connections, and Supavisor's own
      # metadata pool holds 27 of them. Each test database is a pool of its
      # own, with an auth_query connection beside its server connections,
      # and the harness connects straight to the server for setup, so a run
      # with nextest's default of one test per CPU exhausts the server. Eight
      # at a time stays near 60. Supavisor's settings stay at their defaults.
      GNOMON_TEST_NAMED_STATEMENTS=0 cargo nextest run --workspace \
        --test-threads "${LANE4_SUPAVISOR_TEST_THREADS:-8}" "$@"
      stop_supavisor
      ;;
    *)
      echo "LANE4_POOLERS: unknown run $run; use pgbouncer, pgbouncer-named or supavisor" >&2
      exit 2
      ;;
  esac
done
