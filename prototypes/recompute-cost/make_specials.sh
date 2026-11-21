#!/usr/bin/env bash
# PROTOTYPE, wipe me. Add the three Erasure target Episodes (early, mid, late in the Nodes' history) to the base database.
set -euo pipefail
DB=${1:-gnomon_proto_base}
P() { psql -h 127.0.0.1 -p 55432 -U postgres -v ON_ERROR_STOP=1 -At -d $DB "$@"; }
D=$(dirname "$0")
psql -h 127.0.0.1 -p 55432 -U postgres -d postgres -qc "drop database if exists gnomon_trial with (force)"
P -q -f $D/special.sql
gen() { P -c "set role gnomon_owner" -c "select gnomon.proto_gen_special('$1', '00000000-0000-0000-0000-0000000000$2', 500, now() - interval '$3 days', '2023-03-01', '2023-04-15', 10)"; }
gen mid 01 370 & gen early 09 615 & gen late 10 125 & wait
P -c "vacuum (analyze)" -c "checkpoint" 
P -c "select label, tenant_id, recorded_at from proto.special order by recorded_at"
