#!/usr/bin/env bash
# PROTOTYPE, throwaway. Usage: PROTO_URL=... ./run.sh <label> [clients] [txns]
set -u
label=$1; export PROTO_CLIENTS=${2:-60} PROTO_TXNS=${3:-50}
for d in tp-named tp-typed sqlx-cache sqlx-nocache sqlx-unnamed; do
  PROTO_LABEL="$label" PROTO_DRIVER=$d timeout 300 ./target/release/pooler-proto 2>&1 | grep -v '^LEAK' | tee -a results.txt
done
