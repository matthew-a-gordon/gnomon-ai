// PROTOTYPE, throwaway. Not gnomon. See ../../FINDINGS.md.
//
// One connection per client. Each transaction does what gnomon's as_user() will:
//   BEGIN; set_config('request.jwt.claims', <claims>, true); SET LOCAL ROLE <r>;
//   parameterized queries; COMMIT.
// BEGIN, COMMIT and SET LOCAL ROLE use the simple protocol (they cannot take
// parameters). Everything else goes through the driver configuration under test.
//
// Env: PROTO_URL (never printed), PROTO_DRIVER = tp-named | tp-typed | sqlx-cache | sqlx-nocache,
//      PROTO_CLIENTS (default 60), PROTO_TXNS (per client, default 50), PROTO_LABEL.

use std::{collections::BTreeMap, str::FromStr, sync::Arc, time::Instant};
use tokio::sync::{Barrier, Mutex};

const Q_SET: &str = "select set_config('request.jwt.claims', $1, true)";
const Q_CTX: &str = "select current_setting('request.jwt.claims', true) as claims, current_user::text as role, $1::int4 as n";
const Q_COUNT: &str = "select count(*)::int8 from public.proto_items where id <= $1::int8";
const Q_PROBE: &str = "select coalesce(current_setting('request.jwt.claims', true), ''), current_user::text";

#[derive(Clone, Copy, PartialEq)]
enum Driver {
    TpNamed,
    TpTyped,
    SqlxCache,
    SqlxNoCache,
    SqlxUnnamed,
}

enum Conn {
    Tp {
        c: tokio_postgres::Client,
        stmts: Option<[tokio_postgres::Statement; 4]>,
    },
    Sx(sqlx::PgConnection, bool),
}

#[derive(Default)]
struct Stats {
    lat_ms: Vec<f64>,
    ok: u64,
    errors: BTreeMap<String, u64>,
    leaks: u64,
    checks: u64,
}

fn te(e: tokio_postgres::Error) -> String {
    match e.as_db_error() {
        Some(d) => format!("db error: {}", d.message()),
        None => e.to_string(),
    }
}

fn tp_connector() -> postgres_native_tls::MakeTlsConnector {
    // Certificates are verified by default. Set PROTO_INSECURE_TLS=1 to skip verification
    // (throwaway tests against hosts with private CAs only).
    let insecure = std::env::var("PROTO_INSECURE_TLS").is_ok_and(|v| v == "1");
    let tls = native_tls::TlsConnector::builder()
        .danger_accept_invalid_certs(insecure)
        .build()
        .unwrap();
    postgres_native_tls::MakeTlsConnector::new(tls)
}

async fn connect(url: &str, d: Driver) -> Result<Conn, String> {
    match d {
        Driver::TpNamed | Driver::TpTyped => {
            let cfg = tokio_postgres::Config::from_str(url).map_err(|e| e.to_string())?;
            let (c, conn) = cfg.connect(tp_connector()).await.map_err(te)?;
            tokio::spawn(async move {
                let _ = conn.await;
            });
            let stmts = if d == Driver::TpNamed {
                // Named statements, prepared once per connection and reused, as a driver-level cache would.
                Some([
                    c.prepare(Q_SET).await.map_err(te)?,
                    c.prepare(Q_CTX).await.map_err(te)?,
                    c.prepare(Q_COUNT).await.map_err(te)?,
                    c.prepare(Q_PROBE).await.map_err(te)?,
                ])
            } else {
                None
            };
            Ok(Conn::Tp { c, stmts })
        }
        Driver::SqlxCache | Driver::SqlxNoCache | Driver::SqlxUnnamed => {
            use sqlx::{ConnectOptions, postgres::PgConnectOptions};
            let mut o = PgConnectOptions::from_str(url).map_err(|e| e.to_string())?;
            if d == Driver::SqlxNoCache {
                o = o.statement_cache_capacity(0);
            }
            let c = o.connect().await.map_err(|e| e.to_string())?;
            Ok(Conn::Sx(c, d != Driver::SqlxUnnamed))
        }
    }
}

type R<T> = Result<T, String>;

impl Conn {
    async fn simple(&mut self, sql: &str) -> R<()> {
        match self {
            Conn::Tp { c, .. } => c.batch_execute(sql).await.map_err(te),
            Conn::Sx(c, pe) => sqlx::raw_sql(sql).execute(&mut *c).await.map(|_| ()).map_err(|e| e.to_string()),
        }
    }

    async fn set_claims(&mut self, claims: &str) -> R<()> {
        use tokio_postgres::types::{ToSql, Type};
        match self {
            Conn::Tp { c, stmts: Some(s) } => c.query(&s[0], &[&claims]).await.map(|_| ()).map_err(te),
            Conn::Tp { c, stmts: None } => {
                let p: &(dyn ToSql + Sync) = &claims;
                c.query_typed(Q_SET, &[(p, Type::TEXT)]).await.map(|_| ()).map_err(te)
            }
            Conn::Sx(c, pe) => sqlx::query(Q_SET).persistent(*pe).bind(claims).execute(&mut *c).await.map(|_| ()).map_err(|e| e.to_string()),
        }
    }

    async fn ctx(&mut self, n: i32) -> R<(String, String, i32)> {
        use sqlx::Row;
        use tokio_postgres::types::{ToSql, Type};
        match self {
            Conn::Tp { c, stmts: Some(s) } => {
                let r = c.query_one(&s[1], &[&n]).await.map_err(te)?;
                Ok((r.get(0), r.get(1), r.get(2)))
            }
            Conn::Tp { c, stmts: None } => {
                let p: &(dyn ToSql + Sync) = &n;
                let rows = c.query_typed(Q_CTX, &[(p, Type::INT4)]).await.map_err(te)?;
                let r = rows.first().ok_or("no row")?;
                Ok((r.get(0), r.get(1), r.get(2)))
            }
            Conn::Sx(c, pe) => {
                let r = sqlx::query(Q_CTX).persistent(*pe).bind(n).fetch_one(&mut *c).await.map_err(|e| e.to_string())?;
                Ok((r.get(0), r.get(1), r.get(2)))
            }
        }
    }

    async fn count(&mut self, n: i64) -> R<i64> {
        use sqlx::Row;
        use tokio_postgres::types::{ToSql, Type};
        match self {
            Conn::Tp { c, stmts: Some(s) } => Ok(c.query_one(&s[2], &[&n]).await.map_err(te)?.get(0)),
            Conn::Tp { c, stmts: None } => {
                let p: &(dyn ToSql + Sync) = &n;
                let rows = c.query_typed(Q_COUNT, &[(p, Type::INT8)]).await.map_err(te)?;
                Ok(rows.first().ok_or("no row")?.get(0))
            }
            Conn::Sx(c, pe) => Ok(sqlx::query(Q_COUNT).persistent(*pe).bind(n).fetch_one(&mut *c).await.map_err(|e| e.to_string())?.get(0)),
        }
    }

    async fn probe(&mut self) -> R<(String, String)> {
        use sqlx::Row;
        use tokio_postgres::types::Type;
        match self {
            Conn::Tp { c, stmts: Some(s) } => {
                let r = c.query_one(&s[3], &[]).await.map_err(te)?;
                Ok((r.get(0), r.get(1)))
            }
            Conn::Tp { c, stmts: None } => {
                let rows = c.query_typed(Q_PROBE, &[]).await.map_err(te)?;
                let _ = Type::TEXT;
                let r = rows.first().ok_or("no row")?;
                Ok((r.get(0), r.get(1)))
            }
            Conn::Sx(c, pe) => {
                let r = sqlx::query(Q_PROBE).persistent(*pe).fetch_one(&mut *c).await.map_err(|e| e.to_string())?;
                Ok((r.get(0), r.get(1)))
            }
        }
    }
}

/// One transaction. Err = a database/driver error. Ok(true) = leak seen.
async fn txn(conn: &mut Conn, id: usize, seq: usize, st: &mut Stats) -> R<()> {
    let role = if id % 2 == 0 { "authenticated" } else { "service_a" };
    let claims = format!(r#"{{"sub":"client-{id}","seq":{seq}}}"#);
    conn.simple("BEGIN").await?;
    conn.set_claims(&claims).await?;
    conn.simple(&format!("SET LOCAL ROLE {role}")).await?;
    let n = (seq % 90 + 1) as i32;
    let (got_claims, got_role, got_n) = conn.ctx(n).await?;
    let cnt = conn.count(n as i64).await?;
    conn.simple("COMMIT").await?;
    st.checks += 1;
    if got_claims != claims || got_role != role || got_n != n || cnt != n as i64 {
        st.leaks += 1;
        eprintln!("LEAK client {id} seq {seq}: claims={got_claims} role={got_role} (want {claims} / {role})");
    }
    // After commit the transaction-local claims and role must be gone.
    // Default: the probe runs in its own transaction. PROTO_PROBE_AUTOCOMMIT=1 runs it bare.
    let auto = std::env::var("PROTO_PROBE_AUTOCOMMIT").is_ok();
    if !auto {
        conn.simple("BEGIN").await?;
    }
    let probed = conn.probe().await;
    if !auto {
        conn.simple("COMMIT").await?;
    }
    let (pc, pr) = probed?;
    st.checks += 1;
    let base = std::env::var("PROTO_BASE_ROLE").unwrap_or_else(|_| "app".into());
    if !pc.is_empty() || pr != base {
        st.leaks += 1;
        eprintln!("LEAK-AFTER-COMMIT client {id} seq {seq}: claims={pc:?} role={pr}");
    }
    Ok(())
}

fn key(e: &str) -> String {
    // Group errors by message, trimming statement names that differ per run.
    let e = e.split(" at character").next().unwrap_or(e);
    let masked: String = e.chars().map(|c| if c.is_ascii_digit() { '#' } else { c }).collect();
    masked.chars().take(110).collect()
}

#[tokio::main]
async fn main() {
    tokio::task::LocalSet::new().run_until(run()).await;
}

async fn run() {
    let url = std::env::var("PROTO_URL").expect("PROTO_URL");
    let driver = match std::env::var("PROTO_DRIVER").expect("PROTO_DRIVER").as_str() {
        "tp-named" => Driver::TpNamed,
        "tp-typed" => Driver::TpTyped,
        "sqlx-cache" => Driver::SqlxCache,
        "sqlx-nocache" => Driver::SqlxNoCache,
        "sqlx-unnamed" => Driver::SqlxUnnamed,
        o => panic!("unknown driver {o}"),
    };
    let clients: usize = std::env::var("PROTO_CLIENTS").ok().and_then(|v| v.parse().ok()).unwrap_or(60);
    let txns: usize = std::env::var("PROTO_TXNS").ok().and_then(|v| v.parse().ok()).unwrap_or(50);
    let label = std::env::var("PROTO_LABEL").unwrap_or_default();
    let dname = std::env::var("PROTO_DRIVER").unwrap();

    let stats = Arc::new(Mutex::new(Stats::default()));
    let barrier = Arc::new(Barrier::new(clients + 1));
    let mut hs = vec![];
    for id in 0..clients {
        let (url, stats, barrier) = (url.clone(), stats.clone(), barrier.clone());
        hs.push(tokio::task::spawn_local(async move {
            let conn = connect(&url, driver).await;
            barrier.wait().await;
            let mut conn = match conn {
                Ok(c) => c,
                Err(e) => {
                    let mut s = stats.lock().await;
                    *s.errors.entry(format!("connect/prepare: {}", key(&e))).or_default() += 1;
                    return;
                }
            };
            let mut local = Stats::default();
            for seq in 0..txns {
                let t = Instant::now();
                match txn(&mut conn, id, seq, &mut local).await {
                    Ok(()) => {
                        local.ok += 1;
                        local.lat_ms.push(t.elapsed().as_secs_f64() * 1e3);
                    }
                    Err(e) => {
                        *local.errors.entry(key(&e)).or_default() += 1;
                        let _ = conn.simple("ROLLBACK").await;
                        if let Conn::Tp { c, .. } = &conn {
                            if c.is_closed() {
                                break;
                            }
                        }
                    }
                }
            }
            let mut s = stats.lock().await;
            s.ok += local.ok;
            s.leaks += local.leaks;
            s.checks += local.checks;
            s.lat_ms.extend(local.lat_ms);
            for (k, v) in local.errors {
                *s.errors.entry(k).or_default() += v;
            }
        }));
    }
    barrier.wait().await;
    let t0 = Instant::now();
    for h in hs {
        let _ = h.await;
    }
    let wall = t0.elapsed().as_secs_f64();
    let mut s = stats.lock().await;
    s.lat_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p = |q: f64| s.lat_ms.get(((s.lat_ms.len() as f64 * q) as usize).min(s.lat_ms.len().saturating_sub(1))).copied().unwrap_or(f64::NAN);
    let total_err: u64 = s.errors.values().sum();
    println!(
        "{label} | {dname} | clients={clients} txns/client={txns} | ok={} err={} leaks={} (of {} checks) | {:.0} txn/s | p50={:.1}ms p95={:.1}ms p99={:.1}ms",
        s.ok, total_err, s.leaks, s.checks, s.ok as f64 / wall, p(0.5), p(0.95), p(0.99)
    );
    let mut errs: Vec<_> = s.errors.iter().collect();
    errs.sort_by(|a, b| b.1.cmp(a.1));
    for (k, v) in errs.iter().take(8) {
        println!("    {v} x {k}");
    }
    if errs.len() > 8 {
        println!("    ... {} more error kinds", errs.len() - 8);
    }
}
