#!/usr/bin/env bash
# Test lane 3 (ADR-0020): the Supabase CLI local stack.
#
#   crates/gnomon/tests/support/supabase/run-lane-3.sh [extra cargo nextest arguments]
#
# 1. Copies gnomon's migration files into the CLI project in this directory
#    under timestamped names, `<version>_gnomon_<name>.sql`, as a consumer
#    does, so `supabase start` applies them to the stack's `postgres`
#    database as `postgres`.
# 2. Runs the lane 3 tests: the CLI's migration history, and that PostgREST
#    reaches no gnomon table or function and exposes no schema that holds
#    what the files created.
# 3. Stops the stack, and only this stack, whether the run passed or not.
#
# The stack is named gnomon-t3 and uses ports 56030 to 56039 only, so it never
# touches another project's stack. Needs Docker and npx.
set -euo pipefail
cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"

project=gnomon-t3
workdir=crates/gnomon/tests/support/supabase
cli=(npx --yes supabase@3.2.0)

for port in $(seq 56030 56039); do
  if ss -ltnH "sport = :$port" | grep -q .; then
    echo "port $port is in use; lane 3 needs 56030 to 56039 free" >&2
    exit 1
  fi
done

stop() {
  "${cli[@]}" stop --workdir "$workdir" --project-id "$project" --no-backup >/dev/null 2>&1 || true
  rm -rf "$workdir/supabase/migrations"
}
trap stop EXIT

rm -rf "$workdir/supabase/migrations"
mkdir -p "$workdir/supabase/migrations"
n=0
for file in crates/gnomon/migrations/*.sql; do
  n=$((n + 1))
  cp "$file" "$workdir/supabase/migrations/$(printf '20250101%06d' "$n")_gnomon_$(basename "$file")"
done
"${cli[@]}" start --workdir "$workdir"

export GNOMON_TEST_DATABASE_URL=postgres://supabase_admin:postgres@127.0.0.1:56032/postgres
export GNOMON_TEST_MIGRATION_ROLE=postgres
export GNOMON_TEST_MIGRATION_PASSWORD=postgres
export GNOMON_TEST_SUPABASE_API_URL=http://127.0.0.1:56031

echo "== lane 3: the CLI stack and PostgREST"
cargo nextest run --workspace --test supabase_cli --run-ignored only "$@"
