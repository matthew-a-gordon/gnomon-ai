# Research: how gnomon should serve MCP

Checked on 2026-11-22 against MCP specification revision 2026-07-28 and `rmcp` 3.11.0. The note uses the glossary's vocabulary where it applies. It lays out what the specification requires of a server that only verifies tokens, what the Rust SDK gives us, and what a design interview has to settle. It does not decide the design.

## The question

The server binary (ADR-0001) serves HTTP and MCP and verifies every JWT itself. What transport, authorization and tool-list rules should its MCP side follow?

Constraints already fixed:

- The server verifies every JWT, including worker tokens (ADR-0001). The JWT carries a role claim and is signed by an authorization server we do not run.
- gnomon runs the Store under RLS as the caller (`as_user(jwt)`, ADR-0001, ADR-0005), so the server must pass the verified identity into every call.
- The server is written in Rust and `rmcp` is the official SDK.

Evidence rules for this note. Every claim cites a primary source: the specification at modelcontextprotocol.io, the official GitHub organization, an RFC, or the `rmcp` crate source. The `rmcp` facts come from the published 3.11.0 crate (downloaded from crates.io and read), not from a blog. Anything I could not confirm is marked unconfirmed and collected at the end.

## Short answer

- The current specification is 2026-07-28. GitHub marks it as the final release, dated 28 July 2026, after a release candidate on 29 May ([releases](https://github.com/modelcontextprotocol/modelcontextprotocol/releases)). It is stateless: no `initialize` handshake, no `Mcp-Session-Id`, no stream resumption ([changelog](https://modelcontextprotocol.io/specification/2026-07-28/changelog)).
- A server that only validates tokens is an OAuth 2.1 resource server. It must publish RFC 9728 protected resource metadata that lists at least one authorization server, must reject tokens not issued for it as the audience, and must not pass tokens through ([authorization](https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization)). It has no registration duty.
- A server that validates JWTs from a third-party authorization server can conform. The specification says the authorization server "may be hosted with the resource server or a separate entity" and says nothing about token format. The hard part is audience: the token's `aud` must name this server.
- Two transports are defined: stdio and Streamable HTTP. The old HTTP+SSE transport is deprecated. For stdio, the specification says implementations "SHOULD NOT" follow the authorization section and should take credentials from the environment ([stdio authorization rule](https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization)).
- `rmcp` 3.11.0 implements 2026-07-28 and earlier versions. It serves Streamable HTTP, stdio, tool annotations, structured output and output schemas. Its `auth` feature is client-side OAuth only. The server side of authorization (protected resource metadata route, 401 challenge, JWT checks) is not in the crate and we would write it.

## 1. Which specification version, and what a resource server must do

### Version

- The latest published revision is 2026-07-28 ([specification index](https://modelcontextprotocol.io/specification/latest)). Its release is marked final and its release notes say "This release marks the stable release of the 2026-07-28 revision" ([GitHub releases](https://github.com/modelcontextprotocol/modelcontextprotocol/releases)). The previous revision is 2025-11-25.
- Revision 2026-07-28 changes the wire protocol. Every request carries its protocol version and client capabilities in `_meta`, servers must implement `server/discover`, and the HTTP transport requires `MCP-Protocol-Version` and `Mcp-Method` headers ([changelog](https://modelcontextprotocol.io/specification/2026-07-28/changelog), [streamable HTTP](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http)).
- A server may serve both eras. The versioning page says a dual-era server "MAY serve both eras concurrently on the same endpoint" and selects legacy behavior only when a client sends `initialize` ([versioning](https://modelcontextprotocol.io/specification/2026-07-28/basic/versioning)).

### Authorization is optional, but follows rules once offered

"Authorization is OPTIONAL for MCP implementations. When supported: implementations using an HTTP-based transport SHOULD conform to this specification" ([authorization, Protocol Requirements](https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization)). The specification is built on OAuth 2.1 (draft 13), RFC 6750, RFC 8414, RFC 7591, RFC 8707, RFC 9728, RFC 9207 and the Client ID Metadata Document draft.

### What the resource server must do

All quotes are from [authorization](https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization), [authorization server discovery](https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization/authorization-server-discovery) and [security considerations](https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization/security-considerations), revision 2026-07-28.

| Duty | Requirement |
| --- | --- |
| Protected resource metadata | "MCP servers MUST implement OAuth 2.0 Protected Resource Metadata (RFC9728)." The document "MUST include the `authorization_servers` field containing at least one authorization server." |
| Pointing to the authorization server | The server MUST implement one of two discovery mechanisms, and may do both. Either a `resource_metadata` parameter in the `WWW-Authenticate` header of a 401 response (RFC 9728 section 5.1), or the metadata at a well-known URI. The well-known URI can sit at the endpoint path (`/.well-known/oauth-protected-resource/public/mcp`) or at the root. |
| `resource` value | RFC 9728 makes `resource` required in the document, and the value "MUST be identical to the protected resource's resource identifier" used to fetch it ([RFC 9728, section 3.3](https://www.rfc-editor.org/rfc/rfc9728.txt)). The authorization tutorial says to use the exact URL clients connect to ([authorization tutorial](https://modelcontextprotocol.io/docs/2026-07-28/tutorials/security/authorization)). |
| Audience binding | "MCP servers MUST validate that access tokens were issued specifically for them as the intended audience, according to RFC 8707 Section 2." They "MUST only accept tokens specifically intended for themselves and MUST reject tokens that do not include them in the audience claim or otherwise verify that they are the intended recipient." Clients MUST send the `resource` parameter in authorization and token requests. |
| Token validation | Servers MUST validate tokens as in OAuth 2.1 section 5.2. Invalid or expired tokens MUST get HTTP 401. A token with too few scopes should get 403 with `error="insufficient_scope"`, a `scope` and `resource_metadata` in `WWW-Authenticate`. |
| Token passthrough | "The MCP server MUST NOT pass through the token it received from the MCP client." An upstream API needs its own token. "MCP servers MUST NOT accept any tokens that were not explicitly issued for the MCP server" ([security best practices](https://modelcontextprotocol.io/docs/2026-07-28/tutorials/security/security_best_practices)). |
| Bearer transport | `Authorization: Bearer` on every HTTP request. Tokens "MUST NOT be included in the URI query string." |
| Scopes | A `scope` parameter in the 401 challenge is SHOULD. `scopes_supported` should hold the minimal set for basic use ([scope minimization](https://modelcontextprotocol.io/docs/2026-07-28/tutorials/security/security_best_practices)). |

### Client registration is not the resource server's job

The authorization page says clients obtain a client ID by one of three routes: Client ID Metadata Documents, pre-registration, or Dynamic Client Registration ([client registration](https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization/client-registration)). Dynamic Client Registration is now deprecated: "New implementations should use Client ID Metadata Documents instead" and it stays for backwards compatibility. The authorization tutorial states the consequence for us: "Client registration happens between the client and the authorization server, so your MCP server needs no registration code" ([tutorial, Choosing an Authorization Server](https://modelcontextprotocol.io/docs/2026-07-28/tutorials/security/authorization)).

The authorization server carries these duties instead: PKCE with `code_challenge_methods_supported` in its metadata, RFC 8414 or OpenID Connect discovery, and `client_id_metadata_document_supported` if it accepts metadata-document clients. The deprecation of Dynamic Client Registration is recorded in the changelog ([changelog, Deprecated item 4](https://modelcontextprotocol.io/specification/2026-07-28/changelog)).

## 2. Transports

### Defined transports

The 2026-07-28 transports page defines two standard bindings: stdio, and Streamable HTTP. It also allows custom transports ([transports](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports)).

### SSE status

- The 2024-11-05 HTTP+SSE transport "has been deprecated since protocol version 2025-03-26" and is now classified Deprecated. "New implementations SHOULD NOT adopt it", and it is "eligible for removal in a future revision" ([streamable HTTP, HTTP+SSE section](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http), [changelog](https://modelcontextprotocol.io/specification/2026-07-28/changelog)).
- `rmcp` does not ship it: the README calls the 2024-11-05 HTTP+SSE transport "a deliberate non-goal" ([rust-sdk README](https://github.com/modelcontextprotocol/rust-sdk)).
- SSE as a response format is still part of Streamable HTTP. A server answers each POST with a JSON object or with an SSE stream scoped to that request.

### What Streamable HTTP requires in 2026-07-28

From [streamable HTTP](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http):

- One endpoint path that accepts POST. Each JSON-RPC request or notification is its own POST.
- Origin check: "Servers MUST validate the `Origin` header on all incoming connections to prevent DNS rebinding attacks." A present and invalid Origin gets 403. Locally run servers SHOULD bind to 127.0.0.1. Servers "SHOULD implement proper authentication for all connections."
- No sessions: "Removal of protocol-level sessions." A server that only supports this revision should ignore a `Mcp-Session-Id` header and not mint one. Servers that need state across calls use explicit handles as tool arguments ([changelog item 1](https://modelcontextprotocol.io/specification/2026-07-28/changelog)). A handle is not authentication ([state handle hijacking](https://modelcontextprotocol.io/docs/2026-07-28/tutorials/security/security_best_practices)).
- No resumability: "Resumable SSE streams via `Last-Event-ID` are not supported." A broken response stream loses the request and the client re-issues it with a new ID.
- No standalone GET stream. A GET or DELETE to the endpoint should get 405. Change notifications travel on the response stream of a `subscriptions/listen` POST.
- Headers: `MCP-Protocol-Version` and `Mcp-Method` on every POST, `Mcp-Name` on `tools/call`, `resources/read` and `prompts/get`. Mismatch with the body gets 400 and error `-32020`.
- A server that supports clients older than 2025-06-18 may treat a missing version header as 2025-03-26.

For older clients (2025-03-26 through 2025-11-25) the earlier shape had sessions, a GET stream, server-initiated requests and `Last-Event-ID` resumption. The 2025-11-25 page remains the reference if we serve them ([transports 2025-11-25](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports)).

### stdio and authorization

- Authorization section: "Implementations using an STDIO transport SHOULD NOT follow this specification, and instead retrieve credentials from the environment" ([authorization](https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization)). The tutorial repeats it and adds that local servers "can also use credentials from libraries embedded in the server" ([tutorial](https://modelcontextprotocol.io/docs/2026-07-28/tutorials/security/authorization)).
- Transport rules ([stdio](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/stdio)): newline-delimited JSON-RPC on stdin and stdout, logging only on stderr, no request metadata headers (everything is in `_meta`), and shutdown by closing stdin.
- Security guidance for local servers: use stdio "to limit access to just the MCP client", and if you use HTTP locally, require an authorization token or use restricted IPC ([local server compromise](https://modelcontextprotocol.io/docs/2026-07-28/tutorials/security/security_best_practices)).

## 3. A resource server over someone else's JWTs

### Can it conform?

Yes, on what the specification says. The authorization page says the authorization server's implementation details "are beyond the scope of this specification" and that it "may be hosted with the resource server or a separate entity". The tutorial says "Use an existing identity provider or authorization server product" and shows JWT verification with a library in every language ([tutorial](https://modelcontextprotocol.io/docs/2026-07-28/tutorials/security/authorization)).

It conforms if it does all of the following:

1. Serves protected resource metadata with `resource` and a non-empty `authorization_servers` (section 1).
2. Returns 401 with `WWW-Authenticate: Bearer resource_metadata="..."` for a missing or invalid token. Adding `scope` is SHOULD.
3. Verifies signature, issuer, expiry and audience. The tutorial lists the checks as: "Always validate the audience", "Accept only access tokens", "Accept only one issuer", "Return 5xx during an outage" (a down JWKS endpoint is not a 401), and "Don't write your own token validation".
4. Does not forward the inbound token to any other service.

### What a role claim means here

- The specification's authorization model is scopes. It also says a mistake is "Treating claimed scopes in token as sufficient without server-side authorization logic" ([scope minimization, common mistakes](https://modelcontextprotocol.io/docs/2026-07-28/tutorials/security/security_best_practices)). A role claim is outside the specification, so the server may read it as it likes.
- The tutorial notes that some authorization servers use `scp` or an array for scopes and says to change the code that reads the claim ([tutorial, common issues](https://modelcontextprotocol.io/docs/2026-07-28/tutorials/security/authorization)). Reading a `role` claim fits that pattern.
- If a role gates tool access, the 403 `insufficient_scope` response with a `scope` value is the only step-up signal the specification defines. A role cannot be stepped up that way unless the authorization server can issue it as a scope.

### The audience problem is the likely break

- The specification requires clients to send `resource` and requires servers to check audience. It says audience binding works "when the Authorization Server supports the capability" ([security considerations](https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization/security-considerations)).
- The tutorial gives the fallback: some products "always use their own identifier. In that case, compare the audience with that identifier, and keep your server's URL as the PRM `resource`. The identifier must belong to this MCP server only" ([tutorial](https://modelcontextprotocol.io/docs/2026-07-28/tutorials/security/authorization)). A shared audience such as `api` must not be accepted.
- So the authorization server must be able to put an audience in the token that is unique to the gnomon server. If it cannot, the server cannot meet the MUST.

### What the server must expose, as a list

- A POST endpoint at one path (for example `/mcp`).
- `GET /.well-known/oauth-protected-resource` or the path-suffixed variant, returning `resource`, `authorization_servers`, and `scopes_supported`.
- `server/discover` ([discover](https://modelcontextprotocol.io/specification/2026-07-28/server/discover)). The page does not say whether it needs a token. See Not confirmed.
- 401 and 403 challenges as in section 1.

## 4. What `rmcp` supports

Version checked: `rmcp` 3.11.0, published 2026-11-17 ([crates.io](https://crates.io/crates/rmcp), repository [modelcontextprotocol/rust-sdk](https://github.com/modelcontextprotocol/rust-sdk)). I read the downloaded 3.11.0 source and its CHANGELOG. The README says the crate "implements the stable MCP 2026-07-28 specification while remaining fully compatible with the 2025-11-25 release and earlier versions."

Cargo features ([docs.rs features, 3.11.0](https://docs.rs/crate/rmcp/3.11.0/features)): default is `base64`, `macros`, `server`. Relevant ones:

- `transport-streamable-http-server` for the HTTP server.
- `transport-io` for stdio.
- `auth` for OAuth. It enables `oauth2`, `url` and a `reqwest` base.
- `auth-client-credentials-jwt` and `auth-enterprise-managed`, both client-side.
- `schemars` for schema generation.

### Streamable HTTP

- `StreamableHttpService` is a Tower service that mounts on an `axum` or `hyper` router (README).
- `StreamableHttpServerConfig` fields (read from `src/transport/streamable_http_server/tower.rs`): `legacy_session_mode` (default `true`), `json_response`, `allowed_hosts`, `allowed_origins`, `stateless_protocol_metadata_required` (default `false`), `session_store`, `max_request_body_bytes`.
- Per the source comment, `legacy_session_mode` "only applies to legacy protocol versions (< 2026-07-28)"; requests negotiating 2026-07-28 "are always served statelessly regardless of this setting."
- Default `allowed_hosts` is `localhost`, `127.0.0.1` and `::1`. The comment says public deployments "should override this list with their own hostnames."
- Default `allowed_origins` is empty, which "disables Origin validation for backward compatibility." A non-empty list turns it on, and requests without an `Origin` header still pass. The specification's MUST for Origin validation therefore needs us to set the list.
- Request parts reach handlers: the service injects `http::request::Parts` into the request extensions, so a handler can read headers set by our middleware (for example, a verified identity).

### Auth

- The README's auth row reads "OAuth 2.0 authentication support", and `docs/OAUTH_SUPPORT.md` describes only client behavior: protected resource discovery, authorization server discovery, Client ID Metadata Document use, dynamic registration fallback, scope selection, step-up ([OAUTH_SUPPORT.md](https://github.com/modelcontextprotocol/rust-sdk/blob/main/docs/OAUTH_SUPPORT.md)).
- I found no server-side helper in 3.11.0: no function that serves protected resource metadata, builds a `WWW-Authenticate` challenge for a server, or verifies a bearer token. The strings `oauth-protected-resource` appear in the client discovery code only.
- The official server examples hand-roll it with axum middleware. `simple_auth_streamhttp.rs` checks `Authorization: Bearer` against a static token list. `complex_auth_streamhttp.rs` is a full demonstration authorization server that serves `/.well-known/oauth-protected-resource/mcp` by hand ([examples](https://github.com/modelcontextprotocol/rust-sdk/tree/main/examples/servers/src)).
- Conclusion: gnomon writes the metadata route, the challenge and the JWT verification, in front of `StreamableHttpService`. That fits ADR-0001, which already puts JWT verification in the server.

### Tool annotations

- `ToolAnnotations` has `title`, `read_only_hint`, `destructive_hint`, `idempotent_hint` and `open_world_hint`, all optional. The doc comment says "all properties in ToolAnnotations are hints" and that clients should never decide on tool use from untrusted servers (`src/model/tool.rs`).
- The `#[tool]` macro accepts `annotations(title = "...", read_only_hint = true)` ([rmcp-macros 3.11.0, `lib.rs`](https://docs.rs/rmcp-macros/3.11.0)).
- `ToolRouter` has `disable_route`, `enable_route` and `remove_route`. The source comment says a disabled tool is "Hidden from `list_all`, `get`, rejected by `call`." It also sends a `tools/list_changed` notification when visibility changes.

### Structured output and output schemas

- `Json<T>` returns structured output and derives `outputSchema` from `T` through `schemars` ([README](https://github.com/modelcontextprotocol/rust-sdk)).
- For 2026-07-28 peers, `outputSchema` may be any JSON Schema type and `structuredContent` any JSON value. For peers before 2026-07-28, a `T` whose schema root is not an object is listed without `outputSchema` and its result goes to the text block (`src/handler/server/wrapper/json.rs`).
- The 3.11.0 changelog lists "omit non-object tool output for legacy peers" as a fix, so this behavior is recent.

### Change notifications

- `enable_tool_list_changed()` sets the capability. On 2026-07-28 notifications flow over `subscriptions/listen` (`src/service/server.rs` has a `subscriptions/listen` notification sink, and the 3.9.0 entry says client `on_*_list_changed` callbacks point to `Peer::listen`).

### Fast-moving surface

Eight minor releases shipped between 2026-09-28 and 2026-11-17 (3.5.0 to 3.11.0), and several touch the HTTP server, for example "stateful session workers ignore config.canc" (3.9.0) and "answer a session-less non-initialize POST with 400" (3.6.0) ([CHANGELOG](https://github.com/modelcontextprotocol/rust-sdk/blob/main/crates/rmcp/CHANGELOG.md)). Pin an exact version.

## 5. Tool descriptions, annotations and dynamic tool lists

### Descriptions

- The `description` field is "A human-readable description of the tool. This can be used by clients to improve the LLM's understanding of available tools. It can be thought of like a 'hint' to the model" ([schema, Tool](https://modelcontextprotocol.io/specification/2026-07-28/schema)).
- The specification gives one rule of thumb for descriptions: where a tool creates state, the "retention policy should be stated in the creation tool's description (e.g., 'baskets expire after 24 hours of inactivity') so the model can see it when deciding to create state" ([tools, Stateful Tools](https://modelcontextprotocol.io/specification/2026-07-28/server/tools)).
- Tool names: 1 to 128 characters, ASCII letters, digits, underscore, hyphen and dot, unique in the server. Clients that aggregate servers may prefix names ([tools, Tool Names](https://modelcontextprotocol.io/specification/2026-07-28/server/tools)).
- Client guidance affects how descriptions get used. Hosts with many tools switch to progressive discovery, where a search over names and one-line descriptions picks which full definitions enter context. "Keyword matching ... Simple and effective, particularly for descriptive tool names and descriptions." Hosts also cache tool lists and note that changing the `tools` array invalidates provider prompt caches ([client best practices](https://modelcontextprotocol.io/docs/2026-07-28/develop/clients/client-best-practices)).
- Servers "SHOULD return tools in a deterministic order" and list results now carry `ttlMs` and `cacheScope` ([tools](https://modelcontextprotocol.io/specification/2026-07-28/server/tools), [changelog](https://modelcontextprotocol.io/specification/2026-07-28/changelog)).
- Errors: input validation and business errors belong in the result with `isError: true` so the model can retry. Unknown tool and malformed requests are protocol errors ([tools, Error Handling](https://modelcontextprotocol.io/specification/2026-07-28/server/tools)).
- Beyond these, the specification and official docs give no style guide for writing descriptions. Anything more would be unconfirmed.

### Annotations

- Fields and defaults ([schema, ToolAnnotations](https://modelcontextprotocol.io/specification/2026-07-28/schema)): `readOnlyHint` default false, `destructiveHint` default true, `idempotentHint` default false, `openWorldHint` default true. The last three matter only when `readOnlyHint` is false (the schema marks `openWorldHint` without that qualifier).
- Because of the defaults, a tool with no annotations reads as "writes, destructive, open world". A read-only search tool must set `readOnlyHint: true` and `openWorldHint: false` to look as it is.
- They are hints only. The tools page says "clients MUST consider tool annotations to be untrusted unless they come from trusted servers", and the overview says annotations "should be considered untrusted, unless obtained from a trusted server" ([tools](https://modelcontextprotocol.io/specification/2026-07-28/server/tools), [overview](https://modelcontextprotocol.io/specification/latest)). The server cannot rely on a client honoring them, so the server must enforce what the hints claim.
- The specification also says "there SHOULD always be a human in the loop with the ability to deny tool invocations" and that servers MUST "validate all tool inputs, implement proper access controls, rate limit tool invocations, sanitize tool outputs."

### Hiding a destructive tool, and changing tool lists

- Supported by the specification: a server's `tools/list` result "MAY vary by the authorization presented on the request, for example returning only the tools the caller's granted scopes permit". It "MUST NOT vary per-connection or as a side effect of other requests on the connection." The set "MAY change over time" ([tools, Capabilities](https://modelcontextprotocol.io/specification/2026-07-28/server/tools)).
- A deployment-level switch (a setting that removes a destructive tool from every list) is not named in the specification. It falls under "the set currently available", and it is allowed. The tools page does not discuss it. I found no official guidance for it, so how to expose it is our decision.
- The `cacheScope` field is the cache signal: `"public"` for a list that is the same for everyone, `"private"` for one that varies by token. A list that hides tools by role should be `"private"` ([schema, ListToolsResult](https://modelcontextprotocol.io/specification/2026-07-28/schema)).
- `listChanged` is supported. Servers that declare it SHOULD send `notifications/tools/list_changed` to clients that opened `subscriptions/listen` with `toolsListChanged: true`. The server MUST NOT send notification types the client did not request, and it MUST acknowledge the subscription first ([subscriptions](https://modelcontextprotocol.io/specification/2026-07-28/basic/patterns/subscriptions)). Because the list varies only by per-request authorization and a deployment setting, a restart-time switch needs no notification. A runtime toggle would.
- Hiding a tool is not access control. A hidden tool must also be refused on call. `rmcp`'s `disable_route` does both (section 4).

## What it means for gnomon

### Options

Transport.

- A. Streamable HTTP only. One transport, one verification path, fits "the server verifies every JWT". Cost: no direct launch from a local agent without an HTTP client.
- B. Streamable HTTP plus stdio. The stdio side reads a JWT from an environment variable and the server verifies it once per process before binding the identity to every call. The specification's own rule for stdio is to take credentials from the environment, so this stays inside it ([authorization](https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization)). Cost: a second entry point to test, and one token for the process lifetime means expiry handling has to be designed.
- C. Add a legacy HTTP session mode for pre-2026-07-28 clients. `rmcp` supports it (`legacy_session_mode`, default on), but the specification treats sessions as removed and Origin plus handle-hijacking risks come back. Cost: two behaviors to test.

Authorization.

- D. Resource server only, over the existing JWT authorization server. Matches ADR-0001. Needs the authorization server to issue an audience unique to this server. We write metadata, challenge and verification in axum, since `rmcp` offers none.
- E. D plus scope-based step-up for destructive tools, with the role claim mapped to scopes at issuance. Cost: depends on the authorization server issuing scopes.
- F. Ship a small built-in authorization server. The specification allows "hosted with the resource server", but it is a large surface (consent, registration, PKCE, rotation) and it gives up the deployment's existing identity setup.

Tool list.

- G. A static list built at startup from a deployment setting that decides which destructive tools exist. Simple, deterministic, `cacheScope: "public"`. Role-based refusal still happens in the Store through RLS.
- H. G plus per-request filtering by the role claim, with `cacheScope: "private"`. The list then shows each caller only what they may do. Cost: list code must read the verified claims, and tests must cover each role.
- I. Dynamic list with `listChanged`. Not needed unless the setting changes while the server runs.

### Recommendation

Take A plus B's stdio only if a local-agent use case appears, which the issue tracker should confirm. Start with Streamable HTTP, 2026-07-28 only, with `legacy_session_mode` off and `stateless_protocol_metadata_required` on, so there are no sessions to hijack. Do option D, and treat the audience requirement as the first thing to verify against the real authorization server, because everything else depends on it. Do option G now and H when roles differ in what they may call.

Concrete rules that follow from the sources:

1. Publish protected resource metadata and a full 401 challenge. Set `allowed_origins` and `allowed_hosts` in `rmcp`, because the defaults leave Origin validation off.
2. Verify one issuer, one audience, access tokens only, and return 5xx when the key set cannot be fetched. Never forward the inbound token. gnomon's upstream calls (model endpoints) use their own keys.
3. Annotate every tool. `readOnlyHint: true` and `openWorldHint: false` for Store reads, `destructiveHint: true` for erasure. Do not rely on a client honoring them, so enforce the same limits in the Store and the server.
4. Write each description for a keyword search over names and one-line text: lead with what the tool does and which Scope it acts on, state retention or expiry for any tool that returns a handle, and keep the order stable.
5. Return `outputSchema` and `structuredContent` for tools with structured results, and also a text block, as the specification asks for compatibility.
6. Keep the hidden destructive tool out of the list and also refuse it on call (`disable_route` does both).

## Not confirmed

- Whether `server/discover` may or must be served without a token. The discover page does not say. Clients can call it before any other request, so an unauthenticated 401 on it would block a client that probes first. Needs a test with the reference client.
- Whether any server-side authorization helper exists in `rmcp` outside the 3.11.0 crate (an unreleased branch, or a separate crate). I found none in the published crate or the examples listing.
- How popular clients behave against a protected resource whose authorization server cannot set an audience, or that lacks `code_challenge_methods_supported`. The tutorial warns clients refuse in the second case. Client behavior is not in the specification.
- Whether the target authorization server supports RFC 8707 `resource` and Client ID Metadata Documents. That depends on the product we pair with and is not in the specification.
- Official guidance on writing tool descriptions beyond the schema comment and the state-retention advice. I found none in the specification or the official docs I read.
- Official guidance for a deployment-level switch that hides a destructive tool. The specification permits varying lists by authorization and over time but does not mention switches.
- Exact `rmcp` behavior for a tool list that varies per request by token claims. The service injects request parts into extensions, so it looks possible, but I did not run it. The 3.6.0 entry "look up tool schemas per request" suggests the router reads per request, which I did not verify.
- Removal timing of the HTTP+SSE transport and of Dynamic Client Registration. The specification says "eligible for removal in a future revision" and gives a minimum twelve-month deprecation window ([changelog, governance](https://modelcontextprotocol.io/specification/2026-07-28/changelog)), with no date.
