---
status: accepted
---

# The bindings expose both APIs, run in the consumer's process and start no background tasks

The Python (PyO3) and Node (napi-rs) bindings expose the Pipeline API as the main entry point and the Store API beside it, so a consumer who only wants `add_episode` and `context` never touches a Store call, and one who needs an As-of read, Retract Episode or Erasure in-process can still make it. A binding owns one process-global tokio runtime, and the core takes a runtime handle. A binding starts no timer, worker or keepalive of its own. Everything it does happens inside a call the consumer made, because a serverless host may freeze a function as soon as its response returns, and a stage stranded half-way by a frozen task would only be recovered when its lease ran out.

The queue is run by a bounded call, `run_queue`, which claims due stage records, runs them, and returns when a deadline or a stage limit is reached or the queue is empty (ADR-0011). The server's polling loop is a loop around the same call. In the bindings `add_episode` waits for its stages by default (`wait: true`). A consumer who wants `wait: false` runs `run_queue` from its own scheduler, or runs the server's worker.

A consumer may replace three of the Pipeline's seams with a function in its own language, and the rest stay in Rust:

- `ModelClient`, `Embedder` and `Reranker` take async callbacks. Each call is a network round trip, so the cost of crossing the FFI is small beside it.
- `Formatter` and `TokenCounter` cannot be replaced from a binding. A binding names one of the built-in formats, and `context` sizes rows with the bytes-based estimate. A Rust consumer can still implement either trait.
- `CommunityDetector` cannot be replaced from a binding.

`context` returns the rows it selected alongside the formatted text, in the same order. The rows are the ones that fit the budget in the chosen format, so a consumer who wants another shape builds it from the rows in its own language, with no FFI round trip per row. A consumer whose shape is longer than the chosen format picks the closest built-in or leaves headroom in the budget. The row types come from the shared Rust types, and a row's properties are plain JSON. The HTTP API returns the rows and the text. MCP returns only the text, because its reader is a model.

We chose this because ADR-0001 put the Pipeline in the core so that it is written once, and a consumer in a serverless function needs the whole of it without deploying a long-lived process.

## Connections

A consumer creates one client object per process, at module scope, and creating it opens no connection. The pool is small and lazy: a default maximum of 2 in the bindings and 10 in the server, a minimum of 0, and connections open on first use. Each connection is checked when it is taken from the pool, and a dead one is replaced once before the call fails. `pool.max` and `pool.idle_timeout` are set in the deployment configuration, which the bindings read from a file path, a dict or object, or the environment. Model calls run before the transaction that writes their output (#23), so a connection is held only for the short transaction.

## Types

Property values cross the bindings as plain JSON (a Python dict or a JS object), and the Store validates them on every write (ADR-0002). `gnomon kinds export --format json-schema` writes one flattened schema per Kind from a deployment's registry, and a consumer runs its own generator in its build. The bindings generate no Kind-specific classes. The Python package ships `.pyi` stubs and a `py.typed` marker, and the Node package ships `.d.ts` files. Both are checked into the repository. The `.pyi` stubs are written by hand, and a CI test runs mypy `stubtest` against the built module, so a stub that drifts from the module fails. napi-rs generates the `.d.ts` files from the shared Rust types, and a CI test fails when the checked-in files differ from the build.

## Packaging

The crate, the wheels, the npm packages and the server binary release together under one version number. Each release embeds the range of `gnomon.schema_version` it accepts, and the core refuses to run outside it (ADR-0006). CI builds on a version tag and publishes through PyPI trusted publishing and npm provenance, so no registry token is stored.

- Python: abi3 wheels with `abi3-py310` as the floor, for manylinux_2_28 and musllinux on x86_64 and arm64, macOS arm64 and Windows x64.
- Node: one package per platform in `optionalDependencies`, for the same targets.
- Server: a static musl binary for linux x64 and arm64, and a container image built from it.

The OTLP exporter is behind an `otlp` Cargo feature that is off by default. The server binary builds with it, and the wheels and npm packages build without it.

## Considered options

- Synchronous `Formatter` and `TokenCounter` callbacks. No consumer asked for a format beyond the four built-ins, and the typed rows let one build any other shape without a callback. The token counter would cross the FFI once per candidate row, up to 100 times per call, to buy back budget the conservative estimate leaves unused. Deferred, with the conditions for reconsidering in `.out-of-scope/binding-formatter-and-token-counter-callbacks.md`.
- Configuration only, with no callbacks. It would avoid the cancellation and callback-latency questions below, but a consumer with a model provider gnomon has no adapter for would need a proxy.
- Telling serverless consumers to deploy the server for its worker. The bindings would be smaller, but it removes the reason for in-process bindings in ADR-0001.
- A larger pool with a background keepalive. It cuts reconnect time on a warm function, but the keepalive is a background task, and many concurrent function instances could exhaust a pooler's connection limit.
- Generating the `.pyi` stubs with `pyo3-stub-gen` or with PyO3's `experimental-inspect` and `maturin generate-stubs`. `pyo3-stub-gen` is pre-1.0, and its return type for an `async fn` is unconfirmed. PyO3 marks its own generator as in development (`docs/research/bindings.md`). Reconsider when PyO3's generator leaves development.
- Generating typed classes per Kind at install or import time. It needs a database connection at build time and a generator in each language.
- Separate version numbers per package. A consumer would have to match binding and server versions against a schema version.

## Consequences

- Dropping a Python awaitable cancels the Rust future, and dropping a JS Promise does not. Neither passes cancellation to a callback, so every callback runs under the call's deadline (`docs/research/bindings.md`).
- Callback latency has not been measured.
- A change to a Python-facing signature needs a hand edit to the `.pyi` stubs, and `stubtest` fails the build until it is made.
- Cloudflare Workers and Supabase Edge Functions are expected to rule out a native module, so they use the server. That rests on a claim the research did not confirm from a primary source.
- Free-threaded Python wheels wait until the stable ABI covers them. macOS x64 and WASI are not shipped.
- The conformance suite replays each case through the Rust API, both bindings, the HTTP API and the MCP tools (ADR-0017).
