//! The harness both database suites share (ADR-0020).
//!
//! Each test gets a fresh database, copied from a template database that
//! holds gnomon's migration files, applied in order, each in one
//! transaction, as a consumer's tooling applies them. The server comes from
//! one of two places:
//!
//! - By default, a container of the image `GNOMON_TEST_IMAGE` names:
//!   `pgvector` (the default, `pgvector/pgvector:pg17`) or `supabase`
//!   (`supabase/postgres` 17.x). See [`server`].
//! - `GNOMON_TEST_DATABASE_URL`, a superuser on a server someone else runs,
//!   such as a pooler lane. Then `GNOMON_TEST_MIGRATION_ROLE` optionally
//!   names the role that applies the migrations, when it is not that
//!   superuser.
//!
//! The same tests run against every server. Nothing here assumes a port.

#![allow(dead_code)] // Each test binary uses a different part of the harness.

pub mod alembic;
pub mod erasure;
pub mod install;
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

pub use server::Server;

/// The role the Store logs in as. The harness grants it the caller roles and
/// nothing else, the authenticator pattern ADR-0001 relies on.
pub const LOGIN_ROLE: &str = "gnomon_test_login";
const LOGIN_PASSWORD: &str = "gnomon-test-login";

/// The settings the suites connect the Store with.
///
/// The test servers run without TLS, so TLS is off, which a deployment
/// must ask for explicitly. Every statement is sent unnamed, as the Store
/// always sends them.
pub fn connect_options() -> gnomon::ConnectOptions {
    gnomon::ConnectOptions::default().tls(gnomon::Tls::plaintext())
}

/// A Store for `url` that has run the schema check, as a server does at
/// startup. `Store::connect` itself opens no connection, so a test that
/// needs the check or a refused connection to fail calls this.
pub async fn connect_and_check(
    url: &str,
    options: &gnomon::ConnectOptions,
) -> Result<gnomon::Store, gnomon::Error> {
    let store = gnomon::Store::connect(url, options).await?;
    store.check().await?;
    Ok(store)
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

/// The start of a day in UTC.
pub fn utc(y: i16, m: i8, d: i8) -> jiff::Timestamp {
    jiff::tz::Offset::UTC
        .to_timestamp(jiff::civil::date(y, m, d).at(0, 0, 0, 0))
        .expect("the day is in range")
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
/// committed (`0004_search.sql`).
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

/// The embedding dimension the files state, which the suites use unless a
/// test chooses another.
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

/// The migration files, in the order a chain applies them, with the
/// dimension line changed to `dimension`, as a consumer whose model has
/// another width edits it.
pub fn migration_files(dimension: u32) -> Vec<(String, String)> {
    // The line of `gnomon.embedding_dimension()` that states the dimension.
    let line = format!("as $$ select {DEFAULT_DIMENSION} $$");
    gnomon::migrations::MIGRATIONS
        .iter()
        .map(|m| {
            if m.version == 1 {
                assert!(m.sql.contains(&line), "0001 states no dimension");
            }
            let sql = m
                .sql
                .replace(&line, &format!("as $$ select {dimension} $$"));
            (format!("{}.sql", m.name), sql)
        })
        .collect()
}

/// Writes the migration files into `dir` as a consumer copies them, each
/// named `<prefix><file name>`, and returns the paths.
pub fn copy_migration_files(
    dir: &Path,
    prefix: impl Fn(usize) -> String,
) -> Vec<std::path::PathBuf> {
    std::fs::create_dir_all(dir).unwrap();
    migration_files(DEFAULT_DIMENSION)
        .into_iter()
        .enumerate()
        .map(|(i, (name, sql))| {
            let path = dir.join(format!("{}{name}", prefix(i)));
            std::fs::write(&path, sql).unwrap();
            path
        })
        .collect()
}

/// The Supabase CLI the suite runs, pinned so every run applies migrations
/// the same way.
pub const SUPABASE_CLI: &str = "supabase@3.2.0";

/// A command that runs the Supabase CLI: the installed one that
/// `GNOMON_TEST_SUPABASE_CLI` names, or else the pinned one through `npx`.
/// Whoever sets the variable installs the pinned version, as CI does.
fn supabase_cli() -> Command {
    match std::env::var_os("GNOMON_TEST_SUPABASE_CLI") {
        Some(installed) if !installed.is_empty() => Command::new(installed),
        _ => {
            let mut npx = Command::new("npx");
            npx.args(["--yes", SUPABASE_CLI]);
            npx
        }
    }
}

/// Runs `supabase migration up` with the pinned CLI, applying the
/// migrations in `workdir/supabase/migrations` that the database at `url`
/// has not applied yet. Returns the CLI's output.
pub fn supabase_migration_up(workdir: &Path, url: &str) -> Result<String, String> {
    let run = supabase_cli()
        .args(["migration", "up", "--yes"])
        .arg("--workdir")
        .arg(workdir)
        .arg(format!("--remote={url}"))
        .output()
        .expect("the Supabase CLI runs; see docs/testing.md");
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
    /// for a consumer's migration tool, such as the Supabase CLI or Alembic. Its password is
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

    /// Applies the migration files to database `db` as the migration role,
    /// each file in one transaction.
    async fn apply(&self, dimension: u32, db: &str) {
        let mut client = self.admin(db).await;
        if let Some(role) = &self.migration_role {
            client
                .batch_execute(&format!("set role {}", quote_ident(role)))
                .await
                .expect("switch to the migration role");
        }
        for (file, sql) in &migration_files(dimension) {
            let tx = client.transaction().await.unwrap();
            tx.batch_execute(sql)
                .await
                .unwrap_or_else(|e| panic!("apply {file}: {e:?}"));
            tx.commit().await.unwrap();
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

    /// The template database holding the migration files applied with
    /// `dimension`, built once per server and migration set. Every process
    /// takes the server's lock first, so two never build one at once.
    async fn template(&self, dimension: u32) -> String {
        let mut h = DefaultHasher::new();
        migration_files(dimension).hash(&mut h);
        let name = format!("gnomon_template_{dimension}_{:016x}", h.finish());

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
        self.apply(dimension, &building).await;
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

    /// A new database with gnomon's migration files applied.
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
    /// `scheme`, for a consumer's migration tool.
    pub fn migration_url(&self, scheme: &str) -> String {
        self.server.migration_url(scheme, &self.name)
    }

    /// The Store, connected as the login role with [`connect_options`].
    pub async fn store(&self) -> gnomon::Store {
        gnomon::Store::connect(&self.login_url(), &connect_options())
            .await
            .expect("the Store connects")
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
