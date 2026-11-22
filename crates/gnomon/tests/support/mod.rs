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
//! The same tests run against every server. Nothing here assumes a port.

#![allow(dead_code)] // Each test binary uses a different part of the harness.

pub mod server;
pub mod wire;

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::process::Command;

use tokio_postgres::{Client, Config, NoTls};

pub use server::Server;

/// The role the Store logs in as. The harness grants it the caller roles and
/// nothing else, the authenticator pattern ADR-0001 relies on.
pub const LOGIN_ROLE: &str = "gnomon_test_login";
const LOGIN_PASSWORD: &str = "gnomon-test-login";

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
    let run = Command::new(env!("CARGO_BIN_EXE_gnomon"))
        .args([
            "migrations",
            "export",
            "--target",
            "plain",
            "--embedding-dimension",
        ])
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
fn exported(dimension: u32) -> Vec<(String, String)> {
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

impl Server {
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
        let files = exported(dimension);
        let mut h = DefaultHasher::new();
        files.hash(&mut h);
        let name = format!("gnomon_template_{dimension}_{:016x}", h.finish());

        let _lock = self.lock();
        let server = self.admin("postgres").await;
        let exists = !server
            .query("select 1 from pg_database where datname = $1", &[&name])
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
        {
            let mut client = self.admin(&building).await;
            if let Some(role) = &self.migration_role {
                client
                    .batch_execute(&format!("set role {}", quote_ident(role)))
                    .await
                    .expect("switch to the migration role");
            }
            for (file, sql) in &files {
                let tx = client.transaction().await.unwrap();
                tx.batch_execute(sql)
                    .await
                    .unwrap_or_else(|e| panic!("apply {file}: {e:?}"));
                tx.commit().await.unwrap();
            }
        }
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
        format!(
            "host={host} port={port} dbname={} user={LOGIN_ROLE} password={LOGIN_PASSWORD}",
            self.name
        )
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

pub fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}
