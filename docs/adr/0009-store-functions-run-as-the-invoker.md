---
status: accepted
---

# Store functions run as the invoker, and only Erasure and Kind registration are definers

The Store's read and write functions are `SECURITY INVOKER` with `search_path = ''`, so every call runs as the caller under RLS. Caller roles get no DELETE on version rows and may update only the Recorded-time end column. A trigger lets that column change only by closing an open Version at the transaction time, apart from replacing a Version written earlier in the same transaction. Erasure and Kind registration are `SECURITY DEFINER` functions owned by a NOLOGIN gnomon role. Erasure checks the Tenant against `gnomon.tenant_id()`, and each deployment decides who may execute either function. We chose this because RLS stays the barrier between Tenants on every ordinary call, and only the two operations that must change or delete history run with more privilege.

## Considered options

- Every function `SECURITY DEFINER`, with callers holding EXECUTE only. No direct DML is possible, but RLS policies for caller roles stop applying, and each function's own Tenant check becomes the only barrier. One bug is a cross-tenant write.
