---
status: accepted
---

# MCP runs over Streamable HTTP and stdio as the caller, with Erasure behind a switch

The server speaks MCP on Streamable HTTP in the same process as the HTTP API, and over stdio for local use. The current specification, 2026-07-28, defines those two transports and is stateless. Origin validation is on. A stdio server takes its JWT from the environment, as the specification directs.

The MCP route accepts the same issuers as the HTTP API (ADR-0018). Each issuer entry has an `mcp_audience`, and a token whose `aud` does not name the server is refused. The server serves RFC 9728 protected resource metadata that lists the deployment's issuers as its authorization servers, answers an unauthenticated request with the 401 challenge, and never passes a token through. gnomon runs no authorization server and no client registration. The verified JWT goes to `as_user`, so an MCP caller runs under row-level security exactly as an HTTP caller does. A worker token is refused on MCP. We chose this because the specification treats the MCP server as a resource server, which the issuers already fit, and because a second set of credentials would be a second place for a Tenant rule to go wrong.

## The tools

- Reads: `context`, as the main read tool, with `search`, `get_as_of`, `traverse` and `citations` for a caller who wants rows.
- Writes: `add_episode` and `retract_episode`.
- Erasure: `erase_episode`, which appears in the tool list only when `mcp.erasure` is on (ADR-0003). It carries `destructiveHint`.
- Not exposed: Kind registration, `maintain`, queue control and migrations.

Read tools carry `readOnlyHint`. Descriptions say what a tool returns and use the glossary's terms, because the model reads them. Annotations are hints a client may ignore, and row-level security stays the barrier.

`add_episode` over MCP waits for its stages when the server's own worker is off, and returns the Episode ID at once when the worker is on, so a model-driven client does not wait on a long call that a worker will finish.

## Considered options

- Every HTTP route as a tool. It is less to design, but it puts Kind registration and queue control in a model's reach, and a long list makes tool selection worse.
- Streamable HTTP only. Stdio is cheap to add, and a local client has no other way to reach a server.
- gnomon as its own authorization server. A large piece of work, and the consumers already have an identity provider.

## Consequences

- `rmcp` 3.11.0 supports Streamable HTTP, stdio, tool annotations and structured output, but its `auth` feature is client-only. gnomon writes the metadata route, the 401 challenge and the checks. Origin validation is off by default in `rmcp`, so gnomon turns it on (`docs/research/mcp.md`).
- The specification lets a tool list vary by caller and over time. The Erasure switch is set in the configuration, so the list changes only when the deployment restarts.
- The specification gives no guidance on a deployment switch for a tool or on writing descriptions. Both are gnomon's own choices.
- The conformance suite drives both transports with a real MCP client, and covers the metadata route, the 401 challenge, the tool list with the switch on and off, and an Origin check.
