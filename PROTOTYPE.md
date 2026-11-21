# Prototype 4: recompute cost of Contribution-based Versions

## Question

Is ADR-0007 fast enough? Versions are computed from Contributions (ordered by Authority, then Reference time, then Recorded time; ranked merge opt-in per Kind), every write cites an Episode, and Erasure deletes an Episode's Contributions, Citations and statements and then recomputes every Version it touched, closed ones included.

## Measure

1. Write latency, p50 and p95, for a patch write to one Node holding 10, 100 and 500 Contributions spread over five years of Valid time, with ranked merge off and on.
2. Erasure of one Episode cited by 2,000 Versions spread over about 500 Nodes that each have history: wall time, rows deleted, rows rewritten, and lock behavior while concurrent reads run.
3. As-of read latency before and after, to show the Contribution tables do not slow the read path.
4. If Erasure or writes look too slow, try one cheaper recompute strategy (for example, recompute a Node's history only from its earliest affected Contribution) and compare.

No target is set. Report the numbers and say plainly whether anything would force a change to ADR-0007.

## Constraints from the ADRs

- Four scalar `timestamptz` time columns with a precision per Valid-time bound, half-open on both axes, empty intervals refused, overlap integrity from exclusion constraints on `tstzrange(...)` expressions, no temporal referential integrity (ADR-0004, ADR-0008).
- Every row has a Tenant. RLS policies call `gnomon.tenant_id()`, which reads `request.jwt.claims`. Run every measured call as a non-owner role under RLS, with `SET LOCAL ROLE` and `set_config(..., true)` per transaction (ADR-0005, ADR-0001).
- Store functions are `SECURITY INVOKER`; Erasure is `SECURITY DEFINER` owned by a NOLOGIN role (ADR-0009). Recorded time comes from the transaction timestamp.
- Volume: about 10 Tenants and about 2 million Version rows in total, matching the earlier as-of benchmark.

## Environment

- `postgres:17` on host port 55432. Nothing else.

## Rules for every prototype

- Read `GLOSSARY.md` and every ADR in `docs/adr/` first, and use the glossary's terms. The decisions there are settled; this prototype tests them, it does not reopen them unless the numbers force it.
- Use the `/prototype` skill. Answer exactly the question above with throwaway code. Do not build gnomon itself.
- Other prototypes run at the same time on this machine (48 cores, 251 GiB RAM). Run Docker containers only on the host ports listed in this brief, name them `gnomon-proto-<this prototype>-<role>`, and remove them when you finish. Never stop, change or remove any container you did not create. Other work is running on this machine.
- Commit only to this worktree's `prototype/...` branch. Never push and never touch `main`.
- The repo is public. Committed files must not name the consuming applications or quote their code, ADRs or issues; call them "the consuming applications".
- Never print, commit or log a secret. Read credentials from the environment or from files outside the repo.
- When you finish, write `FINDINGS.md` at the worktree root: the question; the answer in one sentence with its numbers; the method; a results table; what it means for which ADR (confirms, amends or contradicts); a recommendation; and caveats, including that other prototypes were running. Commit it, then tell the user you are done.
