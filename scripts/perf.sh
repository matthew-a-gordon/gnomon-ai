#!/usr/bin/env bash
# Runs the on-demand performance suite (ticket #21, ADR-0010) on both images,
# then the 200,000-row Search case again in a container capped at 4 GiB, on
# each image. See docs/testing.md.
#
#   scripts/perf.sh [search|search50k|search200k|patch|capped|words|words50k|words200k|writes|all]
#                                                                       (default: all)
#
# Containers are named gnomon-t21-<image> and gnomon-t21-4g-<image>, publish on
# loopback with a port Docker picks, keep their data on disk in an anonymous
# volume (a tmpfs would count against the memory cap), and are removed with
# their volume when a stage ends. Only containers with those names are touched.
set -euo pipefail
cd "$(dirname "$0")/.."
unset GNOMON_TEST_DATABASE_URL GNOMON_TEST_MIGRATION_ROLE
what="${1:-all}"
images="${PERF_IMAGES:-pgvector supabase}"

# Prototype 5's settings: uncapped, then the 4 GiB container's.
uncapped_args="-c shared_buffers=8GB -c work_mem=64MB -c maintenance_work_mem=512MB -c max_parallel_workers_per_gather=0 -c fsync=off -c autovacuum=off -c max_wal_size=16GB"
capped_args="-c shared_buffers=1GB -c effective_cache_size=3GB -c work_mem=16MB -c maintenance_work_mem=512MB -c max_parallel_workers_per_gather=0 -c fsync=off -c autovacuum=off -c max_wal_size=8GB"

# The patch case builds a small Store and a large one. It and the long-text
# writes (#85) run on Postgres's own settings with fsync off, so they measure
# the writes and not the disk's flush time (#84), as prototype 4 did.
# PERF_PATCH_ARGS="" runs them with durable commits instead. Their tables
# must not run with autovacuum off: they would never be analyzed.
patch_args="${PERF_PATCH_ARGS--c fsync=off}"

teardown() { # <container name>: remove it and its anonymous volumes, nothing else
  local name="$1" volumes
  if docker inspect "$name" >/dev/null 2>&1; then
    volumes=$(docker inspect "$name" --format '{{range .Mounts}}{{if eq .Type "volume"}}{{.Name}} {{end}}{{end}}')
    docker rm -f "$name" >/dev/null
    for v in $volumes; do docker volume rm "$v" >/dev/null 2>&1 || true; done
  fi
}

# Every stage has a hard time limit, so a stuck stage fails loudly. A stage
# builds its data set (20 minutes at most for 200,000 rows) and measures it.
stage_limit_50k="${PERF_LIMIT_50K:-25m}"
stage_limit_200k="${PERF_LIMIT_200K:-45m}"
stage_limit_patch="${PERF_LIMIT_PATCH:-25m}"
# The data sets with realistic word frequencies hold about 41 words a text,
# so their token rows take longer to build (#85).
stage_limit_words_50k="${PERF_LIMIT_WORDS_50K:-30m}"
stage_limit_words_200k="${PERF_LIMIT_WORDS_200K:-75m}"
stage_limit_writes="${PERF_LIMIT_WRITES:-15m}"

stage() { # <image> <prefix> <memory or ""> <postgres args> <time limit> <cargo test filters...>
  local image="$1" prefix="$2" memory="$3" args="$4" limit="$5"; shift 5
  echo "== image=$image container=$prefix-$image memory=${memory:-none}"
  teardown "$prefix-$image"
  set +e
  GNOMON_TEST_IMAGE="$image" GNOMON_TEST_CONTAINER_PREFIX="$prefix" \
  GNOMON_TEST_DATA_DIR=disk GNOMON_TEST_MEMORY="$memory" GNOMON_TEST_POSTGRES_ARGS="$args" \
    timeout --signal=TERM --kill-after=60 "$limit" cargo test -p gnomon --features performance --test performance "$@" -- --nocapture --test-threads=1
  local status=$?
  set -e
  teardown "$prefix-$image"
  return $status
}

status=0
for image in $images; do
  case "$what" in
    search|search50k|all) stage "$image" gnomon-t21 "" "$uncapped_args" "$stage_limit_50k" 50000_visible || status=1 ;;
  esac
  case "$what" in
    search|search200k|all) stage "$image" gnomon-t21 "" "$uncapped_args" "$stage_limit_200k" 200000_visible || status=1 ;;
  esac
  case "$what" in
    patch|all) stage "$image" gnomon-t21 "" "$patch_args" "$stage_limit_patch" each_patch_write || status=1 ;;
  esac
  case "$what" in
    capped|all) stage "$image" gnomon-t21-4g 4g "$capped_args" "$stage_limit_200k" 200000_visible || status=1 ;;
  esac
  case "$what" in
    words|words50k|all) stage "$image" gnomon-t21 "" "$uncapped_args" "$stage_limit_words_50k" word_frequencies_at_50k || status=1 ;;
  esac
  case "$what" in
    words|words200k|all) stage "$image" gnomon-t21 "" "$uncapped_args" "$stage_limit_words_200k" word_frequencies_at_200k || status=1 ;;
  esac
  case "$what" in
    words|writes|all) stage "$image" gnomon-t21 "" "$patch_args" "$stage_limit_writes" long_text_writes || status=1 ;;
  esac
done
docker ps -a --filter name=gnomon-t21 --format 'left over: {{.Names}} {{.Status}}'
exit $status
