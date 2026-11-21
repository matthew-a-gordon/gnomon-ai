# Prototype 1: vector recall for a small Tenant under RLS

## Question

Does pgvector return good recall for a small Tenant in a large shared embedding table when every query runs under RLS? If not, what does the Store's search need?

## Measure

Recall@10 against exact nearest neighbours, and query latency p50 and p95, for:

1. A large Tenant and a small Tenant (100 to 1,000 rows) in one table of at least 1 million embeddings across many Tenants.
2. HNSW with the default `hnsw.ef_search`, then with `hnsw.iterative_scan` (`relaxed_order` and `strict_order`) and `hnsw.max_scan_tuples` tuned.
3. Alternatives worth comparing: an exact scan driven by a B-tree on the Tenant for small Tenants, and a per-Tenant partial index or partitioning, if cheap to try.

Use clustered synthetic vectors or a public embedding dataset so that recall means something; uniform random vectors do not. Use 1,536 dimensions unless a smaller real dataset is far easier, and say which.

## Constraints from the ADRs

- One embedding table; embeddings carry Citations (decided in the first grilling). Every row has a Tenant, RLS policies call `gnomon.tenant_id()` reading `request.jwt.claims`, and queries run as a non-owner role with `SET LOCAL ROLE` and `set_config(..., true)` per transaction (ADR-0001, ADR-0005).
- PostgreSQL 17 is the floor (ADR-0006).

## Environment

- `pgvector/pgvector:pg17` on host port 55433.
- Supabase's Postgres image `supabase/postgres` at a 17.x tag close to hosted Supabase (17.11 or 17.12) on host port 55434.
- Report the pgvector version each server runs. Do not use any Supabase stack you did not create.

## Rules for every prototype

- Read `GLOSSARY.md` and every ADR in `docs/adr/` first, and use the glossary's terms. The decisions there are settled; this prototype tests them, it does not reopen them unless the numbers force it.
- Use the `/prototype` skill. Answer exactly the question above with throwaway code. Do not build gnomon itself.
- Other prototypes run at the same time on this machine (48 cores, 251 GiB RAM). Run Docker containers only on the host ports listed in this brief, name them `gnomon-proto-<this prototype>-<role>`, and remove them when you finish. Never stop, change or remove any container you did not create. Other work is running on this machine.
- Commit only to this worktree's `prototype/...` branch. Never push and never touch `main`.
- The repo is public. Committed files must not name the consuming applications or quote their code, ADRs or issues; call them "the consuming applications".
- Never print, commit or log a secret. Read credentials from the environment or from files outside the repo.
- When you finish, write `FINDINGS.md` at the worktree root: the question; the answer in one sentence with its numbers; the method; a results table; what it means for which ADR (confirms, amends or contradicts); a recommendation; and caveats, including that other prototypes were running. Commit it, then tell the user you are done.
