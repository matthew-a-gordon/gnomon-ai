---
status: accepted
---

# Store functions run as the invoker, and only Erasure, Kind registration and the merge-rule setter are definers

The Store's read and write functions are `SECURITY INVOKER` with `search_path = ''`, so every call runs as the caller under RLS. Caller roles may update only the Recorded-time end column of a Version, and may delete a Version or a Citation only when the current transaction wrote it. Guard triggers enforce both. The update guard lets the Recorded-time end change only by closing an open Version when the current transaction started, apart from replacing a Version written earlier in the same transaction, which may also reopen a Version that transaction closed. The delete guard refuses any row an earlier transaction wrote. Each row is marked at insert with the transaction that wrote it, and no caller may supply or change the mark. Erasure is the only route that deletes committed history. Erasure, Kind registration and the merge-rule setter, `gnomon.set_merge_rule`, are `SECURITY DEFINER` functions owned by a NOLOGIN gnomon role. Each checks the Tenant against `gnomon.tenant_id()`, and each deployment decides who may execute any of them. The setter is a definer for the reason Kind registration is: a Kind's rule changes after registration, and no caller may write `kind_merge` directly. We chose this because RLS stays the barrier between Tenants on every ordinary call, and only the operations that must delete history or write the Ontology run with more privilege.

## Considered options

- Every function `SECURITY DEFINER`, with callers holding EXECUTE only. No direct DML is possible, but RLS policies for caller roles stop applying, and each function's own Tenant check becomes the only barrier. One bug is a cross-tenant write.
- No DELETE at all, with a replaced Version closed at its own Recorded-time start. That leaves an empty Recorded time, which ADR-0008 refuses, and a dead row with its Citations.
- A further definer function that deletes only rows the current transaction wrote. It would run with more privilege than the write it serves, for a case the guard covers.
