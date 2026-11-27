---
status: accepted
---

# Communities keep their identity by member overlap

The Pipeline, not the Community detector, gives a Community its ID. The `CommunityDetector` trait takes one Scope's graph, an optional previous partition and a seed, and returns a partition with no IDs. An implementation may ignore the previous partition. The Pipeline drops every detected Community of fewer than three Nodes, then compares each of the rest with the Scope's stored Communities by Jaccard similarity, which is the members the two share over all the members of either. A similarity strictly above 0.5 is a match. The match keeps the stored ID, closes the stored row and adds a new one (ADR-0015). An unmatched new Community gets a new ID, and an unmatched stored Community ends.

The default detector is Leiden with a fixed seed and a size cap. It warm-starts from the previous partition, which the Pipeline builds from the stored Communities with every other Node on its own. gnomon will vendor its code into the repository from `leiden-rs` 0.8.1 (upstream commit `fad950eea78feab2c9a839c93986edc3ac8d716f`, MIT OR Apache-2.0) and maintain it as gnomon's own. The trait, the matching and the vendored detector are planned, not built (#38).

We chose matching because a Community keeps its identity while most of its members stay (`GLOSSARY.md`), and the labels in a partition carry no identity from one run to the next. Each side of a match is a partition, so at most one stored Community is above 0.5 for a new one, and at most one new Community for a stored one. The match therefore needs no tie-break.

We chose the warm start by measurement. The rule was fixed before the run (#91): keep the warm start if it regenerates at least a third fewer Communities in total than a cold run, where a Community is regenerated when its member set changed or it got a new ID. It regenerated 4 against the cold run's 119. Each regenerated Community needs a new Summary from the main model, so over the 50 refreshes the cold run would have paid for more of those calls.

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

## Consequences

- A Community that splits keeps its ID on a part whose similarity to it is still above 0.5. When no part is, every part gets a new ID and the stored Community ends. Two Communities that merge keep one of their IDs only if the result is above 0.5 against that one.
- A Community that an Erasure removed has no stored members to match, so its Nodes' next Community gets a new ID.
- In the measurement the warm-started partition did not change after the first refresh, through 490 further Edge changes. `leiden-rs` returns early, without refinement or aggregation, when the first local-moving phase moves no Node. That is not what held the partition still: with the early return removed in a scratch copy, both strategies gave the same partitions on this graph.
- A Node moves only to a neighbour's community. So a warm start can keep a Community whose halves no Edge joins, which a cold run would split. No Community in the measurement was disconnected.
- The measurement used one graph and one seed, and its batches kept the planted structure. No Node was added, and no community was made to grow, split or merge, so it does not show how fast either strategy follows such a change.
- The size cap never bound, because the largest Community had 80 Nodes. The parallel path of the crate's `rayon` feature did not run.
- #38 sets the Community size cap.
