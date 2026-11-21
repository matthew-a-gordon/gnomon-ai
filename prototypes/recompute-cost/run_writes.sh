#!/usr/bin/env bash
# PROTOTYPE, wipe me. Rounds of the write benchmark, each on a fresh clone of the base. Usage: run_writes.sh tag:sync ...
set -euo pipefail
cd "$(dirname "$0")"
for round in "$@"; do
  tag=${round%%:*}; sync=${round##*:}
  python3 -c "import bench; bench.clone('gnomon_proto_base','gnomon_run_writes')"
  BENCH_SYNC=$sync python3 bench.py writes gnomon_run_writes $tag > /dev/null
  echo "done $tag"
done
python3 -c "import bench; bench.drop('gnomon_run_writes')"
