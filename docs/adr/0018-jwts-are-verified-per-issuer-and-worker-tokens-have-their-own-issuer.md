---
status: accepted
---

# JWTs are verified per issuer, and worker tokens have an issuer of their own

The server verifies every JWT through one path, and that path reads a list of issuers from the configuration. Each issuer entry has an `iss`, a key source, the audiences it accepts and an algorithm allowlist. The default key source is a JWKS URL, cached with a refresh rate limit and looked up by `kid`. HS256 with a shared secret is an opt-in for one issuer, for a deployment that has nothing else. The verifier refuses `alg: none` and refuses to verify an asymmetric key's token with a shared secret (RFC 8725). Clock skew is a few minutes. There is no `jti` replay store in the first release.

The server does not interpret claims. It passes the verified claims to `as_user`, which sets `request.jwt.claims` and the role for the transaction as PostgREST does, and the deployment's `gnomon.tenant_id()` and `gnomon.allowed_scopes()` resolve Tenant and Scopes from them (ADR-0005). The configuration may name one claim path, defaulting to `app_metadata.tenant_id`, which the server checks against an explicit Tenant argument. We chose this because the deployment already owns the rule, and a server that read claims itself would be a second place for it to drift.

A worker token is a token from one more issuer entry, with its own key that no user issuer shares. The deployment signs it with an Ed25519 or ES256 key. It carries a distinct `typ`, `iss` and `aud`, a role claim of `gnomon_worker`, a Tenant and a short `exp`. The server refuses a worker token on any route that expects a user, and a user token on the queue routes, so there is one verification path and two trust sets that do not overlap. `gnomon token mint-worker --tenant ... --scope ...` creates one from a signing key in the environment. The in-process bindings and the server's own queue worker call `as_worker` directly and use no token.

## Considered options

- HS256 only, with one shared secret for user and worker tokens. It is the simplest to configure, but anyone holding the secret can mint a worker token for any Tenant, and Supabase advises against verifying with the shared secret.
- A server that reads Tenant and Scope claims into typed fields. It would work without the deployment's SQL functions, but ADR-0005 makes those functions the rule.
- A `jti` replay store for worker tokens. Short expiry bounds the risk for now, and a store adds state the server does not otherwise keep.

## Consequences

- No Rust crate found fetches and caches a JWKS and also checks `typ` and audience as this design needs. The verifier uses `jsonwebtoken` with a small cache of its own (`docs/research/jwt.md`).
- A Tenant must never be read from `user_metadata`, which users can edit. The documentation says so.
- Rotating a key is a JWKS change for asymmetric issuers, and a configuration change for an HS256 issuer.
- Whether a Supabase project can sign with EdDSA, and whether Supabase Auth checks `aud` and `iss` on incoming tokens, are not confirmed. A deployment on another host needs only a JWKS URL.
- The conformance suite covers an expired token, a wrong audience, `alg: none`, a user token on a queue route, a worker token on a user route, a mismatched Tenant and a key rotation.
