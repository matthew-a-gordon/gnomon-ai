//! The proof for the squash of ticket #80, commit 1: the migration chain in
//! `SQUASH_CHAIN` (a directory of `NNNN_name.sql` files) applied to fresh
//! databases, with what an equal chain must leave written to `SQUASH_OUT`.
//!
//! Ignored by default. `scripts/compare-squash.sh` runs it once for the old
//! chain and once for the new, each on a server of its own, and compares the
//! outputs: `pg_dump --schema-only` with ownership and grants, every owner
//! and ACL in the catalog, the roles' attributes and memberships, and the
//! rows the migrations insert other than `gnomon.schema_version`. Each runs
//! with pgvector in `public` and in `extensions`, and with plain default
//! privileges and default privileges granted to the caller roles.

mod support;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use support::{Server, quote_ident};
use tokio_postgres::{Client, NoTls};

const DIMENSION: &str = "1536";

fn env_path(name: &str) -> PathBuf {
    PathBuf::from(std::env::var(name).unwrap_or_else(|_| panic!("set {name}")))
}

fn chain(dir: &Path) -> Vec<(String, String)> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "sql"))
        .collect();
    files.sort();
    files
        .into_iter()
        .map(|p| {
            let sql = std::fs::read_to_string(&p)
                .unwrap()
                .replace("{{embedding_dimension}}", DIMENSION);
            (p.file_name().unwrap().to_string_lossy().into_owned(), sql)
        })
        .collect()
}

async fn connect(server: &Server, db: &str) -> Client {
    let (client, conn) = server
        .admin_config_for(db)
        .connect(NoTls)
        .await
        .expect("connect");
    tokio::spawn(conn);
    client
}

/// `psql` or `pg_dump` inside the harness's own container, as its superuser.
fn in_container(tool: &[&str]) -> String {
    let image = std::env::var("GNOMON_TEST_IMAGE").unwrap_or_else(|_| "pgvector".into());
    let prefix =
        std::env::var("GNOMON_TEST_CONTAINER_PREFIX").unwrap_or_else(|_| "gnomon-test".into());
    let admin = if image == "supabase" {
        "supabase_admin"
    } else {
        "postgres"
    };
    let run = Command::new("docker")
        .args(["exec", &format!("{prefix}-{image}")])
        .args(tool)
        .args(["-U", admin, "-h", "/var/run/postgresql"])
        .output()
        .expect("docker exec runs");
    assert!(
        run.status.success(),
        "{tool:?}: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    String::from_utf8(run.stdout).unwrap()
}

/// The rows of every gnomon table but the ledger, with each UUID, timestamp
/// and transaction ID replaced by its order of first appearance, so
/// generated values compare equal and references between rows stay visible.
async fn rows(client: &Client) -> String {
    let tables: Vec<String> = client
        .query(
            "select c.oid::regclass::text from pg_class c
             where c.relnamespace = 'gnomon'::regnamespace and c.relkind in ('r', 'p')
               and c.relname <> 'schema_version'
             order by c.relname",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    let mut out = String::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    let generated = regex_lite_uuid_or_time();
    for t in tables {
        let rows = client
            .query(
                &format!("select to_jsonb(t)::text from {t} t order by to_jsonb(t)::text"),
                &[],
            )
            .await
            .unwrap();
        let mut lines: Vec<String> = Vec::new();
        for r in rows {
            let text: String = r.get(0);
            lines.push(generated(&text, &mut seen));
        }
        lines.sort();
        out.push_str(&format!("== {t} ({} rows)\n", lines.len()));
        for l in lines {
            out.push_str(&l);
            out.push('\n');
        }
    }
    out
}

/// Replaces UUIDs and timestamps inside a JSON text by numbered tokens.
fn regex_lite_uuid_or_time() -> impl Fn(&str, &mut HashMap<String, usize>) -> String {
    |text: &str, seen: &mut HashMap<String, usize>| {
        let mut out = String::new();
        for (i, part) in text.split('"').enumerate() {
            if i > 0 {
                out.push('"');
            }
            let is_uuid = part.len() == 36
                && part.chars().enumerate().all(|(k, c)| {
                    if [8, 13, 18, 23].contains(&k) {
                        c == '-'
                    } else {
                        c.is_ascii_hexdigit()
                    }
                });
            let is_time = part.len() >= 19
                && part.as_bytes()[4] == b'-'
                && part.as_bytes()[10] == b'T'
                && part[..4].chars().all(|c| c.is_ascii_digit());
            if is_uuid || is_time {
                let n = seen.len();
                let id = *seen.entry(part.to_owned()).or_insert(n);
                out.push_str(&format!("{}#{id}", if is_uuid { "uuid" } else { "time" }));
            } else {
                out.push_str(part);
            }
        }
        out
    }
}

#[tokio::test]
#[ignore = "run by scripts/compare-squash.sh"]
async fn apply_the_chain_and_record_what_it_leaves() {
    let files = chain(&env_path("SQUASH_CHAIN"));
    let out = env_path("SQUASH_OUT");
    std::fs::create_dir_all(&out).unwrap();
    let server = Server::get().await;
    let migration_role = server.migration_role_name();

    // The first database creates the roles, so the roles are recorded from it.
    let variants = [
        ("public_plain", false, false),
        ("extensions_plain", true, false),
        ("public_defaults", false, true),
        ("extensions_defaults", true, true),
    ];
    for (n, (name, in_extensions, defaults)) in variants.into_iter().enumerate() {
        let db = format!("squash_{name}");
        let admin = connect(server, "postgres").await;
        admin
            .batch_execute(&format!("drop database if exists {db} with (force)"))
            .await
            .unwrap();
        admin
            .batch_execute(&format!(
                "create database {db} owner {}",
                quote_ident(&migration_role)
            ))
            .await
            .unwrap();
        let mut client = connect(server, &db).await;
        if in_extensions {
            // Owned by the migration role, as Supabase's `extensions` schema
            // lets `postgres` grant on it, so the chain can grant USAGE.
            client
                .batch_execute(&format!(
                    "create schema if not exists extensions authorization {};
                     create extension if not exists vector with schema extensions;",
                    quote_ident(&migration_role)
                ))
                .await
                .unwrap();
        }
        client
            .batch_execute(&format!("set role {}", quote_ident(&migration_role)))
            .await
            .unwrap();
        if defaults {
            client
                .batch_execute(
                    "do $$ begin
                       if not exists (select from pg_roles where rolname = 'authenticated') then
                         create role authenticated nologin noinherit;
                       end if;
                       if not exists (select from pg_roles where rolname = 'gnomon_worker') then
                         create role gnomon_worker nologin noinherit nobypassrls;
                       end if;
                     end $$;
                     alter default privileges grant all on tables to authenticated, gnomon_worker;
                     alter default privileges grant all on functions to authenticated, gnomon_worker;
                     alter default privileges grant all on sequences to authenticated, gnomon_worker;
                     alter default privileges grant all on types to authenticated, gnomon_worker;
                     alter default privileges grant all on schemas to authenticated, gnomon_worker;",
                )
                .await
                .unwrap();
        }
        for (file, sql) in &files {
            let tx = client.transaction().await.unwrap();
            tx.batch_execute(sql)
                .await
                .unwrap_or_else(|e| panic!("apply {file}: {}", support::chain(&e)));
            tx.commit().await.unwrap();
        }
        if n == 0 {
            let roles = in_container(&[
                "psql",
                "-X",
                "-At",
                "-d",
                &db,
                "-c",
                "select rolname, rolsuper, rolinherit, rolcreaterole, rolcreatedb, rolcanlogin,
                        rolreplication, rolbypassrls, rolconnlimit, rolconfig
                 from pg_roles where rolname in ('authenticated', 'gnomon_worker', 'gnomon_definer')
                 order by 1;
                 select r.rolname, m.rolname, g.rolname, a.admin_option, a.inherit_option, a.set_option
                 from pg_auth_members a
                 join pg_roles r on r.oid = a.roleid
                 join pg_roles m on m.oid = a.member
                 join pg_roles g on g.oid = a.grantor
                 where r.rolname in ('authenticated', 'gnomon_worker', 'gnomon_definer')
                    or m.rolname in ('authenticated', 'gnomon_worker', 'gnomon_definer')
                 order by 1, 2, 3;",
            ]);
            std::fs::write(out.join("roles.txt"), roles).unwrap();
        }
        let dump = in_container(&[
            "pg_dump",
            "--schema-only",
            "--restrict-key=squash",
            "-d",
            &db,
        ]);
        std::fs::write(out.join(format!("{name}.dump.sql")), dump).unwrap();
        std::fs::write(out.join(format!("{name}.rows.txt")), rows(&client).await).unwrap();
        // pg_dump leaves out the functions a type creates, such as a range
        // type's constructors, so every owner and ACL comes from the catalog
        // too.
        let acls = in_container(&[
            "psql",
            "-X",
            "-At",
            "-d",
            &db,
            "-c",
            "select 'function', p.oid::regprocedure::text, p.proowner::regrole, p.proacl::text
             from pg_proc p where p.pronamespace = 'gnomon'::regnamespace
             union all
             select 'relation', c.oid::regclass::text, c.relowner::regrole, c.relacl::text
             from pg_class c where c.relnamespace = 'gnomon'::regnamespace
             union all
             select 'column', a.attrelid::regclass::text || '.' || a.attname, null, a.attacl::text
             from pg_attribute a join pg_class c on c.oid = a.attrelid
             where c.relnamespace = 'gnomon'::regnamespace and a.attacl is not null
             union all
             select 'type', t.oid::regtype::text, t.typowner::regrole, t.typacl::text
             from pg_type t where t.typnamespace = 'gnomon'::regnamespace
             union all
             select 'schema', n.nspname, n.nspowner::regrole, n.nspacl::text
             from pg_namespace n where n.nspname in ('gnomon', 'public', 'extensions')
             order by 1, 2;",
        ]);
        std::fs::write(out.join(format!("{name}.acl.txt")), acls).unwrap();
        drop(client);
        admin
            .batch_execute(&format!("drop database {db} with (force)"))
            .await
            .unwrap();
    }
}
