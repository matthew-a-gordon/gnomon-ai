---
status: accepted
---

# Versions are computed from Contributions, and Erasure recomputes history

Every property value an Episode states is stored as a Contribution: the Episode, the property, the value and its Valid time. A Version's values are chosen from the Contributions that remain, ordered by Authority, then Reference time, then Recorded time. Authority is an integer the consumer gives each Episode, defaulting to 0, and it decides only between Contributions whose Valid times overlap. Correction, End of validity, Retraction and Refinement are statements of the Episode that makes them, held the same way. Every write cites at least one Episode, and a person's edit is an Episode. Erasure deletes the erased Episode's Contributions, Citations and statements, then recomputes every Version it touched, current and closed. We chose this because Erasure must be deterministic and complete: a Version that merges several Episodes would otherwise keep an erased Episode's value, and once a losing value is thrown away nothing can restore it.

## Considered options

- Discard values that lose a merge. Erasure could then only blank a property, not bring back the next-best value.
- Re-extract from the remaining Episodes with the Pipeline after an Erasure. Not deterministic, and it costs model calls.
- A merge rule decided only in the Pipeline. Consumers that write extracted values straight to the Store would bypass it.
- An `append_only` merge rule for evidence. Evidence is held as Episodes, which are already immutable.

## Consequences

- Ranked merge is a rule a Kind turns on. Without it, the order is Reference time, then Recorded time. A child Kind may turn ranked merge on but not off.
- With ranked merge on, Authority ranks statements as it ranks values. A Correction, End of validity or Retraction from a lower-Authority Episode loses to the values a higher-Authority Episode supplied where their Valid times overlap. It stays held, like any losing Contribution, and takes effect if the higher-Authority Episode is erased. We added this when designing the Pipeline, because the Pipeline writes statements from Episode text it does not trust. Tickets #9 and #11 need it spelled out.
- A rule change applies to later writes only. Each Version records the rule that chose its values, and an Erasure recomputes each Version under that recorded rule, so the result does not depend on when the Erasure runs.
- As-of reads at a past known-at instant are stable except across an Erasure, which rewrites Recorded-time history as if the erased Episode never existed.
- Erasing the Episode behind a Retraction makes the retracted fact current again.
- A Node left with no Versions stays as a bare identity while any Edge joins it, and is erased with its last Edge.
- A no-op write creates no Version but adds the new Episode's Citation, so a repeated statement corroborates the Version it matches.
- A write and an Erasure serialise on each Node with a per-Node advisory lock (`pg_advisory_xact_lock`). Without it, a write could compute a Version from Contributions that an Erasure is about to delete, and that Version would carry the erased value. Both paths take the lock. An Erasure takes the locks of every Node it touches up front, in a fixed order, so two Erasures cannot deadlock. A write to an affected Node waits until the Erasure commits. Reads never wait.
- Recompute cost was measured in the `prototype/recompute-cost` branch (`FINDINGS.md`), on 24,620 Nodes and 1.9 million Version rows, as a non-owner role under RLS. The data were synthetic and the numbers come from a scratch server with fsync off.
  - A patch write to a Node with 10, 100 or 500 Contributions took about 1, 3 and 10 ms at p50. Its p95 was about 2.5, 6 and 30 ms. Ranked merge added no measurable cost.
  - A write with an open-ended Valid time recomputes from its start to the end of time. At 500 Contributions it took 200 to 270 ms at p50 and about 540 ms at p95. Bounding such writes is an open design choice.
  - Erasure of one Episode cited by 2,086 Versions across 500 Nodes took 24 to 35 seconds in one transaction, and it matched an oracle on every as-of read checked. Cost grows with each affected Node's history, at about 2 ms per replay step.
  - Truncate-and-replay rewrote about 22 times more Version rows than changed. Replaying only from the earliest recorded time the erased Episode touched cut the time by 6% when the Episode was early in the history, 31% in the middle and 73% when it was late.
  - As-of reads were unaffected during an Erasure (p50 about 4 ms against 5 ms with 8 readers). After an Erasure they ran about 50% slower until vacuum and about 20% slower afterwards, so an Erasure is followed by a vacuum.
  - The 500 advisory locks of one Erasure needed `max_locks_per_transaction` raised to 1024. A larger Erasure needs a larger lock table or a different lock design.
