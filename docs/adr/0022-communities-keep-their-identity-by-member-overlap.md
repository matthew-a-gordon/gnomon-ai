---
status: accepted
---

# Communities keep their identity by member overlap

The Pipeline, not the Community detector, gives a Community its ID. The `CommunityDetector` trait takes one Scope's graph, an optional previous partition and a seed, and returns a partition with no IDs. An implementation may ignore the previous partition. The Pipeline splits every detected Community into its connected parts, drops each part of fewer than three Nodes, then compares each of the rest with the Scope's stored Communities by Jaccard similarity, which is the members the two share over all the members of either. A similarity strictly above 0.5 is a match. The match keeps the stored ID. Writing it closes the stored row and adds a new one (ADR-0015); ADR-0025 says when that happens. An unmatched new Community gets a new ID, and an unmatched stored Community ends.

A Community's ID lasts across its rows, so the Store holds it apart from the row's ID. A write that replaces Communities names which of them, if any, passes its ID on. Reads and Search return both IDs. A Store call ends a Community that no write replaces and records that it ended on purpose, so the gap listing skips it. A Community that an Erasure left without a replacement is still listed.

The detector sees one Scope's graph: the Edges with both ends in the Scope that have a Version current in Recorded time and valid at the refresh's `as_of_valid` instant. `different_from` Edges are left out and `same_as` Edges are kept (ADR-0012). The graph is undirected, and the weight of a pair of Nodes is the number of Edges that join it.

The default detector is Leiden with a fixed seed and a size cap of 100 Nodes, which a deployment can change. It warm-starts from the previous partition, which the Pipeline builds from the stored Communities with every other Node on its own. gnomon will vendor the sequential Leiden code, and neither the parallel path nor the other algorithms, from `leiden-rs` 0.8.1 (upstream commit `fad950eea78feab2c9a839c93986edc3ac8d716f`, MIT OR Apache-2.0) and maintain it as gnomon's own. The trait, the graph, the split, the matching, the lasting ID, the end call and the vendored detector are planned, not built (#38).

We chose matching because a Community keeps its identity while most of its members stay (`GLOSSARY.md`), and the labels in a partition carry no identity from one run to the next. Each side of a match is a partition, so at most one stored Community is above 0.5 for a new one, and at most one new Community for a stored one. The match therefore needs no tie-break.

We chose the warm start by measurement. The rule was fixed before the run (#91): keep the warm start if it regenerates at least a third fewer Communities in total than a cold run, where a Community is regenerated when its member set changed or it got a new ID. It regenerated 4 against the cold run's 119. A Community regenerated in this sense needs a new Summary from the main model. ADR-0025 also regenerates one when an input to its Summary changes, which the measurement did not count, so these numbers compare membership churn and do not predict model calls.

## Measurement

The prototype is on the `prototype/community-warm-start` branch (`FINDINGS.md`). It used one synthetic Scope graph of 2,000 Nodes in 44 planted communities with 10,000 Edges, and 50 batches of 5 Ends of validity and 5 new Edges, with a refresh after each batch. Both strategies ran `leiden-rs` 0.8.1 without default features, with modularity at resolution 1.0, a size cap of 100 and seed 42, and both matched as above. Each strategy gave the same partition on every refresh in two runs.

| | Warm start | Cold run |
| --- | --- | --- |
| Communities regenerated over 50 refreshes | 4 | 119 |
| of which with a new ID | 0 | 3 |
| Refreshes that regenerated any Community | 1 | 31 |
| Communities after a refresh | 43 on all 50 | 42 to 44 |
| Modularity after refresh 50 | 0.774 | 0.775 |

No Community of fewer than three Nodes was detected, so the whole previous partition that the prototype passed on was the same as one built from the stored Communities.

## Considered options

- A cold run on each refresh, with the same matching. It regenerated about 30 times as many Communities.
- The warm start with no matching. A warm start renumbers the partition, so the stored IDs would be lost.
- Matching inside the detector. Each implementation of the trait would need its own rule, and swapping detectors could change which Communities keep their IDs.
- A dependency on the `leiden-rs` crate. Its source is on gitcode, and we could not check its maintenance.
- No lasting ID, with only the replacement linking an old row to a new one. A consumer would have nothing stable to hold.
- Ending a Community by closing its row alone. The gap listing could take it for one an Erasure left without a replacement, and list it on every run.
- Vendoring the parallel path. Its source says its results may differ from the sequential path's, and only the sequential path was measured.

## Consequences

- A Community that splits keeps its ID on a part whose similarity to it is still above 0.5. When no part is, every part gets a new ID and the stored Community ends. Two Communities that merge keep one of their IDs only if the result is above 0.5 against that one.
- A Community that an Erasure removed has no stored members to match, so its Nodes' next Community gets a new ID.
- In the measurement the warm-started partition did not change after the first refresh, through 490 further Edge changes. `leiden-rs` returns early, without refinement or aggregation, when the first local-moving phase moves no Node. That is not what held the partition still: with the early return removed in a scratch copy, both strategies gave the same partitions on this graph.
- A Node moves only to a neighbour's community. So a warm start can keep a Community whose halves no Edge joins, which a cold run would split. The Pipeline's split removes such a case whatever the detector. No Community in the measurement was disconnected, so the split does not change its numbers.
- The measurement used one graph and one seed, and its batches kept the planted structure. No Node was added, and no community was made to grow, split or merge, so it does not show how fast either strategy follows such a change.
- The size cap never bound, because the largest Community had 80 Nodes, so nothing measured supports another default. The parallel path of the crate's `rayon` feature did not run.
- The lasting ID and the end call change the SQL files in place, as ADR-0006 allows until the first tagged release.
