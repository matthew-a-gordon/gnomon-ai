---
status: accepted
---

# The HTTP API is RPC-style POST over shared types, and one binary serves it

Each operation has one route, named after the method the bindings expose: `POST /v1/add_episode`, `POST /v1/context`, and so on. Reads use POST as well, because As-of reads and Search carry large structured bodies. One set of Rust request and response types feeds the bindings, the server and an OpenAPI document that the server publishes at `/v1/openapi.json`. Kind schemas stay separate (ADR-0002). We chose this because most operations are not resources: they take a valid-at and a known-at instant, a Scope and a budget, and a resource layout would end up with `context` and Search as awkward POSTs anyway.

- Errors are one enum. HTTP returns them as RFC 9457 problem details, and the `code` is the same stable string the bindings put on their exceptions. A row-level security denial, a stale schema version, a reused Episode ID with different content and a permanent model refusal each have their own code.
- List and traversal calls use cursor pagination, with an opaque cursor and a maximum page size set by the server.
- There is no streaming in the first release. `context` returns one assembled body, and `add_episode` returns after its last stage or, with `wait: false`, the Episode ID.
- The shared types derive `schemars::JsonSchema`, the same derive rmcp uses for MCP tool schemas (ADR-0019). The server builds the OpenAPI 3.1 document from a route table that lists each method with its request type and its response type. Every route is a POST and every error is a problem details body, so the table needs nothing more. OpenAPI 3.1 schemas are JSON Schema 2020-12, which is what schemars writes.
- Tenant and Scope come from the verified token (ADR-0018). A body names a Tenant only in multi-tenant mode, and it must match the claims (ADR-0005).

## The server

`gnomon serve` is one command in one binary. It runs the HTTP API, MCP (ADR-0019) and the queue worker (#43) by default, and flags turn each off, so a deployment can run API-only replicas beside a worker-only replica from one configuration document. The same binary has the `gnomon kinds export` subcommand and the `gnomon token mint-worker` command (ADR-0018). The schema is plain SQL files a consumer copies, so no command exports it (ADR-0006).

- `/healthz` reports liveness only. `/readyz` reports ready when the database answers, `gnomon.schema_version` is in the range the binary accepts (ADR-0006), and each JWKS issuer has loaded keys at least once. A later JWKS refresh failure does not drop readiness, because cached keys still verify.
- On SIGTERM the server stops accepting requests and stops claiming stages. It waits for in-flight requests and the running stage for a configured grace period. A stage still running after that is abandoned, and its lease running out lets another worker retry it (ADR-0011).
- The server enforces a request body size limit and a cap on concurrent model calls per process.
- Tracing events carry usage and cost (#23). OpenTelemetry export over OTLP is optional and off by default. Logs never contain keys, Episode text or token contents.

## Rate limiting

The first release has no rate-limit policy, but the place for one exists. A middleware layer sits after JWT verification and before the handler, and HTTP and MCP both pass through it. It calls a `RateLimiter` trait with the verified identity (issuer, subject, Tenant, Scope) and the operation name, and the trait returns allow or a retry delay. The default implementation allows everything. The error enum reserves a `rate_limited` code that maps to HTTP 429 with `Retry-After`, and the configuration reserves a `limits` section. Enabling a limit later adds an implementation and configuration, and changes no route, error or existing configuration key. The bindings have no limiter, because the consumer controls how often it calls them.

## Considered options

- Resource-style REST (`/episodes/{id}`, `/nodes/{id}/versions`) with the bi-temporal instants as query parameters. It reads familiar, but most operations do not fit a resource.
- Separate binaries for the API and the worker. Each would be simpler to scale, but packaging and the release matrix double for a split that flags already give.
- Rate limits in the first release. No consumer has asked for one, and a per-caller limit belongs to the consumer's gateway until one does.
- Streaming results. No consumer needs it yet.
- `utoipa` for the OpenAPI document. It is the most widely used generator, but it has its own `ToSchema` trait, so every shared type would derive two schema traits that can disagree.
- `aide`. It builds on schemars, but it generates the document from axum handler types, and it has fewer users. The route table is shorter than the glue it would replace.

## Consequences

- A route's name is part of the API. Adding an operation adds a route, and the bindings and MCP tools follow from the same types.
- The OpenAPI document is generated and checked in, and a test fails when the types change without it.
- The conformance suite replays each case through the HTTP API with the same expected results as the core.
