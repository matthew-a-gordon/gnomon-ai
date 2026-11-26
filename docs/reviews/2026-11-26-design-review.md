# Design review: ADRs, requirements and dependencies

Reviewed on 2026-11-26 at `c42ce73`, before work starts on issues #72 to #89 and on the Pipeline (#23) and bindings and server (#49) specs.

Sources read: `GLOSSARY.md`, ADR-0001 to ADR-0020, specs #1, #23 and #49, open issues #72 and #80 to #89, the research notes in `docs/research/`, `Cargo.toml`, `src/connect.rs`, `src/store.rs` and parts of the migrations. Nothing was run. Numbers quoted here come from the ADRs and issues, not from new measurements.

## How to read this

Each decision and requirement is judged on three questions.

| Question | Values |
|---|---|
| Practical or theoretical | **Practical**: a consumer will meet it in normal use. **Edge case**: real, but rare. **Theoretical**: no consumer or measurement asks for it yet. |
| Idiomatic PostgreSQL and Rust | **Yes**, **Partly** (sound, with a non-standard part), or **No**. |
| Best practice | **Aligned**, **Stricter** (goes beyond common practice on purpose), or **Divergent**. |

"Stricter" is not a criticism. Several of gnomon's decisions are stricter than common practice because the product promises more than most memory stores do.

## The gnomon solution in one page

gnomon is a bi-temporal knowledge graph for agentic applications, held in plain Postgres. The decisions fit together as five layers.

1. **One implementation, several ways in (ADR-0001, ADR-0016, ADR-0017, ADR-0019).** A Rust crate holds the Store API and the Pipeline API. Python and Node bindings run it in the consumer's process, and one server binary serves HTTP, MCP and the queue worker. The Pipeline is prompt-heavy logic, and writing it once is the only way to keep it from drifting between languages.

2. **The database holds the guarantees (ADR-0005, ADR-0006, ADR-0009).** Every table has a Tenant and a Scope, and row-level security checks both on every row. The guarantees live in SQL functions, policies, grants and constraints, shipped as migrations the consumer applies through its own chain. A caller who reaches the database with raw SQL gets the same isolation as one who goes through the crate. The deployment supplies `gnomon.tenant_id()` and `gnomon.allowed_scopes()`, so each consumer maps its own claims to Tenants and Scopes.

3. **History is computed, not stored as final values (ADR-0003, ADR-0004, ADR-0007, ADR-0008).** An Episode is an immutable source record. Each value an Episode states is a Contribution. A Version's values are chosen from the Contributions that remain, by Authority, Reference time and Recorded time. Each Version has a Valid time and a Recorded time, stored as four scalar columns, with a precision on each Valid-time bound. Because values are computed, Erasure can delete an Episode's Contributions and recompute every Version it touched, current and closed, and the result is the same as if the Episode had never existed.

4. **Search is exact under row-level security (ADR-0010, ADR-0014, ADR-0015).** Search fuses a vector branch, a full-text branch and a graph-neighbourhood branch with reciprocal rank fusion. Each branch runs as the caller under RLS. Because Postgres will only use a leakproof condition as an index condition under RLS, the design avoids HNSW and GIN and uses B-tree shapes: a binary-quantized scan with a full-precision re-rank, and a token table with BM25 computed from the caller's own Scopes. Summaries and Communities belong to one Scope and carry the instants they describe, so no Search leaks another Scope's words or presents current prose as past fact.

5. **The Pipeline is resumable and stores no model output (ADR-0011, ADR-0012, ADR-0013).** `add_episode` runs its stages, and each stage commits its output with its stage record in one transaction. The stage table is also the queue. Resolution never merges Nodes; it records `same_as` and `different_from` Edges. A Sentence is written from the committed Version, never from extraction output, so it cannot state a value the Store's merge rejected or an Erasure removed.

Testing (ADR-0020) runs every suite against containerised Postgres images with the same statement mode the Store uses, and an independent oracle checks merge and Erasure results.

## Findings to act on before #72 to #89 and specs #23 and #49

These are ordered by how much later work depends on them.

1. **Search misses its budget on realistic queries, and `context` is built on it.** ADR-0010 says the fused Search meets 100 ms at p95 for 50,000 visible rows. #87 shows that a natural-language question takes 526 ms there and 1,820 ms at 200,000 rows, even with #84 applied. The earlier pass rested on a suite whose query word appears in one text (#79). `context` (#41) sends natural-language text, so this is the normal case, not an edge case. #86 removes the per-row As-of lookup, which was about 270 ms of a 391 ms call, but scoring alone is near the budget. Standard information-retrieval practice is to limit the work spent on very common terms: skip terms above a document-frequency cutoff, or score only the rarest few query terms, or prune with a max-score bound. Each of these changes ranking slightly, so it needs a recorded decision. I recommend taking that decision before #41 starts, and adding it to #87.

   *Status: decided. ADR-0010 records it as planned (#90), and #87 builds it.*

2. **The requirement documents no longer describe the requirements.** Spec #1 stories 20 to 23, 71, 76 (adjacency rows), 79 (the vacuum call) and 87 (named statements) are replaced by ADR changes or by #80, #81, #82 and #86. Spec #49 still names `gnomon migrations export` in the server binary and the vacuum call in story 83 and in its HTTP section. Someone reading the specs to learn what gnomon must do will read requirements that were withdrawn. A short "Superseded" list at the top of each spec, naming the story and what replaced it, fixes this at low cost.

   *Status: done. Specs #1 and #49 gained a Superseded list on 2026-11-26. Spec #23 gets one when finding 5 or 6 replaces a story.*

3. **The TLS crypto provider is unrecorded and will conflict.** `src/connect.rs` builds rustls with `ring`. jsonwebtoken 12, chosen in ADR-0018, offers only `aws_lc_rs` or `rust_crypto`. reqwest and rmcp also pull in rustls. Without a decision, the server ends up with two or three crypto stacks, and any dependency that calls `ClientConfig::builder()` without a provider can panic when more than one provider feature is on. Record one choice in an ADR before #53 and #54. aws-lc-rs is the rustls default and builds statically on musl with a C toolchain and CMake. ring avoids CMake but would leave jsonwebtoken on `rust_crypto`, a second implementation of the same algorithms.

   *Status: decided. ADR-0021 chooses aws-lc-rs and bans `ring` (branch `docs/adr-0021-one-crypto-provider`). #82 switches the features, and #53, #54 and #88 carry the rest. aws-lc-rs needs no CMake for non-FIPS builds.*

4. **ADR-0009 should be rewritten, not amended.** Its main rejected option says that with `SECURITY DEFINER` functions "RLS policies for caller roles stop applying". That is true only when the owner owns the tables or has `BYPASSRLS`. A definer function owned by a `NOLOGIN`, `NOBYPASSRLS` role that owns no tables still runs under RLS, as #81 says. #81 and #83 replace most of the guard triggers and the `gnomon.erasure_reach` policies with that pattern. It is the more common PostgreSQL design, and the result is shorter and easier to audit. Since three tickets change ADR-0009, write it once, from the final design, when #83 lands.

   *Status: decided. #83 rewrites ADR-0009 in place, and corrects ADR-0010's rejected definer option. #81 adds a note only. See the comments on #81 and #83.*

5. **The Formatter and TokenCounter callbacks have no consumer.** ADR-0016 exposes five seams to the bindings. The model client, embedder and reranker callbacks are practical: they let a consumer use a provider gnomon has no adapter for. The two sync callbacks are not asked for by any story outside #49 itself, and the bindings research recommended a narrower set. A simpler design returns typed rows from `context` alongside the formatted text, so a consumer who wants another format builds it in its own language with no FFI round trip per row. I recommend deferring #60.

   *Correction: the bindings research does not recommend a narrower set. Its option 3 (`docs/research/bindings.md`) would expose callbacks only for the Formatter and the TokenCounter, and its recommendation does not choose among the callback options. The case for deferring rests on the missing consumer, not on the research.*

   *Status: decided. ADR-0016 keeps the `ModelClient`, `Embedder` and `Reranker` callbacks and defers the other two. `context` returns the selected rows alongside the text, in the same order, through the bindings and the HTTP API; MCP returns the text only. The Rust traits stay public. #60 is closed, with the conditions for reconsidering in `.out-of-scope/binding-formatter-and-token-counter-callbacks.md`. Spec #49 lists stories 23 to 25 as superseded, and #41 and #42 carry the change. No story of spec #23 is replaced.*

6. **The Leiden crate is chosen for one feature that has a library-free alternative.** `leiden-rs` is chosen because it alone offers a warm start, which keeps Community membership stable between refreshes. Its source is on gitcode and its maintenance could not be checked. Stability can also be had by running any seeded detector from cold and matching the new Communities to the stored ones by member overlap (Jaccard similarity), keeping the old ID where the overlap is high. That keeps the stability requirement (story 70 of #23) and removes the reason to depend on one weakly maintained crate. The `CommunityDetector` trait already allows either.

   *Correction: neither design keeps a Community's ID on its own. `leiden-rs`'s warm start renumbers the partition, so the stored IDs must be matched to the new partition in both. Overlap matching keeps the ID but not the members, and story 72 regenerates a Community Summary whenever the member set changes, so a cold run may cost more main-model calls. The alternative crates are no better maintained than `leiden-rs`.*

   *Status: decided. A Community keeps its identity while most of its members stay (`GLOSSARY.md`). The Pipeline matches above the `CommunityDetector` trait, by Jaccard similarity strictly above 0.5, and the detector is vendored into the repository. #91 measures warm start against a cold run with a fixed seed, and keeps the warm start only if it regenerates at least a third fewer Communities. ADR-0022 records the result. #38 is blocked by #91, and spec #23 lists story 70 as superseded.*

7. **No supply-chain check runs in CI.** #88 adds `fmt` and `clippy`. Add `cargo deny` (advisories, licence allowlist for Apache-2.0 compatibility, duplicate crates) to the `lint` job. It catches finding 3 automatically and costs seconds.

   *Status: decided. ADR-0021 had already committed to `cargo deny` banning `ring` in `lint`, with no ticket to carry it. #92, blocked by #82 and #88, adds it. `lint` runs bans, licences and sources, which depend only on `Cargo.lock`. Advisories run in a pull request job that is not required, and weekly. Licences are a permissive allowlist with per-crate exceptions, duplicates warn, and only crates.io is allowed. #92 updates ADR-0020 when it lands.*

8. **btree_gist was avoided at a readability cost.** The exclusion constraint on Versions writes Tenant and Node equality as overlap of one-point ranges over a custom `gnomon.uuid_range` type, because btree_gist was judged outside the floor. btree_gist is a contrib module marked trusted since PostgreSQL 13, and I expect it on every host the spec names, though that should be checked per host. If it is available, the #80 squash is the cheapest moment to adopt it and write the constraint the usual way. If it is not, keep the current form, which is correct.

   *Correction: the rule that kept btree_gist out, spec #1's "no extension beyond pgvector", was not a requirement the consuming applications have. The requirement is the hosts.*

   *Status: decided. ADR-0023 records the rule: gnomon must run on Supabase and DigitalOcean (both editions), and RDS, Aurora, Cloud SQL, AlloyDB, Azure Flexible Server and Neon are desirable. Only a required host blocks a feature. All eight hosts offer btree_gist, so the floor becomes PostgreSQL 17, pgvector 0.7.0 and btree_gist. #80 adopts it in its second commit and removes `gnomon.uuid_range`. Spec #1 lists story 8 as superseded, and ADR-0006 points to ADR-0023.*

## ADRs

### ADR-0001: one Rust core with bindings and a server

The Store and Pipeline are written once in Rust. Bindings run them in-process and a server runs them as a service. The Rust core sends unnamed statements (`query_typed`) and runs every call in an explicit transaction with `SET LOCAL ROLE` and `set_config(..., true)`.

| Practical | Idiomatic | Best practice |
|---|---|---|
| Practical | Yes | Aligned |

- The drift argument is sound. Prompt logic written three times does drift.
- The pooler findings are measured (364,491 checks across 41 runs) and the chosen statement mode is the standard answer for transaction-mode poolers.
- #82 removes the named-statement option. That removes a setting few deployments would turn on and a cache that can bind to another client's statement, so it is a good simplification.
- The cost is breadth: one crate, two bindings, a server and MCP, each with its own packaging. Finding 5 trims part of it.

### ADR-0002: Kind properties are a JSON Schema profile compiled to jsonpath

| Practical | Idiomatic | Best practice |
|---|---|---|
| Practical | Partly | Aligned |

- Validation in the database is the right place, because any caller can write with raw SQL.
- Using JSON Schema means one document serves validation, structured model output and client type generation. That is a real saving.
- Compiling a profile to `jsonb_path_match` rules is unusual, but it uses only core Postgres and avoids `pg_jsonschema`, which not every host ships. The profile leaves out `pattern` and `oneOf` for stated reasons.
- The risk sits in the compiler, which is custom code. Keep its tests table-driven, one case per keyword, accepted and refused.

### ADR-0003: Episodes are first-class, and Erasure is deterministic

| Practical | Idiomatic | Best practice |
|---|---|---|
| Practical | Yes | Stricter |

- Erasure on request is a legal duty for some consumers, so this is a practical requirement.
- Keeping sources out of the Node table is the right call. A source that cannot be versioned, resolved or merged is much easier to erase completely.
- Dropping the vacuum call was correct. The library cannot promise physical removal from WAL, backups or replicas, and the ADR now says plainly what it does promise: nothing derived is readable after commit.
- #83 adds a call that lists the Episodes behind a Node. That closes a real gap: Erase Node keeps the source text that names the person.

### ADR-0004: time intervals are scalar columns

| Practical | Idiomatic | Best practice |
|---|---|---|
| Practical | Yes | Aligned |

- The leakproof limit under RLS is a known PostgreSQL behaviour, and the measurement (87 ms against 2 ms) is a good reason to depart from `tstzrange` columns.
- Keeping exclusion constraints over `tstzrange(...)` expressions keeps the integrity guarantee. See finding 8 for the btree_gist question.
- Skipping PG 18's `WITHOUT OVERLAPS` follows from the same RLS limit, not from a host floor alone. Good reasoning.

### ADR-0005: every row has a Tenant

| Practical | Idiomatic | Best practice |
|---|---|---|
| Practical | Yes | Aligned |

- One schema and one policy shape for both tenancy modes is the standard multi-tenant RLS design, and it avoids `NULL` in constraints.
- Deployment-supplied `tenant_id()` and `allowed_scopes()` follow the Supabase and PostgREST pattern.
- The measured cost of an opaque per-row Scope function (575 ms against 3 ms) justifies requiring a key set.
- #84 adds the `(select gnomon.tenant_id())` wrapping in policies. Supabase documents the same pattern for `auth.uid()`. Record it in this ADR as a rule for replacement functions, as #84 plans.
- #84 also found that the wrapping pushed a Search function over `jit_above_cost`. `ALTER FUNCTION ... SET jit = off` on the Search functions is an ordinary, function-scoped fix. Prefer it to a database-wide setting.

### ADR-0006: migrations are exported into each consumer's chain

| Practical | Idiomatic | Best practice |
|---|---|---|
| Practical (after #80) | Yes (after #80) | Aligned |

- Never running DDL from the library is the right call for consumers whose platforms apply migrations only through review.
- The exporter with three targets and templating was more machinery than the problem needs. #80 replaces it with plain SQL files that consumers copy. That is how most Postgres libraries that ship SQL do it.
- #80's `search_path` change (end every path with `pg_temp`) fixes a real hole: PostgreSQL searches the temporary schema first unless it is named. The manual recommends exactly this for definer functions.
- #80 makes vector columns untyped `vector` with a dimension check. That is fine for exact search, but an HNSW index later needs an expression index with a cast. Say so in ADR-0010 when it is amended, as #80 already notes.

### ADR-0007: Versions are computed from Contributions

| Practical | Idiomatic | Best practice |
|---|---|---|
| Practical | Yes | Stricter |

- Storing every Contribution is what makes Erasure deterministic, and the rejected options explain why the alternatives fail.
- Recomputing closed history, not only current Versions, goes further than most erasure implementations. It follows from the bi-temporal promise: an As-of read at a past known-at must not show an erased value. Keep it.
- Ranked merge applied to statements (a low-Authority Retraction loses) is a practical defence against injected Episode text.
- Per-Node `pg_advisory_xact_lock` taken in a fixed order is the idiomatic way to serialise writes and Erasure. #83 records why row locks do not work once callers cannot update the rows they read.
- Watch item: Erasure of an Episode cited by 500 Nodes took 24 to 35 seconds in one transaction. A large Erase Scope with dependents in other Scopes could be much longer. #80's README notes about `statement_timeout` and `max_locks_per_transaction` are the right mitigation for now.

### ADR-0008: every Version is bi-temporal, with a precision on each bound

| Practical | Idiomatic | Best practice |
|---|---|---|
| Practical | Yes | Aligned |

- Sources state dates loosely, so precision per bound is practical, and it lets a more exact date be a Refinement instead of a false contradiction.
- Half-open intervals and refused empty intervals are standard temporal practice.
- No Kind exempt from Valid time: the four columns are paid for anyway, so the exemption would save nothing.

### ADR-0009: Store functions run as the invoker

| Practical | Idiomatic | Best practice |
|---|---|---|
| Practical | Partly (today), Yes (after #81 and #83) | Aligned after #81 |

- Today callers hold INSERT, UPDATE and DELETE on tables, and guard triggers judge every possible direct write. That works but needs many triggers and an intricate set of Erasure reach policies.
- #81 moves every caller write into definer functions owned by `gnomon_writer`, a role that owns no tables and cannot bypass RLS, so policies still apply. #83 adds `gnomon_eraser` with Tenant-only policies and drops `gnomon.erasure_reach`.
- See finding 4: the rejected option rested on an incorrect premise, and the ADR should be rewritten from the final design.
- #81's threat-model table (what gnomon protects against and what it leaves to the operator) is the most useful single record for a security reviewer. Make it its own ADR, as #81 plans.
- Accepted cost recorded in #83: a bug in an Erasure function could remove rows in another Scope of the same Tenant. That is a fair trade for removing the reach policies, as long as the refusal tests in #89 cover each Erasure function.

### ADR-0010: Search is exact over the visible rows

| Practical | Idiomatic | Best practice |
|---|---|---|
| Practical (exact recall), budgets not yet met | Partly | Stricter |

- The reason is sound. HNSW under RLS post-filters, and the measured recall@10 of 0.13 for a small Tenant is the known failure. Exact recall per Scope on core Postgres plus pgvector is a principled guarantee.
- The shapes are non-standard: binary quantization with a re-rank pinned by `lateral ... limit 1`, BM25 implemented in SQL over a token table, and reciprocal rank fusion in SQL. Each has a reason, and the pin has a plan test. Together they are the largest share of the SQL to maintain.
- The rejected `SECURITY DEFINER` full-text function with GIN is what many production systems do: one function, explicit Tenant and Scope predicates, heavy testing. gnomon rejected it to keep RLS as the only barrier. That is a defensible choice. If #85 to #87 cannot bring the budgets in, it is the option to revisit, with the threat-model ADR from #81 as the frame.
- See finding 1 for the budget gap.
- #85 (count term frequency at query time, keep per-Scope totals) and #86 (one search-text table and one embedding call) both simplify the design and keep results identical. Both are worth doing.
- Mentions and connectedness only reorder candidates other branches found. That is a good rule: it stops hubs entering on popularity.

### ADR-0011: `add_episode` is one resumable call, and stage records hold no model output

| Practical | Idiomatic | Best practice |
|---|---|---|
| Practical | Yes | Aligned |

- A queue in a Postgres table, claimed with `FOR UPDATE SKIP LOCKED`, with a lease, exponential backoff and a visible failed state, is the standard pattern and needs no extra infrastructure.
- Committing a stage's output with its record is correct, and #82 adds the public transaction that makes it possible for embeddings too.
- Storing no model output costs a repeat model call on retry. Erasure stays simpler and the no-residue test has less to cover, so the cost is worth paying.
- `run_queue` as a bounded call is the right shape for serverless consumers.

### ADR-0012: `same_as` and `different_from` are reserved Edge Kinds

| Practical | Idiomatic | Best practice |
|---|---|---|
| Practical | Yes | Aligned |

- Never merging Nodes keeps history reproducible, and recording the finding as an ordinary Edge reuses Valid time, Citations, Retraction and Erasure with no new mechanism.

### ADR-0013: a Sentence is written after the Version commits

| Practical | Idiomatic | Best practice |
|---|---|---|
| Practical | Yes | Aligned |

- The Store's merge can reject an extracted value, so a Sentence from extraction output can be wrong. One extra small-model call is a fair price.

### ADR-0014: Summaries and Communities belong to one Scope

| Practical | Idiomatic | Best practice |
|---|---|---|
| Practical | Yes | Aligned |

- Prose cannot be filtered after it is written, so a Summary built across Scopes would leak. This is the correct rule.

### ADR-0015: Summaries and Communities are versioned by Recorded time

| Practical | Idiomatic | Best practice |
|---|---|---|
| Practical | Yes | Aligned |

- A past valid-at never sees present prose, and a past known-at sees the Summary as it stood. Writing Summaries from current Versions and Sentences bounds the input and makes Citations exact.
- Closed rows accumulate. They are small, so this is acceptable.

### ADR-0016: the bindings run in-process and start no background tasks

| Practical | Idiomatic | Best practice |
|---|---|---|
| Practical, apart from two callbacks | Yes | Aligned |

- No background task in the bindings is the right rule for hosts that freeze a function after its response.
- One process-global tokio runtime, and a core that takes a runtime handle, matches how pyo3-async-runtimes and napi-rs work.
- A lazy pool with a maximum of 2 and a minimum of 0 suits many concurrent function instances behind a pooler.
- Property values as plain JSON, plus `kinds export` for consumers who want generated types, avoids a build-time database connection.
- One version number across all packages is standard for a library shipped in several languages.
- See finding 5 for the Formatter and TokenCounter callbacks.
- The ADR says the Python stubs "come from the shared Rust types". The bindings research recommends hand-written stubs checked with mypy `stubtest` at first, because PyO3's generator is experimental and `pyo3-stub-gen` is pre-1.0. Settle which one #58 uses and make the ADR say it.
- Native modules on Cloudflare Workers and Supabase Edge Functions: the ADR says the claim is unconfirmed. Workers run V8 isolates, not Node, so a Node-API addon cannot load there. Confirming it from Cloudflare's runtime documentation would let the ADR drop the caveat.

### ADR-0017: the HTTP API is RPC-style POST over shared types

| Practical | Idiomatic | Best practice |
|---|---|---|
| Practical | Yes | Aligned |

- Most operations take two instants, a Scope and a budget, so they are not resources. One POST route per method, as Slack's Web API or gRPC transcoding do, is a reasonable fit.
- RFC 9457 problem details with a stable `code` shared with the bindings, cursor pagination, `/healthz` for liveness only and `/readyz` for dependencies, body size limits and a SIGTERM drain are all standard practice.
- The `RateLimiter` trait with an allow-all default is speculative, but it is a few lines, and reserving the error code and configuration section costs nothing. Keep it small.
- The OpenAPI generator is not chosen yet. See the dependency table.

### ADR-0018: JWTs are verified per issuer, and worker tokens have their own issuer

| Practical | Idiomatic | Best practice |
|---|---|---|
| Practical | Yes | Aligned |

- Per-issuer key source, audience and algorithm allowlist, `alg: none` refused, no HMAC verification of an asymmetric key's token: this follows RFC 8725.
- Passing verified claims to the database unchanged keeps one place for the Tenant rule.
- A separate issuer and key for worker tokens, with distinct `typ`, `iss` and `aud`, stops a user token acting as a worker. Good.
- The research notes that `DecodingKey::from_jwk` accepts an `oct` (HMAC) key. The JWKS cache must drop any key whose `kty` does not match the issuer's algorithm family. Put that in #53's acceptance criteria if it is not there.
- See finding 3 for the crypto backend.

### ADR-0019: MCP over Streamable HTTP and stdio, as the caller

| Practical | Idiomatic | Best practice |
|---|---|---|
| Practical | Yes | Aligned |

- Acting only as a resource server, with RFC 9728 metadata and the 401 challenge, matches the MCP specification and reuses the deployment's identity provider.
- A small tool list with Erasure behind a switch is the right default for a model-driven client.
- Origin validation is off by default in rmcp and the ADR turns it on. That matters: it is the defence against DNS rebinding from a web page.
- rmcp released eight minor versions in seven weeks. Pin the exact minor version and read its changelog on each bump.

### ADR-0020: tests run under nextest against containerised Postgres

| Practical | Idiomatic | Best practice |
|---|---|---|
| Practical | Yes | Aligned |

- Testing through the same driver and statement mode as the Store is correct, and the reason for rejecting sqlx as the test driver is measured.
- `CREATE DATABASE ... TEMPLATE` per test is the standard fast isolation method.
- The idle watcher inside the container is a practical answer to testcontainers-rs having no reaper.
- #89's rule (test behaviour through the Store, with a short named list of things only the catalog can show) is the right direction and fixes the drift from spec #1's own testing decisions.
- CI pins Rust 1.93.1 while `rust-version` is 1.89. Either test the MSRV in CI or raise `rust-version` to what CI tests.

## Requirements

The three specs hold 309 user stories. Most are practical. This section names the groups, what they require, and the stories that are edge cases, theoretical or out of date.

### Spec #1: the Store and its Rust Store API

| Area | Stories | Verdict | Notes |
|---|---|---|---|
| Installing and migrating | 1 to 11 | Practical | Story 2 (three export targets) is replaced by #80's plain files. Story 5 (model change path) becomes a hand-written migration. Story 11 (single to multi-tenant, one way) is an edge case but cheap. |
| Tenancy, Scope and callers | 12 to 23 | Practical | Stories 20 to 23 are replaced by #81. Story 22's empty `search_path` is replaced by #80. #81 adds writable Scopes, which extends story 12. |
| Kinds | 24 to 32 | Practical | Story 29 (a child may turn ranked merge on, not off) is an edge case. Story 32 (each Version records its rule) is needed for deterministic Erasure. |
| Episodes and writing | 33 to 45 | Practical | Story 42 (same-transaction replacement) matters for batch writes. |
| Ending a belief | 46 to 51 | Practical | The Refinement and Correction split (49, 50) is subtle but grounded in ADR-0008. |
| As-of reads | 52 to 58 | Practical | |
| Search | 59 to 72 | Practical | Story 61 (exact recall at every Scope size) drives most of the Search complexity and is a deliberate guarantee. Story 63 (BM25 from allowed Scopes only) guards against a statistical leak, a real but narrow risk, and is cheap after #85. Story 71 is replaced by #86. Story 72 (budgets) is not met for natural-language queries (finding 1). |
| Erasure | 73 to 85 | Practical | Story 76 names adjacency rows that were never built. Story 79 (vacuum call) was dropped. |
| Connections and hosts | 86 to 90 | Practical | Story 87 (named statements opt-in) is replaced by #82. |

### Spec #23: the Pipeline

| Area | Stories | Verdict | Notes |
|---|---|---|---|
| Adding an Episode | 1 to 13 | Practical | Story 5 (concurrent calls for one Episode ID wait) matters because job systems deliver at least once. |
| Extraction | 14 to 25 | Practical | The bound stripping and one repair retry (24, 25) follow from a known Anthropic structured-output limit. |
| Valid time | 26 to 32 | Practical | Checking the model's dates in code and storing `unknown` on disagreement is the right distrust of model arithmetic. |
| Resolution | 33 to 42 | Practical | Stories 41 and 42 use a per-Scope advisory lock around a short read-decide-write section. Under a bulk load into one Scope, every Resolution write waits on that lock. Measure it in #32. |
| Ending a belief | 43 to 50 | Practical | Story 47 (an unknown start closes nothing) is a good safety rule. |
| Sentences | 51 to 55 | Practical | |
| Embeddings | 56 to 63 | Practical | Input types and prefixes (60 to 62) are needed for asymmetric models. |
| Summaries and Communities | 64 to 75 | Practical | Story 70 asks for a warm start. See finding 6 for a way to meet it without depending on `leiden-rs`. |
| Maintenance and repair | 76 to 81 | Practical | Finding gaps by query (77) avoids bookkeeping that Erasure would have to get right. |
| Context | 82 to 98 | Practical, budget at risk | Depends on Search meeting its budget (finding 1). Story 97 makes TOON the default on token cost, measured on one model and one domain. TOON is new and models have seen far less of it than JSON or Markdown. Before fixing the default, compare TOON with compact JSON and Markdown tables, not only with pretty JSON, and prefer the widely known format if the token gap is small. |
| Models and configuration | 99 to 105 | Practical | Story 102 lists five reranker adapters. vLLM serves Jina- and Cohere-compatible rerank routes, so check whether the `vllm` adapter is the `cohere` or `jina` one with another base URL before writing it. |
| Failures and trust | 106 to 114 | Practical | Story 113 (refuse `retracted_by_source`) is a cheap, practical switch for deployments with untrusted sources. |
| Queue, workers and usage | 115 to 121 | Practical | |
| Erasure and retraction | 122 to 125 | Practical | |

### Spec #49: the bindings and server

| Area | Stories | Verdict | Notes |
|---|---|---|---|
| Python | 1 to 13 | Practical | Sync and async versions of every call (6) double the API but match what Python users expect. |
| Node | 14 to 20 | Practical | Documenting that a dropped Promise does not cancel the work (19) is the honest answer to a napi-rs limit. |
| Replacing the Pipeline's seams | 21 to 28 | Practical for 21, 22, 26 to 28. Theoretical for 23 to 25 | See finding 5. |
| Serverless and queued work | 29 to 38 | Practical | |
| Running the server | 39 to 51 | Practical | Health, readiness, drain, limits and log hygiene are standard operations practice. |
| HTTP API | 52 to 60 | Practical | |
| Authentication and tokens | 61 to 71 | Practical | |
| MCP | 72 to 84 | Practical | Story 83 names the vacuum call, which no longer exists. |
| Types and packaging | 85 to 89 | Practical | |
| Testing and growth | 90 to 94 | Practical for 90 to 92. Edge case for 93 and 94 | Five conformance runners is a lot of test code, but it is the only way to show the entry points agree. |

The spec also names a gap it leaves open: an API-only replica cannot tell whether a worker runs elsewhere, so it cannot choose the MCP `add_episode` behaviour. That needs a configuration key in #66.

## Dependencies

### Current dependencies

| Crate | Why | Alternatives | Recorded? | Verdict |
|---|---|---|---|---|
| `tokio` 1 | Async runtime. tokio-postgres, napi-rs, pyo3-async-runtimes and rmcp all assume it. | async-std (discontinued), smol. Neither fits the dependencies above. | Implied by ADR-0016 | Keep. The only practical choice. |
| `tokio-postgres` 0.7 | Driver. `query_typed` sends unnamed statements, which work on every pooler at default settings. | `sqlx`: sends named statements even with `statement_cache_capacity(0)`. `diesel-async`: an ORM layer over SQL that already lives in functions. | Yes, ADR-0001 and ADR-0020, measured | Keep. The loss of sqlx's compile-checked queries is small, because the Rust side mostly calls SQL functions. |
| `tokio-postgres-rustls` 0.14 | rustls connector for tokio-postgres. | `postgres-native-tls`, which links OpenSSL and makes static musl builds harder. | No | Keep. Record it with the crypto decision (finding 3). |
| `rustls` 0.23 with `ring` | TLS with certificate verification on by default (story 89 of #1). | `native-tls` or OpenSSL; the `aws-lc-rs` provider. | No | Keep rustls. Decide the provider (finding 3). |
| `rustls-native-certs` 0.8 | Loads the platform's trusted roots. | `webpki-roots`, which bundles Mozilla's roots and ignores a corporate CA in the system store. | No | Keep. The right default for managed hosts. `Tls::trust_pem` covers a provider CA. |
| `rustls-pki-types` 1 | Listed but unused directly. | | | Remove, as #82 plans. |
| `serde_json` 1 | JSON property values and claims. | `simd-json`, which brings no gain at these sizes. | No | Keep. `serde` with `derive` arrives with the shared API types in #50. |
| `chrono` 0.4 | Times, through tokio-postgres's `with-chrono-0_4` feature. | `time` (also supported by tokio-postgres), `jiff` (newer, better time-zone handling, no tokio-postgres feature yet). | No | Keep. Valid-time conversion is UTC-only, so jiff's advantages do not apply. Changing later would touch every public type with a timestamp, so record the choice. |
| `uuid` 1 | IDs. UUID v7 is generated in SQL. | | No | Keep. |
| `thiserror` 2 | Library error enum. | `anyhow` (for applications, not library APIs), `snafu`. | No | Keep. The standard choice for a library's public error type. |
| `clap` 4 | The `gnomon` binary's commands. | `argh`, `lexopt`. | No | #80 removes it with the exporter. #54 brings a CLI back for `gnomon serve`, `kinds export` and `token mint-worker`, so clap will return. That is fine, but #80 could keep `src/main.rs` to avoid the churn. |

### Current dev-dependencies

| Crate | Why | Alternatives | Recorded? | Verdict |
|---|---|---|---|---|
| `testcontainers` 0.28 | Starts Postgres images for tests, reused across nextest processes. | Docker Compose scripts, embedded-Postgres crates (which cannot run the pgvector or Supabase images). | Yes, ADR-0020 | Keep. |
| `proptest` 1 | Oracle tests with shrinking. | `quickcheck`, which shrinks less well for structured sequences. | Yes, ADR-0020 | Keep. |
| `rcgen` 0.14, `tokio-rustls` 0.26 | Generate certificates and run a TLS endpoint to test the verification modes. | Fixed certificate files that expire. | No | Keep. |
| `tempfile` 3 | Temporary files in tests. | | No | Keep. |
| `criterion` | Not used. | | Yes, ADR-0020 rejects it | The performance suite asserts budgets, so a benchmark framework adds nothing. |

### Planned dependencies

| Crate | For | Alternatives | Recorded? | Verdict |
|---|---|---|---|---|
| `deadpool-postgres` | Connection pool (#82). | `bb8`, sqlx's pool (tied to sqlx). | In #82, without the alternatives | Good fit. It starts no background task, opens connections lazily, and its fast recycle mode runs no query on a healthy connection, which matches ADR-0016. |
| `jsonwebtoken` 12 | JWT signatures and claims (#53). | `jwtk` (own JWKS cache, checks only `alg`, `exp`, `nbf`), `jwks` (pins jsonwebtoken 10), `jwt-simple`, `josekit` (OpenSSL). | Yes, `docs/research/jwt.md` | Good choice. The backend must agree with finding 3. |
| `reqwest` | JWKS fetching and model HTTP calls. | `hyper-util`'s client directly, `ureq` (blocking). | Mentioned only | Fine. Turn off default features and pick the rustls feature that matches finding 3, or it links OpenSSL. |
| `rmcp` 3.11 | MCP server (#65). | Writing the transport by hand. | Yes, `docs/research/mcp.md` | Keep. It is the official SDK. Pin the minor version. |
| `axum` | HTTP server. rmcp's Streamable HTTP service mounts on it. | `actix-web`, `poem`, `hyper` directly. | Implied | The natural choice with tower and rmcp. Record it with the server ticket. |
| OpenAPI generator | `/v1/openapi.json` from the shared types (#56). | `utoipa` (derive macros, widely used), `aide` (generates from axum handlers), `schemars` plus a hand-built document. | No | Choose in #50, because the shared types carry its derives. utoipa is the common choice. |
| Configuration loader | One typed document from a file, a dict and the environment. | `figment`, `config`, plain `serde` with `toml` and manual environment overrides. | No | Choose in #27. The bindings also pass a dict, so a plain serde type with a small environment overlay may be enough. |
| `tracing`, `tracing-opentelemetry`, `opentelemetry-otlp` | Tracing events and optional OTLP export. | | Implied by ADR-0017 | Standard. Keep OTLP behind a Cargo feature so the bindings do not carry it. |
| `pyo3` 0.30, `pyo3-async-runtimes`, `maturin` | Python binding. | `uniffi` (one generator for many languages, weaker async and Python ergonomics), `cffi` over a C ABI. | Yes, `docs/research/bindings.md` | Keep. The standard stack. |
| `pyo3-stub-gen` | `.pyi` stubs. | Hand-written stubs checked with `stubtest`; PyO3's experimental generator. | Research recommends against it at first | Settle in #58 (see ADR-0016). |
| `napi` 3, `napi-derive` | Node binding. | `neon`. | Yes, `docs/research/bindings.md` | Keep. napi-rs has the stronger async story and generates `.d.ts`. |
| `leiden-rs` 0.8 | Community detection with a warm start. | `single-clustering`, `rune-leiden`, `clust-libreville-rs`, label propagation as Graphiti uses it. | Yes, `docs/research/pipeline.md` and #23 | Acceptable behind the trait. See finding 6. If kept, pin the exact version and review its source on each update. |

## Decisions made but not recorded

These decisions exist in code or tickets but in no ADR:

- the TLS crypto provider (finding 3);
- the HTTP framework, the OpenAPI generator and the configuration loader;
- the threat model (planned in #81);
- the Python stub source (ADR-0016 and the research disagree);
- `chrono` as the time type in the public API;
- the Rust MSRV and edition. The workspace uses edition 2021, and edition 2024 has been stable since Rust 1.85. Moving a new crate is cheap now and harder after the first release.

*Status: ADR-0021 records the crypto provider, and #54 names axum. #81 still plans the threat model. The other decisions in this list are open, together with OTLP behind a Cargo feature. They are taken as one more finding.*

*Status of the open decisions, taken as finding 9: decided.*

- *The Python stubs are hand-written and checked with mypy `stubtest`, and napi-rs generates the `.d.ts` files (ADR-0016, #58).*
- *The shared types derive `schemars::JsonSchema`, and the OpenAPI 3.1 document is built from a route table (ADR-0017, #50, #56).*
- *The configuration is one plain serde type with a `GNOMON_` environment overlay and no configuration crate (#27).*
- *The public API keeps `chrono` (ADR-0001, #82).*
- *The workspace moves to edition 2024, and `rust-version` equals the pinned CI toolchain (#88, then ADR-0020).*
- *OTLP is behind an `otlp` Cargo feature that only the server binary enables (ADR-0016, #67).*

*The smaller recommendations in the sections above are now carried by tickets: #53 drops `oct` keys from a fetched JWKS, #65 pins rmcp, #32 measures the Resolution lock under a bulk load, #42 compares TOON with compact JSON and Markdown before fixing the default, and #40 tries the `jina` adapter on vLLM. Cloudflare's current Node.js compatibility page does not name native addons, and Supabase does not document them, so ADR-0016 keeps its caveat.*

## Suggested order

1. Add "Superseded" lists to specs #1 and #49 (finding 2). This is documentation only.
2. Record the crypto provider and the server crates in an ADR, and add `cargo deny` to #88 (findings 3 and 7).
3. Add the term-pruning decision to #87, so it lands with or right after #86 and before #41 (finding 1).
4. Rewrite ADR-0009 when #83 lands, and publish the threat-model ADR from #81 (finding 4).
5. Defer #60, the sync callbacks, and have `context` return typed rows (finding 5).
6. Before #38, decide between `leiden-rs` with a warm start and a seeded detector with overlap matching (finding 6).
7. Check btree_gist on the named hosts during #80 (finding 8).

*Status: steps 1, 2, 3, 5, 6 and 7 are decided and recorded. Step 4 waits for #81 and #83 to land.*
