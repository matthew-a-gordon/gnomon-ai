---
status: accepted
---

# The Store and the Pipeline ship as one Rust core with bindings and a server

gnomon has two parts. The Store holds the graph and its guarantees as SQL functions in Postgres and makes no model calls. The Pipeline turns Episodes into Versions and assembles retrieval context with model calls, and it writes and reads only through the Store's functions. Both are implemented once, in a Rust crate that opens its own Postgres connections and makes its own model calls. Python (PyO3) and Node (napi-rs) bindings run it in the consumer's process, and a server binary built from the same crate serves HTTP and MCP. We chose this because the Pipeline is prompt-heavy logic that would drift if it were written three times, and because the consuming applications need it both in-process and as a service.

## Considered options

- A container service only. One implementation, but it forces a new deployable on consumers that run in serverless functions.
- A native library per language. Three Pipelines that drift apart.
- A sans-IO core, where the consumer makes every model call and every Store call. Rejected: gnomon is meant to be a complete interface layer, as Graphiti is, not a library the consumer has to drive. It also ruled out the server.

## Consequences

- Model providers are configured per role (main model, small model, embedder, reranker) with OpenAI-compatible or Anthropic endpoints. Zero data retention and air-gap rules are the consumer's responsibility, met through the endpoints it configures.
- gnomon acts as the caller under RLS: `as_user(jwt)` sets `request.jwt.claims` and `SET LOCAL ROLE authenticated` per transaction, and `as_worker(tenant, scope)` runs under a role that cannot bypass RLS. The server verifies every JWT, including worker tokens.
- Connections go through transaction-mode poolers (Supavisor, PgBouncer). The `prototype/pooler-prepared-statements` branch (`FINDINGS.md`) tested this on PgBouncer 1.26.1, Supavisor 2.10.1 and DigitalOcean's managed PgBouncer, and settled the driver behavior:
  - The Rust core sends only unnamed statements (`query_typed` in `tokio-postgres`; `.persistent(false)` on each `sqlx` query would do the same). They worked on every pooler in its default configuration. `statement_cache_capacity(0)` is not enough, because `sqlx` still sends named statements.
  - Named statements work only where the pooler supports them: PgBouncer 1.21 or later with `max_prepared_statements` above 0, or Supavisor with `named_prepared_statements` enabled. Elsewhere they fail, and with the `sqlx` cache they can fail by binding to another client's statement. The Store had them as an opt-in setting, which #82 removed with its statement cache, so no deployment has to check its pooler.
  - Every call runs in an explicit transaction. `as_user` and `as_worker` set their context with `set_config(..., true)` and `SET LOCAL ROLE`, never at session level. The Store sends `begin` and `set local role` in one simple query, one round trip. An unnamed statement also needs the transaction on a transaction-mode pooler, because `sqlx` sends Parse and Bind as two round trips and the pooler can reassign the server connection between them.
  - The prototype found no leaked claims or roles in 364,491 checks across 41 runs.
  - The conformance suite runs against both pooler types. The unnamed path still has to be measured on the Store's real queries.
- Times in the public Rust API are `jiff` 0.2 types, and every instant is a `jiff::Timestamp`. gnomon used `chrono` first and moved off it (#105) when it became unmaintained (RUSTSEC-2026-0543), before the shared types and the bindings could raise the cost of the change. On 2026-11-27:
  - tokio-postgres 0.7.18, PyO3 0.30.1 and `schemars` 1.2.3 all converted `jiff` 0.2. None of them converted `jiff` 1.0. rust-postgres/rust-postgres#1405 was open, and a PyO3 maintainer expected 1.0 in 0.31 (PyO3/pyo3#6609).
  - `time` 0.3 was the other crate the advisory recommended. `schemars` did not convert it.
  - napi 3.16.3 converted no `jiff` version, so gnomon will write the Node conversion. Its `chrono` conversion made a JS `Date` in whole milliseconds, which loses part of a `timestamptz`, so gnomon could not have relied on that either.
  - `rmcp` 3.12.0 enabled `chrono` in its default features, so gnomon must take it without them. It also depends on `jiff` 1, so the dependency tree will hold two `jiff` versions.
- The move to `jiff` 1.0 is a second breaking change to the public API. #109 tracks it.
- Valid time is UTC only, so gnomon enables no time zone database feature of `jiff`.
- A `jiff::Timestamp` ends at 9999-12-30T22:00 UTC, and `timestamptz` goes much further. The Store functions can be called from SQL, so a time outside the range can be stored, and a call that reads one returns an error. The Rust API refuses a retry delay that would store one.
- The bindings, the HTTP API, JWT verification and MCP are settled in ADR-0016 to ADR-0019. The bindings expose both APIs and start no background tasks. The server verifies worker tokens against an issuer of their own, and an MCP caller runs under RLS as any other caller does.
