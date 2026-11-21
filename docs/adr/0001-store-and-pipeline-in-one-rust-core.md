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
- Connections go through transaction-mode poolers (Supavisor, PgBouncer), so the driver's prepared-statement behavior must be proven on both before it is relied on.
