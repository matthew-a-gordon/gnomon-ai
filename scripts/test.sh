#!/usr/bin/env bash
# Runs the whole suite in both container lanes, one image after the other:
# pgvector/pgvector:pg17, then supabase/postgres 17.x. See docs/testing.md.
#
#   scripts/test.sh [extra cargo nextest arguments]
#
# Set GNOMON_TEST_CONTAINER_PREFIX to name the containers (default
# gnomon-test). The containers remove themselves once the suite is idle.
set -euo pipefail
cd "$(dirname "$0")/.."
unset GNOMON_TEST_DATABASE_URL GNOMON_TEST_MIGRATION_ROLE
for image in pgvector supabase; do
  echo "== GNOMON_TEST_IMAGE=$image"
  GNOMON_TEST_IMAGE=$image cargo nextest run --workspace "$@"
done
