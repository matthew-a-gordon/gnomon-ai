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
- The policy is `tenant_id = (select gnomon.tenant_id()) and scope = any ((select gnomon.allowed_scopes())::text[])`, so a B-tree on `(tenant_id, scope)` serves both conditions (ADR-0010). Each call is a sub-select of its own (#84, below).
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

## Amendment: policies call the functions in sub-selects, and the reference functions are PL/pgSQL (#84)

- Every policy calls `gnomon.tenant_id()`, `gnomon.allowed_scopes()` and `gnomon.writable_scopes()` each in a sub-select of its own, the writer's and the eraser's included: `tenant_id = (select gnomon.tenant_id()) and scope = any ((select gnomon.allowed_scopes())::text[])`. PostgreSQL runs such a sub-select once for the statement, as an InitPlan, and a bare call once for each row the policy checks. The cast keeps the sub-select one array for `any`. In test plans the conditions stayed index conditions. A catalog test reads every policy and fails on a call outside a sub-select of its own.
- The reference `gnomon.tenant_id()`, `gnomon.allowed_scopes()`, `gnomon.writable_scopes()` and `gnomon.user_id()` are PL/pgSQL with `search_path = pg_catalog, pg_temp`. A SQL function with a `SET` clause is never inlined, and PostgreSQL 17 plans its body again for each statement that calls it, while PL/pgSQL keeps its plans for the session. A SQL-standard body without a `SET` clause could be inlined only if it held no sub-query, and `tenant_id()` and `allowed_scopes()` hold one, so the rule that every search path ends in `pg_temp` keeps no exception. Only `tenant_id()` and `allowed_scopes()` were measured. The other two follow so the four read alike. No other function changed its language.
- A replacement follows the same advice: PL/pgSQL, and a sub-select of its own in any policy of the deployment's that calls it. The README shows a replacement written so.
- Why: at 500 Contributions one patch write called `gnomon.tenant_id()` about 860 times and `gnomon.allowed_scopes()` about 520 times, and PostgreSQL planned the SQL functions again for each statement. Measured at `c42ce73` in SQL on `pgvector/pgvector:pg17`, as `gnomon_worker` under row-level security, p50 write time with its commit, in ms:

  | Change | 10 Contributions | 100 | 500 |
  |---|---|---|---|
  | None | 18.4 | 18.0 | 23.3 |
  | Policy functions in PL/pgSQL only | 12.0 | 12.5 | 22.8 |
  | Policy calls in a sub-select only | 17.9 | 18.0 | 16.3 |
  | Both | 12.1 | 12.5 | 14.1 |

- Calls of the functions outside policies, in the Store functions' own checks and queries, stay as they are.
