//! A consumer's Alembic chain applies gnomon's migration files (ADR-0006),
//! checked by Alembic itself through `uvx` with pinned versions.
//!
//! The consumer copies the files beside its `versions/` directory and writes
//! one revision that runs them in order, each with `exec_driver_sql` and no
//! parameters, so neither Alembic nor the driver reads a `%` or `:` in the
//! SQL as a placeholder. The README shows the same revision.

mod support;

use support::install::{Extensions, assert_installed, prepare};

/// The consumer's revision, as the README shows it.
fn revision(files: &[String]) -> String {
    let names = files
        .iter()
        .map(|f| format!("    {f:?},\n"))
        .collect::<String>();
    format!(
        r#""""gnomon's migration files."""

from pathlib import Path

from alembic import op

revision = "gnomon_0006"
down_revision = None
branch_labels = None
depends_on = None

GNOMON = Path(__file__).resolve().parent.parent / "gnomon"
FILES = [
{names}]


def upgrade() -> None:
    for name in FILES:
        sql = (GNOMON / name).read_text(encoding="utf-8")
        op.get_bind().exec_driver_sql(sql, execution_options={{"no_parameters": True}})


def downgrade() -> None:
    raise NotImplementedError("gnomon's migrations have no downgrade")
"#
    )
}

async fn alembic_applies_the_files(placement: Extensions) {
    let db = support::TestDb::empty_with_login().await;
    let schema = prepare(&db, placement).await;
    let project = support::alembic::Project::new();
    let files: Vec<String> =
        support::copy_migration_files(&project.gnomon_files(), |_| String::new())
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
    std::fs::write(project.versions().join("gnomon_0006.py"), revision(&files)).unwrap();
    project
        .alembic(
            &["upgrade", "head"],
            Some(&db.migration_url("postgresql+psycopg")),
        )
        .unwrap_or_else(|e| panic!("alembic upgrade head failed: {e}"));
    assert_installed(&db, &schema).await;
}

#[tokio::test]
async fn alembic_applies_the_files_with_pgvector_in_the_default_schema() {
    alembic_applies_the_files(Extensions::Default).await;
}

#[tokio::test]
async fn alembic_applies_the_files_with_pgvector_in_extensions() {
    alembic_applies_the_files(Extensions::InExtensions).await;
}
