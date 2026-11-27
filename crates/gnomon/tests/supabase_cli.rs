//! A consumer's Supabase CLI chain applies gnomon's migration files
//! (ADR-0006).
//!
//! The consumer copies the files into `supabase/migrations` under
//! timestamped names, `<version>_gnomon_<name>.sql`, after its own
//! migrations, and the CLI applies them. The first two tests run the pinned
//! CLI's own runner, `supabase migration up`, against a database of the
//! suite's server, with pgvector in the default schema and in `extensions`.
//! They need `npx`, or an installed CLI that `GNOMON_TEST_SUPABASE_CLI`
//! names.
//!
//! The rest is lane 3 (ADR-0020): the Supabase CLI local stack, after
//! `supabase start` applied the files copied the same way into the stack's
//! `postgres` database. Those tests need the stack, so they are ignored by
//! default. `crates/gnomon/tests/support/supabase/run-lane-3.sh` starts it
//! and runs them with `--run-ignored only`. They never touch a hosted
//! project.

mod support;

use serde_json::Value;
use support::install::{Extensions, assert_installed, prepare};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_postgres::{Client, NoTls};

/// The file name prefix of the `n`th file, counting from 0: a Supabase CLI
/// version one second after the last, after a consumer's own migration of
/// 2024, then `gnomon_`, as the README names them.
fn supabase_file_prefix(n: usize) -> String {
    format!("20250101{:06}_gnomon_", n + 1)
}

async fn the_cli_applies_the_files(placement: Extensions) {
    let db = support::TestDb::empty_with_login().await;
    let schema = prepare(&db, placement).await;
    let workdir = tempfile::tempdir().unwrap();
    let migrations = workdir.path().join("supabase/migrations");
    std::fs::create_dir_all(&migrations).unwrap();
    std::fs::write(
        migrations.join("20240101000000_consumer_table.sql"),
        "create table public.consumer_table (id bigint primary key);\n",
    )
    .unwrap();
    support::copy_migration_files(&migrations, supabase_file_prefix);
    let applied = support::supabase_migration_up(workdir.path(), &db.migration_url("postgresql"))
        .unwrap_or_else(|e| panic!("supabase migration up failed: {e}"));
    assert_eq!(
        applied.matches("Applying migration").count(),
        1 + gnomon::migrations::MIGRATIONS.len(),
        "{applied}"
    );
    assert_installed(&db, &schema).await;
}

#[tokio::test]
async fn the_cli_applies_the_files_with_pgvector_in_the_default_schema() {
    the_cli_applies_the_files(Extensions::Default).await;
}

#[tokio::test]
async fn the_cli_applies_the_files_with_pgvector_in_extensions() {
    the_cli_applies_the_files(Extensions::InExtensions).await;
}

/// The stack's `postgres` database, where the CLI applied the migrations,
/// as the superuser `GNOMON_TEST_DATABASE_URL` names.
async fn stack_database() -> Client {
    let url = std::env::var("GNOMON_TEST_DATABASE_URL").expect("GNOMON_TEST_DATABASE_URL is set");
    let mut config: tokio_postgres::Config = url.parse().unwrap();
    config.dbname("postgres");
    let (client, conn) = config.connect(NoTls).await.expect("connect to the stack");
    tokio::spawn(conn);
    client
}

/// The stack's API gateway, such as `http://127.0.0.1:56031`.
fn api() -> (String, u16) {
    let url =
        std::env::var("GNOMON_TEST_SUPABASE_API_URL").expect("GNOMON_TEST_SUPABASE_API_URL is set");
    let authority = url.strip_prefix("http://").expect("an http:// API URL");
    let (host, port) = authority.trim_end_matches('/').rsplit_once(':').unwrap();
    (host.to_owned(), port.parse().unwrap())
}

/// One HTTP/1.1 request to the stack's API, as a REST client sends it with
/// no key. Returns the status and the JSON body.
async fn rest(method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> (u16, Value) {
    let (host, port) = api();
    let mut stream = tokio::net::TcpStream::connect((host.as_str(), port))
        .await
        .expect("connect to the stack's API");
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\
         Accept: application/json\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let raw = String::from_utf8(raw).expect("a UTF-8 response");
    let (head, payload) = raw.split_once("\r\n\r\n").expect("an HTTP response");
    let status: u16 = head.split(' ').nth(1).unwrap().parse().unwrap();
    let chunked = head.lines().any(|l| {
        l.to_ascii_lowercase()
            .starts_with("transfer-encoding: chunked")
    });
    let payload = if chunked {
        dechunk(payload)
    } else {
        payload.to_owned()
    };
    let json = serde_json::from_str(&payload).unwrap_or(Value::String(payload));
    (status, json)
}

fn dechunk(mut s: &str) -> String {
    let mut out = String::new();
    while let Some((size, rest)) = s.split_once("\r\n") {
        let n = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
        if n == 0 {
            break;
        }
        out.push_str(&rest[..n]);
        s = rest[n..].trim_start_matches("\r\n");
    }
    out
}

/// The gnomon schema's tables, views and functions, read from the catalog.
async fn gnomon_objects(db: &Client) -> (Vec<String>, Vec<String>) {
    let names = |sql: &'static str| async move {
        db.query_typed(sql, &[])
            .await
            .unwrap()
            .iter()
            .map(|r| r.get::<_, String>(0))
            .collect::<Vec<_>>()
    };
    let relations = names(
        "select c.relname::text from pg_class c
         where c.relnamespace = 'gnomon'::regnamespace and c.relkind in ('r', 'p', 'v', 'm', 'f')
         order by 1",
    )
    .await;
    let functions = names(
        "select distinct p.proname::text from pg_proc p
         where p.pronamespace = 'gnomon'::regnamespace order by 1",
    )
    .await;
    (relations, functions)
}

#[tokio::test]
#[ignore = "lane 3: needs the Supabase CLI stack; run crates/gnomon/tests/support/supabase/run-lane-3.sh"]
async fn the_cli_applied_every_gnomon_migration_to_the_stack_as_postgres() {
    let db = stack_database().await;
    let applied: Vec<String> = db
        .query_typed(
            "select name from supabase_migrations.schema_migrations order by version",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    let expected: Vec<String> = gnomon::migrations::MIGRATIONS
        .iter()
        .map(|m| format!("gnomon_{}", m.name))
        .collect();
    assert_eq!(applied, expected, "the CLI's migration history");

    let owner: String = db
        .query_typed(
            "select nspowner::regrole::text from pg_namespace where nspname = 'gnomon'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(owner, "postgres", "the role that applied the migrations");

    // Creates the harness's login role, which the Store connects as.
    let _creates_the_login_role = support::TestDb::empty_with_login().await;
    gnomon::Store::connect(
        &support::login_url_to("postgres").await,
        &support::connect_options(),
    )
    .await
    .expect("the Store accepts the schema the CLI applied");
}

#[tokio::test]
#[ignore = "lane 3: needs the Supabase CLI stack; run crates/gnomon/tests/support/supabase/run-lane-3.sh"]
async fn postgrest_reaches_no_gnomon_table_or_function() {
    let db = stack_database().await;
    let (relations, functions) = gnomon_objects(&db).await;
    assert!(!relations.is_empty() && !functions.is_empty());

    for table in &relations {
        let path = format!("/rest/v1/{table}?select=*");
        let (status, body) = rest("GET", &path, &[], "").await;
        assert_eq!(
            (status, &body["code"]),
            (404, &"PGRST205".into()),
            "{path}: {body}"
        );
        let (status, body) = rest("GET", &path, &[("Accept-Profile", "gnomon")], "").await;
        assert_eq!(
            (status, &body["code"]),
            (406, &"PGRST106".into()),
            "{path} in gnomon: {body}"
        );
    }
    for function in &functions {
        let path = format!("/rest/v1/rpc/{function}");
        let json = [("Content-Type", "application/json")];
        let (status, body) = rest("POST", &path, &json, "{}").await;
        assert_eq!(
            (status, &body["code"]),
            (404, &"PGRST202".into()),
            "{path}: {body}"
        );
        let profiled = [json[0], ("Content-Profile", "gnomon")];
        let (status, body) = rest("POST", &path, &profiled, "{}").await;
        assert_eq!(
            (status, &body["code"]),
            (406, &"PGRST106".into()),
            "{path} in gnomon: {body}"
        );
    }
}

#[tokio::test]
#[ignore = "lane 3: needs the Supabase CLI stack; run crates/gnomon/tests/support/supabase/run-lane-3.sh"]
async fn the_stack_exposes_no_schema_that_holds_what_the_migrations_created() {
    // PostgREST names the schemas it exposes when it refuses another.
    let (status, body) = rest(
        "GET",
        "/rest/v1/any_table",
        &[("Accept-Profile", "pg_catalog")],
        "",
    )
    .await;
    assert_eq!(status, 406, "{body}");
    let hint = body["hint"].as_str().expect("a hint naming the schemas");
    let exposed: Vec<&str> = hint
        .rsplit_once(": ")
        .expect("a list of schemas")
        .1
        .split(", ")
        .collect();

    let db = stack_database().await;
    let vector_schema: String = db
        .query_typed(
            "select extnamespace::regnamespace::text from pg_extension where extname = 'vector'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert!(!exposed.contains(&"gnomon"), "exposed: {exposed:?}");
    assert!(
        !exposed.contains(&vector_schema.as_str()),
        "pgvector, which 0001 creates, is in {vector_schema}, an exposed schema: {exposed:?}"
    );
}
