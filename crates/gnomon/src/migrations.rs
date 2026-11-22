//! The Store's numbered migrations and their exporter (ADR-0006).
//!
//! gnomon never applies DDL itself. A consumer exports the migrations into
//! its own chain, its tooling applies them, and the Store refuses to run
//! against any schema version but [`SCHEMA_VERSION`].

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// The placeholder a migration template uses for the embedding dimension.
const DIMENSION_PLACEHOLDER: &str = "{{embedding_dimension}}";

/// One numbered migration. Once released, its template never changes.
#[derive(Debug, Clone, Copy)]
pub struct Migration {
    /// The number recorded in `gnomon.schema_version`.
    pub version: i32,
    /// The file stem, such as `0001_gnomon_schema`.
    pub name: &'static str,
    template: &'static str,
}

/// Every migration, in the order a chain applies them.
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "0001_gnomon_schema",
        template: include_str!("../migrations/0001_gnomon_schema.sql"),
    },
    Migration {
        version: 2,
        name: "0002_episodes",
        template: include_str!("../migrations/0002_episodes.sql"),
    },
    Migration {
        version: 3,
        name: "0003_kinds",
        template: include_str!("../migrations/0003_kinds.sql"),
    },
    Migration {
        version: 4,
        name: "0004_nodes_and_versions",
        template: include_str!("../migrations/0004_nodes_and_versions.sql"),
    },
    Migration {
        version: 5,
        name: "0005_tenancy_modes",
        template: include_str!("../migrations/0005_tenancy_modes.sql"),
    },
];

/// The schema version this build of the Store expects: the last migration's.
pub const SCHEMA_VERSION: i32 = MIGRATIONS[MIGRATIONS.len() - 1].version;

/// The width of every embedding vector column, chosen at export.
///
/// pgvector's `vector` type holds at most 16,000 dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmbeddingDimension(u16);

impl EmbeddingDimension {
    /// The largest dimension pgvector's `vector` type stores.
    pub const MAX: u16 = 16_000;

    pub fn new(dimension: u32) -> Result<Self, InvalidEmbeddingDimension> {
        match u16::try_from(dimension) {
            Ok(d) if (1..=Self::MAX).contains(&d) => Ok(Self(d)),
            _ => Err(InvalidEmbeddingDimension(dimension.to_string())),
        }
    }

    pub fn get(self) -> u16 {
        self.0
    }
}

impl FromStr for EmbeddingDimension {
    type Err = InvalidEmbeddingDimension;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let n: u32 = s
            .parse()
            .map_err(|_| InvalidEmbeddingDimension(s.to_owned()))?;
        Self::new(n)
    }
}

/// An embedding dimension outside 1 to [`EmbeddingDimension::MAX`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("embedding dimension must be a whole number from 1 to 16000, got {0:?}")]
pub struct InvalidEmbeddingDimension(String);

/// A migration chain gnomon can export into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// Numbered `.sql` files, each applied in one transaction, in name order,
    /// for example with `psql -v ON_ERROR_STOP=1 -1 -f`.
    Plain,
}

impl FromStr for Target {
    type Err = UnknownTarget;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "plain" => Ok(Self::Plain),
            other => Err(UnknownTarget(other.to_owned())),
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Plain => f.write_str("plain"),
        }
    }
}

/// A target name gnomon does not export to.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown migration target {0:?}; the supported target is plain")]
pub struct UnknownTarget(String);

/// One migration rendered for a target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportedMigration {
    pub file_name: String,
    pub sql: String,
}

/// Renders every migration for `target` with `dimension` substituted.
pub fn render(target: Target, dimension: EmbeddingDimension) -> Vec<ExportedMigration> {
    match target {
        Target::Plain => MIGRATIONS
            .iter()
            .map(|m| ExportedMigration {
                file_name: format!("{}.sql", m.name),
                sql: m
                    .template
                    .replace(DIMENSION_PLACEHOLDER, &dimension.get().to_string()),
            })
            .collect(),
    }
}

/// Why an export wrote nothing, or stopped part way.
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("{path} already exists with different content; released migrations never change")]
    WouldRewrite { path: PathBuf },
    #[error("could not write {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Writes every migration for `target` into `out_dir`, creating it if needed.
///
/// A file that already exists with the same content is left alone, so
/// exporting again into the same chain is safe. A file that exists with other
/// content is never overwritten.
pub fn export(
    target: Target,
    dimension: EmbeddingDimension,
    out_dir: &Path,
) -> Result<Vec<PathBuf>, ExportError> {
    let rendered = render(target, dimension);
    let io_error_at = |path: &Path| {
        let path = path.to_owned();
        move |source| ExportError::Io { path, source }
    };

    // Check every file before writing any, so a refused export writes nothing.
    for m in &rendered {
        let path = out_dir.join(&m.file_name);
        match std::fs::read_to_string(&path) {
            Ok(existing) if existing != m.sql => return Err(ExportError::WouldRewrite { path }),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_error_at(&path)(e)),
        }
    }

    std::fs::create_dir_all(out_dir).map_err(io_error_at(out_dir))?;
    let mut written = Vec::with_capacity(rendered.len());
    for m in rendered {
        let path = out_dir.join(&m.file_name);
        std::fs::write(&path, &m.sql).map_err(io_error_at(&path))?;
        written.push(path);
    }
    Ok(written)
}
