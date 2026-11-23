//! The Postgres server a test run uses, shared by every test process.
//!
//! `cargo nextest` runs each test in its own process, so the server cannot
//! belong to one process. The first process to need it starts a container
//! with testcontainers. Later processes find it by name and reuse it. A file
//! lock in Cargo's target directory makes sure only one process starts it, or
//! builds a template database, at a time.
//!
//! The container is named `$GNOMON_TEST_CONTAINER_PREFIX-<image>` (default
//! prefix `gnomon-test`), publishes 5432 on loopback with an ephemeral host
//! port, keeps its data on a tmpfs, and is auto-removed when it stops. It
//! stops itself: a watcher inside it shuts Postgres down once no client has
//! been connected for `IDLE_SECONDS`, which happens soon after the last test
//! process exits, whether the run passed, failed or was killed.

use std::collections::HashMap;
use std::fs::File;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use testcontainers::bollard::models::{HostConfig, PortBinding};
use testcontainers::core::IntoContainerPort;
use testcontainers::runners::AsyncRunner;
use testcontainers::{GenericImage, ImageExt, ReuseDirective};
use tokio::sync::OnceCell;
use tokio_postgres::config::Host;
use tokio_postgres::{Config, NoTls};

/// Seconds with no client connected after which the container stops.
const IDLE_SECONDS: u32 = 30;
const PASSWORD: &str = "gnomon-test";

/// Starts Postgres through the image's own entrypoint, then watches for
/// clients and stops Postgres when none has been connected for a while.
/// Before the first client it waits up to ten minutes.
const WRAPPER: &str = r#"
set -u
docker-entrypoint.sh "$@" &
pg=$!
trap 'kill -INT "$pg" 2>/dev/null' INT TERM
idle=0
seen=0
while kill -0 "$pg" 2>/dev/null; do
  sleep 1
  n=$(psql -X -h /var/run/postgresql -U "$GNOMON_ADMIN" -d postgres -tAc \
    "select count(*) from pg_stat_activity where backend_type = 'client backend' and pid <> pg_backend_pid()" \
    2>/dev/null) || continue
  if [ "$n" -gt 0 ]; then seen=1; idle=0; else idle=$((idle + 1)); fi
  limit=$GNOMON_IDLE_SECONDS
  [ "$seen" = 1 ] || limit=600
  if [ "$idle" -ge "$limit" ]; then kill -INT "$pg"; break; fi
done
wait "$pg"
"#;

/// An image the suite runs against.
struct TestImage {
    key: &'static str,
    name: &'static str,
    tag: &'static str,
    /// The image's superuser.
    admin: &'static str,
    /// The role a hosted chain's migrations run as, when it is not the
    /// superuser. supabase/postgres keeps `postgres` as an ordinary role
    /// with CREATEROLE, as a hosted project does.
    migration_role: Option<&'static str>,
    /// The image's own command, passed on to its entrypoint.
    cmd: &'static [&'static str],
}

const IMAGES: &[TestImage] = &[
    TestImage {
        key: "pgvector",
        name: "pgvector/pgvector",
        tag: "pg17",
        admin: "postgres",
        migration_role: None,
        cmd: &["postgres"],
    },
    TestImage {
        key: "supabase",
        name: "public.ecr.aws/supabase/postgres",
        tag: "17.12.0.000",
        admin: "supabase_admin",
        migration_role: Some("postgres"),
        cmd: &["postgres", "-D", "/etc/postgresql"],
    },
];

pub struct Server {
    /// A superuser on the server, with no database chosen. Setup steps
    /// connect here, straight to the server even in a pooler lane.
    pub(super) admin: Config,
    /// Where callers connect: the Store and raw login-role connections. The
    /// pooler in a pooler lane, otherwise the server itself.
    callers: (String, u16),
    /// The user name the login role connects with: the role itself, or
    /// `<role>.<tenant>` behind Supavisor.
    pub(super) login_user: String,
    pub(super) migration_role: Option<String>,
    lock_path: PathBuf,
}

/// Holds the server's cross-process lock until dropped.
pub struct Lock(File);

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

static SERVER: OnceCell<Server> = OnceCell::const_new();

impl Server {
    /// The server for this run, started or found on first use.
    pub async fn get() -> &'static Server {
        SERVER.get_or_init(Self::start).await
    }

    async fn start() -> Server {
        if let Ok(url) = std::env::var("GNOMON_TEST_DATABASE_URL") {
            let tests: Config = url
                .parse()
                .expect("GNOMON_TEST_DATABASE_URL is a Postgres connection string");
            // In a pooler lane, setup goes straight to the server behind
            // the pooler, and only callers go through it.
            let admin: Config = match env("GNOMON_TEST_DIRECT_DATABASE_URL") {
                Some(direct) => direct
                    .parse()
                    .expect("GNOMON_TEST_DIRECT_DATABASE_URL is a Postgres connection string"),
                None => tests.clone(),
            };
            let migration_role = env("GNOMON_TEST_MIGRATION_ROLE");
            let (host, port) = host_port(&admin);
            let server = Server {
                callers: host_port(&tests),
                admin,
                migration_role,
                login_user: login_user(),
                lock_path: lock_path(&format!("url-{host}-{port}")),
            };
            server.wait_ready().await;
            return server;
        }

        let key = std::env::var("GNOMON_TEST_IMAGE").unwrap_or_else(|_| "pgvector".into());
        let image = IMAGES.iter().find(|i| i.key == key).unwrap_or_else(|| {
            panic!("GNOMON_TEST_IMAGE={key:?} is not an image the suite knows: use pgvector or supabase")
        });
        let prefix =
            std::env::var("GNOMON_TEST_CONTAINER_PREFIX").unwrap_or_else(|_| "gnomon-test".into());
        let container_name = format!("{prefix}-{}", image.key);
        let lock_path = lock_path(&container_name);

        let port = {
            let _lock = Lock::take(&lock_path);
            start_container(image, &prefix, &container_name).await
        };
        let mut admin = Config::new();
        admin
            .host("127.0.0.1")
            .port(port)
            .user(image.admin)
            .password(PASSWORD)
            .dbname("postgres");
        let server = Server {
            callers: host_port(&admin),
            admin,
            migration_role: image.migration_role.map(str::to_owned),
            login_user: login_user(),
            lock_path,
        };
        server.wait_ready().await;
        server
    }

    /// Takes the server's cross-process lock.
    pub fn lock(&self) -> Lock {
        Lock::take(&self.lock_path)
    }

    /// The server's own address, where setup steps connect.
    pub fn admin_host_port(&self) -> (String, u16) {
        host_port(&self.admin)
    }

    /// Where callers connect: the pooler in a pooler lane, otherwise the
    /// server's own address.
    pub fn callers_host_port(&self) -> (String, u16) {
        self.callers.clone()
    }

    /// Waits until the superuser can run a query. A new container accepts
    /// TCP connections only once its init scripts have finished.
    async fn wait_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(180);
        loop {
            let attempt = async {
                let (client, conn) = self.admin.connect(NoTls).await?;
                tokio::spawn(conn);
                client.simple_query("select 1").await
            };
            match attempt.await {
                Ok(_) => return,
                Err(e) if Instant::now() > deadline => {
                    panic!("the test server never became ready: {e}")
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(250)).await,
            }
        }
    }
}

impl Lock {
    fn take(path: &PathBuf) -> Lock {
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .expect("open the test server's lock file");
        file.lock().expect("take the test server's lock");
        Lock(file)
    }
}

/// An environment variable, if it is set and not empty.
fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// The login role's user name: `GNOMON_TEST_POOLER_TENANT`, when set, is
/// Supavisor's tenant, which it reads from the user name's suffix.
fn login_user() -> String {
    match env("GNOMON_TEST_POOLER_TENANT") {
        Some(tenant) => format!("{}.{tenant}", super::LOGIN_ROLE),
        None => super::LOGIN_ROLE.to_owned(),
    }
}

fn lock_path(key: &str) -> PathBuf {
    let safe: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("{safe}.lock"))
}

fn host_port(config: &Config) -> (String, u16) {
    let host = match &config.get_hosts()[0] {
        Host::Tcp(h) => h.clone(),
        #[cfg(unix)]
        Host::Unix(_) => panic!("the suite connects over TCP"),
    };
    (host, config.get_ports().first().copied().unwrap_or(5432))
}

/// Starts the image's container, or finds the running one with that name,
/// and returns its host port.
async fn start_container(image: &TestImage, prefix: &str, container_name: &str) -> u16 {
    let mut cmd = vec![
        "-c".to_owned(),
        WRAPPER.to_owned(),
        "gnomon-test-db".to_owned(),
    ];
    cmd.extend(image.cmd.iter().map(|s| (*s).to_owned()));
    let container = GenericImage::new(image.name, image.tag)
        .with_exposed_port(5432.tcp())
        .with_entrypoint("bash")
        .with_cmd(cmd)
        .with_env_var("POSTGRES_PASSWORD", PASSWORD)
        .with_env_var("GNOMON_ADMIN", image.admin)
        .with_env_var("GNOMON_IDLE_SECONDS", IDLE_SECONDS.to_string())
        .with_container_name(container_name)
        .with_label("gnomon.test-db", prefix)
        .with_startup_timeout(Duration::from_secs(180))
        // Shared by every test process; the watcher removes it, not Drop.
        .with_reuse(ReuseDirective::Always)
        .with_host_config_modifier(|host: &mut HostConfig| {
            host.auto_remove = Some(true);
            // Loopback only, with a port Docker picks.
            host.publish_all_ports = Some(false);
            host.port_bindings = Some(HashMap::from([(
                "5432/tcp".to_owned(),
                Some(vec![PortBinding {
                    host_ip: Some("127.0.0.1".to_owned()),
                    host_port: Some(String::new()),
                }]),
            )]));
            host.tmpfs = Some(HashMap::from([(
                "/var/lib/postgresql/data".to_owned(),
                "rw".to_owned(),
            )]));
        })
        .start()
        .await
        .expect("start the test database container");
    container
        .get_host_port_ipv4(5432.tcp())
        .await
        .expect("the container publishes 5432")
}
