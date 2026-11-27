# Findings: warm start against cold Community detection

## Question

Should gnomon's default Community detector warm-start Leiden from the previous partition, or run it cold on each refresh?

## Answer

Keep the warm start: over 50 refreshes it regenerated 4 Communities in total and the cold run regenerated 119, which is 96.6% fewer against the rule's threshold of a third fewer (at most 79).

## Method

Throwaway Rust harness in `prototypes/community-warm-start/`. `PROTOTYPE.md` holds the decision rule and every input, and was committed (`99ce323`) before any code.

- One synthetic Scope graph: 2,000 Nodes in 44 planted communities, 10,000 Edges.
- 50 batches, each 5 Ends of validity and 5 new Edges, so the graph stays at 10,000 Edges.
- `leiden-rs` 0.8.1 without default features: modularity, resolution 1.0, size cap 100, seed 42 on every refresh.
- Refresh 0 is a cold run for both strategies and is not counted. One refresh follows each batch.
- Communities under three Nodes are dropped, then each new Community takes the ID of the stored Community whose Jaccard similarity with it is strictly above 0.5.
- A Community is regenerated when its member set changed or it got a new ID.

Run it with `./run.sh`. It runs the prototype as two processes and compares the files.

## Results

| | Warm start | Cold run |
| --- | --- | --- |
| Communities regenerated, total over 50 refreshes | 4 | 119 |
| of which with a new ID | 0 | 3 |
| Refreshes that regenerated any Community | 1 of 50 | 31 of 50 |
| Most regenerated in one refresh | 4 | 10 |
| Communities after a refresh | 43 on all 50 | 42 to 44 |
| Modularity after refresh 50 (diagnostic) | 0.774 | 0.775 |
| Largest Community on any refresh (diagnostic) | 80 | 80 |
| Communities dropped for being under three Nodes | 0 | 0 |

The per-refresh table is in `prototypes/community-warm-start/results/report.txt`.

### Two runs, same partition

Two processes on the same input and seed gave byte-identical partitions on refresh 0 and all 50 refreshes, for each strategy (`results/repeat-check.txt`):

| File | SHA-256, the same in both runs |
| --- | --- |
| `partitions-warm.txt` | `15920f8d57dea724c704dd868ed1b39c28c7b898ac3e2583f5e6d55c5dd1829e` |
| `partitions-cold.txt` | `2a2d6067eb382bd09ad2e580f7f2b71e87925e5432472e0fb366ce4a17a27d5f` |

`tests/strategies.rs` checks the same within one process.

A prototype has no named mutation. The two-runs check above stands in its place.

## What it means for the records

- ADR-0022 (to be written): the default detector warm-starts from the previous partition.
- Spec #23, story 70: confirms the warm start.
- ADR-0014 and ADR-0015: unaffected. Detection stays within one Scope, and a match closes the old row and adds a new one.

## Recommendation

#38 warm-starts the default detector from the stored Communities, with every other Node on its own, and matches by overlap in the Pipeline. The detector it vendors starts from `leiden-rs` 0.8.1 with the sequential path.

## Caveats

- One graph, one graph seed and one detector seed.
- The graph has clear structure (a mean of 8 Edges per Node inside its planted community, 2 outside), and the batches keep it: 80% of new Edges are inside a community. No Node is added, and no community is made to grow, split or merge. The result says how stable each strategy is while the structure holds. It does not say how fast either follows a real change.
- The warm-started partition did not change at all after refresh 1, through 490 further Edge changes. `leiden-rs` 0.8.1 stops as soon as the first local-moving phase moves no Node, without running refinement or aggregation (`run_core` in `src/leiden.rs`). Such a refresh can then neither split a Community nor merge two. A Node moves only to a neighbour's community, so two halves that no Edge joins stay one Community, which a cold run splits (`tests/warm_start_limits.rs`). No Community of this run was disconnected under either strategy. Anyone who changes that stopping rule in the vendored detector changes what was measured here.
- The size cap never bound, because the largest Community had 80 Nodes against a cap of 100.
- No Community under three Nodes was detected. So starting from the stored Communities, as the Pipeline will, would have given the detector the same input as the whole previous partition did.
- The same seed on every refresh makes cold runs on similar graphs agree more than runs with changing seeds would.
- Only the sequential path ran. `leiden-rs`'s default `rayon` feature takes a parallel path on a graph this size, and its stability and repeatability were not measured.
