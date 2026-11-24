//! The harness both database suites share (ADR-0020).
//!
//! Each test gets a fresh database, copied from a template database that
//! holds gnomon's migrations as the `gnomon` binary exports them. The server
//! comes from one of two places:
//!
//! - By default, a container of the image `GNOMON_TEST_IMAGE` names:
//!   `pgvector` (the default, `pgvector/pgvector:pg17`) or `supabase`
//!   (`supabase/postgres` 17.x). See [`server`].
//! - `GNOMON_TEST_DATABASE_URL`, a superuser on a server someone else runs,
//!   such as the Supabase CLI stack or a pooler lane. Then
//!   `GNOMON_TEST_MIGRATION_ROLE` optionally names the role that applies the
//!   migrations, when it is not that superuser.
//!
//! `GNOMON_TEST_TARGET` picks the export target whose own tooling applies
//! the migrations to the template: `plain` (the default) applies each file in
//! one transaction, `supabase` runs `supabase migration up` through `npx`,
//! and `alembic` runs `alembic upgrade head` through `uvx`. See [`Target`].
//!
//! The same tests run against every server. Nothing here assumes a port.

#![allow(dead_code)] // Each test binary uses a different part of the harness.

pub mod alembic;
pub mod erasure;
pub mod perf;

pub mod server;
pub mod tls;
pub mod vectors;
pub mod wire;

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::process::Command;

use tokio_postgres::types::Type;
use tokio_postgres::{Client, Config, NoTls};

pub use gnomon::migrations::Target;
pub use server::Server;

/// The role the Store logs in as. The harness grants it the caller roles and
/// nothing else, the authenticator pattern ADR-0001 relies on.
pub const LOGIN_ROLE: &str = "gnomon_test_login";
const LOGIN_PASSWORD: &str = "gnomon-test-login";

/// The role the vacuum call logs in as: a login role that holds
/// `gnomon_maintainer` with INHERIT and without SET, as migration 0013 tells
/// a deployment to grant it, and may switch to `authenticated` only for the
/// Store's schema check. It shares the login role's password.
pub const MAINTENANCE_LOGIN_ROLE: &str = "gnomon_test_maintenance";

/// The settings the suites connect the Store with.
///
/// The test servers run without TLS, so TLS is off, which a deployment
/// must ask for explicitly. Named statements are on only when
/// `GNOMON_TEST_NAMED_STATEMENTS` is `1`, for a pooler that supports them.
pub fn connect_options() -> gnomon::ConnectOptions {
    gnomon::ConnectOptions::default()
        .tls(gnomon::Tls::plaintext())
        .named_statements(named_statements_from_env())
}

/// Whether `GNOMON_TEST_NAMED_STATEMENTS` turns named statements on.
pub fn named_statements_from_env() -> bool {
    match server::env("GNOMON_TEST_NAMED_STATEMENTS").as_deref() {
        None | Some("0") => false,
        Some("1") => true,
        Some(other) => panic!("GNOMON_TEST_NAMED_STATEMENTS={other:?}: use 0 or 1"),
    }
}

/// An error with every error under it, where the reason is: Postgres's
/// message, or why a TLS handshake failed.
pub fn chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        out.push_str(": ");
        out.push_str(&s.to_string());
        source = s.source();
    }
    out
}

/// A new Tenant, unknown to every other test.
pub fn tenant() -> gnomon::TenantId {
    gnomon::TenantId(uuid::Uuid::new_v4())
}

/// Verified JWT claims for an end user, in the shape the reference
/// `gnomon.tenant_id()` and `gnomon.allowed_scopes()` read.
pub fn user(
    tenant: gnomon::TenantId,
    scopes: &[&str],
) -> serde_json::Map<String, serde_json::Value> {
    match serde_json::json!({
        "sub": uuid::Uuid::new_v4().to_string(),
        "role": "authenticated",
        "app_metadata": { "tenant_id": tenant.to_string(), "scopes": scopes },
    }) {
        serde_json::Value::Object(m) => m,
        _ => unreachable!(),
    }
}

/// Stores a Sentence of an Edge Version through `gnomon.store_sentence`,
/// as an end user who sees the Version's Scope, after the Version
/// committed (migration 0021).
pub async fn store_sentence(
    db: &TestDb,
    tenant: gnomon::TenantId,
    scope: &str,
    version: uuid::Uuid,
    sentence: &str,
) {
    let claims = serde_json::Value::Object(user(tenant, &[scope])).to_string();
    let mut client = db.login().await;
    let tx = client.transaction().await.unwrap();
    tx.batch_execute("set local role authenticated")
        .await
        .unwrap();
    tx.query_typed(
        "select set_config('request.jwt.claims', $1, true)",
        &[(&claims, tokio_postgres::types::Type::TEXT)],
    )
    .await
    .unwrap();
    tx.execute_typed(
        "select gnomon.store_sentence($1, $2, $3)",
        &[
            (&tenant.0, tokio_postgres::types::Type::UUID),
            (&version, tokio_postgres::types::Type::UUID),
            (&sentence, tokio_postgres::types::Type::TEXT),
        ],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

/// Kind registration's signature, as grants and `regprocedure` name it.
pub const REGISTER_KIND: &str =
    "gnomon.register_kind(uuid, text, text, text, jsonb, text[], text[], text[])";

/// The merge rule setter's signature, which a deployment grants like Kind
/// registration.
pub const SET_MERGE_RULE: &str = "gnomon.set_merge_rule(uuid, text, text)";

/// Erasure's signatures, which a deployment grants like Kind registration:
/// Erase Episode, and the Erasure of a Node, an Edge or a Scope.
pub const ERASE_EPISODE: &str = "gnomon.erase_episode(uuid, uuid)";
pub const ERASE_NODE: &str = "gnomon.erase_node(uuid, uuid)";
pub const ERASE_EDGE: &str = "gnomon.erase_edge(uuid, uuid)";
pub const ERASE_SCOPE: &str = "gnomon.erase_scope(uuid, text)";
pub const ERASURES: &[&str] = &[ERASE_EPISODE, ERASE_NODE, ERASE_EDGE, ERASE_SCOPE];

/// The embedding dimension the suites export with unless a test chooses one.
pub const DEFAULT_DIMENSION: u32 = 1536;

pub struct TestDb {
    server: &'static Server,
    pub name: String,
}

async fn connect(config: &Config) -> Client {
    let (client, connection) = config
        .connect(NoTls)
        .await
        .expect("connect to the test server");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

/// Writes the migrations with the `gnomon` binary, as a consumer would.
pub fn export_plain(dimension: u32, out: &Path) {
    export_to(Target::Plain, dimension, out)
}

/// Writes the migrations for `target` into `out` with the `gnomon` binary.
pub fn export_to(target: Target, dimension: u32, out: &Path) {
    let run = Command::new(env!("CARGO_BIN_EXE_gnomon"))
        .args(["migrations", "export", "--target", &target.to_string()])
        .arg("--embedding-dimension")
        .arg(dimension.to_string())
        .arg("--out")
        .arg(out)
        .output()
        .expect("the gnomon binary runs");
    assert!(
        run.status.success(),
        "export failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
}

/// The exported files, in the order a chain applies them.
pub fn exported(dimension: u32) -> Vec<(String, String)> {
    let out = tempfile::tempdir().unwrap();
    export_plain(dimension, out.path());
    let mut files: Vec<_> = std::fs::read_dir(out.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    files.sort();
    files
        .into_iter()
        .map(|p| {
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            (name, std::fs::read_to_string(&p).unwrap())
        })
        .collect()
}

/// The Supabase CLI the suite runs, pinned so every run applies migrations
/// the same way.
pub const SUPABASE_CLI: &str = "supabase@3.2.0";

/// The export target whose tooling applies the migrations to a template:
/// the one `GNOMON_TEST_TARGET` names, `plain` by default.
pub fn target_from_env() -> Target {
    match std::env::var("GNOMON_TEST_TARGET") {
        Err(_) => Target::Plain,
        Ok(name) if name.is_empty() => Target::Plain,
        Ok(name) => name
            .parse()
            .unwrap_or_else(|e| panic!("GNOMON_TEST_TARGET: {e}")),
    }
}

/// Runs `supabase migration up` with the pinned CLI, applying the
/// migrations in `workdir/supabase/migrations` that the database at `url`
/// has not applied yet. Returns the CLI's output.
pub fn supabase_migration_up(workdir: &Path, url: &str) -> Result<String, String> {
    let run = Command::new("npx")
        .args(["--yes", SUPABASE_CLI, "migration", "up", "--yes"])
        .arg("--workdir")
        .arg(workdir)
        .arg(format!("--remote={url}"))
        .output()
        .expect("npx runs; see docs/testing.md");
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    if run.status.success() {
        Ok(output)
    } else {
        Err(output)
    }
}

impl Server {
    /// A URL for the role that applies the migrations, connected to `db`,
    /// for the tools of the supabase and alembic targets. Its password is
    /// `GNOMON_TEST_MIGRATION_PASSWORD`, or the superuser's when that is
    /// unset, as on the Supabase CLI stack and the test images.
    fn migration_url(&self, scheme: &str, db: &str) -> String {
        let (host, port) = self.admin_host_port();
        let user = self
            .migration_role
            .clone()
            .or_else(|| self.admin.get_user().map(str::to_owned))
            .expect("the server names its superuser");
        let password = std::env::var("GNOMON_TEST_MIGRATION_PASSWORD").unwrap_or_else(|_| {
            String::from_utf8(self.admin.get_password().unwrap_or_default().to_vec()).unwrap()
        });
        format!(
            "{scheme}://{}:{}@{host}:{port}/{db}",
            percent_encode(&user),
            percent_encode(&password)
        )
    }

    /// Applies the migrations exported for `target` to database `db`, with
    /// that target's own tooling.
    async fn apply(&self, target: Target, dimension: u32, db: &str) {
        match target {
            Target::Plain => {
                let mut client = self.admin(db).await;
                if let Some(role) = &self.migration_role {
                    client
                        .batch_execute(&format!("set role {}", quote_ident(role)))
                        .await
                        .expect("switch to the migration role");
                }
                for (file, sql) in &exported(dimension) {
                    let tx = client.transaction().await.unwrap();
                    tx.batch_execute(sql)
                        .await
                        .unwrap_or_else(|e| panic!("apply {file}: {e:?}"));
                    tx.commit().await.unwrap();
                }
            }
            Target::Supabase => {
                let workdir = tempfile::tempdir().unwrap();
                export_to(
                    target,
                    dimension,
                    &workdir.path().join("supabase/migrations"),
                );
                supabase_migration_up(workdir.path(), &self.migration_url("postgresql", db))
                    .unwrap_or_else(|e| panic!("supabase migration up failed: {e}"));
            }
            Target::Alembic => {
                let project = alembic::Project::new();
                export_to(target, dimension, &project.versions());
                project
                    .alembic(
                        &["upgrade", "head"],
                        Some(&self.migration_url("postgresql+psycopg", db)),
                    )
                    .unwrap_or_else(|e| panic!("alembic upgrade head failed: {e}"));
            }
        }
    }

    /// The superuser's connection to database `db`.
    fn admin_config(&self, db: &str) -> Config {
        let mut c = self.admin.clone();
        c.dbname(db);
        c
    }

    async fn admin(&self, db: &str) -> Client {
        connect(&self.admin_config(db)).await
    }

    /// The `owner` clause that gives a new database to the migration role.
    fn owner_clause(&self) -> String {
        self.migration_role
            .as_deref()
            .map(|r| format!(" owner {}", quote_ident(r)))
            .unwrap_or_default()
    }

    /// The template database holding the migrations exported with
    /// `dimension`, built once per server and migration set. Every process
    /// takes the server's lock first, so two never build one at once.
    async fn template(&self, dimension: u32) -> String {
        // The plain export names the migration set. A supabase export's
        // file names change with its clock, so they cannot.
        let target = target_from_env();
        let mut h = DefaultHasher::new();
        exported(dimension).hash(&mut h);
        let name = match target {
            Target::Plain => format!("gnomon_template_{dimension}_{:016x}", h.finish()),
            _ => format!("gnomon_template_{target}_{dimension}_{:016x}", h.finish()),
        };

        let _lock = self.lock();
        let server = self.admin("postgres").await;
        let exists = !server
            .query_typed(
                "select 1 from pg_database where datname = $1",
                &[(&name, Type::TEXT)],
            )
            .await
            .unwrap()
            .is_empty();
        if exists {
            return name;
        }

        // Build under another name and rename at the end, so a build that
        // dies part way never leaves a template that looks complete.
        let building = format!("{name}_building");
        // Each statement alone: Postgres runs a multi-statement query as one
        // transaction, and neither statement may run inside one.
        server
            .batch_execute(&format!("drop database if exists {building} with (force)"))
            .await
            .expect("drop a half-built template database");
        server
            .batch_execute(&format!(
                "create database {building}{}",
                self.owner_clause()
            ))
            .await
            .expect("create the template database");
        self.apply(target, dimension, &building).await;
        server
            .batch_execute(&format!("alter database {building} rename to {name}"))
            .await
            .expect("publish the template database");
        self.ensure_login_role(&server).await;
        name
    }

    /// What a deployment does once: a login role that may switch to the
    /// caller roles and nothing more. The caller roles come from the
    /// migrations; a server with none yet gets them here, as if a deployment
    /// had set its roles up first.
    async fn ensure_login_role(&self, server: &Client) {
        server
            .batch_execute(&format!(
                "do $$ begin
                   if not exists (select from pg_roles where rolname = 'authenticated') then
                     create role authenticated nologin noinherit;
                   end if;
                   if not exists (select from pg_roles where rolname = 'gnomon_worker') then
                     create role gnomon_worker nologin noinherit nobypassrls;
                   end if;
                   if not exists (select from pg_roles where rolname = '{LOGIN_ROLE}') then
                     create role {LOGIN_ROLE} login noinherit password '{LOGIN_PASSWORD}';
                   end if;
                   if not exists (select from pg_roles where rolname = '{MAINTENANCE_LOGIN_ROLE}') then
                     create role {MAINTENANCE_LOGIN_ROLE} login noinherit password '{LOGIN_PASSWORD}';
                   end if;
                   if exists (select from pg_roles where rolname = 'gnomon_maintainer') then
                     grant gnomon_maintainer to {MAINTENANCE_LOGIN_ROLE} with inherit true, set false;
                   end if;
                 end $$;
                 grant authenticated, gnomon_worker to {LOGIN_ROLE};
                 grant authenticated to {MAINTENANCE_LOGIN_ROLE};"
            ))
            .await
            .expect("create the login role");
    }
}

impl TestDb {
    async fn create(server: &'static Server, template: Option<&str>) -> TestDb {
        let name = format!("gnomon_test_{}", uuid::Uuid::new_v4().simple());
        let from = template
            .map(|t| format!(" template {t}"))
            .unwrap_or_default();
        server
            .admin("postgres")
            .await
            .batch_execute(&format!(
                "create database {name}{from}{}",
                server.owner_clause()
            ))
            .await
            .expect("create the test database");
        TestDb { server, name }
    }

    /// A new database with gnomon's migrations exported and applied.
    pub async fn migrated() -> TestDb {
        Self::migrated_with_dimension(DEFAULT_DIMENSION).await
    }

    pub async fn migrated_with_dimension(dimension: u32) -> TestDb {
        let server = Server::get().await;
        let template = server.template(dimension).await;
        Self::create(server, Some(&template)).await
    }

    /// A database with no gnomon schema, on a server whose caller roles and
    /// login role exist, as if a deployment had set up its roles and not
    /// applied the migrations.
    pub async fn empty_with_login() -> TestDb {
        let server = Server::get().await;
        server.template(DEFAULT_DIMENSION).await;
        Self::create(server, None).await
    }

    /// Renames this database to `name` and stops managing it: it is not
    /// dropped when `self` goes. The performance suite keeps its loaded
    /// data this way, so a run builds it once. Every connection to the
    /// database must be closed first.
    pub async fn keep_as(self, name: &str) {
        let server = self.server;
        server
            .admin("postgres")
            .await
            .batch_execute(&format!(
                "alter database {} rename to {}",
                quote_ident(&self.name),
                quote_ident(name)
            ))
            .await
            .expect("rename the database to keep");
        std::mem::forget(self);
    }

    /// The superuser, connected to this database. Only setup and tamper
    /// steps use it.
    pub async fn admin(&self) -> Client {
        self.server.admin(&self.name).await
    }

    /// What a deployment does to let `role` register Kinds. The migrations
    /// grant Kind registration to no one (ADR-0009).
    pub async fn grant_kind_registration(&self, role: &str) {
        let role = quote_ident(role);
        self.admin()
            .await
            .batch_execute(&format!(
                "grant execute on function {REGISTER_KIND} to {role};
                 grant execute on function {SET_MERGE_RULE} to {role}"
            ))
            .await
            .expect("grant Kind registration");
    }

    /// What a deployment does to let `role` erase Episodes, Nodes, Edges
    /// and Scopes. The migrations grant Erasure to no one (ADR-0009).
    pub async fn grant_erasure(&self, role: &str) {
        let role = quote_ident(role);
        let admin = self.admin().await;
        for erasure in ERASURES {
            admin
                .batch_execute(&format!("grant execute on function {erasure} to {role}"))
                .await
                .expect("grant Erasure");
        }
    }

    /// The deployment's operator: the role that applied the migrations and
    /// owns their objects, connected to this database. It switches the
    /// tenancy mode, which no caller role may do.
    pub async fn operator(&self) -> Client {
        let client = self.admin().await;
        if let Some(role) = &self.server.migration_role {
            client
                .batch_execute(&format!("set role {}", quote_ident(role)))
                .await
                .expect("switch to the migration role");
        }
        client
    }

    /// The default Tenant the migrations created. In multi-tenant mode it
    /// is the deployment's first Tenant.
    pub async fn default_tenant(&self) -> gnomon::TenantId {
        let row = self
            .operator()
            .await
            .query_typed("select default_tenant_id from gnomon.tenancy", &[])
            .await
            .expect("read the tenancy row");
        gnomon::TenantId(row[0].get(0))
    }

    /// What an operator does once to make the deployment multi-tenant.
    pub async fn switch_to_multi_tenant(&self) {
        self.operator()
            .await
            .batch_execute("select gnomon.switch_to_multi_tenant()")
            .await
            .expect("switch to multi-tenant mode");
    }

    /// This database's URL for the role that applies the migrations, with
    /// `scheme`, for a target's own migration tool.
    pub fn migration_url(&self, scheme: &str) -> String {
        self.server.migration_url(scheme, &self.name)
    }

    /// The Store, connected as the login role with [`connect_options`].
    pub async fn store(&self) -> gnomon::Store {
        gnomon::Store::connect(&self.login_url(), &connect_options())
            .await
            .expect("the Store connects")
    }

    /// The Store, connected as [`MAINTENANCE_LOGIN_ROLE`] through the callers'
    /// path, the pooler in a pooler lane, to run the vacuum call.
    pub async fn maintenance_store(&self) -> gnomon::Store {
        gnomon::Store::connect(&self.maintenance_url(), &connect_options())
            .await
            .expect("the Store connects as the maintenance role")
    }

    /// The maintenance role's connection string, through the callers' path.
    pub fn maintenance_url(&self) -> String {
        let (host, port) = self.server.callers_host_port();
        login_url(
            &server::pooled_user(MAINTENANCE_LOGIN_ROLE),
            &host,
            port,
            &self.name,
        )
    }

    /// A raw connection as the login role, for the SQL-boundary suite.
    pub async fn login(&self) -> Client {
        connect(&self.login_config()).await
    }

    fn login_config(&self) -> Config {
        let (host, port) = self.server.callers_host_port();
        let mut c = Config::new();
        c.host(&host)
            .port(port)
            .dbname(&self.name)
            .user(&self.server.login_user)
            .password(LOGIN_PASSWORD);
        c
    }

    /// The login role's connection string, in the key=value form the Store
    /// accepts.
    pub fn login_url(&self) -> String {
        let (host, port) = self.server.callers_host_port();
        self.login_url_at(&host, port)
    }

    /// The login role's connection string, through a proxy at `addr`.
    pub fn login_url_through(&self, addr: std::net::SocketAddr) -> String {
        self.login_url_at(&addr.ip().to_string(), addr.port())
    }

    fn login_url_at(&self, host: &str, port: u16) -> String {
        login_url(&self.server.login_user, host, port, &self.name)
    }

    /// Where callers connect, for a proxy to forward to: the pooler in a
    /// pooler lane, otherwise the server.
    pub async fn server_addr(&self) -> std::net::SocketAddr {
        let (host, port) = self.server.callers_host_port();
        resolve(&host, port).await
    }

    /// The server's own address, behind any pooler, for a test of what the
    /// Store sends whichever pooler the lane runs.
    pub async fn direct_server_addr(&self) -> std::net::SocketAddr {
        let (host, port) = self.server.admin_host_port();
        resolve(&host, port).await
    }

    /// The login role's connection string, through a proxy at `addr` in
    /// front of the server itself, with no pooler's user name suffix.
    pub fn direct_login_url_through(&self, addr: std::net::SocketAddr) -> String {
        login_url(LOGIN_ROLE, &addr.ip().to_string(), addr.port(), &self.name)
    }
}

impl Drop for TestDb {
    /// Drops the database, so a long-lived server does not fill up.
    fn drop(&mut self) {
        let config = self.server.admin_config("postgres");
        let name = self.name.clone();
        // Drop runs inside the test's runtime, which cannot block on itself.
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                if let Ok((client, conn)) = config.connect(NoTls).await {
                    tokio::spawn(conn);
                    let _ = client
                        .batch_execute(&format!("drop database if exists {name} with (force)"))
                        .await;
                }
            });
        })
        .join();
    }
}

async fn resolve(host: &str, port: u16) -> std::net::SocketAddr {
    let mut addrs = tokio::net::lookup_host((host, port)).await.unwrap();
    addrs.next().unwrap()
}

/// The login role's connection string for database `db` on the server.
pub async fn login_url_to(db: &str) -> String {
    let server = Server::get().await;
    let (host, port) = server.callers_host_port();
    login_url(&server.login_user, &host, port, db)
}

fn login_url(user: &str, host: &str, port: u16, db: &str) -> String {
    format!("host={host} port={port} dbname={db} user={user} password={LOGIN_PASSWORD}")
}

/// `s` with every byte but unreserved URL characters percent-encoded.
fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

pub fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}
