---
status: accepted
---

# Tests run under nextest against containerised Postgres

The Rust suites talk to Postgres through `tokio-postgres` and `query_typed`, the same driver and statement mode as the Store (ADR-0001), so neither the Store nor the harness sends a named statement unless a test opts in, or a lane sets `GNOMON_TEST_NAMED_STATEMENTS=1` for a pooler that supports them. They run under plain `cargo test` with `#[tokio::test]` and under `cargo nextest`, and every test passes under both. The harness starts its server with the `testcontainers` crate: one container per image, named, published on loopback with an ephemeral port, its data on a tmpfs, and removed when it stops. Each test gets a fresh database made with `CREATE DATABASE ... TEMPLATE` from a template that holds the exported migrations, built once per server and migration set. `GNOMON_TEST_DATABASE_URL` replaces the container with a server someone else runs. The same test code runs against every server, and an image parameter, `GNOMON_TEST_IMAGE`, picks the container. We chose this because the tests must exercise the Store's own statement mode, isolate tests from each other on a shared server, start fast, and never collide with the other projects and agents that share the developer's machine.

## The four lanes

The suite runs in four lanes, and none replaces another:

1. A plain container of `pgvector/pgvector:pg17`.
2. A plain container of `supabase/postgres` 17.x.
3. The Supabase CLI local stack, with the migrations applied through the Supabase export target and the suite pointed at it through `GNOMON_TEST_DATABASE_URL`.
4. PgBouncer and Supavisor in transaction mode at their default settings, again through `GNOMON_TEST_DATABASE_URL`, which points at the pooler. The harness builds templates and creates test databases straight on the server behind it, through `GNOMON_TEST_DIRECT_DATABASE_URL`, so only callers go through the pooler.

Lane 3 exists apart from lane 2 because the two test different things. The bare image tests the database. The CLI stack tests the path a Supabase consumer takes: migrations the CLI applies, the stack's own roles, and PostgREST in front of the database. Lane 3 runs with the stack's pooler off. Supavisor is lane 4's.

## What CI runs, and when

GitHub Actions runs `.github/workflows/ci.yml` on each pull request and on demand (`workflow_dispatch`), never on a push to `main`. Three jobs run in parallel:

| Job | Runs |
|---|---|
| `test-pg17` | `cargo nextest run --workspace`, which is lane 1 |
| `test-supabase` | The same with `GNOMON_TEST_IMAGE=supabase`, which is lane 2 |
| `lint` | `cargo fmt --all --check`, and `cargo clippy --workspace --all-targets --all-features -- -D warnings` |

A ruleset on `main` requires the three checks, accepted only from GitHub Actions, and requires the branch to be up to date, so the pull request run tests what lands on `main`. The repository admin role can bypass it, so the owner can still commit documents directly. No run caches anything. We accept that each new pull request builds from nothing, and that a direct commit to `main` runs no tests. The builds leave out debug information.

The repository is public, so the workflow token only reads the repository, the workflow uses no secret, it is triggered by `pull_request` and never `pull_request_target`, and every third-party action is pinned to a full commit SHA. Lanes 3 and 4 run on demand.

Only the performance suite asserts a time budget, and CI does not run it, because it needs a quiet machine. CI builds it through Clippy's `--all-features`. The time limits in `.config/nextest.toml` stop a hung test and are not budgets.

`rust-toolchain.toml` pins the toolchain CI uses. That version is also the minimum supported Rust version, so `rust-version` in `Cargo.toml` moves with it. Nothing is released, so a recent minimum costs no consumer anything.

## Considered options

- `sqlx` as the test driver. Its `statement_cache_capacity(0)` still sends named statements (the `prototype/pooler-prepared-statements` branch), so the suites would not test the statement mode the Store uses.
- One container per test process. nextest runs each test in its own process, so this would start a container for every test. Instead the first process starts the container, later processes find it by name and reuse it, and a file lock in Cargo's target directory keeps two processes from starting it, or building a template, at once.
- Removing the container from the test process. testcontainers-rs has no reaper container, and a container held in a static is never dropped, so a test process cannot remove it reliably. A watcher inside the container stops Postgres once no client has been connected for about 30 seconds, and Docker then removes the container. This works whether the run passed, failed or was killed.
- Applying the migrations in every test. Correct, but slower than copying a template, and it repeats the cluster-wide role setup that two chains must not race on.
- `criterion` for the performance suite. The performance suite asserts budgets rather than comparing runs, so it is a separate on-demand test target with no benchmark framework.

## Consequences

- Other projects share the machine, so no test assumes a port. Containers publish on `127.0.0.1` with an ephemeral port, and each run names its containers with `GNOMON_TEST_CONTAINER_PREFIX`, so a teardown removes only its own.
- Tests run in parallel against one server. A test that changes cluster-wide state, such as roles, does so under the server's lock.
- Template databases outlive the tests and stay on an external server, named by the embedding dimension and a hash of the exported migrations, so a changed migration gets a new template. Test databases are dropped when their test ends.
- A run that starts in the seconds before an idle container stops can find it shutting down. A rerun starts a new one.
- The oracle tests, from the first ticket that writes Versions, use `proptest`, so a failing sequence of writes shrinks to a short one.
- Lanes 1 and 2 need only Docker. Lanes 3 and 4 need the Supabase CLI and the pooler images, and they reuse the same tests through `GNOMON_TEST_DATABASE_URL`.
