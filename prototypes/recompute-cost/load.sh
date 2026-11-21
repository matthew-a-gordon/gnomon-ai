#!/usr/bin/env bash
# PROTOTYPE, wipe me. Build the base database: 10 Tenants, ~2M Version rows, plus the write-benchmark Nodes.
# Usage: load.sh [dbname]   (needs PGPASSWORD; postgres:17 on 127.0.0.1:55432)
set -euo pipefail
DB=${1:-gnomon_proto_base}; D=$(cd "$(dirname "$0")" && pwd)
P="psql -h 127.0.0.1 -p 55432 -U postgres -d $DB -v ON_ERROR_STOP=1 -q"
BULK_PER_TENANT=${BULK_PER_TENANT:-1900}   # about 61 Versions per Node
CELL_NODES=${CELL_NODES:-270}              # 250 measured + 20 warm-up per cell
$D/setup_db.sh $DB
$P <<SQL
create schema proto authorization gnomon_owner; create table proto.cell_node (cell text, idx int, tenant_id uuid, node_id uuid); alter table proto.cell_node owner to gnomon_owner; grant usage on schema proto to authenticated; grant select on proto.cell_node to authenticated;
insert into gnomon.tenant select ('00000000-0000-0000-0000-0000000000' || lpad(i::text, 2, '0'))::uuid from generate_series(1, 10) i;
insert into gnomon.kind select t.id, k.name, k.ranked from gnomon.tenant t cross join (values ('plain', false), ('ranked', true)) k(name, ranked);
SQL
tid() { printf '00000000-0000-0000-0000-0000000000%02d' "$1"; }
run() { PGOPTIONS="-c role=gnomon_owner -c synchronous_commit=off" $P -At -o /dev/null -c "$1"; }
start=$(date +%s)
pids=()
# bulk: 2 workers per Tenant, 25 Nodes per call
for t in $(seq 1 10); do for w in 1 2; do
  ( for c in $(seq 1 $((BULK_PER_TENANT / 2 / 25))); do run "select gnomon.proto_gen_bulk('$(tid $t)', 25)"; done ) &
  pids+=($!)
done; done
# cells: Tenants 2..7, three workers each
cell=0
for spec in "n10_plain:10:plain" "n10_ranked:10:ranked" "n100_plain:100:plain" "n100_ranked:100:ranked" "n500_plain:500:plain" "n500_ranked:500:ranked"; do
  IFS=: read name n kind <<<"$spec"; cell=$((cell+1)); t=$((cell+1)); per=$((CELL_NODES / 3))
  for w in 0 1 2; do
    ( run "select gnomon.proto_gen_cell('$name', '$(tid $t)', $per, $n, '$kind', $((w * per)))" ) &
    pids+=($!)
  done
done
for p in "${pids[@]}"; do wait $p; done
echo "load wall time: $(( $(date +%s) - start )) s"
$P -c "reset role" -c "vacuum (analyze)" -c "checkpoint"
$P -At <<SQL
select 'nodes', count(*) from gnomon.node union all select 'episodes', count(*) from gnomon.episode union all
select 'contributions', count(*) from gnomon.contribution union all select 'versions', count(*) from gnomon.version union all
select 'citations', count(*) from gnomon.citation union all select 'db size', pg_size_pretty(pg_database_size(current_database()));
SQL
