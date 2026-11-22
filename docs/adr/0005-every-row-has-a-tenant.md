---
status: accepted
---

# Every row has a Tenant, and single-tenant mode supplies a default one

Every table carries a Tenant and every RLS policy checks it, whether or not the deployment serves more than one customer. Tenancy is a setting chosen at install. In single-tenant mode gnomon creates one default Tenant, and callers may leave the Tenant out. In multi-tenant mode a call without a Tenant is an error and never falls back to the default. A deployment may switch from single to multi, and the default Tenant becomes its first Tenant, but not back. We chose this so that the schema, the policies and the conformance suite are the same in both modes, and so that a multi-tenant caller that forgets the Tenant fails instead of writing into a shared default.

## Considered options

- A nullable Tenant meaning no tenancy. `NULL` misbehaves in unique and exclusion constraints, the policies would need a second branch, and every earlier row would need migrating on a move to multi-tenant.
- Two schema variants. Two schemas and two conformance suites to keep in step.

## Consequences

- RLS policies call two functions that each deployment supplies, so each consumer resolves the Tenant its own way. `gnomon.tenant_id()` returns the Tenant, and `gnomon.allowed_scopes()` returns the set of Scope keys the caller may read.
- The policy is `tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes())`, so a B-tree on `(tenant_id, scope)` serves both conditions (ADR-0010).
- A consumer's Scope rule must be a set of allowed keys, or an inlinable SQL expression over one. Hierarchical Scopes are flattened when they are assigned.
- An opaque per-row function such as `scope_allowed(scope)` scans the whole Tenant. Measured, it took 575 ms for a 1,000-row Scope inside a 200,000-row Tenant, against 3 ms for the key set.
- Every Store function also takes an explicit Tenant. When claims bind one, the two must match.
