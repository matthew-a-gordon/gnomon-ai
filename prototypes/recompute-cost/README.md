# PROTOTYPE: recompute cost of Contribution-based Versions

Throwaway code. It answers one question (see `../../PROTOTYPE.md` and `../../FINDINGS.md`). Nothing here is gnomon.
Wipe it, do not build on it.

- `schema.sql`: scratch schema (tables, RLS, grants, guard trigger).
- `functions.sql`: write path, replay primitive, Erasure, data generator.
- `load.sh`: builds the base database (about 2 million Version rows over 10 Tenants) and the benchmark Nodes.
- `bench.py`: the measurements (writes, Erasure, as-of reads).

Needs `postgres:17` on host port 55432 and a password in `PGPASSWORD`.
