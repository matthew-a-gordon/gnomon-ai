# Testing

The suite runs with plain `cargo test` or with `cargo nextest run`, and every test passes under both. ADR-0020 records why the suite is built this way.

## What runs where

- `crates/gnomon/tests/export.rs` drives `gnomon migrations export` through the binary for the plain and supabase targets. It needs no database. It shows that a re-export after a new release adds only the new migrations, with Supabase versions after every migration already in the chain, and refuses to rewrite a released one. It also compares the supabase and alembic output with the released copies in `crates/gnomon/tests/golden`, because consumers commit that output and a re-export refuses a released file that changed. A unit test in `crates/gnomon/src/migrations.rs` exports a fake newer release on every target.
- `crates/gnomon/tests/supabase_target.rs` applies the supabase export with the Supabase CLI's own runner, `supabase migration up`, to a database of the suite's server: an earlier release, then a consumer's later migration, then the new release, of which the CLI applies only the new migration. It needs `npx`.
- `crates/gnomon/tests/alembic.rs` drives the alembic target and checks the result with Alembic itself: the down-revision links `alembic history` reports, a single head, the consumer's own revisions before and after gnomon's, and `alembic upgrade head` giving a schema the Store accepts, first from an empty chain and then from an earlier release's. It needs `uvx`, and its last two tests need a database.
- `crates/gnomon/tests/supabase_cli.rs` is lane 3 only, ignored elsewhere. On the Supabase CLI stack it shows that the CLI applied every gnomon migration as `postgres`, that PostgREST reaches no gnomon table or function, and that no schema PostgREST exposes holds what the migrations created.
- `crates/gnomon/tests/store_api.rs` is seam 1: the Rust core's Store API against real Postgres. Two of its tests record the wire through a proxy: by default every call sends only unnamed statements, inside one transaction that sets its role and claims locally, and with named statements turned on each statement is prepared once per connection and reused. The second connects straight to the server, behind any pooler, because a lane's pooler may not support named statements.
- `crates/gnomon/tests/connect.rs` is seam 1 for connecting: TLS with certificate verification by default, no fallback to plain text when the server refuses TLS, `sslmode=disable` refused, and verification or TLS off only through an explicit setting. A TLS-terminating proxy in `crates/gnomon/tests/support/tls.rs` stands in front of the test server with a certificate from a CA each test makes, because the test images run without TLS.
- `crates/gnomon/tests/concurrency.rs` runs 24 Stores at once, as users and workers on 12 Tenants with a Scope each, beside 6 raw login-role connections. Each caller checks that it reads its own Episodes, not its sibling's in another Scope, and that a call naming another Tenant fails. Each bystander checks that its transactions see no claims and no role but the login role's. On a server reached directly it shows Tenant isolation under concurrency. Through a pooler in lane 4, where 30 clients share a pool of 20 server connections, it also shows that no role or claims outlive the call that set them.
- `crates/gnomon/tests/kinds.rs` is seam 1 for Kind registration: registering and reading Kinds, and evaluating their compiled rules with `jsonb_path_match`.
- `crates/gnomon/tests/nodes.rs` is seam 1 for Nodes: writing a patch of property values citing an Episode, the merge order, same-transaction replacement, As-of reads and Citations. It ends with a smoke check that prints the p50 of a bounded patch on a Node with 100 Contributions and asserts a loose bound. The performance suite proper is a later ticket.
- `crates/gnomon/tests/node_oracle.rs` is the oracle test (ADR-0020). An oracle picks each property's winner straight from the Contributions, and `proptest` compares it with As-of reads after random sequences of writes, some batched in one transaction. A failing sequence shrinks to a short one.
- `crates/gnomon/tests/tenancy.rs` is seam 1 for tenancy modes (ADR-0005): calls that name no Tenant in single-tenant and multi-tenant mode, a Tenant that differs from the claims, the switch to multi-tenant mode keeping every row readable under the former default Tenant, the refused switch back, a switch granted to another role, and a deployment's own `gnomon.tenant_id()` surviving migration 0005.
- `crates/gnomon/tests/sql_boundary.rs` is seam 2: adversarial tests that connect as a caller role with claims set, bypassing the Rust core. For Versions they show that a caller cannot delete committed history, rewrite a closed Recorded time, choose a Recorded time, or forge the mark that lets a transaction delete the rows it wrote. For tenancy they show that no caller role can switch the mode or write the tenancy row, that in multi-tenant mode claims that bind no Tenant reach no row of the former default Tenant, and that after the switch the first Tenant's claims see every row of every table with a Tenant, read from the catalog.

The database suites share the harness in `crates/gnomon/tests/support`. Each test gets a fresh database, created with `CREATE DATABASE ... TEMPLATE` from a template that holds the exported migrations. The harness builds that template once per server and migration set.

## Running the database suites

You need Docker. The harness starts the server itself with testcontainers.

```sh
cargo nextest run                            # pgvector/pgvector:pg17
GNOMON_TEST_IMAGE=supabase cargo nextest run # supabase/postgres 17.x
scripts/test.sh                              # both images, one after the other
```

The container:

- is named `$GNOMON_TEST_CONTAINER_PREFIX-<image>`, with the prefix defaulting to `gnomon-test`. On a machine where several agents or projects run containers, give each its own prefix, such as `gnomon-t2`;
- publishes 5432 on `127.0.0.1` only, on an ephemeral port that Docker picks, so it never collides with another stack;
- keeps its data on a tmpfs and is removed when it stops;
- stops itself about 30 seconds after the last test process disconnects, whether the run passed, failed or was killed. A run that starts within that window reuses it.

To check that nothing is left: `docker ps -a --filter name=gnomon-test`. To remove one by hand: `docker rm -f gnomon-test-pgvector`.

## Choosing the export target

`GNOMON_TEST_TARGET` picks the export target whose own tool builds the template database. The suite then runs against a schema that tool applied.

- `plain`, the default, applies each file in one transaction.
- `supabase` exports into a temporary `supabase/migrations` and runs `supabase migration up --remote=<url>` with the pinned CLI through `npx`. Lane 3 uses it.
- `alembic` exports into a temporary Alembic project and runs `alembic upgrade head` through `uvx`, with Alembic, SQLAlchemy and psycopg pinned, so nothing is installed globally. Its `env.py` runs every pending revision in one transaction, as Alembic's own template does.

```sh
GNOMON_TEST_TARGET=alembic cargo nextest run
```

The supabase and alembic tools connect as the migration role with a password: `GNOMON_TEST_MIGRATION_PASSWORD`, or the superuser's when that is unset.

## Lane 3: the Supabase CLI stack

```sh
crates/gnomon/tests/support/supabase/run-lane-3.sh
```

The script needs Docker and `npx`. It:

1. checks that ports 56030 to 56039 are free. The stack uses only that block, so it never collides with another project's stack on the CLI's default ports;
2. exports the migrations with `--target supabase` into the CLI project in `crates/gnomon/tests/support/supabase`, whose `config.toml` sets `project_id = "gnomon-t3"` and runs only the database, Kong and PostgREST;
3. starts the stack, so the CLI applies the migrations to its `postgres` database as `postgres`;
4. runs `tests/supabase_cli.rs` with `--run-ignored only`, then the whole suite with `GNOMON_TEST_TARGET=supabase` and `GNOMON_TEST_DATABASE_URL` pointed at the stack. `GNOMON_TEST_SUPABASE_API_URL` gives those tests the stack's API gateway, `http://127.0.0.1:56031`;
5. stops the stack with `supabase stop --project-id gnomon-t3 --no-backup`, whether the run passed or not, and stops no other stack.

To check that nothing is left: `docker ps -a --filter name=gnomon-t3`.

The supabase target creates pgvector in the `extensions` schema before migration 0001, as Supabase does, because `create extension` would otherwise put it in `public`, which PostgREST exposes. On the plain and alembic targets pgvector goes where `create extension` puts it. A migration that uses a pgvector type or operator from a function with `search_path = ''` must therefore find the extension's schema in `pg_extension` when it runs, never name `public` or `extensions`.

## Lane 4: PgBouncer and Supavisor

```sh
crates/gnomon/tests/support/pooler/run-lane-4.sh
LANE4_POOLERS=supavisor crates/gnomon/tests/support/pooler/run-lane-4.sh  # one run only
```

The script needs Docker, `npx` and the images `edoburu/pgbouncer`, `pgvector/pgvector:pg17` and `supabase/supavisor:2.10.1`. It runs the whole suite three times, each with `GNOMON_TEST_DATABASE_URL` at the pooler and `GNOMON_TEST_DIRECT_DATABASE_URL` at the server behind it, and prints the pooler's settings before each run:

1. `pgbouncer`: PgBouncer in front of a plain `pgvector/pgvector:pg17` container, on a Docker network of its own, both publishing on loopback with ports Docker picks. The lane sets `POOL_MODE=transaction` and nothing else that pools. `AUTH_TYPE=scram-sha-256` and the `DB_*` variables only connect PgBouncer to the server: the image routes every database to it and looks up every role with PgBouncer's default `auth_query`. PgBouncer 1.26.1 then runs with its defaults, among them `default_pool_size = 20`, `max_client_conn = 100`, `max_prepared_statements = 200` and `server_reset_query_always = 0`, so nothing resets a server connection between transactions.
2. `pgbouncer-named`: the same, with `GNOMON_TEST_NAMED_STATEMENTS=1`. PgBouncer supports named statements at its defaults since 1.24, because `max_prepared_statements` is above 0.
3. `supavisor`: the Supabase CLI local stack in `crates/gnomon/tests/support/pooler`, whose `config.toml` sets `project_id = "gnomon-t5"`, uses ports 56050 to 56059 only, and runs only the database and Supavisor in transaction mode. The CLI registers Supavisor's tenant, `pooler-dev`, the way it does for every local stack: `require_user = false`, `auth_query = "SELECT * FROM pgbouncer.get_auth($1)"`, a pool of 20 and 100 clients. No `NAMED_PREPARED_STATEMENTS` flag is set, so named statements are off, Supavisor's default, and this run sends only unnamed statements. It runs eight tests at a time, because the stack's Postgres allows 100 connections, Supavisor's own metadata pool holds 27, and every test database is a Supavisor pool of its own with an `auth_query` connection beside it. `LANE4_SUPAVISOR_TEST_THREADS` changes that.

The CLI stack runs Supavisor as Supabase configures it, which a standalone `supabase/supavisor` container with a tenant registered by hand would not. Supavisor routes each client to the database it names, so every test still gets its own database.

The script removes what it started, and only that, whether the run passed or not: `docker rm -f gnomon-t5-pgbouncer gnomon-t5-postgres`, the `gnomon-t5-net` network, and `supabase stop --project-id gnomon-t5 --no-backup`. To check that nothing is left: `docker ps -a --filter name=gnomon-t5`.

Ports 56050 to 56059 lie inside Linux's ephemeral port range, so a client socket of an earlier run can hold one for up to a minute in TIME_WAIT. The script refuses to start if a port of the block is listening, and otherwise waits until no socket uses one.

## Running against a server you start yourself

Set `GNOMON_TEST_DATABASE_URL` to a superuser on that server, and the harness starts no container. The Supabase CLI and pooler lanes use this.

```sh
GNOMON_TEST_DATABASE_URL=postgres://postgres:secret@127.0.0.1:55432/postgres \
GNOMON_TEST_MIGRATION_ROLE=postgres \
cargo nextest run
```

Through a pooler, set `GNOMON_TEST_DATABASE_URL` to the pooler and `GNOMON_TEST_DIRECT_DATABASE_URL` to a superuser on the server behind it. The harness then builds templates, creates and drops test databases, and makes every `admin()` and `operator()` connection on the server directly, and only callers go through the pooler: the Store, `TestDb::login()`, and the URLs `login_url` returns. Setup needs a session of its own, which a transaction-mode pooler does not keep. `GNOMON_TEST_POOLER_TENANT` names Supavisor's tenant, which it reads from the user name, so the login role connects as `gnomon_test_login.<tenant>`.

`GNOMON_TEST_MIGRATION_ROLE` is optional. It names the role that applies the migrations when that is not the superuser. On supabase/postgres it is `postgres`, the role a hosted project's migrations run as.

The suites connect the Store with `support::connect_options()`: TLS off, because the test servers run without it, which a deployment must ask for with `Tls::plaintext()`, and named statements on only when `GNOMON_TEST_NAMED_STATEMENTS=1`.

The harness creates a login role, `gnomon_test_login`, and grants it `authenticated` and `gnomon_worker`. The Store connects as that role, as a deployment's own login role would.

The migrations grant Kind registration to no role, because each deployment decides who may register Kinds (ADR-0009). A test that registers Kinds first calls `TestDb::grant_kind_registration(role)`, which grants EXECUTE on `gnomon.register_kind` in that test's database only. Every other test runs without the grant, as a fresh deployment does.

The migrations install single-tenant mode, with a default Tenant. Most tests name a Tenant of their own, which single-tenant mode allows when the claims bind it. A test that needs multi-tenant mode calls `TestDb::switch_to_multi_tenant()`, which runs `gnomon.switch_to_multi_tenant()` as `TestDb::operator()`, the role that applied the migrations, as a deployment's operator would. `TestDb::default_tenant()` reads the default Tenant the same way.

The migrations also create `gnomon_definer`, the NOLOGIN role that owns the definer functions. The role that applies them needs CREATEROLE, as `postgres` has on supabase/postgres.

Test databases are dropped when their test ends. Template databases stay on the server, named `gnomon_template_<dimension>_<hash>`.
