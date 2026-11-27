---
status: accepted
---

# TLS and signatures use one crypto provider

gnomon uses aws-lc-rs for every TLS connection and every JWT signature. rustls, tokio-postgres-rustls, tokio-rustls, jsonwebtoken (`aws_lc_rs`), reqwest (`rustls`) and rmcp (`reqwest`) all enable it, and nothing in the dependency graph enables `ring`. `cargo deny` bans `ring`, dev-dependencies included, in the `lint` job.

The library never installs a process-wide default provider. Each TLS config it builds names its provider through `ClientConfig::builder_with_provider`. The `gnomon` server binary installs aws-lc-rs as the process default at the start of `main`, before any client or verifier is built.

We chose this because aws-lc-rs is the default of every crate gnomon uses or plans to use: rustls 0.23, tokio-rustls 0.26, reqwest 0.13 and rmcp. reqwest 0.13 has no ring feature, and jsonwebtoken 12 has no ring backend. When a graph enables both rustls providers, a plain `ClientConfig::builder()` panics at runtime because it cannot choose. The same holds for jsonwebtoken when both of its backends are on. One provider removes that panic and removes a second implementation of the same algorithms.

The library leaves the process default alone because the bindings and Rust consumers run gnomon inside someone else's process (ADR-0016). A library that installed a default would decide for every other crate in that process, depending on which one started first.

## Considered options

- Keep `ring`. It builds with only a C compiler and is what `src/connect.rs` used first. But jsonwebtoken would need `rust_crypto`, a second implementation of the same algorithms. reqwest and rmcp would need `rustls-no-provider`, which panics unless a process default is installed, so the library would have to install one. ring's last release, 0.17.14, was in March 2025.
- Install a default on first use when none is set. Whichever library starts first would then set the default for the whole process.
- OpenSSL through `native-tls`. It makes static musl builds harder and adds a system dependency to every wheel.

## Consequences

- Non-FIPS aws-lc-rs builds need a C compiler only, with no CMake, Go or bindgen. On Windows x64, rustls's `aws_lc_rs` feature uses prebuilt NASM objects.
- aarch64 cross builds in maturin-action's manylinux containers have an open failure (aws/aws-lc-rs#1022). The arm64 Linux wheels, npm packages and server binary build on native arm64 runners, with no cross-compiling.
- Static musl linking with aws-lc-sys is not yet confirmed. The server's packaging smoke test checks that the binary is statically linked and starts in an empty container. If it fails, the provider is decided again. It does not quietly change.
- aws-lc-rs has a `fips` feature. gnomon makes no FIPS promise and builds no FIPS artifacts. A FIPS build would need CMake and Go.
- A Rust consumer that enables `ring` or jsonwebtoken's `rust_crypto` in its own graph can reach the ambiguous state in its own calls. gnomon's own calls stay safe, because each config names its provider. jsonwebtoken picks its backend from crate features, so gnomon's JWT verification relies on the graph enabling only `aws_lc_rs`. The server binary also installs jsonwebtoken's aws-lc-rs provider explicitly at start, beside the rustls one.
