#!/usr/bin/env bash
# Opens psql in a running test container, as a caller role with the claims the
# Store sets, with auto_explain on for nested statements, so each statement
# shows the plans of the statements inside the functions it calls. See
# docs/testing.md.
#
#   scripts/plans.sh [-r gnomon_worker|authenticated] [-t TENANT] [-s SCOPE]
#                    DATABASE [psql arguments...]
#
# The container is $GNOMON_TEST_CONTAINER_PREFIX-$GNOMON_TEST_IMAGE (default
# gnomon-test-pgvector), as the harness names it. The role defaults to
# gnomon_worker, and the Tenant and Scope to the performance data sets'. -t ''
# binds no Tenant, as a worker of a single-tenant deployment does. SQL piped
# to the script runs in the same session. Plans arrive as notices, on stderr.
set -euo pipefail

usage() {
  echo "usage: scripts/plans.sh [-r gnomon_worker|authenticated] [-t TENANT] [-s SCOPE] DATABASE [psql arguments...]" >&2
}

role=gnomon_worker
tenant=21000000-0000-4000-8000-000000000021
scope=perf
while getopts r:t:s:h option; do
  case "$option" in
    r) role="$OPTARG" ;;
    t) tenant="$OPTARG" ;;
    s) scope="$OPTARG" ;;
    h) usage; exit 0 ;;
    *) usage; exit 2 ;;
  esac
done
shift $((OPTIND - 1))
case "$role" in
  gnomon_worker|authenticated) ;;
  *) echo "-r takes a caller role, gnomon_worker or authenticated, not $role" >&2; exit 2 ;;
esac

container="${GNOMON_TEST_CONTAINER_PREFIX:-gnomon-test}-${GNOMON_TEST_IMAGE:-pgvector}"
if [ "$(docker inspect --format '{{.State.Running}}' "$container" 2>/dev/null)" != true ]; then
  echo "no container $container is running. The harness's running containers:" >&2
  docker ps --filter label=gnomon.test-db --format '  {{.Names}}' >&2
  echo "A run with GNOMON_TEST_IDLE_SECONDS set keeps its container up. See docs/testing.md." >&2
  exit 1
fi

if [ $# -eq 0 ]; then
  usage
  echo "the databases in $container:" >&2
  docker exec "$container" sh -c 'psql -X -h /var/run/postgresql -U "$GNOMON_ADMIN" -d postgres -tAc \
    "select datname from pg_database where not datistemplate order by 1"' >&2
  exit 2
fi
database="$1"
shift

# The claims, as JSON.
json_string() {
  local s=${1//\\/\\\\}
  printf '"%s"' "${s//\"/\\\"}"
}
claims="{\"role\":$(json_string "$role"),\"app_metadata\":{"
if [ -n "$tenant" ]; then claims+="\"tenant_id\":$(json_string "$tenant"),"; fi
claims+="\"scopes\":[$(json_string "$scope")]}}"

# The session's settings are connection options, so the server applies them as
# it opens each of psql's connections, whatever psql is told to skip, and
# opens none it cannot give the caller role. Only a superuser may preload
# auto_explain and set it.
options=
for setting in \
  auto_explain.log_min_duration=0 \
  auto_explain.log_analyze=on \
  auto_explain.log_buffers=on \
  auto_explain.log_nested_statements=on \
  auto_explain.log_level=notice \
  auto_explain.log_parameter_max_length=64 \
  client_min_messages=notice \
  "request.jwt.claims=$claims" \
  "role=$role"; do
  # A backslash keeps a space, or a backslash, inside its option.
  setting=${setting//\\/\\\\}
  options+=" -c ${setting// /\\ }"
done

echo "Role $role, claims $claims" >&2
terminal=(-i)
if [ -t 0 ] && [ -t 1 ]; then terminal=(-it); fi
# auto_explain joins the libraries the server already preloads in a session,
# which on the Supabase image is supautils.
# shellcheck disable=SC2016 # The container's shell expands these.
exec docker exec "${terminal[@]}" -e GNOMON_OPTIONS="$options" "$container" sh -c '
  database=$1
  shift
  libraries=$(psql -X -h /var/run/postgresql -U "$GNOMON_ADMIN" -d postgres -tAc \
    "show session_preload_libraries") || exit 1
  PGOPTIONS="-c session_preload_libraries=${libraries:+$libraries,}auto_explain$GNOMON_OPTIONS" \
    exec psql -h /var/run/postgresql -U "$GNOMON_ADMIN" -d "$database" "$@"
' sh "$database" "$@"
