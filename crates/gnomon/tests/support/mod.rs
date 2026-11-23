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
pub mod server;
pub mod wire;

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::process::Command;

use tokio_postgres::types::Type;
use tokio_postgres::{Client, Config, NoTls};

pub use server::Server;

/// The role the Store logs in as. The harness grants it the caller roles and
/// nothing else, the authenticator pattern ADR-0001 relies on.
pub const LOGIN_ROLE: &str = "gnomon_test_login";
const LOGIN_PASSWORD: &str = "gnomon-test-login";

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

/// Kind registration's signature, as grants and `regprocedure` name it.
pub const REGISTER_KIND: &str =
    "gnomon.register_kind(uuid, text, text, text, jsonb, text[], text[], text[])";

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
        .args(["migrations", "export", "--target", target.name()])
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

/// The export target whose tooling applies the migrations to a template.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Plain,
    Supabase,
    Alembic,
}

impl Target {
    /// The target `GNOMON_TEST_TARGET` names, `plain` by default.
    pub fn from_env() -> Target {
        match std::env::var("GNOMON_TEST_TARGET").as_deref() {
            Err(_) | Ok("") | Ok("plain") => Target::Plain,
            Ok("supabase") => Target::Supabase,
            Ok("alembic") => Target::Alembic,
            Ok(other) => panic!(
                "GNOMON_TEST_TARGET={other:?} is not a target the suite knows: use plain, supabase or alembic"
            ),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Target::Plain => "plain",
            Target::Supabase => "supabase",
            Target::Alembic => "alembic",
        }
    }
}

impl Server {
    /// A URL for the role that applies the migrations, connected to `db`,
    /// for the tools of the supabase and alembic targets. Its password is
    /// `GNOMON_TEST_MIGRATION_PASSWORD`, or the superuser's when that is
    /// unset, as on the Supabase CLI stack and the test images.
    fn migration_url(&self, scheme: &str, db: &str) -> String {
        let (host, port) = self.host_port();
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
                let run = Command::new("npx")
                    .args(["--yes", SUPABASE_CLI, "migration", "up", "--yes"])
                    .arg("--workdir")
                    .arg(workdir.path())
                    .arg(format!("--remote={}", self.migration_url("postgresql", db)))
                    .output()
                    .expect("npx runs; see docs/testing.md");
                assert!(
                    run.status.success(),
                    "supabase migration up failed: {}{}",
                    String::from_utf8_lossy(&run.stdout),
                    String::from_utf8_lossy(&run.stderr)
                );
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
        let target = Target::from_env();
        let mut h = DefaultHasher::new();
        exported(dimension).hash(&mut h);
        let name = match target {
            Target::Plain => format!("gnomon_template_{dimension}_{:016x}", h.finish()),
            _ => format!(
                "gnomon_template_{}_{dimension}_{:016x}",
                target.name(),
                h.finish()
            ),
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
                 end $$;
                 grant authenticated, gnomon_worker to {LOGIN_ROLE};"
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

    /// The superuser, connected to this database. Only setup and tamper
    /// steps use it.
    pub async fn admin(&self) -> Client {
        self.server.admin(&self.name).await
    }

    /// What a deployment does to let `role` register Kinds. The migrations
    /// grant Kind registration to no one (ADR-0009).
    pub async fn grant_kind_registration(&self, role: &str) {
        self.admin()
            .await
            .batch_execute(&format!(
                "grant execute on function {REGISTER_KIND} to {}",
                quote_ident(role)
            ))
            .await
            .expect("grant Kind registration");
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

    /// A raw connection as the login role, for the SQL-boundary suite.
    pub async fn login(&self) -> Client {
        connect(&self.login_config()).await
    }

    fn login_config(&self) -> Config {
        let mut c = self.server.admin_config(&self.name);
        c.user(LOGIN_ROLE).password(LOGIN_PASSWORD);
        c
    }

    /// The login role's connection string, in the key=value form the Store
    /// accepts.
    pub fn login_url(&self) -> String {
        let (host, port) = self.server.host_port();
        self.login_url_at(&host, port)
    }

    /// The login role's connection string, through a proxy at `addr`.
    pub fn login_url_through(&self, addr: std::net::SocketAddr) -> String {
        self.login_url_at(&addr.ip().to_string(), addr.port())
    }

    fn login_url_at(&self, host: &str, port: u16) -> String {
        login_url(host, port, &self.name)
    }

    /// The server's TCP address, for a proxy to forward to.
    pub async fn server_addr(&self) -> std::net::SocketAddr {
        let (host, port) = self.server.host_port();
        let mut addrs = tokio::net::lookup_host((host.as_str(), port))
            .await
            .unwrap();
        addrs.next().unwrap()
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

/// The login role's connection string for database `db` on the server.
pub async fn login_url_to(db: &str) -> String {
    let (host, port) = Server::get().await.host_port();
    login_url(&host, port, db)
}

fn login_url(host: &str, port: u16, db: &str) -> String {
    format!("host={host} port={port} dbname={db} user={LOGIN_ROLE} password={LOGIN_PASSWORD}")
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
