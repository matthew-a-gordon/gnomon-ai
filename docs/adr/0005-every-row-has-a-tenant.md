---
status: accepted
---

# Every row has a Tenant, and single-tenant mode supplies a default one

Every table carries a Tenant and every RLS policy checks it, whether or not the deployment serves more than one customer. Tenancy is a setting chosen at install. In single-tenant mode gnomon creates one default Tenant, and callers may leave the Tenant out. In multi-tenant mode a call without a Tenant is an error and never falls back to the default. A deployment may switch from single to multi, and the default Tenant becomes its first Tenant, but not back. We chose this so that the schema, the policies and the conformance suite are the same in both modes, and so that a multi-tenant caller that forgets the Tenant fails instead of writing into a shared default.

## Considered options

- A nullable Tenant meaning no tenancy. `NULL` misbehaves in unique and exclusion constraints, the policies would need a second branch, and every earlier row would need migrating on a move to multi-tenant.
- Two schema variants. Two schemas and two conformance suites to keep in step.

## Consequences

- Two tables have no Tenant, because they describe the deployment, not a graph: `gnomon.schema_version`, the ledger of applied migrations (ADR-0006), and `gnomon.tenancy`, the one row that holds the tenancy mode and the default Tenant. Caller roles may read both and write neither.

- RLS policies call two functions that each deployment supplies, so each consumer resolves the Tenant its own way. `gnomon.tenant_id()` returns the Tenant, and `gnomon.allowed_scopes()` returns the set of Scope keys the caller may read.
- The policy is `tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes())`, so a B-tree on `(tenant_id, scope)` serves both conditions (ADR-0010).
- A consumer's Scope rule must be a set of allowed keys, or an inlinable SQL expression over one. Hierarchical Scopes are flattened when they are assigned.
- An opaque per-row function such as `scope_allowed(scope)` scans the whole Tenant. Measured, it took 575 ms for a 1,000-row Scope inside a 200,000-row Tenant, against 3 ms for the key set.
- Every Store function also takes an explicit Tenant. When claims bind one, the two must match.

## Amendment: writable Scopes, the user, the writer's policies and the tenancy row (#81)

- Two more functions are each deployment's to supply. `gnomon.writable_scopes()` returns the Scope keys the caller may write, and by default returns `gnomon.allowed_scopes()`. `gnomon.user_id()` returns who records an Episode or its Retraction (ADR-0003), and by default the `sub` claim.
- Callers read under `tenant_id = gnomon.tenant_id() and scope = any(gnomon.allowed_scopes())` and write nothing directly. Every write runs as `gnomon_writer` (ADR-0009, ADR-0024), whose read policy is the callers' and whose write policies also need `scope = any(gnomon.writable_scopes())`. A write needs the Scope of every row it creates, changes, closes or deletes to be in both lists. A row it only refers to, such as a Node an Edge joins or a Summary describes, needs to be readable. Each write function checks the Scope first and refuses a read-only one with `insufficient_privilege`, because a policy alone lets an UPDATE or a DELETE change no row without an error.
- The writer's update policies let a Version close only at the start of the current transaction, or reopen when this transaction closed it, and let a Summary or Community only close. Its delete policies need `written_in = pg_current_xact_id()`, so it deletes only the rows its own transaction wrote. These replace the guard triggers of ADR-0009.
- Erasure needs the Scope of the named Episode, Node or Edge, or the named Scope, to be writable. It still removes what depends on it in every Scope, including Scopes the caller cannot write to or read.
- The tenancy row keeps its update policy, which admits only the switch to multi-tenant mode, and loses its guard triggers. `gnomon.switch_to_multi_tenant()` sets `switched_at` itself, so a role a deployment grants the switch needs UPDATE (mode, switched_at). No caller role holds a right to write the row, and gnomon offers no defence against the schema's owner, who could change it (ADR-0024).
- The four functions run as the caller during reads, as `gnomon_writer` during writes, as `gnomon_eraser` during Erasure (#83) and as `gnomon_definer` during Kind registration, the merge-rule setter and the search index trigger. A replacement reads only the claims or reaches other data through a `SECURITY DEFINER` function, does not depend on `current_user`, makes anything it calls outside gnomon usable by every one of those roles, answers for worker claims, which it tells by the `role` claim, and ends every search path in `pg_temp`.
