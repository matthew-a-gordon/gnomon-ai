---
status: accepted
---

# Summaries and Communities are versioned by Recorded time and describe one valid-at instant

A Summary or Community row carries a Recorded time and one `as_of_valid` instant, the valid-at the Pipeline wrote it for. Regenerating one closes the old row and adds a new one, as for Versions. Search includes a row at `(v, k)` only when its Recorded time contains `k` and `v >= as_of_valid`. A Search at a past valid-at therefore finds no Summary, and a Search at the present finds the latest.

The Pipeline writes a Summary from the Node's current Versions and the Sentences of its current Edges, not from raw Episodes. Its Citations are the union of theirs. We chose this because a Summary describes a Node as the Pipeline knew it when it wrote it, so a present-day Summary must never answer a question about a past valid-at, and because a bounded input keeps both the model call and the Citations exact.

## Considered options

- Replace the Summary in place and ignore the As-of instants in Search. A past read would show prose about the present.
- Give Summaries a full Valid time and generate one per period. The Pipeline would pay for Summaries nobody asked for.
- Generate Summaries from raw Episodes. The input is unbounded, and the Citations would list Episodes the Summary did not use.

## Consequences

- Ticket #16 needs the Recorded time and `as_of_valid` columns, and ticket #20's Search needs the filter.
- Erasure removes every row that cites the erased Episode, closed or open, as for Versions (ADR-0003).
- Closed Summary rows accumulate. They are small, and a deployment may erase them by Scope.
