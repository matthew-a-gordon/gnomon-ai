//! `gnomon migrations export --target plain`, driven through the binary a
//! consumer runs (ADR-0006). These tests need no database.

use std::path::Path;
use std::process::{Command, Output};

fn gnomon(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_gnomon"))
        .args(args)
        .output()
        .expect("the gnomon binary runs")
}

fn export_plain(dimension: &str, out: &Path) -> Output {
    gnomon(&[
        "migrations",
        "export",
        "--target",
        "plain",
        "--embedding-dimension",
        dimension,
        "--out",
        out.to_str().unwrap(),
    ])
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

    let names = exported_files(out.path());
    assert!(!names.is_empty());
    for (i, name) in names.iter().enumerate() {
        let number = format!("{:04}_", i + 1);
        assert!(
            name.starts_with(&number) && name.ends_with(".sql"),
            "{name} is not numbered {number}*.sql"
        );
    }
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
