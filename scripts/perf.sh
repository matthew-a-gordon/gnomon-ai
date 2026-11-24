#!/usr/bin/env bash
# Runs the on-demand performance suite (ticket #21, ADR-0010) on both images,
# then the 200,000-row Search case again in a container capped at 4 GiB, on
# each image. See docs/testing.md.
#
#   scripts/perf.sh [search|patch|capped|all]      (default: all)
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

teardown() { # <container name>: remove it and its anonymous volumes, nothing else
  local name="$1" volumes
  if docker inspect "$name" >/dev/null 2>&1; then
    volumes=$(docker inspect "$name" --format '{{range .Mounts}}{{if eq .Type "volume"}}{{.Name}} {{end}}{{end}}')
    docker rm -f "$name" >/dev/null
    for v in $volumes; do docker volume rm "$v" >/dev/null 2>&1 || true; done
  fi
}

stage() { # <image> <prefix> <memory or ""> <postgres args> <cargo test filters...>
  local image="$1" prefix="$2" memory="$3" args="$4"; shift 4
  echo "== image=$image container=$prefix-$image memory=${memory:-none}"
  teardown "$prefix-$image"
  set +e
  GNOMON_TEST_IMAGE="$image" GNOMON_TEST_CONTAINER_PREFIX="$prefix" \
  GNOMON_TEST_DATA_DIR=disk GNOMON_TEST_MEMORY="$memory" GNOMON_TEST_POSTGRES_ARGS="$args" \
    cargo test -p gnomon --features performance --test performance "$@" -- --nocapture --test-threads=1
  local status=$?
  set -e
  teardown "$prefix-$image"
  return $status
}

status=0
for image in $images; do
  case "$what" in
    search|all) stage "$image" gnomon-t21 "" "$uncapped_args" fused_search || status=1 ;;
  esac
  case "$what" in
    patch|all) stage "$image" gnomon-t21 "" "$uncapped_args" bounded_patch || status=1 ;;
  esac
  case "$what" in
    capped|all) stage "$image" gnomon-t21-4g 4g "$capped_args" 200000 || status=1 ;;
  esac
done
docker ps -a --filter name=gnomon-t21 --format 'left over: {{.Names}} {{.Status}}'
exit $status
