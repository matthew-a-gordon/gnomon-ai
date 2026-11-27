#!/usr/bin/env bash
# The proof for the squash of ticket #80, commit 1.
#
#   scripts/compare-squash.sh [old ref] [images...]
#
# Applies the migration chain of the old ref (default 3189a0f, the last commit
# with the numbered chain) and the chain in the working tree, each on a server
# of its own, so each chain creates the roles. On every image (default
# pgvector and supabase) it records, for four databases (pgvector in `public`
# and in `extensions`, each with plain default privileges and with default
# privileges granted to the caller roles), `pg_dump --schema-only` with
# ownership and grants, the roles' attributes and memberships, and the rows
# the migrations insert other than `gnomon.schema_version`. Then it diffs the
# two chains' records. tests/squash_proof.rs does the recording.
#
# The containers are the test harness's own, named gnomon-t80-sqa-<image> and
# gnomon-t80-sqb-<image>, and stop themselves when the test ends.
set -euo pipefail
cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"

old_ref=${1:-3189a0f}
shift || true
images=("${@:-pgvector}")
[ $# -gt 0 ] || images=(pgvector supabase)

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
git archive "$old_ref" crates/gnomon/migrations | tar -x -C "$work"
mv "$work/crates/gnomon/migrations" "$work/old"
cp -r crates/gnomon/migrations "$work/new"

status=0
for image in "${images[@]}"; do
  for side in old new; do
    prefix=gnomon-t80-sq$([ "$side" = old ] && echo a || echo b)
    # Each chain needs a fresh server, so wait for an earlier one to stop.
    while docker ps -a --format '{{.Names}}' | grep -qx "$prefix-$image"; do sleep 2; done
    GNOMON_TEST_IMAGE=$image GNOMON_TEST_CONTAINER_PREFIX=$prefix \
    SQUASH_CHAIN="$work/$side" SQUASH_OUT="$work/out/$image/$side" \
      cargo test -q -p gnomon --test squash_proof -- --ignored >/dev/null
  done
  if diff -r "$work/out/$image/old" "$work/out/$image/new"; then
    echo "$image: no difference in $(ls "$work/out/$image/old" | tr '\n' ' ')"
  else
    status=1
  fi
done
exit $status
