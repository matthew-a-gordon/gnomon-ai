# Prototype 2: prepared statements and tenant context through transaction-mode poolers

## Question

Do the candidate Rust drivers' prepared statements, and per-transaction tenant context, work through Supavisor and through DigitalOcean's managed PgBouncer, both in transaction mode? gnomon opens its own connections (ADR-0001), so this decides the driver settings.

## Measure

For each driver configuration and each pooler, with at least 60 concurrent clients, each transaction doing `set_config('request.jwt.claims', <claims>, true)`, `SET LOCAL ROLE`, then parameterized queries:

- errors such as "prepared statement already exists" or "does not exist";
- leaks: a client ever seeing another client's claims or role (expect 0 of N);
- throughput and latency, as a rough comparison only.

Driver configurations: `tokio-postgres` with named statements, and with unnamed or typed statements only; `sqlx` with its statement cache on and off. Add another only if it is clearly a better fit.

## Targets

1. Local: `postgres:17` on host port 55435 behind a PgBouncer container in transaction mode on host port 56432. Add a local Supavisor if practical, on host port 56543.
2. Hosted Supabase Supavisor, transaction mode (port 6543). The user has approved using the Supabase project `szdzzrluksxakkxvuymp`. Its `main` branch is production: there, run only `SELECT` statements and `set_config`, with no DDL and no table writes. If you need any schema, create a separate Supabase branch with the Supabase MCP tools (confirm the cost), and delete it when finished. Ask the user for the pooler connection string and password; do not search other repositories for secrets.
3. DigitalOcean managed PostgreSQL. `doctl` is installed and authenticated, and the account has no database clusters. Create the smallest PostgreSQL 17 cluster (one node, `db-s-1vcpu-1gb`) in a nearby region, add a transaction-mode pool, and restrict trusted sources to this machine's public IP. Tell the user when you create it, with its hourly cost. Destroy it as soon as the tests finish, confirm with `doctl databases list` that it is gone, and report how long it ran.

Record the pooler versions where they can be seen.

## Rules for every prototype

- Read `GLOSSARY.md` and every ADR in `docs/adr/` first, and use the glossary's terms. The decisions there are settled; this prototype tests them, it does not reopen them unless the numbers force it.
- Use the `/prototype` skill. Answer exactly the question above with throwaway code. Do not build gnomon itself.
- Other prototypes run at the same time on this machine (48 cores, 251 GiB RAM). Run Docker containers only on the host ports listed in this brief, name them `gnomon-proto-<this prototype>-<role>`, and remove them when you finish. Never stop, change or remove any container you did not create. Other work is running on this machine.
- Commit only to this worktree's `prototype/...` branch. Never push and never touch `main`.
- The repo is public. Committed files must not name the consuming applications or quote their code, ADRs or issues; call them "the consuming applications".
- Never print, commit or log a secret. Read credentials from the environment or from files outside the repo.
- When you finish, write `FINDINGS.md` at the worktree root: the question; the answer in one sentence with its numbers; the method; a results table; what it means for which ADR (confirms, amends or contradicts); a recommendation; and caveats, including that other prototypes were running. Commit it, then tell the user you are done.
