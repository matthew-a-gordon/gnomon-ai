---
status: accepted
---

# Callers write only through definer functions owned by roles that cannot bypass RLS

The Store's read functions are `SECURITY INVOKER` and run as the caller under row-level security. Callers hold no INSERT, UPDATE, DELETE or TRUNCATE privilege on any gnomon table or column, and write only through `SECURITY DEFINER` functions. Each definer function is owned by one of three NOLOGIN, NOINHERIT roles that own no table, cannot bypass row-level security and are members of no role, and no caller role is a member of any of them:

| Role | Owns | Its policies reach |
|---|---|---|
| `gnomon_writer` | every write function: recording an Episode, creating and writing Nodes and Edges, statements, Retract Episode, stage records, Summaries, Communities, embeddings and Sentences | the caller's Tenant and allowed Scopes for reads, and Scopes that are writable as well for writes |
| `gnomon_definer` | Kind registration, the merge-rule setter (`gnomon.set_merge_rule`) and the search index trigger (`gnomon.maintain_search_index`) | the caller's Tenant for the Ontology, the caller's allowed Scopes for the search tables, and, on the BM25 totals, the changes of its own transaction in any Scope of the Tenant |
| `gnomon_eraser` | the four Erasures: `gnomon.erase_episode`, `gnomon.erase_node`, `gnomon.erase_edge` and `gnomon.erase_scope` | every Scope of the caller's Tenant |

Every function, invoker and definer, pins a search path that ends in `pg_temp`. We chose this because a definer function owned by a role that owns no table and cannot bypass row-level security is still held to the policies that name that role. So row-level security stays the barrier between Tenants on every call, the definers included, and a definer runs with only the rights its role's grants and policies give it. The checks a direct write would need live in the write functions, the policies and the grants, instead of in triggers that must foresee every statement a caller could send. ADR-0024 says what gnomon protects against.

## The writer

- Its grants let it insert only the columns a write may choose, so an ID, a Recorded time, a transaction mark and `recorded_by` always take their defaults. It updates only the columns a write changes: a Version's, Summary's or Community's Recorded-time end, a Sentence's text, a stage record's status and usage, and an embedding's text hash and bits.
- Its update policies let a committed Version, Summary or Community change only by closing at the start of the current transaction, and let a Version reopen only when this transaction closed it. Its delete policies need `written_in = pg_current_xact_id()`, so it deletes only the rows its own transaction wrote.
- Each write function checks the Scope first and refuses a read-only one with `insufficient_privilege`, because a policy alone lets an UPDATE or a DELETE change no row without an error (ADR-0005).

## The definer

- Kind registration and the merge-rule setter are definers because a Kind's rules and its merge rule are the Ontology, which no caller may write directly. Each checks the Tenant against `gnomon.tenant_id()`, and each deployment decides who may execute them.
- The search index trigger writes Search's token rows and BM25 totals, Resolution's name rows, and the token rows of Summaries and Communities, from the Versions, Sentences, Summaries and Communities a write inserts or deletes (ADR-0010). When a Sentence is stored or replaced, it also deletes the Version's embedding of a text the Version no longer has, and that text's full vector unless another row of the Scope still uses it. It is granted to no role, so no caller can execute it or attach it to a table of its own. It is a definer because an invoker trigger needs callers to hold write privileges on the search tables, and a caller could then write them from a trigger of its own on a temporary table.
- `gnomon_definer` holds no write privilege on the Episodes, Versions, Citations, Contributions, statements, bound statements or Episode Retractions. That is a privilege, not a convention: `tests/sql_boundary.rs` checks it, and a change that let the role write one of those tables fails there.

## The eraser

- Erasure is the only route that deletes or rewrites committed history (ADR-0003, ADR-0007). It runs as `gnomon_eraser`, whose policies check the Tenant against `gnomon.tenant_id()` and nothing else. So an Erasure removes what depends on what it erases in every Scope of the Tenant: an Edge of another Scope that joins an erased Node, a Summary of it in another Scope, and the rows Search derived from them.
- No policy limits an Erasure to the caller's Scopes, so each function checks them itself. The Scope of the named Episode, Node or Edge, or the named Scope, must be readable and writable. A hidden item and an absent one get the same error, so a caller learns nothing of a Scope it cannot read. A result names only the Nodes and Edges in Scopes the caller can read. Every other query of an Erasure names its Scope or reads by IDs that came from the named item.
- A Node left with nothing, once an Erasure removes its last Edge, is erased in whichever Scope it is: it holds no Contribution, statement or Version, no other Edge joins it, and no Summary or Community names it. So the same request removes the same rows for every caller (ADR-0007).
- `gnomon.running_as_eraser()` is true only when the current user is `gnomon_eraser`, which holds only inside an Erasure. It lets an Erasure past the triggers that check a write, so its replay can write Versions, Citations, Contributions and statements at the Recorded times it replays. No other role is let past, and no caller can become the role or change the check with a setting. The rows the replay writes carry `written_in = '0'`, which no transaction has, so no transaction may delete or reopen them afterwards.
- The search index trigger also fires during an Erasure, and runs as its owner, `gnomon_definer`. It needs no right beyond its usual ones. The token rows an Erasure deletes in a Scope the caller cannot read are deleted by the eraser, and the trigger then records the count changes of its own transaction, which its policies allow in any Scope of the Tenant, and applies them at commit to the counts of the Scopes whose changes that transaction recorded. Every Version an Erasure inserts or reopens is in the Scope of the erased Episode, which the caller can read, so the trigger reads and indexes it under its usual policies.
- The accepted cost: a bug in an Erasure could remove rows in another Scope of the same Tenant. It cannot reach another Tenant's rows, because the eraser's policies check the Tenant on every query.

## Considered options

- Callers writing tables directly, under their own policies, with guard triggers that judged every possible direct write. This was the design before #81. Each trigger had to foresee every statement a caller could send, and a commit check that read rows under the claims at commit could be passed by changing the claims first (#75).
- Every function a definer owned by the tables' owner or by a role with `BYPASSRLS`. Policies stop applying to those two kinds of owner, so each function's own Tenant check would become the only barrier, and one bug would be a cross-tenant write. Policies still apply to a definer owned by a role that owns no table and cannot bypass row-level security, which is the design above.
- Erasure owned by `gnomon_definer`, under the caller's Scope policies, with reach policies that opened Edges and Summaries of other Scopes only for the IDs the running Erasure listed in `gnomon.erasure_reach`. This was the design before #83. There were many such policies, every new derived table needed more of them, and they gave Kind registration, the merge-rule setter and the search index trigger the same reach. An endpoint left with nothing was erased only when the caller could see its Scope, so two callers could get different results from one request.
- Every write function owned by the role that owns Erasure. The eraser's Tenant-wide policies would then apply to every write.
- Invoker triggers for the search tables, with a guard that refuses a write below a trigger depth of 2. Every role may create a temporary table and a trigger on it, so a caller reaches that depth from a trigger of its own and writes the token rows and counts of its own Scopes.
- No DELETE at all, with a replaced Version closed at its own Recorded-time start. That leaves an empty Recorded time, which ADR-0008 refuses, and a dead row with its Citations.

## Consequences

- Each deployment grants Kind registration, the merge-rule setter and the four Erasures to the roles it chooses. The files grant them to no role, and the search index trigger to none at all.
- A deployment's own `gnomon.tenant_id()`, `gnomon.allowed_scopes()`, `gnomon.writable_scopes()` and `gnomon.user_id()` run as the caller, as `gnomon_writer`, as `gnomon_definer` and as `gnomon_eraser`. They read only the claims or reach other data through a definer function of the deployment's, do not depend on `current_user`, and make anything they call outside gnomon usable by all of those roles (README).
- A table added later gets the policies its roles need: the callers' read policy, the writer's policies where it writes, the definer's where the search index trigger reads or writes it, and the eraser's, so that an Erasure reaches it in every Scope.
- `SELECT ... FOR UPDATE` needs an UPDATE right on some column, so the eraser holds UPDATE of an Episode's content, which it uses only to lock the rows of the Episodes it erases.

## Search paths end in `pg_temp` (#80)

No gnomon function has an empty `search_path`. Every function, invoker and definer, sets `pg_catalog, pg_temp`, or `pg_catalog, <pgvector's schema>, pg_temp` when it uses pgvector. This replaces the empty search path of story 22 of #1.

With `search_path = ''`, PostgreSQL still searches the temporary schema first for tables and types, and the manual says to list `pg_temp` last for `SECURITY DEFINER` functions. A caller who created a temporary domain named like a built-in type, with a check that calls its own function, had that function run as a definer role inside the merge-rule setter. Setting the path on that one function was not enough, because it calls invoker functions, which run with the definer's rights. So the rule covers every function. `tests/sql_boundary.rs` creates such domains for fourteen built-in type names and calls Kind registration, the merge-rule setter, a write that fires the search index trigger, a Summary and an Erasure, and finds that none of the caller's code ran as `gnomon_writer`, `gnomon_definer` or `gnomon_eraser`.

A deployment's own `gnomon.tenant_id()` or `gnomon.allowed_scopes()` also runs inside the definer functions, so any `search_path` it sets, or one a function it calls sets, must end in `pg_temp` too.
