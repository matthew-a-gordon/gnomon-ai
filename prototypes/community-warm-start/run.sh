#!/usr/bin/env bash
# Runs the prototype twice, as two processes, checks that both give the same
# partition on every refresh for each strategy, and keeps the report.
set -euo pipefail
cd "$(dirname "$0")"

scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT

cargo run --quiet --release --locked -- "$scratch/first" >/dev/null
cargo run --quiet --release --locked -- "$scratch/second" >/dev/null

mkdir -p results
{
  for file in partitions-warm.txt partitions-cold.txt report.txt; do
    cmp "$scratch/first/$file" "$scratch/second/$file"
    echo "$file: identical in both runs, $(wc -l <"$scratch/first/$file") lines"
    (cd "$scratch" && sha256sum "first/$file" "second/$file")
  done
} | tee results/repeat-check.txt
cp "$scratch/first/report.txt" results/report.txt
cat results/report.txt
