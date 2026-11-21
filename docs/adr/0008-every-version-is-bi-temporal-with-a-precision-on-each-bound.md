---
status: accepted
---

# Every Version is bi-temporal, with a precision on each bound

No Kind is exempt from Valid time. A Version whose source states no Valid time gets an unknown start and an open end. Each Valid-time bound carries a precision, `unknown`, `year`, `month`, `day` or `instant`, and stores the start of that period in UTC. Both axes are half-open, `[from, to)`. We chose this because sources state dates loosely ("in March 2024"), because a thing that seems timeless still changes in the world (a firm renames itself), and because, with four scalar columns on every row (ADR-0004), a Kind without Valid time would save almost nothing.

## Considered options

- A per-Kind `temporality` flag that keeps Recorded time only for some Kinds. A real-world change to such a Kind could then only be recorded as a Correction, which claims the earlier state was always wrong.
- Bounds as plain instants, converted by the caller. Precision is lost, and a more exact date looks like a contradiction instead of a Refinement.

## Consequences

- `unknown` is the coarsest precision. It replaces the separate unknown flag in ADR-0004, and an unknown start still reads as open.
- Stating a bound that was unknown, or narrowing a precision within the same period, is a Refinement. A bound outside that period is a Correction.
- An As-of read marks a result `uncertain` when its valid-at instant falls inside the period of an imprecise bound.
- Temporal referential integrity is not enforced. A Node's Valid time belongs to each Version and says when that state held, not when the Node existed, so there is no lifespan to check an Edge against. The Store checks only that both endpoints exist in the Tenant and that their Kinds are allowed.
- Empty intervals are refused. A second write to the same Node or Edge in one transaction replaces the first Version in place, because no read could have seen it.
