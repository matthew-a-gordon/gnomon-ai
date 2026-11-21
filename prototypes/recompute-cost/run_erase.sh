#!/usr/bin/env bash
# PROTOTYPE, wipe me. Every Erasure run on a fresh clone of the base (mid/full without concurrency already ran).
set -euo pipefail
cd "$(dirname "$0")"
for spec in "mid from_earliest" "early full" "early from_earliest" "late full" "late from_earliest" "mid full --concurrent" "mid from_earliest --concurrent"; do
  python3 bench.py erase gnomon_proto_base $spec > /dev/null
  echo "done $spec"
done
