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
- A rule change applies to later writes only. Each Version records the rule that chose its values, and an Erasure recomputes each Version under that recorded rule, so the result does not depend on when the Erasure runs.
- As-of reads at a past known-at instant are stable except across an Erasure, which rewrites Recorded-time history as if the erased Episode never existed.
- Erasing the Episode behind a Retraction makes the retracted fact current again.
- A Node left with no Versions stays as a bare identity while any Edge joins it, and is erased with its last Edge.
- A no-op write creates no Version but adds the new Episode's Citation, so a repeated statement corroborates the Version it matches.
- Write and Erasure cost under recompute is unmeasured. A prototype measures both before the schema is fixed.
