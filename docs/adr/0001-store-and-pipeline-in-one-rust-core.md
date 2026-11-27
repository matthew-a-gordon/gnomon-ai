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
  - The Rust core sends unnamed or typed statements by default (`query_typed` in `tokio-postgres`, `.persistent(false)` on each `sqlx` query). They worked on every pooler in its default configuration. `statement_cache_capacity(0)` is not enough, because `sqlx` still sends named statements.
  - Named statements are an opt-in setting, off by default. A deployment turns them on only after confirming its pooler supports them: PgBouncer 1.21 or later with `max_prepared_statements` above 0, or Supavisor with `named_prepared_statements` enabled. Elsewhere they fail, and with the `sqlx` cache they can fail by binding to another client's statement.
  - Every call runs in an explicit transaction. `as_user` and `as_worker` set their context with `set_config(..., true)` and `SET LOCAL ROLE`, never at session level. An unnamed statement also needs the transaction on a transaction-mode pooler, because `sqlx` sends Parse and Bind as two round trips and the pooler can reassign the server connection between them.
  - The prototype found no leaked claims or roles in 364,491 checks across 41 runs.
  - The conformance suite runs against both pooler types. The unnamed path still has to be measured on the Store's real queries.
- Times in the public Rust API are `chrono` types, which tokio-postgres (`with-chrono-0_4`), PyO3 and napi-rs all convert. Valid time is UTC only, so `jiff`'s time-zone handling brings nothing, and `time` has no advantage that would justify the change. A later change would touch every public type that has a timestamp.
- The bindings, the HTTP API, JWT verification and MCP are settled in ADR-0016 to ADR-0019. The bindings expose both APIs and start no background tasks. The server verifies worker tokens against an issuer of their own, and an MCP caller runs under RLS as any other caller does.
