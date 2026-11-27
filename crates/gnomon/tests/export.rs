//! `gnomon migrations export`, driven through the binary a consumer runs
//! (ADR-0006). These tests need no database.

use std::path::Path;
use std::process::{Command, Output};

fn gnomon(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_gnomon"))
        .args(args)
        .output()
        .expect("the gnomon binary runs")
}

fn export(target: &str, dimension: &str, out: &Path) -> Output {
    gnomon(&[
        "migrations",
        "export",
        "--target",
        target,
        "--embedding-dimension",
        dimension,
        "--out",
        out.to_str().unwrap(),
    ])
}

fn export_plain(dimension: &str, out: &Path) -> Output {
    export("plain", dimension, out)
}

fn assert_ok(run: &Output) {
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
}

/// The file names and contents in `dir`, sorted by name.
fn chain(dir: &Path) -> Vec<(String, String)> {
    exported_files(dir)
        .into_iter()
        .map(|n| {
            let sql = std::fs::read_to_string(dir.join(&n)).unwrap();
            (n, sql)
        })
        .collect()
}

/// The plain export, the migrations as gnomon releases them.
fn plain_chain(dimension: &str) -> Vec<(String, String)> {
    let out = tempfile::tempdir().unwrap();
    assert_ok(&export_plain(dimension, out.path()));
    chain(out.path())
}

fn exported_files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

#[test]
fn plain_export_writes_numbered_sql_files_starting_at_0001() {
    let out = tempfile::tempdir().unwrap();
    let run = export_plain("1536", out.path());
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );

    // Each file is its migration's number and name. The numbers start at
    // 0001 and rise.
    let names = exported_files(out.path());
    let expected: Vec<String> = gnomon::migrations::MIGRATIONS
        .iter()
        .map(|m| {
            assert!(
                m.name.starts_with(&format!("{:04}_", m.version)),
                "{}",
                m.name
            );
            format!("{}.sql", m.name)
        })
        .collect();
    assert_eq!(names, expected);
    assert!(names[0].starts_with("0001_"));
    let versions: Vec<i32> = gnomon::migrations::MIGRATIONS
        .iter()
        .map(|m| m.version)
        .collect();
    assert!(versions.windows(2).all(|w| w[0] < w[1]), "{versions:?}");
}

#[test]
fn plain_export_writes_the_chosen_embedding_dimension_and_no_placeholder() {
    let out = tempfile::tempdir().unwrap();
    let run = export_plain("768", out.path());
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );

    let all: String = exported_files(out.path())
        .iter()
        .map(|n| std::fs::read_to_string(out.path().join(n)).unwrap())
        .collect();
    assert!(
        all.contains("768"),
        "the dimension is not in the exported SQL"
    );
    assert!(
        !all.contains("{{"),
        "an unreplaced placeholder is left in the exported SQL"
    );
}

#[test]
fn plain_export_refuses_a_dimension_pgvector_cannot_store() {
    for bad in ["0", "16001", "-3", "many"] {
        let out = tempfile::tempdir().unwrap();
        let run = export_plain(bad, out.path());
        assert!(!run.status.success(), "dimension {bad} was accepted");
        assert!(
            exported_files(out.path()).is_empty(),
            "dimension {bad} wrote files"
        );
    }
}

#[test]
fn plain_export_refuses_an_unknown_target() {
    let out = tempfile::tempdir().unwrap();
    let run = gnomon(&[
        "migrations",
        "export",
        "--target",
        "flyway",
        "--embedding-dimension",
        "1536",
        "--out",
        out.path().to_str().unwrap(),
    ]);
    assert!(!run.status.success());
}

#[test]
fn exporting_again_into_the_same_chain_never_rewrites_a_migration() {
    let out = tempfile::tempdir().unwrap();
    assert!(export_plain("1536", out.path()).status.success());
    let before: Vec<String> = exported_files(out.path())
        .iter()
        .map(|n| std::fs::read_to_string(out.path().join(n)).unwrap())
        .collect();

    // The same export again is a no-op.
    assert!(export_plain("1536", out.path()).status.success());

    // A different dimension would change released files, so it is refused.
    let run = export_plain("768", out.path());
    assert!(!run.status.success());
    let after: Vec<String> = exported_files(out.path())
        .iter()
        .map(|n| std::fs::read_to_string(out.path().join(n)).unwrap())
        .collect();
    assert_eq!(before, after);
}

// The Supabase target.

/// Splits a Supabase CLI migration file name, `<timestamp>_<name>.sql`.
fn supabase_version(file: &str) -> (&str, &str) {
    let (version, rest) = file.split_once('_').unwrap();
    (version, rest.strip_suffix(".sql").unwrap())
}

#[test]
fn supabase_export_writes_timestamped_files_the_cli_applies_in_gnomon_order() {
    let out = tempfile::tempdir().unwrap();
    assert_ok(&export("supabase", "1536", out.path()));

    let released = plain_chain("1536");
    let names = exported_files(out.path());
    assert_eq!(names.len(), released.len());
    for (file, (plain, _)) in names.iter().zip(&released) {
        let (version, name) = supabase_version(file);
        assert_eq!(version.len(), 14, "{file} has no YYYYMMDDHHMMSS version");
        assert!(version.bytes().all(|b| b.is_ascii_digit()), "{file}");
        assert_eq!(
            name,
            format!("gnomon_{}", plain.strip_suffix(".sql").unwrap()),
            "{file} does not name the gnomon migration it holds"
        );
    }
    let versions: Vec<_> = names.iter().map(|n| supabase_version(n).0).collect();
    assert!(
        versions.windows(2).all(|w| w[0] < w[1]),
        "the versions do not increase: {versions:?}"
    );
}

/// A chain a consumer exported from an earlier gnomon release, one that
/// shipped every migration but the last, and then added a migration of
/// their own dated `consumer_version`.
fn supabase_chain_from_an_earlier_release(out: &Path, consumer_version: &str) -> String {
    assert_ok(&export("supabase", "1536", out));
    let last = exported_files(out).pop().unwrap();
    std::fs::remove_file(out.join(&last)).unwrap();
    std::fs::write(
        out.join(format!("{consumer_version}_consumer_table.sql")),
        "create table public.consumer_table (id bigint primary key);\n",
    )
    .unwrap();
    last
}

#[test]
fn supabase_export_after_a_new_release_adds_only_the_new_migration_after_the_whole_chain() {
    let out = tempfile::tempdir().unwrap();
    // Dated after anything this export could choose from its clock.
    let consumer = "20991231235959";
    let dropped = supabase_chain_from_an_earlier_release(out.path(), consumer);
    let before = chain(out.path());

    assert_ok(&export("supabase", "1536", out.path()));
    let after = chain(out.path());

    // Every file already in the chain is untouched.
    for file in &before {
        assert!(after.contains(file), "{} was rewritten or removed", file.0);
    }
    let added: Vec<_> = after.iter().filter(|f| !before.contains(f)).collect();
    assert_eq!(added.len(), 1, "added {added:?}");
    let (file, sql) = added[0];
    let (version, name) = supabase_version(file);
    assert_eq!(
        name,
        supabase_version(&dropped).1,
        "added the wrong migration"
    );
    assert!(
        version > consumer,
        "{file} sorts before the consumer's {consumer}, so the CLI refuses it"
    );
    let released = plain_chain("1536");
    assert_eq!(sql, &released.last().unwrap().1);
}

#[test]
fn supabase_export_into_its_own_chain_again_writes_nothing() {
    let out = tempfile::tempdir().unwrap();
    assert_ok(&export("supabase", "1536", out.path()));
    let before = chain(out.path());
    // A later second, so a fresh export would choose other versions.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let run = export("supabase", "1536", out.path());
    assert_ok(&run);
    assert_eq!(chain(out.path()), before);
    assert!(run.stdout.is_empty(), "reported files it did not write");
}

#[test]
fn supabase_export_refuses_to_rewrite_a_released_migration_and_writes_nothing() {
    let out = tempfile::tempdir().unwrap();
    assert_ok(&export("supabase", "1536", out.path()));
    let last = exported_files(out.path()).pop().unwrap();
    std::fs::remove_file(out.path().join(last)).unwrap();
    let before = chain(out.path());

    // Another dimension would change every released file.
    let run = export("supabase", "768", out.path());
    assert!(!run.status.success(), "rewrote the chain for 768");
    assert_eq!(chain(out.path()), before, "a refused export wrote files");
}

#[test]
fn supabase_export_refuses_a_chain_missing_an_earlier_gnomon_migration() {
    let out = tempfile::tempdir().unwrap();
    assert_ok(&export("supabase", "1536", out.path()));
    let first = exported_files(out.path()).remove(0);
    std::fs::remove_file(out.path().join(first)).unwrap();
    let before = chain(out.path());

    let run = export("supabase", "1536", out.path());
    assert!(!run.status.success(), "filled a gap after later migrations");
    assert_eq!(chain(out.path()), before);
}
