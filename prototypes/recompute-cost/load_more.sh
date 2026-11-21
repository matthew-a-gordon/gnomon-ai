#!/usr/bin/env bash
# PROTOTYPE, wipe me. Second load stage (the first stage's cell inserts failed on table ownership): benchmark cells plus extra bulk.
set -euo pipefail
DB=${1:-gnomon_proto_base}
P="psql -h 127.0.0.1 -p 55432 -U postgres -d $DB -v ON_ERROR_STOP=1 -q"
tid() { printf '00000000-0000-0000-0000-0000000000%02d' "$1"; }
run() { PGOPTIONS="-c role=gnomon_owner -c synchronous_commit=off" $P -At -o /dev/null -c "$1"; }
CELL_NODES=${CELL_NODES:-270}
pids=()
for t in $(seq 1 10); do ( for c in 1 2 3 4; do run "select gnomon.proto_gen_bulk('$(tid $t)', 100)"; done ) & pids+=($!); done
cell=0
for spec in "n10_plain:10:plain" "n10_ranked:10:ranked" "n100_plain:100:plain" "n100_ranked:100:ranked" "n500_plain:500:plain" "n500_ranked:500:ranked"; do
  IFS=: read name n kind <<<"$spec"; cell=$((cell+1)); t=$((cell+1)); per=$((CELL_NODES / 3))
  for w in 0 1 2; do ( run "select gnomon.proto_gen_cell('$name', '$(tid $t)', $per, $n, '$kind', $((w * per)))" ) & pids+=($!); done
done
fail=0; for p in "${pids[@]}"; do wait $p || fail=1; done
[ $fail = 0 ] || { echo "a worker failed"; exit 1; }
$P -c "reset role" -c "vacuum (analyze)" -c "checkpoint"
$P -At -c "select 'nodes', count(*) from gnomon.node union all select 'episodes', count(*) from gnomon.episode union all select 'contributions', count(*) from gnomon.contribution union all select 'versions', count(*) from gnomon.version union all select 'citations', count(*) from gnomon.citation union all select 'db size', pg_database_size(current_database())"
