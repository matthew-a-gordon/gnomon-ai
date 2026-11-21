---
status: accepted
---

# Valid time and Recorded time are stored as four scalar columns

Each Version stores valid-from, valid-to, recorded-from and recorded-to as `timestamptz` columns with B-tree indexes, not as two `tstzrange` columns. Under row-level security, Postgres will not use the range operators (`@>`, `&&`) as index conditions because they are not leakproof, so a selective As-of read fell back to filtering: about 87 ms through a GiST range index against about 2 ms with scalar columns, measured with 2.4 million edge versions across 10 tenants. Overlap integrity still comes from exclusion constraints, written on `tstzrange(...)` expressions over the scalar columns.

## Consequences

- Marking the range functions leakproof would restore the range plan, but that needs a superuser, which no managed host grants.
- PG 18's `WITHOUT OVERLAPS` and `PERIOD` foreign keys are range-based and would not be adopted even when every host runs 18. Temporal referential integrity is enforced by triggers.
- An open bound is `infinity`. An unknown bound is flagged separately; an unknown start reads as open and the read returns the flag.
