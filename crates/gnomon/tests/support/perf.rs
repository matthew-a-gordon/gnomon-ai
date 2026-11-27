//! The performance suite's harness (ticket #21, ADR-0010, ADR-0020): the run
//! record, percentiles, and the data set of prototype 5 built with set-based
//! SQL.
//!
//! A data set is built once per server: in a database named for its size and
//! for the migration files, which stays on the server, so a rerun against
//! the same container skips the build. The cases only read it.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};
use tokio_postgres::types::Type;

use super::timestamptz::Timestamptz;
use super::vectors::Rng;
use super::{DEFAULT_DIMENSION, Server, TestDb};

/// The Scope the data set lives in.
pub const SCOPE: &str = "perf";

/// Percentiles of a sample, in milliseconds.
#[derive(Debug, Clone)]
pub struct Stats {
    pub n: usize,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub max: f64,
}

impl Stats {
    pub fn of(times: &[Duration]) -> Stats {
        let mut ms: Vec<f64> = times.iter().map(|t| t.as_secs_f64() * 1000.0).collect();
        ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let at = |q: f64| ms[((ms.len() as f64 * q).ceil() as usize).clamp(1, ms.len()) - 1];
        Stats {
            n: ms.len(),
            p50: at(0.5),
            p95: at(0.95),
            p99: at(0.99),
            max: *ms.last().unwrap(),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({ "n": self.n, "p50_ms": self.p50, "p95_ms": self.p95,
                "p99_ms": self.p99, "max_ms": self.max })
    }
}

/// The machine's load at one instant.
#[derive(Debug, Clone)]
pub struct Load {
    pub loadavg: String,
    pub load1: f64,
    pub io_pressure: Option<String>,
    /// The `some avg10` share of time tasks waited on I/O.
    pub io_some_avg10: Option<f64>,
}

impl Load {
    pub fn now() -> Load {
        let loadavg = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
        let load1 = loadavg
            .split_whitespace()
            .next()
            .and_then(|l| l.parse().ok())
            .unwrap_or(0.0);
        let io_pressure = std::fs::read_to_string("/proc/pressure/io").ok();
        let io_some_avg10 = io_pressure.as_deref().and_then(|p| {
            p.lines()
                .find(|l| l.starts_with("some"))?
                .split_whitespace()
                .find_map(|f| f.strip_prefix("avg10=")?.parse().ok())
        });
        Load {
            loadavg: loadavg.trim().to_owned(),
            load1,
            io_pressure: io_pressure.map(|p| p.trim().to_owned()),
            io_some_avg10,
        }
    }

    pub fn to_json(&self) -> Value {
        json!({ "loadavg": self.loadavg, "io_pressure": self.io_pressure })
    }
}

pub fn cpus() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

/// Whether the machine was busy: a one-minute load average of half the CPUs
/// or more, before or after, or tasks waiting on I/O a tenth of the time.
pub fn loaded(before: &Load, after: &Load) -> bool {
    let half = cpus() as f64 / 2.0;
    before.load1 >= half
        || after.load1 >= half
        || before.io_some_avg10.unwrap_or(0.0) >= 10.0
        || after.io_some_avg10.unwrap_or(0.0) >= 10.0
}

/// One case's record: its environment, the machine load around it and its
/// numbers. [`Record::finish`] writes it under the test output directory and
/// prints a summary.
pub struct Record {
    case: String,
    environment: Value,
    before: Load,
    started: std::time::Instant,
}

impl Record {
    pub async fn start(case: &str, db: &str) -> Record {
        let before = Load::now();
        let server = Server::get().await;
        let admin = server.admin(db).await;
        let setting = |name: &'static str| {
            let admin = &admin;
            async move {
                let row = admin
                    .query_typed(&format!("show {name}"), &[])
                    .await
                    .unwrap();
                row[0].get::<_, String>(0)
            }
        };
        let version = setting("server_version").await;
        let pgvector: String = admin
            .query_typed(
                "select extversion from pg_extension where extname = 'vector'",
                &[],
            )
            .await
            .unwrap()[0]
            .get(0);
        let mut settings = serde_json::Map::new();
        for name in [
            "shared_buffers",
            "effective_cache_size",
            "work_mem",
            "maintenance_work_mem",
            "max_parallel_workers_per_gather",
            "fsync",
            "synchronous_commit",
            "autovacuum",
            "jit",
        ] {
            settings.insert(name.into(), setting(name).await.into());
        }
        let mem_total_kib: u64 = std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|m| {
                m.lines().find_map(|l| {
                    l.strip_prefix("MemTotal:")?
                        .split_whitespace()
                        .next()?
                        .parse()
                        .ok()
                })
            })
            .unwrap_or(0);
        let container = std::env::var("GNOMON_TEST_CONTAINER_PREFIX")
            .unwrap_or_else(|_| "gnomon-test".into())
            + "-"
            + &std::env::var("GNOMON_TEST_IMAGE").unwrap_or_else(|_| "pgvector".into());
        let inspected = std::process::Command::new("docker")
            .args([
                "inspect",
                &container,
                "--format",
                "memory={{.HostConfig.Memory}} swap={{.HostConfig.MemorySwap}} mounts={{range .Mounts}}{{.Type}}:{{.Destination}} {{end}}tmpfs={{.HostConfig.Tmpfs}}",
            ])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned());
        let environment = json!({
            "date": jiff::Timestamp::now().to_string(),
            "image": server.image,
            "postgres": version,
            "pgvector": pgvector,
            "cpus": cpus(),
            "host_memory_gib": mem_total_kib as f64 / 1024.0 / 1024.0,
            "container_memory_cap": server.memory_cap.clone().unwrap_or_else(|| "none".into()),
            "container": container,
            "container_inspect": inspected,
            "postgres_settings": settings,
            "embedding_dimension": DEFAULT_DIMENSION,
        });
        Record {
            case: case.to_owned(),
            environment,
            before,
            started: std::time::Instant::now(),
        }
    }

    /// Writes the record to `<target tmp>/performance/` and prints a
    /// summary. Returns whether the machine was loaded.
    pub fn finish(self, results: Value) -> bool {
        let after = Load::now();
        let was_loaded = loaded(&self.before, &after);
        let record = json!({
            "case": self.case,
            "environment": self.environment,
            "machine_load": {
                "before": self.before.to_json(),
                "after": after.to_json(),
                "loaded": was_loaded,
            },
            "wall_seconds": self.started.elapsed().as_secs_f64(),
            "results": results,
        });
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("performance");
        std::fs::create_dir_all(&dir).unwrap();
        let stamp = jiff::Timestamp::now().strftime("%Y%m%dT%H%M%SZ");
        let cap = self.environment["container_memory_cap"].as_str().unwrap();
        let image = self.environment["image"]
            .as_str()
            .unwrap()
            .replace(['/', ':'], "_");
        let path = dir.join(format!("{stamp}-{}-{image}-cap-{cap}.json", self.case));
        std::fs::write(&path, serde_json::to_string_pretty(&record).unwrap()).unwrap();
        println!("--- performance record: {}", self.case);
        println!(
            "environment: {}",
            serde_json::to_string(&self.environment).unwrap()
        );
        println!(
            "machine load before: {} | io pressure: {}",
            self.before.loadavg,
            self.before
                .io_pressure
                .as_deref()
                .unwrap_or("n/a")
                .replace('\n', " / ")
        );
        println!(
            "machine load after:  {} | io pressure: {}",
            after.loadavg,
            after
                .io_pressure
                .as_deref()
                .unwrap_or("n/a")
                .replace('\n', " / ")
        );
        if was_loaded {
            println!(
                "THIS RUN WAS LOADED: the one-minute load average or the I/O pressure was high \
                 on a machine with {} CPUs, so its numbers may be worse than an idle machine's. \
                 Rerun when the load is lower before judging a failed budget.",
                cpus()
            );
        }
        println!("results: {}", serde_json::to_string(&results).unwrap());
        println!("record written to {}", path.display());
        was_loaded
    }
}

/// A loaded data set, ready to search.
pub struct Dataset {
    pub db: String,
    pub texts: usize,
    /// Node Versions held at `valid_at` at the current belief: the rows a
    /// Search over the Scope can reach.
    pub visible: i64,
    pub versions: i64,
}

/// Visible rows are counted at this instant.
pub fn valid_at() -> jiff::Timestamp {
    super::erasure::at(2025, 1, 1)
}

/// Ratio of visible Versions to texts that the generator below produces
/// (about 54%, as prototype 5's: 92,000 texts leave about 50,000 visible).
pub fn texts_for_visible(visible: usize) -> usize {
    (visible as f64 / 0.54).round() as usize
}

/// The one Tenant every data set is built for, so a data set found in the
/// server by a later run belongs to the Tenant that run names.
fn data_tenant() -> gnomon::TenantId {
    gnomon::TenantId(uuid::Uuid::from_u128(
        0x2100_0000_0000_4000_8000_0000_0000_0021,
    ))
}

fn dataset_name(texts: usize) -> String {
    let mut h = DefaultHasher::new();
    super::migration_files(DEFAULT_DIMENSION).hash(&mut h);
    BUILDER_SQL.hash(&mut h);
    format!("gnomon_perf_{texts}_{:016x}", h.finish())
}

impl Dataset {
    /// The data set of `texts` texts, built into a database that stays on the
    /// server, or found there when this run or an earlier one on the same
    /// container built it.
    pub async fn get(texts: usize) -> Dataset {
        let server = Server::get().await;
        let name = dataset_name(texts);
        let admin = server.admin("postgres").await;
        let exists = !admin
            .query_typed(
                "select 1 from pg_database where datname = $1",
                &[(&name, Type::TEXT)],
            )
            .await
            .unwrap()
            .is_empty();
        if !exists {
            build(texts, &name).await;
        }
        let db = server.admin(&name).await;
        let row = db
            .query_typed(
                "select count(*) filter (where valid_from <= $1 and $1 < valid_to
                                         and recorded_to = 'infinity'),
                        count(*)
                 from gnomon.version where node_id is not null",
                &[(&Timestamptz(valid_at()), Type::TIMESTAMPTZ)],
            )
            .await
            .unwrap();
        Dataset {
            db: name,
            texts,
            visible: row[0].get(0),
            versions: row[0].get(1),
        }
    }

    /// The Store's worker connection to the data set.
    pub async fn store(&self) -> gnomon::Store {
        gnomon::Store::connect(
            &super::login_url_to(&self.db).await,
            &super::connect_options(),
        )
        .await
        .unwrap()
    }

    pub fn tenant(&self) -> gnomon::TenantId {
        data_tenant()
    }

    /// The subtopic centres of the data set, to draw query vectors near.
    pub async fn centres(&self) -> Vec<Vec<f32>> {
        let admin = Server::get().await.admin(&self.db).await;
        admin
            .query_typed("select centre::real[] from perf.centre order by id", &[])
            .await
            .unwrap()
            .iter()
            .map(|r| r.get(0))
            .collect()
    }

    pub fn query_for(&self, rng: &mut Rng, centres: &[Vec<f32>]) -> (String, Vec<f32>) {
        let c = rng.below(centres.len());
        let q = rng.near(&centres[c], 0.6);
        // The word is a number that one text holds. The generated texts have
        // no word-frequency structure, so this suite cannot query the
        // mid-frequency words prototype 5 did, and its full-text branch is
        // cheaper than a real vocabulary's.
        (rng.below(self.texts).to_string(), q)
    }
}

/// Everything the build runs, in order, as one string so a change to it names
/// a new data set.
const BUILDER_SQL: &str = include_str!("perf_data.sql");

async fn build(texts: usize, name: &str) {
    let started = std::time::Instant::now();
    let db = TestDb::migrated().await;
    db.grant_kind_registration("gnomon_worker").await;
    let tenant = data_tenant();
    {
        let store = gnomon::Store::connect(&db.login_url(), &super::connect_options())
            .await
            .unwrap();
        store
            .as_worker(tenant, gnomon::Scope::new(SCOPE), None)
            .register_kind(&gnomon::NewKind {
                tenant: Some(tenant),
                name: "company".into(),
                of: gnomon::KindOf::Node,
                parent: None,
                properties: json!({ "type": "object", "properties": {} }),
                search: vec![],
            })
            .await
            .unwrap();
        // The Edges' Episode.
        store
            .as_worker(tenant, gnomon::Scope::new(SCOPE), None)
            .record_episode(&gnomon::NewEpisode {
                tenant: Some(tenant),
                scope: gnomon::Scope::new(SCOPE),
                content: "the performance data set's Episode".into(),
                reference_time: super::erasure::at(2024, 1, 1),
                authority: None,
            })
            .await
            .unwrap();
    }
    let admin = db.admin().await;
    let ext: String = admin
        .query_typed(
            "select n.nspname from pg_extension e join pg_namespace n on n.oid = e.extnamespace
             where e.extname = 'vector'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    let sections: Vec<&str> = BUILDER_SQL.split("-- @@\n").collect();
    let mut step = 0;
    for section in &sections {
        let sql = section
            .replace("{{ext}}", &super::quote_ident(&ext))
            .replace("{{texts}}", &texts.to_string())
            .replace("{{tenant}}", &tenant.0.to_string())
            .replace("{{scope}}", SCOPE)
            .replace("{{valid_at}}", &valid_at().to_string())
            .replace("{{dim}}", &DEFAULT_DIMENSION.to_string());
        step += 1;
        let t = std::time::Instant::now();
        if sql.contains("@@embeddings@@") {
            embeddings(&db, &sql, tenant).await;
        } else if sql.contains("@@index-search-text@@") {
            index_search_text(&db, tenant).await;
        } else {
            admin
                .batch_execute(&sql)
                .await
                .unwrap_or_else(|e| panic!("data set step {step}: {e:?}\n{sql}"));
        }
        eprintln!(
            "data set of {texts} texts: step {step}/{} took {:.1}s",
            sections.len(),
            t.elapsed().as_secs_f64()
        );
    }
    drop(admin);
    db.keep_as(name).await;
    eprintln!(
        "data set of {texts} texts built in {:.0}s",
        started.elapsed().as_secs_f64()
    );
}

/// The token rows, which the inserts with triggers off did not write,
/// through the function the search index trigger calls, in chunks of
/// versions.
async fn index_search_text(db: &TestDb, tenant: gnomon::TenantId) {
    let claims = json!({ "role": "gnomon_worker",
        "app_metadata": { "tenant_id": tenant.0.to_string(), "scopes": [SCOPE] } })
    .to_string();
    let mut admin = db.admin().await;
    let ids: Vec<uuid::Uuid> = admin
        .query_typed("select id from gnomon.version order by id", &[])
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    for chunk in ids.chunks(50_000) {
        let tx = admin.transaction().await.unwrap();
        tx.query_typed(
            "select set_config('request.jwt.claims', $1, true)",
            &[(&claims, Type::TEXT)],
        )
        .await
        .unwrap();
        tx.execute_typed(
            "select gnomon.index_search_text($1, $2, false)",
            &[(&tenant.0, Type::UUID), (&chunk.to_vec(), Type::UUID_ARRAY)],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }
}

/// Stores every Version's embedding on one connection, in batches of
/// Versions. Each batch is two statements in one transaction: the full
/// vectors, then the quantized rows. The vector of a text is computed from the
/// pools in `perf` inside the statement.
async fn embeddings(db: &TestDb, sql: &str, tenant: gnomon::TenantId) {
    let mut admin = db.admin().await;
    // The section holds the statements to run per batch, after a marker line.
    let (_, per_batch) = sql.split_once("-- per batch\n").unwrap();
    let (vectors, bits) = per_batch.split_once("-- then\n").unwrap();
    let batches: i32 = admin
        .query_typed("select max(batch) + 1 from perf.vers", &[])
        .await
        .unwrap()[0]
        .get(0);
    for batch in 0..batches {
        let tx = admin.transaction().await.unwrap();
        for statement in [vectors, bits] {
            tx.execute_typed(statement, &[(&tenant.0, Type::UUID), (&batch, Type::INT4)])
                .await
                .unwrap_or_else(|e| panic!("store embeddings of batch {batch}: {e:?}"));
        }
        tx.commit().await.unwrap();
    }
}

/// The rows that make a Store large for the patch cases (#84): for each of
/// `nodes` Nodes of the Scope, one Episode, two Contributions, two Versions
/// (the first closed by the second) and a Citation of each Version. None of
/// them is a Node a case writes. They are inserted as the superuser with
/// `session_replication_role = replica`, so no trigger runs, and analyzed.
const UNRELATED_ROWS_SQL: &str = "
begin;
set local session_replication_role = replica;
create temporary table unrelated on commit drop as
  select i,
         md5('unrelated node ' || i)::uuid as node,
         md5('unrelated episode ' || i)::uuid as episode,
         timestamptz '2021-01-01 00:00:00+00' + (i % 1500) * interval '1 day' as reference_time,
         timestamptz '2024-01-01 00:00:00+00' + (i % 50000) * interval '1 minute' as recorded_at,
         timestamptz '2020-01-01 00:00:00+00' + (i % 2000) * interval '1 day' as valid_from
  from generate_series(1, {{nodes}}) i;
insert into gnomon.node (tenant_id, scope, id, kind)
  select '{{tenant}}', '{{scope}}', node, 'node' from unrelated;
insert into gnomon.episode (tenant_id, scope, id, content, reference_time, recorded_at)
  select '{{tenant}}', '{{scope}}', episode, 'an unrelated Episode ' || i, reference_time,
         recorded_at
  from unrelated;
insert into gnomon.contribution (tenant_id, scope, node_id, episode_id, property, value,
                                 valid_from, valid_from_precision, valid_to, valid_to_precision,
                                 recorded_at, episode_reference_time, episode_recorded_at)
  select '{{tenant}}', '{{scope}}', u.node, u.episode, 'name',
         to_jsonb('unrelated ' || u.i || ' ' || k), u.valid_from, 'instant', 'infinity', 'unknown',
         u.recorded_at + k * interval '1 second', u.reference_time, u.recorded_at
  from unrelated u, generate_series(1, 2) k;
insert into gnomon.version (tenant_id, scope, id, node_id, props, valid_from,
                            valid_from_precision, valid_to, valid_to_precision,
                            recorded_from, recorded_to, merge_rule, closed_in)
  select '{{tenant}}', '{{scope}}', md5('unrelated version ' || u.i || ' ' || k)::uuid, u.node,
         jsonb_build_object('name', 'unrelated ' || u.i || ' ' || k),
         u.valid_from, 'instant', 'infinity', 'unknown',
         u.recorded_at + k * interval '1 second',
         case k when 1 then u.recorded_at + 2 * interval '1 second' else 'infinity' end,
         'recency', case k when 1 then '0'::xid8 end
  from unrelated u, generate_series(1, 2) k;
insert into gnomon.citation (tenant_id, scope, version_id, episode_id, property, recorded_at,
                             episode_reference_time, episode_recorded_at)
  select '{{tenant}}', '{{scope}}', md5('unrelated version ' || u.i || ' ' || k)::uuid,
         u.episode, 'name', u.recorded_at + k * interval '1 second', u.reference_time,
         u.recorded_at
  from unrelated u, generate_series(1, 2) k;
commit;
analyze;
";

/// The tables [`add_unrelated_rows`] fills.
pub const UNRELATED_TABLES: [&str; 5] = ["node", "episode", "contribution", "version", "citation"];

/// Makes the Store of `db` large: [`UNRELATED_ROWS_SQL`] for `nodes` Nodes
/// in `scope` of `tenant`. Returns the rows each of [`UNRELATED_TABLES`]
/// then holds.
pub async fn add_unrelated_rows(
    db: &TestDb,
    tenant: gnomon::TenantId,
    scope: &str,
    nodes: usize,
) -> Vec<(&'static str, i64)> {
    let admin = db.admin().await;
    admin
        .batch_execute(
            &UNRELATED_ROWS_SQL
                .replace("{{tenant}}", &tenant.0.to_string())
                .replace("{{scope}}", scope)
                .replace("{{nodes}}", &nodes.to_string()),
        )
        .await
        .unwrap_or_else(|e| panic!("unrelated rows: {e:?}"));
    let mut counts = vec![];
    for table in UNRELATED_TABLES {
        let n: i64 = admin
            .query_typed(&format!("select count(*) from gnomon.{table}"), &[])
            .await
            .unwrap()[0]
            .get(0);
        counts.push((table, n));
    }
    counts
}
