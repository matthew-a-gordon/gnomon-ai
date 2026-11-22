# Research: Python and Node bindings for the Rust core

Checked on 2026-11-22. This note collects primary-source facts about shipping the core (ADR-0001) as a PyO3 extension and a napi-rs addon, and about running those in serverless hosts. It uses the glossary's terms for gnomon concepts. Section 7 recommends; sections 1 to 6 only report what the sources say. Anything a primary source did not settle is marked "unconfirmed" and repeated in the final list.

Versions and pins. Crate versions are those on crates.io on 2026-11-22: [pyo3](https://crates.io/crates/pyo3) 0.30.1, [pyo3-async-runtimes](https://crates.io/crates/pyo3-async-runtimes) 0.30.0, [pyo3-stub-gen](https://crates.io/crates/pyo3-stub-gen) 0.23.3, [napi](https://crates.io/crates/napi) 3.16.2, [napi-derive](https://crates.io/crates/napi-derive) 3.7.1, [napi-build](https://crates.io/crates/napi-build) 2.6.0. [maturin](https://pypi.org/project/maturin/) is 1.16.0 on PyPI and [@napi-rs/cli](https://www.npmjs.com/package/@napi-rs/cli) is 3.10.12 on npm. Source was read at these pins: [pyo3-async-runtimes 137f180](https://github.com/PyO3/pyo3-async-runtimes/tree/137f180) (2026-10-20, the 0.30.0 line), [pyo3 be9e428](https://github.com/PyO3/pyo3/tree/be9e428) (2026-11-19, five weeks after the 0.30.1 release, so guide text may describe unreleased behavior), [napi-rs 7a88d90](https://github.com/napi-rs/napi-rs/tree/7a88d90) (2026-11-22), [maturin c909db1](https://github.com/PyO3/maturin/tree/c909db1) (2026-11-16). Vendor and docs pages carry no version, so each is cited with the date it was read (2026-11-22). The Node-API page read was headed "Node.js v26.13.1", and the Python stable-ABI page was the 3.15.2 docs.

## Short answer

- Both bridges give you an awaitable per call and a tokio runtime that the binding owns, global to the process. Neither supports a runtime per gnomon instance through its own helpers.
- Dropping a Python awaitable cancels the Rust future. Dropping a JS Promise does not. Both directions of callbacks (Rust awaiting a Python coroutine or a JS Promise) work, with no cancellation passed to the foreign side.
- Rust errors become Python exception classes you define, and plain JS `Error` objects with a string `code`. JS subclasses need a wrapper layer.
- Python wheels: `abi3` from Python 3.9 at the lowest (PyO3 floor), not loadable on free-threaded builds until `abi3t` (Python 3.15). npm: one prebuilt `.node` per platform in `optionalDependencies`. The wasm fallback is `wasm32-wasip1-threads`, and no source says it can open sockets.
- Native code runs on AWS Lambda (Amazon Linux 2023, glibc 2.34, x86_64 and arm64), on Vercel and on Cloud Run (x86_64 only, stated). Cloudflare Workers and Supabase Edge Functions run V8 isolates and Deno, so a native module is not an option there.

## 1. Async across the boundary

### Python: pyo3-async-runtimes

The crate documents that Rust futures cannot run on Python's event loop and Python coroutines cannot run on a Rust runtime, so it bridges two separate loops ([lib.rs, "Why two event loops"](https://github.com/PyO3/pyo3-async-runtimes/blob/137f180/src/lib.rs)).

- `pyo3_async_runtimes::tokio::future_into_py(py, fut)` returns a Python awaitable. It reads the running asyncio loop and the `contextvars` context from the calling thread (`get_current_locals`), spawns `fut` on tokio, and completes the asyncio future with `call_soon_threadsafe` ([generic.rs L608](https://github.com/PyO3/pyo3-async-runtimes/blob/137f180/src/generic.rs)). The caller must be inside a running event loop, or pass explicit `TaskLocals` with `future_into_py_with_locals`.
- Cancelling the asyncio future cancels the Rust future. The doc comment says "new behaviour in `v0.15`" ([generic.rs L502](https://github.com/PyO3/pyo3-async-runtimes/blob/137f180/src/generic.rs)), and the code wires a `oneshot` cancel channel into the spawned task ([generic.rs L618](https://github.com/PyO3/pyo3-async-runtimes/blob/137f180/src/generic.rs)).
- Synchronous Python functions called from inside the Rust future do not see the right `contextvars`. The doc suggests wrapping them in `contextvars.Context.run` ([generic.rs L502 to L520](https://github.com/PyO3/pyo3-async-runtimes/blob/137f180/src/generic.rs)).
- PyO3 itself has `async fn` in `#[pyfunction]` and `#[pymethods]` behind the `experimental-async` feature. The guide labels it "still in active development", says the awaitable works "only in `asyncio` context", requires `Send + 'static` futures, and says there is no simple way to detach from the interpreter across an `.await` ([async-await.md](https://github.com/PyO3/pyo3/blob/be9e428/guide/src/async-await.md)). It also holds `&self` borrowed for the life of the future.

### Python: owning the tokio runtime

- The crate keeps one process-global runtime: `static TOKIO_RUNTIME: OnceLock<Pyo3Runtime>`, built lazily by `get_runtime()` as `Builder::new_multi_thread().enable_all()`. `init(builder)` replaces the builder and `init_with_runtime(&'static Runtime)` supplies a runtime. Both are set-once ([tokio.rs L66, L176 to L206](https://github.com/PyO3/pyo3-async-runtimes/blob/137f180/src/tokio.rs)).
- The runtime hooks are a static trait: `GenericRuntime::spawn` and `spawn_blocking` take no `self`, and the tokio impl calls the global `get_runtime()` ([tokio.rs L88 to L102](https://github.com/PyO3/pyo3-async-runtimes/blob/137f180/src/tokio.rs)). A runtime per gnomon instance therefore cannot go through `tokio::future_into_py`. It can go through `generic::future_into_py` only if you implement a runtime type that finds its instance some other way (inference from the signature).

### Python: GIL and event-loop pitfalls

- A thread that holds the GIL and waits on a thread that needs the GIL deadlocks. The guide says to call `Python::detach` around work that spawns threads, "especially" when workers need the GIL ([parallelism.md](https://github.com/PyO3/pyo3/blob/be9e428/guide/src/parallelism.md)).
- `std::sync::OnceLock`, `LazyLock` and `once_cell` can deadlock when the initializer calls into Python. The guide points to `PyOnceLock` and the `OnceExt` and `OnceLockExt` helpers ([faq.md](https://github.com/PyO3/pyo3/blob/be9e428/guide/src/faq.md)). This applies to a lazily built global runtime if its init touches Python.
- `asyncio.run` makes a new loop on every call, so a cached loop reference goes stale. The crate's own history says it dropped a global loop reference for that reason ([lib.rs, "The Main Dilemma"](https://github.com/PyO3/pyo3-async-runtimes/blob/137f180/src/lib.rs)).
- `os.fork()` in a multi-threaded process is, per the Python docs, "never been safe" on POSIX, and 3.12 raises a `DeprecationWarning` when it detects threads ([os docs, read 2026-11-22](https://docs.python.org/3/library/os.html)). A tokio runtime built at import time would carry worker threads into a forked child. What tokio does after a fork is unconfirmed.
- The free-threaded build has no GIL, but the guide notes new panics from concurrent `&mut self` borrows that the GIL used to prevent ([free-threading.md](https://github.com/PyO3/pyo3/blob/be9e428/guide/src/free-threading.md)). Since PyO3 0.28 modules are assumed thread-safe by default, and a module opts out with `#[pymodule(gil_used = true)]`, which makes the interpreter re-enable the GIL at import with a `RuntimeWarning`.

### Python: a sync API next to the async one

No primary source recommends a pattern. The pieces it documents are these. `tokio::runtime::Runtime::block_on` "runs a future to completion on the Tokio runtime", blocking the current thread ([tokio docs](https://docs.rs/tokio/latest/tokio/runtime/struct.Runtime.html)). PyO3 says to wrap long Rust work in `Python::detach` so other Python threads run ([parallelism.md](https://github.com/PyO3/pyo3/blob/be9e428/guide/src/parallelism.md)). Combining the two (`py.detach(|| get_runtime().block_on(fut))`) is my inference, not a documented recipe. It breaks if the sync call is made from a thread that already runs an asyncio loop, and a Python async callback (section 2) would have no running loop to land on.

### Node: napi-rs

- With the `tokio_rt` feature (alias `async`), `#[napi] async fn` runs on a runtime napi-rs manages and returns a `Promise`. `Err` rejects the promise ([async-fn](https://napi.rs/docs/concepts/async-fn.md), [error-handling](https://napi.rs/docs/concepts/error-handling.md), both read 2026-11-22).
- Dropping the JS Promise does not cancel the Rust future. The guide says callers need "an explicit handle or operation ID bridged to a Rust-owned flag or token" ([async-concurrency](https://napi.rs/docs/more/async-concurrency.md)).
- The runtime is a process-global `LazyLock`, built as `new_multi_thread().enable_all()` on native targets and `new_current_thread()` on wasm without `tokio_unstable` ([tokio_runtime.rs L1324 to L1336](https://github.com/napi-rs/napi-rs/blob/7a88d90/crates/napi/src/tokio_runtime.rs)). `create_custom_tokio_runtime(rt)` swaps it in from a `#[module_init]` hook ([L1403](https://github.com/napi-rs/napi-rs/blob/7a88d90/crates/napi/src/tokio_runtime.rs)). The async-concurrency guide says "A custom runtime instance is currently consumed once", and after a shutdown and restart napi-rs falls back to its default runtime. It calls this an open limitation.
- Shutdown happens when the last Node-API environment using the module exits. Do not call `shutdown_async_runtime` with pending promises. The doc comment says pending tasks are dropped and their promises rejected with `Error("task was cancelled before completion")`, and a promise nothing handles ends the Node process with exit code 1 on the main thread ([tokio_runtime.rs L1490 to L1500](https://github.com/napi-rs/napi-rs/blob/7a88d90/crates/napi/src/tokio_runtime.rs)).
- Each worker-thread environment needs its own resources. "Don't cache an `Env` or a ThreadsafeFunction globally and reuse it in a worker isolate" ([async-concurrency](https://napi.rs/docs/more/async-concurrency.md)). Node-API says the same of `napi_env` ([n-api docs](https://nodejs.org/api/n-api.html)).
- A long synchronous computation inside an async fn "still occupies a Tokio worker thread". The guide points to `spawn_blocking` or `AsyncTask` (libuv pool) for that.
- napi-rs 3 also has an `async-runtime` feature and an `AsyncRuntime` trait for a non-tokio executor ([async-runtime](https://napi.rs/docs/concepts/async-runtime.md)). That is not needed for gnomon, since the core is tokio.

## 2. Calling back into Python or JS from Rust

The core's model client, embedder, reranker, formatter and token counter are Rust traits with async methods (ADR-0001). An adapter holds the foreign callable and implements the trait.

### Python callback

- Hold a `Py<PyAny>`. Inside the async method, call `Python::attach(|py| ...)`, call the Python function, and pass the returned coroutine to `pyo3_async_runtimes::into_future_with_locals(&locals, awaitable)`. The result is a `Send` Rust future ([lib.rs example under "The Solution"](https://github.com/PyO3/pyo3-async-runtimes/blob/137f180/src/lib.rs), [generic.rs L491](https://github.com/PyO3/pyo3-async-runtimes/blob/137f180/src/generic.rs)).
- Thread rule: `into_future` needs the Python loop. It uses `TaskLocals` from tokio task-local storage, falling back to `asyncio.get_running_loop`, which only works on a Python thread ([get_current_locals](https://github.com/PyO3/pyo3-async-runtimes/blob/137f180/src/tokio.rs)). The adapter must capture `TaskLocals` when the Python call enters, then move them into the spawned future.
- Mechanism and cost: the awaitable is scheduled with `call_soon_threadsafe` and `ensure_future` on the Python loop, and a done-callback sends the result back over a `oneshot` ([lib.rs L540 to L600](https://github.com/PyO3/pyo3-async-runtimes/blob/137f180/src/lib.rs)). Each callback takes the GIL twice (the call and the completion) and waits for the loop to get to it. No primary source gives a latency number. Under the free-threaded build there is no GIL, but the loop is still single-threaded.
- Errors: a Python exception arrives as `PyErr` in the `PyResult`. The adapter maps it into the core's error type. A panic in a spawned future surfaces as `RustPanic` ([err.rs](https://github.com/PyO3/pyo3-async-runtimes/blob/137f180/src/err.rs)).
- Cancellation: when the outer Python awaitable is cancelled, the Rust future is dropped. I found no code in `lib.rs` that cancels the inner Python task when the Rust future is dropped. Its only cancellation handling is `CancelledError` creation in `generic.rs` for the other direction. Treat the Python callback as running to completion (unconfirmed by a test).
- A synchronous Python callable can run through `spawn_blocking` plus `Python::attach`.

### JS callback

- Take a `ThreadsafeFunction<Args, Promise<Ret>>` as an argument. Rust calls `tsfn.call_async(Ok(args)).await?` to get a `Promise<Ret>`, then awaits that to get the value. The napi-rs example does exactly this: `func.call_async(Ok(1)).await?.await?` ([examples/napi/src/threadsafe_function.rs L228](https://github.com/napi-rs/napi-rs/blob/7a88d90/examples/napi/src/threadsafe_function.rs)).
- Thread rules (Node-API): the JS function only ever runs on the thread that created the environment. Calls go through a queue. `napi_tsfn_blocking` "should not be called ... from a JavaScript thread, because, if the queue is full, it may cause the JavaScript thread to deadlock". With `max_queue_size` 0 the call never blocks. Once `napi_closing` is returned the function must not be used again ([n-api, "Calling a thread-safe function"](https://nodejs.org/api/n-api.html)).
- napi-rs: the second generic is the JS return type, and arguments and return types stored by a `ThreadsafeFunction` must be `'static` ([threadsafe-function](https://napi.rs/docs/concepts/threadsafe-function.md)). A strong function keeps the event loop alive until released. `Weak: true` removes that, and queued callbacks "may never run if nothing else keeps Node running". A queue size of 0 is unbounded ([async-concurrency](https://napi.rs/docs/more/async-concurrency.md)).
- Errors: with `CalleeHandled = true`, Rust errors go to JS as the first callback argument. With plain `call` and `false`, a synchronous JS throw goes through `napi_fatal_exception`. Use `call_async` or `call_async_catch` to get the throw back as `Err` ([threadsafe-function](https://napi.rs/docs/concepts/threadsafe-function.md), [source L601](https://github.com/napi-rs/napi-rs/blob/7a88d90/crates/napi/src/threadsafe_function.rs)). A rejected inner Promise surfaces when the `Promise<Ret>` is awaited (the napi-rs test `tsfn_throw_from_js` covers the throw case).
- Cancellation: nothing in the sources cancels a JS Promise when the Rust future is dropped (unconfirmed, inferred from the API having no hook).
- Cost: each call is a queue hop to the JS thread plus argument conversion on that thread. No source gives numbers.

## 3. Errors and types

### Rust error enum to exceptions and errors

- PyO3: a `#[pyfunction]` may return `Result<T, E>` if `E: Into<PyErr>`. The guide shows `impl From<MyError> for PyErr` ([error-handling.md](https://github.com/PyO3/pyo3/blob/be9e428/guide/src/function/error-handling.md)). `create_exception!(module, MyError, PyException)` defines a class, and a third argument can be another exception class, so a base `GnomonError` plus one subclass per variant is the documented tool ([exception.md](https://github.com/PyO3/pyo3/blob/be9e428/guide/src/exception.md)). Async functions go through the same conversion.
- napi-rs: `Error::new(status, message)` sets `error.message` from the reason and `error.code` from `status.as_ref()`. Any status type implementing `AsRef<str>` works, so a gnomon status enum can set `code`. "This sets `error.code` without changing the JavaScript error subclass" ([error-handling](https://napi.rs/docs/concepts/error-handling.md)). Built-in subclasses (`TypeError`, `RangeError`) come from `Env::throw_*`. `Env::throw(value)` can throw any object, so a custom class instance is possible in synchronous code. For async fns the page only says `Err` rejects with the converted error.
- Panics: napi-rs says an uncaught panic in a sync callback can end the process. `#[napi(catch_unwind)]` turns an unwinding panic into a `GenericFailure` and does not work with `panic = "abort"`.
- anyhow errors become `GenericFailure` with the chain in the message, so map domain errors explicitly to keep stable codes.

### Type stubs and declaration files

- napi-rs: `napi build` generates `index.d.ts` from the macros. The repo's own example output (`examples/napi/index.d.cts`) is checked in, and attributes `ts_args_type` and `ts_return_type` override single declarations ([types-overwrite](https://napi.rs/docs/concepts/types-overwrite.md), [napi-config](https://napi.rs/docs/cli/napi-config.md)). This is the mainline path in v3. What the page does not describe is the generation process. The output for a `Result`-returning async fn is `Promise<T>`, as the async-fn page shows.
- PyO3: stub generation is "still in active development" (tracking issue 5137). It needs the `experimental-inspect` feature, then `maturin generate-stubs --output stubs` or `maturin build --generate-stubs` ([type-stub.md](https://github.com/PyO3/pyo3/blob/be9e428/guide/src/type-stub.md), [maturin distribution.md](https://github.com/PyO3/maturin/blob/c909db1/guide/src/distribution.md)).
- pyo3-stub-gen 0.23.3 is a separate crate, pre-1.0. Its dependency range is `pyo3 >=0.27.0, <0.31.0`, so it covers 0.30. It needs `#[gen_stub_pyfunction]` and friends placed before the PyO3 macros, a `stub_gen` binary, and an `rlib` crate type next to `cdylib`. It states "Minimum supported Python version is 3.10" ([docs.rs README, read 2026-11-22](https://docs.rs/crate/pyo3-stub-gen/latest)). The README does not mention `async fn`. Whether an async method gets a `Coroutine` return type is unconfirmed.

## 4. Packaging

### Python wheels with maturin

- PyO3 0.30 needs at least Python 3.9 ([getting-started.md](https://github.com/PyO3/pyo3/blob/be9e428/guide/src/getting-started.md)). The `abi3-py39` through `abi3-py315` features set the wheel floor, giving a name like `cp39-abi3-manylinux...`. The lowest feature set wins ([building-and-distribution.md](https://github.com/PyO3/pyo3/blob/be9e428/guide/src/building-and-distribution.md)). Python's docs: an extension built for stable ABI 3.x is compatible with 3.x and above ([stable ABI, 3.15.2 docs](https://docs.python.org/3/c-api/stable.html)).
- Free-threaded builds: `abi3` is "compatible with non-free-threaded builds" only. `abi3t` arrived in Python 3.15 (PEP 803) and loads on both. A free-threaded interpreter "only recognizes the `abi3t` tag" ([stable ABI docs](https://docs.python.org/3/c-api/stable.html)). PyO3 ignores `abi3` when building on a free-threaded interpreter, so a free-threaded 3.13 or 3.14 needs a version-specific wheel (`cp313t`, `cp314t`) ([free-threading.md](https://github.com/PyO3/pyo3/blob/be9e428/guide/src/free-threading.md)). maturin 1.14 or newer supports `abi3t`. It builds one stable-ABI family per build, so a full matrix needs separate builds ([maturin bindings.md](https://github.com/PyO3/maturin/blob/c909db1/guide/src/bindings.md)).
- Platforms: maturin builds wheels for Linux x86, x86_64, armv7, aarch64, ppc64le (musl) and s390x (gnu), macOS x86_64 and aarch64, and Windows x86, x64 and arm64. It supports `manylinux2014` and newer, and `musllinux_1_1` and newer. Rust needs glibc 2.17, so `manylinux2014` is the floor ([platform_support.md](https://github.com/PyO3/maturin/blob/c909db1/guide/src/platform_support.md), [distribution.md](https://github.com/PyO3/maturin/blob/c909db1/guide/src/distribution.md)). Linux wheels need a manylinux container or `--zig`. `PyO3/maturin-action` handles both. The build backend is `maturin` (`build-backend = "maturin"` in `pyproject.toml`), which lets `pip` build an sdist ([tutorial.md](https://github.com/PyO3/maturin/blob/c909db1/guide/src/tutorial.md), PEP 517).
- Publishing: PyPI trusted publishing exchanges short-lived OIDC identity tokens in place of API tokens ([docs.pypi.org/trusted-publishers](https://docs.pypi.org/trusted-publishers/)). maturin's generated workflow supports it with `trusted-publishing = true` and `uv publish --trusted-publishing always` ([distribution.md](https://github.com/PyO3/maturin/blob/c909db1/guide/src/distribution.md)). Attestations are "generated and uploaded automatically by default" with `pypa/gh-action-pypi-publish` ([producing attestations](https://docs.pypi.org/attestations/producing-attestations/)). maturin's CI generator has a `skip-attestation` option, so it attests by default.

### npm prebuilt binaries with napi-rs

- Layout: a root package plus one package per platform, for example `@scope/addon-linux-x64-gnu` and `-linux-x64-musl`. Each holds one `.node` file and sets `os`, `cpu` and `libc`. The root pins them by exact version in `optionalDependencies`, and its generated loader picks the match. No install-time download or compiler is needed ([release docs](https://napi.rs/docs/deep-dive/release.md)).
- Steps: `napi create-npm-dirs`, `napi build` per CI matrix row, `napi artifacts`, `napi pre-publish`, then `npm publish` for the root. Publishing is not atomic and npm has no rollback. The docs say not to publish the root until every platform package passed.
- Targets the CLI accepts: macOS x64, arm64 and universal. Windows MSVC x64, x86, arm64. Linux glibc x64, arm64, armv7 and more. Linux musl x64, arm64, armv7. Android, FreeBSD, OpenHarmony and threaded WASI. The `napi new` templates cover a subset: macOS x64 and arm64, Windows MSVC x64, x86 and arm64, Linux glibc x64, arm64 and armv7, Linux musl x64 and arm64, Android, FreeBSD and WASI ([support-compatibility](https://napi.rs/docs/more/support-compatibility.md)). Listing `targets` in `package.json` does not compile them. Each build makes one target.
- Node-API level: the `napi` crate defaults to `napi4` (which enables ThreadsafeFunction and the async runtime), with `napi1` to `napi10` as cumulative features ([cargo-features](https://napi.rs/docs/concepts/cargo-features.md)). Node-API is ABI-stable across Node versions. Version 9 is in Node 18.17+ and 20.3+, version 10 in 22.14+ and 23.6+. If `NAPI_VERSION` is unset, C addons default to 8 ([n-api, read 2026-11-22](https://nodejs.org/api/n-api.html)). The CLI itself needs Node `>=23.5.0 || ^22.13.0 || ^20.17.0`. An addon may target older Node than the CLI.
- WASI: the supported target is `wasm32-wasip1-threads`, with generated Node and browser loaders. It needs `SharedArrayBuffer`, and "A WASI build is not automatically equivalent to a native addon" ([webassembly](https://napi.rs/docs/concepts/webassembly.md)). The page I read did not mention sockets or tokio networking. The first 100,000 characters of a 251,443-character page were read.
- Provenance and trusted publishing: the napi-rs release docs say the publish job needs `id-token: write` for npm provenance, and the `repository` field must match the GitHub workflow. Its template authenticates with an `NPM_TOKEN` unless replaced. npm's own docs: trusted publishing needs npm CLI 11.5.1+ and Node 22.14+, supports GitHub Actions, GitLab CI/CD and CircleCI, and generates provenance automatically from GitHub Actions or GitLab CI/CD for public repositories and packages ([npm trusted publishers, read 2026-11-22](https://docs.npmjs.com/trusted-publishers)). Each package can have up to 10 trusted publishers.

## 5. Serverless hosts

All limits below were read on 2026-11-22.

| Host | Languages and OS | Native module | Size limit | libc and arch |
|---|---|---|---|---|
| AWS Lambda | `nodejs22.x`, `nodejs24.x`, `nodejs26.x`; `python3.12` to `python3.15`; all on Amazon Linux 2023 (`python3.11` is on AL2) | Yes. Wheels and `.node` files go in the zip, a layer or a container image | 50 MB zipped direct upload, 250 MB unzipped including layers, 5 layers, 10 GB container image | glibc 2.34 on AL2023 (stated on the Python page). x86_64 and arm64, both supported by all runtimes |
| Vercel Functions | Node 22, 24, 26 (26 default); Python 3.12 (default), 3.13, 3.14 | Python and Node packages are bundled. Native support is not stated as such | 250 MB uncompressed, 500 MB for Python, "large functions" beta up to 5 GB | Not stated on the pages read |
| Cloud Run (functions) | Node, Python and others, built into a container | Yes, as container contents | No direct image size limit | "Linux x86_64 ABI" only. Multi-arch images must include `linux/amd64`. libc not stated |
| Cloudflare Workers | V8 isolates. Python Workers use Pyodide | No source allows it. WebAssembly is the native path | Worker size 64 MiB | Not applicable |
| Supabase Edge Functions | Deno, TypeScript | Not stated. Wasm is supported | 20 MB bundled locally (CLI), 5 MB bundled server-side | Not stated |

Sources for the table:

- Lambda: [runtimes](https://docs.aws.amazon.com/lambda/latest/dg/lambda-runtimes.html) ("All supported Lambda runtimes support both x86_64 and arm64"), [quotas](https://docs.aws.amazon.com/lambda/latest/dg/gettingstarted-limits.html), [Python packages](https://docs.aws.amazon.com/lambda/latest/dg/python-package.html). The Python page says runtimes from 3.12 "are built on Amazon Linux 2023, which includes glibc 2.34" and tells you to install `manylinux_2_28_x86_64` or `manylinux_2_28_aarch64` wheels. Amazon Linux 2 reached end of life on 2026-06-30 per the runtimes page. The [Node.js packaging page](https://docs.aws.amazon.com/lambda/latest/dg/nodejs-package.html) has no statement on `.node` files or libc. Node on AL2023 means glibc, which is inference from the shared OS.
- Vercel: [limitations](https://vercel.com/docs/functions/limitations) ("These limits are enforced by AWS"), [Node versions](https://vercel.com/docs/functions/runtimes/node-js/node-js-versions), [Python runtime](https://vercel.com/docs/functions/runtimes/python). Python bundles "include all files from your project that are reachable at build time", with no tree-shaking. Maximum duration is 300s default and 800s maximum on Pro, with a 1800s beta maximum.
- Cloud Run: [container contract](https://cloud.google.com/run/docs/container-contract). The function runtimes page lists Node.js, Python, Go, Java, Ruby, PHP and .NET but does not name an OS version in the text I read ([function runtimes](https://cloud.google.com/run/docs/runtimes/function-runtimes)).
- Cloudflare: [how Workers works](https://developers.cloudflare.com/workers/reference/how-workers-works/) (V8 isolates), [limits](https://developers.cloudflare.com/workers/platform/limits/) (Worker size 64 MiB on both plans in the table, CPU 10 ms free and 5 min paid, memory 128 MB, six simultaneous open connections), [Python packages](https://developers.cloudflare.com/workers/languages/python/packages/) ("pure and PyEmscripten" wheels and Pyodide packages). Whether the 64 MiB is compressed is unconfirmed.
- Supabase: [limits](https://supabase.com/docs/guides/functions/limits) (256 MB memory, CPU 2s, wall clock 150s free and 400s paid, 150s request idle timeout), [quickstart](https://supabase.com/docs/guides/functions/quickstart) ("only supports creating functions in TypeScript with the Deno runtime"), [overview](https://supabase.com/docs/guides/functions) ("supports WASM").
- Deno 2.8: Node-API addons are "Supported with local node_modules and `--allow-ffi`" ([Deno Node compatibility](https://docs.deno.com/runtime/fundamentals/node/)). Bun 1.4.5 "implements this interface from scratch, so most existing Node-API extensions work" ([Bun Node-API](https://bun.com/docs/runtime/node-api)). napi-rs's own docs say Deno is not in its CI and "Do not infer Deno support from Node-API compatibility alone", and that Bun is best effort ([support-compatibility](https://napi.rs/docs/more/support-compatibility.md)). Whether a gnomon `.node` file loads in the Supabase Edge runtime is unconfirmed.

## 6. Connections and background work in short-lived functions

- AWS Lambda: Lambda "freezes the execution environment when the runtime and each extension have completed and there are no pending events". Objects outside the handler stay initialized, and "if your Lambda function establishes a database connection, instead of reestablishing the connection, the original connection is used in subsequent invocations". The docs recommend checking whether a connection exists before creating one. "Background processes or callbacks that were initiated by your Lambda function and did not complete when the function ended resume if Lambda reuses the execution environment", and the docs say to finish them before the code exits. Environments are terminated "every few hours" ([execution environment lifecycle](https://docs.aws.amazon.com/lambda/latest/dg/lambda-runtime-environment.html)). For database access, the docs call RDS Proxy "recommended for production" because it "manages a pool of shared database connections which enables your function to reach high concurrency levels without exhausting database connections" ([RDS and Lambda](https://docs.aws.amazon.com/lambda/latest/dg/configuration-database.html)). The Init phase is limited to 10 seconds unless provisioned concurrency or SnapStart is on.
- Vercel: Fluid compute lets multiple invocations share one instance. `waitUntil` "extends the lifetime of the request handler for the lifetime of the given Promise", and the promises "will have the same timeout as the function itself. If the function times out, the promises will be cancelled" ([functions package](https://vercel.com/docs/functions/functions-api-reference/vercel-functions-package)). `attachDatabasePool(pool)` should be called right after creating a pool. It "ensures that idle pool clients are properly released before functions suspend", and the page lists `pg`, MySQL2, MongoDB and ioredis. It is a Node API, so a Rust pool inside an addon is not covered (inference).
- Google Cloud Run: with request-based billing "when the Cloud Run service finishes handling a request, the instance's access to CPU will be disabled or severely limited". The page says not to start background threads outside request handlers, and to use instance-based billing for background work ([general tips](https://cloud.google.com/run/docs/tips/general), [CPU allocation](https://cloud.google.com/run/docs/configuring/cpu-allocation)). It also says global-scope variables are reused across requests but "Whether any individual request receives the benefit of this reuse cannot be known ahead of time".
- Cloudflare Workers: `ctx.waitUntil()` can extend execution "up to 30 seconds after the response is sent or the client disconnects", and unfinished promises are cancelled. A call that is neither awaited nor passed to `waitUntil` "can be canceled when the invocation ends" ([context](https://developers.cloudflare.com/workers/runtime-apis/context/)). Hyperdrive provides edge connection setup and pooling for databases ([how Hyperdrive works](https://developers.cloudflare.com/hyperdrive/concepts/how-hyperdrive-works/)).
- Supabase Edge Functions: `EdgeRuntime.waitUntil(promise)` keeps the instance running until the promise completes ([background tasks](https://supabase.com/docs/guides/functions/background-tasks)). The Postgres example creates a pool of one connection and notes the pooler options ([connect to Postgres](https://supabase.com/docs/guides/functions/connect-to-postgres)). The page text I read did not state a guidance sentence on transaction poolers.

What these mean for a tokio runtime, as inference from the sources above. On Lambda and on Cloud Run with request-based billing, the process is frozen between calls. Worker threads stop, timers do not fire, and sockets can be closed by the other side during the freeze. A runtime that keeps spawned tasks running past the response gets two outcomes: the work stalls and resumes on the next invocation (Lambda's documented behavior), or it is cancelled (Vercel and Cloudflare, with their `waitUntil` time limits). No host documents what a native thread pool sees when the process thaws.

## 7. What it means for gnomon

### Options

Runtime ownership in the core:

1. The core builds its own runtime per `Gnomon` instance. Isolated, but neither binding's async helper can spawn onto it (section 1), and several runtimes in one process multiply worker threads.
2. The core takes a `tokio::runtime::Handle` from its builder. Bindings pass the handle of one process-global runtime they own. A Rust-only consumer passes `Handle::current()`. One thread pool, and the binding decides its size and lifetime.
3. The core spawns nothing detached and only awaits futures the caller drives. Nothing outlives a call. The work happens on whichever runtime polls the future.

Callbacks (model client, embedder, reranker, formatter, token counter):

1. Ship built-in Rust implementations for OpenAI-compatible and Anthropic endpoints (ADR-0001) and expose no foreign trait at first. Most consumers need no callback.
2. Expose foreign implementations of the traits, with the adapters of section 2.
3. Expose callbacks only for the traits where a consumer has no endpoint alternative (formatter, token counter), and keep them synchronous. A sync callback avoids the loop and queue hops.

Serverless:

1. Native bindings everywhere they load: Lambda, Vercel, Cloud Run.
2. For Cloudflare Workers and Supabase Edge Functions, use the server binary over HTTP (ADR-0001). The wasm route would need a socket-capable target, and no source shows one.

### Recommendation

- Use option 2 for the runtime. The core takes a `Handle` and never builds a runtime itself. Each binding owns one lazily built, process-global multi-thread runtime with a small, explicit worker count, created on first call and not at import (fork and `OnceLock` pitfalls). Expose an explicit `close()` on the client that closes the Postgres pool, so serverless consumers can end a call with no live connection state.
- Keep every Pipeline call self-contained: no detached task outlives its call. ADR-0011 already makes `add_episode` one resumable call, which fits hosts that freeze the process after the response. Use pool settings that tolerate a freeze (zero minimum idle connections, short idle timeout, a liveness check on acquire). The driver is not named in the ADRs, so the exact knobs are open. Route through the transaction pooler the ADR already requires. On Lambda, add RDS Proxy or the Postgres provider's pooler.
- Python: one `abi3-py310` wheel set (the maturin docs' own example floor, and `pyo3-stub-gen` needs 3.10), with `manylinux_2_28` x86_64 and aarch64 (matches Lambda's documented tag), `musllinux_1_2`, macOS arm64 and x86_64, and Windows x64. Free-threaded interpreters need separate `cp313t` and `cp314t` wheels until `abi3t` (3.15) is the floor, so decide whether to ship them in 1.0 or declare `gil_used = true` until the `unsafe` code and shared state (the global runtime, the `Py<PyAny>` callback holders) are audited. Use `pyo3-async-runtimes` `future_into_py` for the async API and `py.detach(|| handle.block_on(fut))` for the sync one, with the sync one refusing to run inside a running loop. Write the `.pyi` by hand and check it with mypy `stubtest` at first. `pyo3-stub-gen` is pre-1.0 and its async support is unconfirmed, and PyO3's own generator is marked in development.
- Node: napi-rs 3 with `tokio_rt`, default `napi4`, and `ThreadsafeFunction<_, Promise<_>>` for any callback. Prebuilt packages for linux x64 and arm64 on gnu and musl, darwin arm64 and x64, and win32 x64. Raise the Node-API level only when a needed API requires it. Do not ship a WASI fallback for the core. Define one `GnomonError` JS class in a thin TypeScript wrapper, mapping `error.code` strings, since napi-rs sets codes and not subclasses. Handle `worker_threads` by keeping no `Env` or TSFN in global state.
- Cancellation: add a cancel token argument (or an operation id) to long Pipeline calls in both bindings. Python gets cancellation of the Rust side for free from asyncio. Node does not (section 1).
- Provenance: publish from GitHub Actions with PyPI trusted publishing and npm trusted publishing. The repository is public, so npm provenance applies.
- Hosts: support Lambda, Vercel and Cloud Run with the native packages, after checking the libc and arch Vercel uses. For Cloudflare Workers and Supabase Edge Functions, point consumers at the server binary.

## Not confirmed

- Vercel's function libc and CPU architecture. The limits page says the bundle limits are "enforced by AWS" and the pages read name no OS.
- Whether a Rust native addon loads in Vercel's Node and Python runtimes. Their docs describe dependencies and bundles but no native-module statement.
- Cloud Run functions OS image and libc. The container contract only states x86_64.
- Any primary statement that Cloudflare Workers or Supabase Edge Functions reject native modules. The conclusion rests on the V8-isolate and Deno architecture pages.
- Whether Cloudflare's 64 MiB Worker size is compressed or uncompressed.
- What happens to a tokio runtime's threads after `os.fork()` in a Python process.
- Whether the Python task started by `into_future` is cancelled when the Rust future is dropped. The source shows no such code, and no test was run.
- Whether dropping a Rust future cancels the JS Promise it awaits. The napi-rs API has no hook for it, and no doc states it.
- Latency and overhead numbers for Python-from-Rust and JS-from-Rust callback calls. No primary source gives any.
- Whether `pyo3-stub-gen` emits a correct return type for `async fn`. Its README is silent.
- Whether napi-rs produces typed errors for async rejections beyond `code` and `message`. The error-handling page covers `Env::throw` for sync code only.
- Whether any sockets-capable wasm target works with napi-rs and a Postgres driver. The WASI page read said nothing on sockets (the first 100,000 of 251,443 characters).
- Whether a gnomon `.node` file loads in Deno 2.8 as run by Supabase Edge Functions. Deno's own docs say it works locally with `--allow-ffi`.
- PEP 803 (`abi3t`) acceptance and its tooling support. The Python 3.15.2 docs describe it as added in 3.15, and the PyO3 and maturin guides cover it, but its wheel-tag handling in pip and other installers was not checked.
- Whether the GitHub-hosted runners' PyPI trusted publishing works for the target repository. Setup on PyPI and npm is the maintainer's step.

## Sources

### PyO3, maturin and Python

- https://github.com/PyO3/pyo3/tree/be9e428/guide/src (async-await.md, parallelism.md, faq.md, exception.md, function/error-handling.md, type-stub.md, features.md, free-threading.md, building-and-distribution.md, getting-started.md)
- https://github.com/PyO3/pyo3-async-runtimes/tree/137f180/src (lib.rs, generic.rs, tokio.rs, err.rs)
- https://github.com/PyO3/maturin/tree/c909db1/guide/src (distribution.md, bindings.md, platform_support.md, tutorial.md, config.md)
- https://docs.rs/crate/pyo3-stub-gen/latest
- https://docs.rs/tokio/latest/tokio/runtime/struct.Runtime.html
- https://docs.python.org/3/c-api/stable.html
- https://docs.python.org/3/library/os.html
- https://docs.pypi.org/trusted-publishers/
- https://docs.pypi.org/attestations/producing-attestations/
- https://crates.io/crates/pyo3, https://crates.io/crates/pyo3-async-runtimes, https://crates.io/crates/pyo3-stub-gen, https://pypi.org/project/maturin/

### napi-rs and Node

- https://github.com/napi-rs/napi-rs/tree/7a88d90 (crates/napi/src/tokio_runtime.rs, threadsafe_function.rs, error.rs; examples/napi/src/threadsafe_function.rs)
- https://napi.rs/docs/concepts/async-fn.md
- https://napi.rs/docs/concepts/threadsafe-function.md
- https://napi.rs/docs/concepts/error-handling.md
- https://napi.rs/docs/concepts/async-runtime.md
- https://napi.rs/docs/concepts/webassembly.md
- https://napi.rs/docs/concepts/cargo-features.md
- https://napi.rs/docs/concepts/types-overwrite.md
- https://napi.rs/docs/cli/napi-config.md
- https://napi.rs/docs/deep-dive/release.md
- https://napi.rs/docs/more/support-compatibility.md
- https://napi.rs/docs/more/async-concurrency.md
- https://nodejs.org/api/n-api.html
- https://docs.npmjs.com/trusted-publishers
- https://crates.io/crates/napi, https://crates.io/crates/napi-derive, https://www.npmjs.com/package/@napi-rs/cli

### Hosts

- https://docs.aws.amazon.com/lambda/latest/dg/lambda-runtimes.html
- https://docs.aws.amazon.com/lambda/latest/dg/gettingstarted-limits.html
- https://docs.aws.amazon.com/lambda/latest/dg/lambda-runtime-environment.html
- https://docs.aws.amazon.com/lambda/latest/dg/python-package.html
- https://docs.aws.amazon.com/lambda/latest/dg/nodejs-package.html
- https://docs.aws.amazon.com/lambda/latest/dg/configuration-database.html
- https://vercel.com/docs/functions/limitations
- https://vercel.com/docs/functions/runtimes/python
- https://vercel.com/docs/functions/runtimes/node-js/node-js-versions
- https://vercel.com/docs/fluid-compute
- https://vercel.com/docs/functions/functions-api-reference/vercel-functions-package
- https://developers.cloudflare.com/workers/reference/how-workers-works/
- https://developers.cloudflare.com/workers/platform/limits/
- https://developers.cloudflare.com/workers/runtime-apis/context/
- https://developers.cloudflare.com/workers/languages/python/packages/
- https://developers.cloudflare.com/hyperdrive/concepts/how-hyperdrive-works/
- https://cloud.google.com/run/docs/container-contract
- https://cloud.google.com/run/docs/runtimes/function-runtimes
- https://cloud.google.com/run/docs/tips/general
- https://cloud.google.com/run/docs/configuring/cpu-allocation
- https://supabase.com/docs/guides/functions
- https://supabase.com/docs/guides/functions/limits
- https://supabase.com/docs/guides/functions/quickstart
- https://supabase.com/docs/guides/functions/background-tasks
- https://supabase.com/docs/guides/functions/connect-to-postgres
- https://docs.deno.com/runtime/fundamentals/node/
- https://bun.com/docs/runtime/node-api
