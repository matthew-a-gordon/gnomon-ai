# Findings: prepared statements and tenant context through transaction-mode poolers

## Question

Do the candidate Rust drivers' prepared statements, and per-transaction tenant context, work through Supavisor and through DigitalOcean's managed PgBouncer, both in transaction mode? gnomon opens its own connections (ADR-0001), so this decides the driver settings.

## Answer

Tenant context is safe on every pooler tested (0 leaks in all 41 runs, 364,491 checks), and unnamed or typed statements work on every pooler in its default configuration. Named statements work only where the pooler supports them: DigitalOcean's managed PgBouncer 1.26.1 (all 3000 of 3000 transactions, all five driver settings), local PgBouncer with `max_prepared_statements=200`, and Supavisor 2.10.1 with its `named_prepared_statements` flag on. They fail on PgBouncer with `max_prepared_statements=0` and on Supavisor with default flags (0 of 3000 for `tokio-postgres` named, 0 to 1657 of 3000 for `sqlx` with its cache).

## Method

Throwaway Rust harness in `prototypes/pooler-prepared-statements/` (`src/main.rs`, `run.sh`). One connection per client, 60 clients (200 in one heavy run), 50 transactions per client unless stated. Each transaction does what `as_user` will do:

1. `BEGIN` (simple protocol).
2. `select set_config('request.jwt.claims', $1, true)`, with claims unique to the client and the transaction.
3. `SET LOCAL ROLE authenticated` or `service_a`, alternating by client (simple protocol, since it takes no parameters).
4. Two parameterized queries. One returns the claims, the role and a parameter echo. The other counts rows from a small table under that role.
5. `COMMIT`.
6. A probe in a second transaction, which must see empty claims and the login role.

A leak is any claims or role that differs from what that client set, in step 4 or in the probe. An error is any statement that fails. The clients connect first, then start together behind a barrier. The pool holds 10 server connections locally (20 on DigitalOcean), so statements are multiplexed across server connections.

Driver settings:

| Setting | What it sends |
| --- | --- |
| `tp-named` | `tokio-postgres`, `prepare()` once per connection, then `query` on the named statement |
| `tp-typed` | `tokio-postgres`, `query_typed()` with explicit parameter types. Unnamed statements. |
| `sqlx-cache` | `sqlx`, default statement cache (capacity 100). Named statements `sqlx_s_N`. |
| `sqlx-nocache` | `sqlx`, `statement_cache_capacity(0)`. Still sends named statements, then closes them. |
| `sqlx-unnamed` | `sqlx`, `.persistent(false)` on each query. Unnamed statements. |

`sqlx-unnamed` is an addition to the brief. It is a documented setting of the same driver, and the harness showed `statement_cache_capacity(0)` does not turn named statements off.

Targets and versions:

| Target | Pooler | Postgres | Notes |
| --- | --- | --- | --- |
| Local PgBouncer | PgBouncer 1.26.1 (`edoburu/pgbouncer`), host port 56432 | 17.11 (`postgres:17`) | Run twice: `max_prepared_statements=0` and `=200` |
| Standalone Supavisor | Supavisor 2.10.1 (`supabase/supavisor`), host port 56543 | 17.11 | Run twice: default flags, and `NAMED_PREPARED_STATEMENTS_ENABLED=true`. One pre-registered tenant, pool 10. |
| Supabase CLI local stack (Supavisor) | Supavisor 2.10.1, Supabase CLI 3.2.0, host port 55623 | 17.12 | Project id `gnomon-proto-p2`, ports 55620 to 55628, pooler enabled in `config.toml` in transaction mode, default flags. Stopped afterwards. |
| DigitalOcean managed PgBouncer | PgBouncer 1.26.1 (read from the admin console with `show version`) | 17.12 | `db-s-1vcpu-1gb`, one node, sfo3, pool size 20, TLS. Global `max_prepared_statements=100`. |

Drivers: `tokio-postgres` 0.7.18, `sqlx` 0.8.6, native TLS with certificate checks off (throwaway).

The Supabase CLI stack and the standalone Supavisor stand in for hosted Supavisor. Hosted Supabase was not used, by the user's instruction. They run the same Supavisor image, but the hosted pooler's version and flags are not known from here.

## Results

Full rows, including latency and every error kind, are in `prototypes/pooler-prepared-statements/results.txt` and `results-control.txt`. All rows: 60 clients, 50 transactions each, 3000 transactions, 0 leaks, unless noted.

Successful transactions out of 3000:

| Target and configuration | tp-named | tp-typed | sqlx-cache | sqlx-nocache | sqlx-unnamed |
| --- | --- | --- | --- | --- | --- |
| Direct Postgres 17 (control) | 3000 | 3000 | 3000 | 3000 | 3000 |
| Local PgBouncer 1.26.1, `max_prepared_statements=0` | 11 | 3000 | 0 | 0 | 3000 |
| Local PgBouncer 1.26.1, `max_prepared_statements=200` | 3000 | 3000 | 3000 | 3000 | 3000 |
| Local PgBouncer, `=200`, 200 clients by 100 transactions (20000 each) | 20000 | 20000 | 20000 | 20000 | 20000 |
| Standalone Supavisor 2.10.1, default flags | 2 | 3000 | 958 | 124 | 3000 |
| Standalone Supavisor 2.10.1, `named_prepared_statements` on | 3000 | 3000 | 3000 | 1498 | 3000 |
| Supabase CLI local stack, Supavisor 2.10.1, default flags | 0 | 3000 | 1657 | 292 | 3000 |
| DigitalOcean managed PgBouncer 1.26.1 | 3000 | 3000 | 3000 | 3000 | 3000 |

Errors seen where a configuration failed:

- `tokio-postgres` named: `prepared statement "sNN" does not exist`, and `already exists` while preparing at connect.
- `sqlx-cache` and `sqlx-nocache`: `prepared statement "sqlx_s_N" already exists` and `does not exist`. With the cache on, a bound statement was also matched to the wrong server-side statement (`bind message supplies N parameters, but prepared statement "sqlx_s_N" requires M`, `incorrect binary data format in bind parameter`, `invalid byte sequence for encoding "UTF8"`). The pooler sends a client's Bind to a server connection where another client's statement of the same name exists. These did not produce a leak, because every one failed the transaction, but they show the failure is not always a clean error.
- Supavisor with the flag on, `sqlx-nocache`: `(EMAXPREPARED) max prepared statements limit reached` and dropped connections.
- `sqlx-unnamed` with the probe in autocommit (instead of in a transaction): `unnamed prepared statement does not exist`, 1884 of 3000, on local PgBouncer with `max_prepared_statements=0`. `sqlx` sends Parse and Bind as two round trips, so the unnamed statement is gone if the pooler reassigns the server connection between them. Inside a transaction the server connection is pinned and it works.

Throughput and latency, rough only (single-threaded harness, other prototypes running). Local, 60 clients, working configurations: 2500 to 4500 transactions per second, p50 12 to 24 ms. Direct Postgres was faster (5000 to 19000 per second), so the poolers cost a factor of two to five on this workload. Where more than one configuration worked, `tokio-postgres` named or typed was fastest, `sqlx` with its cache next, and `sqlx` unnamed or with capacity 0 slowest (for example, 3061 against 3496 transactions per second at 200 clients on PgBouncer). On DigitalOcean from Colorado to sfo3: 54 to 55 transactions per second for `tp-named`, `tp-typed` and `sqlx-cache` (p50 about 1.07 s), and 34 to 35 for `sqlx-nocache` and `sqlx-unnamed` (p50 about 1.7 s). That is round-trip bound, about eight round trips per transaction across 60 clients sharing 20 server connections, not a measure of the pooler.

## What it means for the ADRs

- ADR-0001 (one Rust core, transaction-mode poolers): confirms the design and amends its last consequence, which asked that prepared-statement behavior be proven on both poolers. The per-transaction context (`set_config(..., true)` plus `SET LOCAL ROLE`) is safe on every pooler tested, with no leaks. The driver setting is the open part, and it should be unnamed or typed statements by default: `tokio-postgres` `query_typed`, and `sqlx` with `.persistent(false)` on each query. `statement_cache_capacity(0)` is not enough. Named statements or the `sqlx` cache should be an opt-in for a deployment that has confirmed its pooler supports them. As an amendment, add to the consequences that gnomon runs every statement inside an explicit transaction, because `sqlx` unnamed statements need that on a transaction-mode pooler.
- ADR-0009 (invoker functions under RLS): no change. The role switch and claims worked as the invoker through every pooler.
- ADR-0006 and the Store's own SQL: no change. Nothing here touches the schema.

## Recommendation

1. Default the Rust core to unnamed or typed statements and test it against both pooler types in the conformance suite. This works on Supavisor with default flags, on PgBouncer with or without `max_prepared_statements`, and directly.
2. Offer named statements as a documented setting, off by default. Say in the docs that it needs PgBouncer 1.21 or later with `max_prepared_statements` above 0 (DigitalOcean's managed pooler had 100), or Supavisor with `named_prepared_statements` enabled.
3. Keep the claims and role set inside the transaction, as designed. Do not use session-level `set_config` or `SET ROLE`.
4. Measure the unnamed path on the real Store queries before relying on it. Here, `tokio-postgres` typed was as fast as named on a pooler that supports both (4272 against 3638 transactions per second on PgBouncer), and `sqlx` unnamed cost 12 to 18 percent against its cache (3121 against 3821, and 3061 against 3496 at 200 clients).

## Caveats

- Other prototypes were running on this machine throughout (containers for other prototypes, plus other stacks), so latency and throughput are only a rough comparison.
- Hosted Supabase was not tested. Its shared pooler, as documented, does not support prepared statements in transaction mode, which matches the Supavisor default-flag result here, but that is not verified against the hosted service.
- The harness drives all clients from one thread, so local throughput numbers are capped by the harness as well as the pooler.
- `tp-named` prepares once per connection and reuses the statement. Preparing a named statement inside each transaction was not tested.
- Only a few distinct statements were used. PgBouncer's statement eviction and Supavisor's per-connection limit were not stressed, except that `sqlx-nocache` reached Supavisor's limit.
- DigitalOcean's admin console shows `pool_mode=session` as the global default and `max_prepared_statements=100`. The pool I created was in transaction mode and behaved that way, but a differently configured cluster could differ.
- The DigitalOcean cluster ran from 22:23:45Z to 22:35:50Z (about 12 minutes, roughly $0.022 per hour). `doctl databases list` was empty afterwards. During setup, one command printed that cluster's pool URI, with its password, into the session transcript. The cluster no longer exists and its firewall allowed only this machine's IP.
- A first Supavisor run with the flag on was invalid: my script generated a new random vault key on restart, so the stored tenant password could not be decrypted. The rows were discarded, the script now recreates the metadata each start, and the reported rows are from the rerun.
- The harness uses certificate verification off for TLS. That is fine for throwaway tests and wrong for gnomon.

## Reproduce

`prototypes/pooler-prepared-statements/`: build with `cargo build --release`, then `PROTO_URL=<url> ./run.sh <label> [clients] [txns]`. `supavisor-local.sh` and `supavisor-pooler.exs` start the standalone Supavisor (it expects a PgBouncer-style scratch Postgres container on a `gnomon-proto-pooler-net` network). `supabase-stack/` holds the CLI project (`npx supabase start --workdir .`). `setup.sql` creates the scratch roles and table. Use `PROTO_BASE_ROLE` to name the login role if it is not `app`.
