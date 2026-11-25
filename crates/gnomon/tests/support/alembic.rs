//! Runs Alembic against a consumer's Alembic project, for the alembic
//! target (ADR-0006).
//!
//! Alembic runs through `uvx` with pinned versions, so nothing is installed
//! globally and every run resolves the same packages.

use std::path::{Path, PathBuf};
use std::process::Command;

const ALEMBIC: &str = "alembic==1.20.1";
const DRIVER: &str = "psycopg[binary]==3.3.9";
const SQLALCHEMY: &str = "sqlalchemy==2.1.9";

/// A consumer's Alembic project in a temporary directory: the fixture's
/// `alembic.ini` and `env.py`, and an empty `versions/`.
pub struct Project {
    dir: tempfile::TempDir,
}

impl Project {
    pub fn new() -> Project {
        let dir = tempfile::tempdir().unwrap();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/alembic");
        for file in ["alembic.ini", "env.py"] {
            std::fs::copy(fixture.join(file), dir.path().join(file)).unwrap();
        }
        std::fs::create_dir(dir.path().join("versions")).unwrap();
        Project { dir }
    }

    /// Where the revisions go: what a consumer passes as `--out`.
    pub fn versions(&self) -> PathBuf {
        self.dir.path().join("versions")
    }

    /// Runs `alembic <args>` in the project and returns its stdout. `url`
    /// is the database, in SQLAlchemy's form, for commands that connect.
    pub fn alembic(&self, args: &[&str], url: Option<&str>) -> Result<String, String> {
        let mut cmd = Command::new("uvx");
        cmd.args([
            "--quiet", "--from", ALEMBIC, "--with", DRIVER, "--with", SQLALCHEMY,
        ])
        .arg("alembic")
        .arg("-c")
        .arg(self.dir.path().join("alembic.ini"))
        .args(args)
        .current_dir(self.dir.path());
        if let Some(url) = url {
            cmd.env("GNOMON_ALEMBIC_URL", url);
        }
        let run = cmd.output().expect("uvx runs; see docs/testing.md");
        if run.status.success() {
            Ok(String::from_utf8_lossy(&run.stdout).into_owned())
        } else {
            Err(String::from_utf8_lossy(&run.stderr).into_owned())
        }
    }

    /// The `(down_revision, revision)` links `alembic history` reports, with
    /// `<base>` for a revision with no parent.
    pub fn links(&self) -> Vec<(String, String)> {
        let history = self.alembic(&["history"], None).expect("alembic history");
        let mut links: Vec<(String, String)> = history
            .lines()
            .filter_map(|line| {
                let (from, rest) = line.split_once(" -> ")?;
                let to = rest.split([' ', ',']).next()?;
                Some((from.trim().to_owned(), to.to_owned()))
            })
            .flat_map(|(from, to)| {
                // A merge revision reports `a, b -> c`.
                from.split(", ")
                    .map(|f| (f.to_owned(), to.clone()))
                    .collect::<Vec<_>>()
            })
            .collect();
        links.sort();
        links
    }

    /// The chain's heads, as `alembic heads` reports them.
    pub fn heads(&self) -> Vec<String> {
        let out = self.alembic(&["heads"], None).expect("alembic heads");
        let mut heads: Vec<String> = out
            .lines()
            .filter_map(|l| l.split_whitespace().next().map(str::to_owned))
            .collect();
        heads.sort();
        heads
    }

    /// Writes a consumer's own revision into `versions/`.
    pub fn add_consumer_revision(&self, revision: &str, down_revision: Option<&str>) {
        let down = down_revision.map_or("None".to_owned(), |d| format!("{d:?}"));
        std::fs::write(
            self.versions().join(format!("{revision}_consumer.py")),
            format!(
                "\"\"\"A consumer's own revision.\"\"\"\n\
                 from typing import Sequence, Union\n\n\
                 from alembic import op\n\n\
                 revision: str = {revision:?}\n\
                 down_revision: Union[str, Sequence[str], None] = {down}\n\
                 branch_labels = None\n\
                 depends_on = None\n\n\n\
                 def upgrade() -> None:\n    \
                     op.execute(\"create table public.consumer_{revision} (id bigint primary key)\")\n\n\n\
                 def downgrade() -> None:\n    \
                     op.execute(\"drop table public.consumer_{revision}\")\n"
            ),
        )
        .unwrap();
    }
}
