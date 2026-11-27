//! `gnomon migrations export --target supabase`, applied by the Supabase
//! CLI's own migration runner, `supabase migration up`, to a database of the
//! suite's server (ADR-0006). These tests need `npx`; see docs/testing.md.

mod support;

use support::TestDb;

#[tokio::test]
async fn supabase_migration_up_applies_only_the_new_migration_after_a_release() {
    // A chain at an earlier release, which shipped every migration but the
    // last, applied by the CLI, then given a later migration of the
    // consumer's own.
    let db = TestDb::empty_with_login().await;
    let url = db.migration_url("postgresql");
    let workdir = tempfile::tempdir().unwrap();
    let migrations = workdir.path().join("supabase/migrations");
    support::export_to(support::Target::Supabase, 1536, &migrations);
    let mut files: Vec<_> = std::fs::read_dir(&migrations)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    files.sort();
    std::fs::remove_file(files.pop().unwrap()).unwrap();
    support::supabase_migration_up(workdir.path(), &url).expect("the earlier release applies");
    std::fs::write(
        migrations.join("20991231235959_consumer_table.sql"),
        "create table public.consumer_table (id bigint primary key);\n",
    )
    .unwrap();
    support::supabase_migration_up(workdir.path(), &url).expect("the consumer's migration applies");
    // The grants come last, so the earlier release's ledger is read as the
    // operator.
    let found: Option<i32> = db
        .operator()
        .await
        .query_one("select max(version) from gnomon.schema_version", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(found, Some(gnomon::migrations::SCHEMA_VERSION - 1));

    // The new release's export, applied by the CLI. It refuses a migration
    // that sorts before one it has applied, so this also checks the order.
    support::export_to(support::Target::Supabase, 1536, &migrations);
    let applied = support::supabase_migration_up(workdir.path(), &url)
        .expect("the CLI applies the new release's migration");
    assert_eq!(
        applied.matches("Applying migration").count(),
        1,
        "the CLI applied more than the new migration: {applied}"
    );
    gnomon::Store::connect(&db.login_url(), &support::connect_options())
        .await
        .expect("the Store accepts the upgraded schema");
}
