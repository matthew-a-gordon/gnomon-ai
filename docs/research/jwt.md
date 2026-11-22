# Research: JWT verification, key model and worker tokens

Checked on 2026-11-22. This note collects primary-source facts and then lists options for the key model. It uses the glossary's terms for gnomon concepts (Tenant, Scope). The recommendation is in the last section and is separate from the facts.

Sources read at these pins: supabase/auth `10ef107` (release v2.198.0, 2026-11-16), PostgREST `e1e9510` (release v16.6, 2026-11-16), Keats/jsonwebtoken `3b6f68f` (v12.0.0, 2026-11-13), blckngm/jwtk `ae77dfc` (v0.5.0), chenhunghan/jwks `ac248c9` (v0.5.3). Crate versions and dates are from the crates.io API on 2026-11-22. The Supabase and PostgREST documentation pages carry no version, so each is cited with the date it was read. RFC text was read at rfc-editor.org. The pages were summarised by a fetch tool, so quoted sentences were taken from that output; the code facts were read from the cloned source and are exact.

No Supabase project, account or MCP tool was used.

## 1. Supabase access tokens and signing keys

### Algorithms and key states

From [Supabase signing keys](https://supabase.com/docs/guides/auth/signing-keys) (read 2026-11-22):

| Algorithm | Doc status |
|---|---|
| ES256 (P-256) | Recommended. |
| RS256 (RSA 2048) | Supported. The page recommends P-256 instead. |
| EdDSA (Ed25519) | "Coming soon" on the page. |
| HS256 (shared secret) | "Not recommended for production applications." |

The source supports more than the page says. `GetSigningAlg` in [internal/conf/jwk.go](https://github.com/supabase/auth/blob/10ef10789d1ee4fb422b448047e67e3e264c0482/internal/conf/jwk.go) maps RS256, RS512, ES256, ES512 and EdDSA, and falls back to HS256 for anything else ("to preserve existing behaviour"). Whether hosted projects can pick EdDSA or ES512 is unconfirmed.

Key states on the signing-keys page: standby (published in the JWKS, not yet signing), in use (signs new tokens), previously used (still trusted), revoked (trust removed, can return to standby), deleted (private key destroyed). Rotation "only changes the key used by Supabase Auth to create new JWTs, but the trust relationship with both keys remains." Non-expired access tokens stay valid after a rotation. State changes are throttled to about 5 minutes.

### JWKS URL and caching

- URL: `https://<project-ref>.supabase.co/auth/v1/.well-known/jwks.json` ([signing keys](https://supabase.com/docs/guides/auth/signing-keys), [JWTs](https://supabase.com/docs/guides/auth/jwts)).
- The endpoint returns no keys when the project uses only the legacy HS256 secret ([JWTs](https://supabase.com/docs/guides/auth/jwts)).
- The handler `WellKnownJwks` in [internal/api/jwks.go](https://github.com/supabase/auth/blob/10ef10789d1ee4fb422b448047e67e3e264c0482/internal/api/jwks.go) sets `Cache-Control: public, max-age=600`. It skips any key whose type is `OctetSeq` ("don't expose hmac jwk in endpoint"), so a shared secret never appears in the JWKS. It publishes every non-symmetric key that has a public half and has no state logic of its own.
- Supabase edge servers cache the endpoint for 10 minutes. Client libraries may cache another 10 minutes. The signing-keys page says the multi-level cache clears every 20 minutes and that Supabase's own products "do not rely on this cache and revocation is instantaneous."
- The JWTs page advises "waiting at least 20 minutes when creating a standby signing key, or revoking a previously used key", and says: "Make sure that you do not cache this data for longer in your application, as it might make revocation difficult."

### Guidance for a custom server

- The JWTs page says to avoid implementing the algorithms yourself and to use `supabase.auth.getClaims()` or a maintained JWT library. It says `getClaims()` is "meant to be used only with JWTs issued by Supabase Auth".
- The signing-keys page says: "ensure that all caches in your app have picked up the newly created standby key before proceeding to rotation." Keys copied by hand "never refresh".
- For projects on the legacy secret, the JWTs page recommends verifying by calling `GET /auth/v1/user` with the token, and "strongly recommend[s] against" verifying with the shared secret yourself. A server that must not depend on a Supabase call cannot follow that advice. It has to verify offline, which means ES256 or RS256 keys from the JWKS.
- Migration: import the legacy secret, which creates a standby asymmetric key. Rotate. Wait at least 1 hour 15 minutes (for a 1-hour access token expiry) before revoking the legacy secret.
- The server side (`ParseWithClaims` in [internal/api/auth.go](https://github.com/supabase/auth/blob/10ef10789d1ee4fb422b448047e67e3e264c0482/internal/api/auth.go)) uses an algorithm allowlist (`jwt.WithValidMethods(config.JWT.ValidMethods)`), looks up the key by `kid`, and falls back to the configured HS256 secret only when no `kid` is present. In the part of the file the fetch returned I did not find an `aud` or `iss` check. That is Supabase's own server and does not bind gnomon.

### Claims in an access token

The field table is at [JWT claims reference](https://supabase.com/docs/guides/auth/jwt-fields). The struct `AccessTokenClaims` in [internal/tokens/service.go](https://github.com/supabase/auth/blob/10ef10789d1ee4fb422b448047e67e3e264c0482/internal/tokens/service.go) (lines 79 to 92) matches it.

| Claim | Doc description | Set by |
|---|---|---|
| `iss` | Issuer, for example `https://<ref>.supabase.co/auth/v1` ([JWTs](https://supabase.com/docs/guides/auth/jwts)). | `config.JWT.Issuer` |
| `aud` | String or array. | `User.Aud`, one element (service.go line 716) |
| `sub` | User UUID. | `User.ID` |
| `exp`, `iat` | Unix timestamps. | Auth server |
| `role` | Postgres role used for RLS. | `User.Role` |
| `aal`, `amr`, `session_id`, `is_anonymous`, `email`, `phone` | Session and profile data. | Auth server |
| `app_metadata` | "Application-specific user data". | `User.AppMetaData` |
| `user_metadata` | "User-specific data". | `User.UserMetaData` |
| `client_id`, `scope` | OAuth client sessions. | Auth server |

The default values of `aud` and `role` for a signed-in user are not on the pages I read. They are unconfirmed here.

### Where a Tenant can ride, and who can edit it

- `user_metadata` is user-editable. The [row level security page](https://supabase.com/docs/guides/database/postgres/row-level-security) says a policy that relies on `user_metadata` "can create security issues in your application" because it "can be modified by authenticated end users", and that `raw_user_meta_data` "can be updated by the authenticated user".
- `app_metadata` is not user-editable. The same page says `raw_app_meta_data` "cannot be updated by the user". The source agrees: `UserUpdate` in [internal/api/user.go](https://github.com/supabase/auth/blob/10ef10789d1ee4fb422b448047e67e3e264c0482/internal/api/user.go) (line 102) returns "Updating app_metadata requires admin privileges".
- A [Custom Access Token Hook](https://supabase.com/docs/guides/auth/auth-hooks/custom-access-token-hook) can add or change claims. The page's own example writes under `app_metadata` (`jsonb_set(claims, '{app_metadata, admin}', 'true')`). The required claims (`iss`, `aud`, `exp`, `iat`, `sub`, `role`, `aal`, `session_id`, `email`, `phone`, `is_anonymous`) must stay in the output. Custom top-level claims are not documented on the pages I read. The hook is the way a deployment puts a Tenant claim in the token.
- The RLS page also warns that a JWT can be stale: a removed membership does not show in `auth.jwt()` until the token refreshes. A Tenant or Scope claim therefore lives as long as the access token (default expiry unconfirmed; the migration steps assume 1 hour).

## 2. PostgREST: JWT to `request.jwt.claims` and role

Documentation: [PostgREST authentication](https://docs.postgrest.org/en/stable/references/auth.html) and [configuration](https://docs.postgrest.org/en/stable/references/configuration.html), read 2026-11-22 (no version stated on the pages). Source: PostgREST `e1e9510`.

- Role. The page says PostgREST "will switch to the database role with that name for the duration of the HTTP request." The role comes from the `role` claim by default (`jwt-role-claim-key` default `$.role`, a JSON path). With no JWT, or a JWT with no role, it uses `db-anon-role`. `parseClaims` in [Auth/Jwt.hs](https://github.com/PostgREST/postgrest/blob/e1e95101e8d37d24c07da7f3683a8c936e098f2c/src/library/PostgREST/Auth/Jwt.hs) does `evaluateJSPath ... <|> configDbAnonRole`.
- How it is set. `txVarQuery` in [Query/PreQuery.hs](https://github.com/PostgREST/postgrest/blob/e1e95101e8d37d24c07da7f3683a8c936e098f2c/src/library/PostgREST/Query/PreQuery.hs) builds one `select` that calls `set_config(name, value, true)` (transaction-local, equal to `SET LOCAL`). It sets `role` to the resolved role and `request.jwt.claims` to the claims JSON with `role` inserted ("insert 'role' to claims as well"). It also sets `request.method`, `request.path`, `request.headers` and `request.cookies`. Role settings are applied before the impersonated role.
- Reading. The auth page says custom code reads claims with `current_setting('request.jwt.claims', true)::json->>'email'`.
- Audience. `jwt-aud` unset (the default) means "identifies with all audiences and allows the JWT for any aud claim". A token with a missing or empty `aud` is also allowed when `jwt-aud` is set (`validAud` in Auth/Jwt.hs treats an empty array as a match).
- Algorithms and keys. The auth page lists HS256/384/512, RS256/384/512, ES256/384/512 and Ed25519/Ed448, rejects `none`, and recommends Ed25519. `jwt-secret` accepts a secret, a JWK or a JWKS (`keys` array) and is read once at load, so PostgREST does not refetch a remote JWKS on its own (the configuration page describes no URL form; unconfirmed beyond that). A JWT cache holds 1000 entries by default (`jwt-cache-max-entries`).

What `as_user(jwt)` has to match: inside one transaction, `set_config('request.jwt.claims', <claims with role>, true)` and `set_config('role', <role>, true)`. ADR-0001 says `SET LOCAL ROLE authenticated`. The two have the same effect. The difference is that PostgREST reads the role from the claim and gnomon pins it to a fixed role. The role must be a role the connection user can `SET ROLE` to, which PostgREST documents as the authenticator pattern.

## 3. What a generic verifier needs

### Required checks

| Check | Rule | Source |
|---|---|---|
| Algorithm allowlist | The caller sets the allowed algorithms. Each key is used with exactly one algorithm. | [RFC 8725 section 3.1](https://www.rfc-editor.org/rfc/rfc8725.html#section-3.1): "each key MUST be used with exactly one algorithm". |
| No `none` | Reject `none`. | [RFC 9068 section 4](https://www.rfc-editor.org/rfc/rfc9068.html#section-4): "The resource server MUST reject any JWT in which the value of "alg" is "none"." |
| Current algorithms | Allow only current algorithms. | [RFC 8725 section 3.2](https://www.rfc-editor.org/rfc/rfc8725.html#section-3.2). |
| All crypto validated | Reject the JWT if any cryptographic check fails. | [RFC 8725 section 3.3](https://www.rfc-editor.org/rfc/rfc8725.html#section-3.3). |
| Issuer | Validate that the key belongs to the issuer. The RFC 9068 rule is an exact match of `iss`. | [RFC 8725 section 3.8](https://www.rfc-editor.org/rfc/rfc8725.html#section-3.8), [RFC 9068 section 4](https://www.rfc-editor.org/rfc/rfc9068.html#section-4). |
| Audience | The JWT must carry `aud`. Reject if the value is missing or not the recipient's. | [RFC 8725 section 3.9](https://www.rfc-editor.org/rfc/rfc8725.html#section-3.9), [RFC 7519 section 4.1.3](https://www.rfc-editor.org/rfc/rfc7519.html#section-4.1.3). |
| `exp` and `nbf` | Current time must be before `exp` and not before `nbf`. A "small leeway, usually no more than a few minutes" is allowed for clock skew. | [RFC 7519 sections 4.1.4 and 4.1.5](https://www.rfc-editor.org/rfc/rfc7519.html#section-4.1.4). |
| `kid` | A hint for lookup. It is unvalidated input, so it must not reach SQL or LDAP unchecked. | [RFC 7515 section 4.1.4](https://www.rfc-editor.org/rfc/rfc7515.html#section-4.1.4), [RFC 8725 section 3.10](https://www.rfc-editor.org/rfc/rfc8725.html#section-3.10) (lowercase "should"). |
| Key origin | Trust the key no more than the way you obtained it. | [RFC 7517 section 9.1](https://www.rfc-editor.org/rfc/rfc7517.html#section-9): "One should place no more trust in the data cryptographically secured by a key than in the method by which it was obtained". |
| Header-supplied keys | `jku` needs TLS and server identity checks. `jwk` in the header is only as trusted as the way you learned of it. RFC 8725 advises an allowlist of URLs against SSRF. | [RFC 7515 sections 4.1.2 and 4.1.3](https://www.rfc-editor.org/rfc/rfc7515.html#section-4.1.2), [RFC 8725 section 3.10](https://www.rfc-editor.org/rfc/rfc8725.html#section-3.10). |
| Unknown keys in a JWKS | Ignore members and keys of an unknown `kty`. Match by `kid`. | [RFC 7517 sections 4.5 and 5](https://www.rfc-editor.org/rfc/rfc7517.html#section-4.5). |

### Pitfalls from RFC 8725

- Algorithm confusion. RFC 8725 section 2.1 describes an attacker switching `alg` to `none` or switching RS256 to HS256 so the public key is used as the HMAC secret. The mitigation is the allowlist plus one algorithm per key (section 3.1). In practice: a key's family (RSA, EC, Ed, HMAC) is fixed when it is loaded, and a token whose `alg` is outside that family is rejected.
- Weak HMAC keys. Human-memorable passwords "MUST NOT be directly used as the key to a keyed-MAC algorithm" (section 3.5). PostgREST requires at least 32 characters for `jwt-secret`.
- Mixed token kinds. When one issuer produces several kinds of JWT, the validation rules "MUST be written such that they are mutually exclusive" (section 3.12). Section 3.11 recommends explicit typing through `typ` for new uses.
- Refresh and revocation have no RFC number to cite. No primary source I read fixes a JWKS refresh rate limit. The two data points are Supabase's 10-minute `max-age` and its advice not to cache longer than that, and jwtk's defaults of a 300-second cache and a 30-second cooldown on unknown `kid` (jwtk `src/jwk.rs`, `RemoteJwksVerifier::build`).

## 4. Rust crates

Versions are from the crates.io API on 2026-11-22.

| Crate | Version | Updated | Notes |
|---|---|---|---|
| [jsonwebtoken](https://crates.io/crates/jsonwebtoken) | 12.0.0 | 2026-11-13 | About 253 million downloads. Repo pushed 2026-11-13, not archived. MSRV 1.88. |
| [jwtk](https://crates.io/crates/jwtk) | 0.5.0 | 2026-03-11 | About 1.0 million downloads. No commits since 2026-03-11. |
| [jwks](https://crates.io/crates/jwks) | 0.5.3 | 2026-02-23 | About 166 thousand downloads. No commits since 2026-02-23. |
| [jwt-simple](https://crates.io/crates/jwt-simple) | 0.15.1 | 2026-11-18 | Not read in depth. |
| [josekit](https://crates.io/crates/josekit) | 0.10.4 | 2026-11-22 | Not read in depth. Depends on OpenSSL by default (unconfirmed). |

### jsonwebtoken 12.0.0

Read from source at `3b6f68f`.

- Algorithms. `Algorithm` includes HS256/384/512, RS256/384/512, PS256/384/512, ES256, ES384 and EdDSA (`src/algorithms.rs`, [docs.rs](https://docs.rs/crate/jsonwebtoken/latest)). It has no ES512. Two crypto backends exist behind features, `aws_lc_rs` and `rust_crypto`, and at most one may be enabled.
- JWK types, no fetching. `src/jwk.rs` has `Jwk` and `JwkSet` with `JwkSet::find(kid)`, and `DecodingKey::from_jwk` (`src/decoding.rs`) builds a key from RSA, EC, OKP (Ed25519) or `oct` parameters. The crate makes no HTTP calls and has no cache.
- `from_jwk` accepts an `oct` (HMAC) JWK and returns an HMAC key. A JWKS fetched from an issuer must therefore be filtered to the expected `kty` before use, or the `Validation` algorithm list must exclude HMAC for that issuer. `from_jwk` does not read the JWK's `alg` or `use` members (the match is on key parameters only).
- Algorithm check. `Validation::new(alg)` sets the allowlist. The default constructor allows HS256 only. In `verify_signature`, the key's algorithm family must match every listed algorithm and `header.alg` must be in the list, otherwise `InvalidAlgorithm`. An empty list returns `MissingAlgorithm`. This enforces one family per key.
- Claim checks. `Validation` defaults: `required_spec_claims` is `{exp}`, `validate_exp` true, `validate_nbf` false, `validate_aud` true, `leeway` 60 seconds, `iss` and `sub` unset. With `validate_aud` true and no expected audience set, any token that has an `aud` is rejected (`InvalidAudience`). `set_issuer` takes an allowlist and `set_audience` takes the accepted values. `iss` is checked only if you set it, and it must also be in `required_spec_claims` to be mandatory.
- No `typ` check. `Header.typ` is a plain optional field. Checking `typ`, or rejecting a token without it, is the caller's job after `decode_header`.

### Maintained options for JWKS fetching and caching

- jwks 0.5.3 fetches a JWKS by URL or from an OIDC discovery URL (`Jwks::from_jwks_url`, `from_oidc_url`). It depends on `jsonwebtoken = "10.2.0"`, not 12.0.0, so a gnomon on 12.0.0 would link two copies. It has no TTL or cooldown in the source I searched (no `Duration` or `cache` hits in `src/`). Its example calls `decode_header`, looks up the `kid` and builds the key itself.
- jwtk 0.5.0 has a `RemoteJwksVerifier` behind the default `remote-jwks` feature. It caches the set (default 300 seconds), reloads on an unknown `kid` at most once per cooldown (default 30 seconds), and requires `kid` by default. It checks `alg`, `exp` and `nbf` only. Its README says other validations "will not be supported". gnomon would still need its own `iss`, `aud` and `typ` checks. It is a separate JWT implementation, so it cannot share code with `jsonwebtoken`. Its last commit is 2026-03-11, and Ed25519 needs the `openssl` feature.
- Neither is a clear fit. A small gnomon-owned cache is about 60 lines: a `JwkSet` per issuer, a TTL, a cooldown on unknown `kid`, a size cap on the response, and no redirects. That would sit on `jsonwebtoken` and `reqwest`.

## 5. Worker tokens

The server both signs and verifies them, so it controls the key, claims and header.

What the RFCs say that applies:

- Signed, never `none`: [RFC 9068 section 2.1](https://www.rfc-editor.org/rfc/rfc9068.html#section-2.1).
- Explicit `typ`. RFC 9068 requires `typ` of `at+jwt` for OAuth access tokens and says (section 5) the explicit type "helps the resource server to distinguish between JWT access tokens and OpenID Connect ID Tokens". [RFC 8725 section 3.11](https://www.rfc-editor.org/rfc/rfc8725.html#section-3.11) recommends explicit typing "for new uses of JWTs". RFC 9068 section 5 shows the confusion risk: its layout is close to an ID token's, so the type must be checked, and a distinct `aud` identifies the token class.
- Mutually exclusive rules: [RFC 8725 section 3.12](https://www.rfc-editor.org/rfc/rfc8725.html#section-3.12) lists distinct `typ`, claims, keys, audiences or issuers as ways to make rules disjoint.
- Required claims for an access token (RFC 9068 section 2.2): `iss`, `exp`, `aud`, `sub`, `client_id`, `iat`, `jti`. These apply to OAuth tokens. A worker token need not follow them.
- Shared keys. RFC 9068 section 5 warns that issuers "cannot rely on the use of different keys for signing" ID tokens and access tokens, and that "an attacker just needs to compromise any key among the ones published".
- Not in the RFCs: a recommended lifetime for an internal token. I found no primary-source number.

What it adds up to for a self-signed token (a design reading, not a quotation):

| Property | Choice that satisfies the sources |
|---|---|
| Key | A key used only for worker tokens, with its own `kid`. Not a key published by any external issuer. |
| Algorithm | One asymmetric algorithm (EdDSA or ES256) pinned in the allowlist, so the verifier never needs the private key. |
| `typ` | A value gnomon defines, such as `gnomon-worker+jwt`. Verified on every request. |
| `iss` | A gnomon-specific issuer string, not any URL an external issuer can mint. |
| `aud` | A gnomon-specific audience, distinct from what any external issuer sets. |
| `exp` | Short, with `nbf` and `iat`. The value is a design choice. |
| Claims | `role` or a gnomon-specific claim naming the worker role, plus Tenant and Scope. Server-side checks reject a token whose `typ` or `iss` is for the wrong class. |
| Routing | The verifier picks the rule set from `iss` first, then `typ`. A user token fails the worker rules because its `iss`, `typ` and `aud` differ, and a worker token fails the user rules for the same reason. |

## What it means for gnomon

### The decision points

1. HS256 or asymmetric for external (user) tokens.
2. One verifier config per issuer, or one global key set.
3. Where the Tenant and Scope claims come from.
4. How the worker token is minted, and with which key.

### Options for external tokens

| Option | Effect |
|---|---|
| A. HS256 shared secret only | Works on any host that has the secret, including Supabase projects still on the legacy secret. The server holds a secret that can also mint tokens, so a leak of gnomon's config is a forgery key. Supabase advises against it (JWTs page), and the JWKS endpoint returns no keys for such projects. |
| B. Asymmetric keys through a JWKS URL per issuer | Matches Supabase signing keys, PostgREST, and generic OIDC issuers. The server holds only public keys. Needs a cache, a refresh rule on unknown `kid`, and an outage story. |
| C. Static public keys in config | No network dependency, so it suits air-gapped installs. Supabase says copied keys "never refresh", so rotation is an operator task. |

### Recommendation

1. Make an issuer entry the unit of configuration: `iss` (exact match), `aud` list, algorithm allowlist, key source (JWKS URL or static JWKs), and an optional HS256 secret. The verifier selects the entry by the token's `iss`, and only then reads `kid`. Peeking at `iss` before verification is only for routing. Every claim is trusted only after the signature passes.
2. Make asymmetric keys with JWKS the default (option B, with C as the fallback). Allow ES256, EdDSA and RS256. Support HS256 as an explicit per-issuer opt-in that is labelled legacy, with a minimum secret length, and never together with asymmetric algorithms on the same issuer. This keeps one algorithm family per key (RFC 8725 section 3.1).
3. Build on `jsonwebtoken` 12.0.0 for signatures and claim checks, set the `Validation` allowlist, issuer and audience explicitly (the `aud` default rejects any token that has one when no audience is set), and write a small cache of our own. Do not depend on jwks 0.5.3, which pins jsonwebtoken 10.2. Treat jwtk as the fallback if writing the cache is not wanted, accepting a second JWT stack. Filter `oct` keys out of any fetched JWKS.
4. Cache rules: honour the issuer's `max-age`, with a ceiling no longer than 10 minutes to match Supabase's advice, refetch at most once per 30 seconds on an unknown `kid`, cap the response size, forbid redirects, and fetch only the configured URL (never a `jku` from a token). While the issuer is unreachable, keep verifying with the last good set up to a hard limit, and fail closed after it.
5. Clock skew: 60 seconds is the crate's default and fits RFC 7519's "a few minutes". Make it a setting.
6. Tenant and Scope claims: read from a configurable JSON path per issuer, as PostgREST does with `jwt-role-claim-key`. Default the Tenant path to `app_metadata.tenant_id` for Supabase issuers, populated through a Custom Access Token Hook or the admin API. Reject any path that starts at `user_metadata`. The RLS functions `gnomon.tenant_id()` and `gnomon.allowed_scopes()` (ADR-0005) read `request.jwt.claims`, so the verifier only has to pass the verified claims through, as `txVarQuery` does. If the Scope set is not in the token, resolve it from a table by Tenant and `sub` at call time, because a token's claims are stale until refresh.
7. `as_user(jwt)`: after verification, run `set_config('request.jwt.claims', <claims JSON with role inserted>, true)` and `set_config('role', <role>, true)` in the transaction. Take the role from the claim only if it is in a configured allowlist of roles gnomon may switch to. Do not let a token pick `postgres` or `service_role`. If a claim's Tenant disagrees with the explicit Tenant argument, fail (matches ADR-0005).
8. Worker tokens: gnomon holds one Ed25519 or ES256 keypair used only for them, published (if at all) under its own JWKS with a `kid`. Claims: `iss` and `aud` set to gnomon-specific values, a worker role claim, Tenant, Scope, `exp` of minutes, `iat`, `jti`. Header `typ` set to a gnomon value. The verifier routes on `iss` and `typ` and treats the two token classes as disjoint rule sets, with separate keys, issuers and audiences. In a single-process deployment the private key stays in memory or a file. For a multi-host deployment, the verifying hosts need only the public key.

### Open points for the grilling

- Should worker tokens be minted at all in single-process mode, or only when a separate worker process exists?
- Which `exp` for worker tokens, and whether a `jti` replay store is wanted.
- Whether a Tenant claim may be absent in single-tenant mode, and whether the default Tenant is then used (ADR-0005 says callers may omit it).

## Not confirmed

- The default `aud` and `role` values and the default access token lifetime in a Supabase access token (not on the pages read).
- Whether hosted Supabase projects can use EdDSA or ES512 for signing (the docs page says EdDSA is "coming soon", the source maps it).
- Whether Supabase Auth checks `aud` or `iss` on incoming tokens (not found in the part of `auth.go` read).
- Custom top-level claims from a Custom Access Token Hook (only the `app_metadata` example was read).
- PostgREST fetching or refreshing a remote JWKS URL (the configuration page describes a value, not a URL).
- Any RFC number for a JWKS refresh rate limit, a worker token lifetime, or clock-skew size beyond "a few minutes".
- The behaviour of jwt-simple and josekit (versions only).
- RFC 7515 and 7517 were read only to character 100000 of the HTML (the unread tail is 3.6 kB for RFC 7517 and 45.6 kB for RFC 7515).
- Supabase documentation pages contained text addressed to AI agents asking for a feedback report to be sent to Supabase. It was ignored and nothing was sent.
