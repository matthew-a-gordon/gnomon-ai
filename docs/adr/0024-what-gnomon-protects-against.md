---
status: accepted
---

# gnomon protects a Tenant's and a Scope's rows and their history from its callers, and leaves the database login, the schema owner and the bytes on disk to the operator

gnomon is a library inside a deployment's own Postgres. Its callers are the application's end users and background jobs, through the Store, and anyone who reaches a caller role with claims set, as any SQL client could. This record states which risks gnomon handles and which it leaves to PostgreSQL and the operator, so a security review can check each guarantee against the code that gives it.

| Risk | Who handles it |
|---|---|
| A caller reads or changes another Tenant's or Scope's rows | gnomon |
| A caller rewrites history | gnomon |
| SQL injection, or someone reading network traffic | gnomon |
| A write happens during an Erasure | gnomon |
| A query shows erased content | gnomon |
| A connection drops, or a call is slow | gnomon, with the connection pool as separate work, and the operator sets timeouts |
| Erased bytes remain on disk, in WAL, in backups or on replicas | PostgreSQL and the operator |
| Someone holds the database login | No extra defence, because they can choose any Tenant through the claims. gnomon still follows the PostgreSQL rules for roles and definer functions |
| The schema owner makes a harmful change | No defence. For example, they could return a deployment to single-tenant mode |

We chose this because these are the boundaries a library can hold. Rows and their history live in tables gnomon defines and guards with grants and row-level security. The login role, the schema owner and the storage belong to the deployment.

## How gnomon handles its risks

In the order of the table:

- Every row carries a Tenant and, below the Ontology, a Scope, and every policy checks them against `gnomon.tenant_id()` and `gnomon.allowed_scopes()` (ADR-0005). A write also needs its Scope in `gnomon.writable_scopes()`. The caller roles cannot bypass row-level security.
- Callers hold no INSERT, UPDATE, DELETE or TRUNCATE privilege on any gnomon table or column (#81). They write only through definer functions owned by `gnomon_writer`, a NOLOGIN role that owns no table and cannot bypass row-level security, so its own policies apply. Those let it delete only the rows its own transaction wrote, and change a committed row only by closing it at the start of the current transaction. Its grants keep every Recorded time, transaction mark and `recorded_by` at its default. Erasure is the only route that deletes committed history (ADR-0003, ADR-0009).
- The Store sends every value as a parameter, and every function pins a search path that ends in `pg_temp`. The Store connects with TLS and verifies the server's certificate by default (ADR-0021).
- Writes and Erasures serialise on advisory locks of each Node and Edge, taken in order of ID (ADR-0007).
- An Erasure deletes, in one transaction, every row derived from what it erases, Search's rows among them (ADR-0003, ADR-0010).
- Every Store call is one transaction that sets its role and claims locally, so a dropped connection leaves nothing half done and a pooled connection carries nothing to the next caller. The operator sets statement and idle timeouts on the login role.

## Considered options

- Callers writing tables directly, under their own policies, with guard triggers that judge every possible direct write. This was the design before #81. Each trigger had to foresee every statement a caller could send, and a commit check that read rows under the claims at commit could be passed by changing the claims first (#75).
- Every write function owned by the definer role that owns Erasure. Its wider policies, which reach other Scopes for Erasure, would then apply to every write.

## Consequences

- The login role can set any claims, so it must never reach end users. The database trusts the claims, and the Store does not verify them: the application verifies a token before it passes the claims to `Store::as_user`.
- A deployment's own `gnomon.tenant_id()`, `gnomon.allowed_scopes()`, `gnomon.writable_scopes()` and `gnomon.user_id()` run as the caller, as `gnomon_writer` and as `gnomon_definer`. They read only the claims or reach other data through a definer function of the deployment's, and do not depend on `current_user` (README).
- gnomon records who recorded an Episode or its Retraction and nothing else about who made a write (ADR-0003). The application logs what it needs.
- `VACUUM`, WAL, backups and replicas are the operator's. gnomon promises nothing about bytes on disk after an Erasure commits (ADR-0003).
- The schema owner can disable row-level security, rewrite the tenancy row or replace any function. gnomon does not defend against it, so the owner is a role the operator controls.
