//! `gnomon migrations export --target alembic`, checked by Alembic itself
//! (ADR-0006). These tests need `uvx`; see docs/testing.md.

mod support;

use std::path::Path;
use std::process::Output;

use support::alembic::Project;

fn export_alembic(dimension: &str, out: &Path) -> Output {
    let run = std::process::Command::new(env!("CARGO_BIN_EXE_gnomon"))
        .args(["migrations", "export", "--target", "alembic"])
        .args(["--embedding-dimension", dimension, "--out"])
        .arg(out)
        .output()
        .expect("the gnomon binary runs");
    run
}

fn assert_ok(run: &Output) {
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
}

/// gnomon's revision ids, in release order.
fn gnomon_revisions() -> Vec<String> {
    gnomon::migrations::MIGRATIONS
        .iter()
        .map(|m| format!("gnomon_{:04}", m.version))
        .collect()
}

/// The links a chain of `revisions` has, each to the one before, starting
/// from `base`.
fn chained(base: &str, revisions: &[String]) -> Vec<(String, String)> {
    let mut from = base.to_owned();
    let mut links = Vec::new();
    for r in revisions {
        links.push((from, r.clone()));
        from = r.clone();
    }
    links
}

fn sorted(mut v: Vec<(String, String)>) -> Vec<(String, String)> {
    v.sort();
    v
}

#[test]
fn alembic_export_into_a_new_chain_links_each_revision_to_the_one_before() {
    let project = Project::new();
    assert_ok(&export_alembic("1536", &project.versions()));

    let revisions = gnomon_revisions();
    assert_eq!(project.links(), sorted(chained("<base>", &revisions)));
    assert_eq!(project.heads(), vec![revisions.last().unwrap().clone()]);
}

/// The revision files in `dir` and their contents, sorted by name.
fn files(dir: &Path) -> Vec<(String, String)> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "py"))
        .map(|p| {
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            (name, std::fs::read_to_string(&p).unwrap())
        })
        .collect();
    files.sort();
    files
}

#[test]
fn alembic_export_follows_the_head_of_a_consumer_chain() {
    let project = Project::new();
    project.add_consumer_revision("c0ffee000001", None);
    project.add_consumer_revision("c0ffee000002", Some("c0ffee000001"));
    assert_ok(&export_alembic("1536", &project.versions()));

    let revisions = gnomon_revisions();
    let mut expected = chained("<base>", &["c0ffee000001".to_owned()]);
    expected.extend(chained("c0ffee000001", &["c0ffee000002".to_owned()]));
    expected.extend(chained("c0ffee000002", &revisions));
    assert_eq!(project.links(), sorted(expected));
    assert_eq!(project.heads(), vec![revisions.last().unwrap().clone()]);
}

#[test]
fn alembic_export_after_a_new_release_adds_only_the_new_revision_after_the_chain_head() {
    // A chain exported from an earlier release that shipped every migration
    // but the last, then given a revision of the consumer's own.
    let project = Project::new();
    assert_ok(&export_alembic("1536", &project.versions()));
    let revisions = gnomon_revisions();
    let (last, earlier) = revisions.split_last().unwrap();
    let last_file = files(&project.versions()).pop().unwrap().0;
    std::fs::remove_file(project.versions().join(&last_file)).unwrap();
    project.add_consumer_revision("beef00000001", earlier.last().map(String::as_str));
    let before = files(&project.versions());

    let run = export_alembic("1536", &project.versions());
    assert_ok(&run);

    let after = files(&project.versions());
    for file in &before {
        assert!(after.contains(file), "{} was rewritten or removed", file.0);
    }
    assert_eq!(after.len(), before.len() + 1);
    let mut expected = chained("<base>", earlier);
    expected.extend(chained(
        earlier.last().unwrap(),
        &["beef00000001".to_owned(), last.clone()],
    ));
    assert_eq!(project.links(), sorted(expected));
    assert_eq!(project.heads(), vec![last.clone()]);
}

#[test]
fn alembic_export_into_its_own_chain_again_writes_nothing() {
    let project = Project::new();
    assert_ok(&export_alembic("1536", &project.versions()));
    let before = files(&project.versions());
    let run = export_alembic("1536", &project.versions());
    assert_ok(&run);
    assert_eq!(files(&project.versions()), before);
    assert!(run.stdout.is_empty(), "reported files it did not write");
}

#[test]
fn alembic_export_refuses_a_chain_with_several_heads_and_writes_nothing() {
    let project = Project::new();
    project.add_consumer_revision("a00000000001", None);
    project.add_consumer_revision("b00000000001", None);
    let before = files(&project.versions());
    let run = export_alembic("1536", &project.versions());
    assert!(!run.status.success(), "exported onto two heads");
    assert_eq!(files(&project.versions()), before);
}

#[test]
fn alembic_export_refuses_to_rewrite_a_released_revision() {
    let project = Project::new();
    assert_ok(&export_alembic("1536", &project.versions()));
    let before = files(&project.versions());
    // Another dimension would change every released revision.
    let run = export_alembic("768", &project.versions());
    assert!(!run.status.success(), "rewrote the chain for 768");
    assert_eq!(files(&project.versions()), before);
}

#[tokio::test]
async fn alembic_upgrade_head_applies_a_schema_the_store_accepts() {
    let db = support::TestDb::empty_with_login().await;
    let project = Project::new();
    project.add_consumer_revision("c0ffee000001", None);
    assert_ok(&export_alembic("1536", &project.versions()));

    let url = db.migration_url("postgresql+psycopg");
    project
        .alembic(&["upgrade", "head"], Some(&url))
        .expect("alembic upgrade head");

    let store = gnomon::Store::connect_no_tls(&db.login_url())
        .await
        .expect("the Store accepts the schema Alembic applied");
    assert_eq!(store.embedding_dimension().get(), 1536);
    let current = project.alembic(&["current"], Some(&url)).unwrap();
    assert_eq!(
        current.split_whitespace().next(),
        gnomon_revisions().last().map(String::as_str),
        "alembic current: {current}"
    );
}
