---
status: accepted
---

# Every row has a Tenant, and single-tenant mode supplies a default one

Every table carries a Tenant and every RLS policy checks it, whether or not the deployment serves more than one customer. Tenancy is a setting chosen at install. In single-tenant mode gnomon creates one default Tenant, and callers may leave the Tenant out. In multi-tenant mode a call without a Tenant is an error and never falls back to the default. A deployment may switch from single to multi, and the default Tenant becomes its first Tenant, but not back. We chose this so that the schema, the policies and the conformance suite are the same in both modes, and so that a multi-tenant caller that forgets the Tenant fails instead of writing into a shared default.

## Considered options

- A nullable Tenant meaning no tenancy. `NULL` misbehaves in unique and exclusion constraints, the policies would need a second branch, and every earlier row would need migrating on a move to multi-tenant.
- Two schema variants. Two schemas and two conformance suites to keep in step.

## Consequences

- RLS policies call deployment-supplied functions, `gnomon.tenant_id()` and `gnomon.scope_allowed(scope)`, so each consumer resolves the Tenant its own way.
- Every Store function also takes an explicit Tenant. When claims bind one, the two must match.
