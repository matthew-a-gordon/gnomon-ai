---
status: accepted
---

# Valid time and Recorded time are stored as four scalar columns

Each Version stores valid-from, valid-to, recorded-from and recorded-to as `timestamptz` columns with B-tree indexes, not as two `tstzrange` columns. Under row-level security, Postgres will not use the range operators (`@>`, `&&`) as index conditions because they are not leakproof, so a selective As-of read fell back to filtering: about 87 ms through a GiST range index against about 2 ms with scalar columns, measured with 2.4 million edge versions across 10 tenants. Overlap integrity still comes from an exclusion constraint, written on `tstzrange(...)` expressions over the scalar columns, with btree_gist's equality on the Tenant and on the Node or Edge (`tenant_id with =`, `versioned_id with =`).

## Consequences

- The floor includes btree_gist (ADR-0023), which every named host offers. Before #80 the constraint wrote those equalities as overlap of one-point ranges of a `gnomon.uuid_range` type, which is gone.

- Marking the range functions leakproof would restore the range plan, but that needs a superuser, which no managed host grants.
- PG 18's `WITHOUT OVERLAPS` and `PERIOD` foreign keys are range-based and would not be adopted even when every host runs 18. Temporal referential integrity is not enforced (ADR-0008).
- An open bound is `infinity`. Each Valid-time bound carries a precision whose coarsest value is `unknown`; an unknown start reads as open and the read returns the precision (ADR-0008).
