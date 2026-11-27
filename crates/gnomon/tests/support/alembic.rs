//! Runs Alembic against a consumer's Alembic project that applies gnomon's
//! migration files (ADR-0006).
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

    /// Where the revisions go.
    pub fn versions(&self) -> PathBuf {
        self.dir.path().join("versions")
    }

    /// Where the consumer copies gnomon's migration files, beside
    /// `versions/`.
    pub fn gnomon_files(&self) -> PathBuf {
        self.dir.path().join("gnomon")
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
}
