# PROTOTYPE: warm start against cold Community detection

Throwaway code. It answers one question (see `../../PROTOTYPE.md` and `../../FINDINGS.md`). Nothing here is gnomon. Wipe it, do not build on it.

- `src/graph.rs`: the synthetic Scope graph and its 50 batches of Edge changes.
- `src/lib.rs`: the overlap matching, the decision rule and the two strategies.
- `src/main.rs`: the report.
- `run.sh`: runs the prototype twice, compares every partition, and writes `results/`.
- `tests/`: the matching, the inputs, the two-runs check, and in `warm_start_limits.rs` two diagnostics added after the result.

```sh
./run.sh
cargo test --release
```

No database. The repository's pinned Rust toolchain is enough.
