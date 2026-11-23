//! The Store's numbered migrations and their exporter (ADR-0006).
//!
//! gnomon never applies DDL itself. A consumer exports the migrations into
//! its own chain, its tooling applies them, and the Store refuses to run
//! against any schema version but [`SCHEMA_VERSION`].

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use chrono::SubsecRound;

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
    Migration {
        version: 6,
        name: "0006_bound_precision",
        template: include_str!("../migrations/0006_bound_precision.sql"),
    },
    Migration {
        version: 7,
        name: "0007_ranked_merge",
        template: include_str!("../migrations/0007_ranked_merge.sql"),
    },
    Migration {
        version: 8,
        name: "0008_edges",
        template: include_str!("../migrations/0008_edges.sql"),
    },
    Migration {
        version: 9,
        name: "0009_statements",
        template: include_str!("../migrations/0009_statements.sql"),
    },
    Migration {
        version: 10,
        name: "0010_retract_episode",
        template: include_str!("../migrations/0010_retract_episode.sql"),
    },
    Migration {
        version: 11,
        name: "0011_erase_episode",
        template: include_str!("../migrations/0011_erase_episode.sql"),
    },
    Migration {
        version: 12,
        name: "0012_erase_node_edge_scope",
        template: include_str!("../migrations/0012_erase_node_edge_scope.sql"),
    },
    Migration {
        version: 13,
        name: "0013_vacuum_after_erasure",
        template: include_str!("../migrations/0013_vacuum_after_erasure.sql"),
    },
    Migration {
        version: 14,
        name: "0014_stage_records",
        template: include_str!("../migrations/0014_stage_records.sql"),
    },
    Migration {
        version: 15,
        name: "0015_reserved_edge_kinds",
        template: include_str!("../migrations/0015_reserved_edge_kinds.sql"),
    },
    Migration {
        version: 16,
        name: "0016_full_text_search",
        template: include_str!("../migrations/0016_full_text_search.sql"),
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
    /// Supabase CLI migrations, `<timestamp>_gnomon_<name>.sql`, for a
    /// project's `supabase/migrations` directory.
    Supabase,
    /// Alembic revisions, `gnomon_<name>.py`, for a project's `versions`
    /// directory. Revision `gnomon_0001` follows the chain's head at the
    /// first export, and each later revision follows the one before.
    Alembic,
}

impl FromStr for Target {
    type Err = UnknownTarget;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "plain" => Ok(Self::Plain),
            "supabase" => Ok(Self::Supabase),
            "alembic" => Ok(Self::Alembic),
            other => Err(UnknownTarget(other.to_owned())),
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Plain => f.write_str("plain"),
            Self::Supabase => f.write_str("supabase"),
            Self::Alembic => f.write_str("alembic"),
        }
    }
}

/// A target name gnomon does not export to.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown migration target {0:?}; the supported targets are plain, supabase and alembic")]
pub struct UnknownTarget(String);

/// Why an export wrote nothing.
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("{path} already exists with different content; released migrations never change")]
    WouldRewrite { path: PathBuf },
    #[error("the chain holds gnomon migration {present} but not the earlier {missing}")]
    Gap {
        present: &'static str,
        missing: &'static str,
    },
    #[error("the chain holds gnomon migration {name} more than once: {files:?}")]
    Duplicate {
        name: &'static str,
        files: Vec<String>,
    },
    #[error(
        "the Alembic chain has several heads, {heads:?}; merge them before exporting, \
         so gnomon's revisions have one parent"
    )]
    SeveralHeads { heads: Vec<String> },
    #[error("could not read or write {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Writes the gnomon migrations that `out_dir`'s chain lacks, creating the
/// directory if needed, and returns the files it wrote.
///
/// A gnomon migration already in the chain is left as it is, after checking
/// that it is exactly what this release would write, so a chain that applied
/// it stays valid. One that differs, such as one exported with another
/// embedding dimension, refuses the whole export. The new migrations go after
/// everything already in the chain, the consumer's own migrations included.
pub fn export(
    target: Target,
    dimension: EmbeddingDimension,
    out_dir: &Path,
) -> Result<Vec<PathBuf>, ExportError> {
    export_set(MIGRATIONS, target, dimension, out_dir, chrono::Utc::now())
}

fn export_set(
    set: &[Migration],
    target: Target,
    dimension: EmbeddingDimension,
    out_dir: &Path,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Vec<PathBuf>, ExportError> {
    let files = Files::read(out_dir)?;

    // Which gnomon migrations the chain already holds. They must be the
    // first ones, in order, because each was exported after the one before.
    let mut held: Vec<String> = Vec::new();
    for m in set {
        let found: Vec<&String> = files.names.iter().filter(|f| target.holds(m, f)).collect();
        match found[..] {
            [] => break,
            [file] => held.push(file.clone()),
            _ => {
                return Err(ExportError::Duplicate {
                    name: m.name,
                    files: found.into_iter().cloned().collect(),
                })
            }
        }
    }
    let (kept, new) = set.split_at(held.len());
    if let Some(later) = new
        .iter()
        .find(|m| files.names.iter().any(|f| target.holds(m, f)))
    {
        return Err(ExportError::Gap {
            present: later.name,
            missing: new[0].name,
        });
    }

    // A refused export writes nothing, so check every kept file first. An
    // Alembic revision's parent was the chain's head when it was exported,
    // so it is compared with the revision this release writes on that parent.
    for (m, file) in kept.iter().zip(&held) {
        let path = out_dir.join(file);
        let existing = std::fs::read_to_string(&path).map_err(io_error_at(&path))?;
        let parent = match target {
            Target::Alembic => match alembic::revisions(&existing).1.as_slice() {
                [] => None,
                [one] => Some(one.clone()),
                // gnomon never writes a merge revision.
                _ => return Err(ExportError::WouldRewrite { path }),
            },
            Target::Plain | Target::Supabase => None,
        };
        if existing != target.contents(m, dimension, parent.as_deref()) {
            return Err(ExportError::WouldRewrite { path });
        }
    }

    let mut placement = target.placement(out_dir, &files, now)?;
    let rendered: Vec<(PathBuf, String)> = new
        .iter()
        .map(|m| {
            let (name, parent) = placement.next(m);
            (
                out_dir.join(name),
                target.contents(m, dimension, parent.as_deref()),
            )
        })
        .collect();

    std::fs::create_dir_all(out_dir).map_err(io_error_at(out_dir))?;
    let mut written = Vec::with_capacity(rendered.len());
    for (path, contents) in rendered {
        std::fs::write(&path, contents).map_err(io_error_at(&path))?;
        written.push(path);
    }
    Ok(written)
}

fn io_error_at(path: &Path) -> impl FnOnce(std::io::Error) -> ExportError {
    let path = path.to_owned();
    move |source| ExportError::Io { path, source }
}

/// The file names in a chain directory. A directory that does not exist yet
/// is an empty chain.
struct Files {
    names: Vec<String>,
}

impl Files {
    fn read(dir: &Path) -> Result<Files, ExportError> {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Files { names: Vec::new() })
            }
            Err(e) => return Err(io_error_at(dir)(e)),
        };
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(io_error_at(dir))?;
            if let Ok(name) = entry.file_name().into_string() {
                names.push(name);
            }
        }
        names.sort();
        Ok(Files { names })
    }
}

/// How a target places the migrations it adds to a chain.
enum Placement {
    /// gnomon's own numbered names.
    Numbered,
    /// Supabase CLI versions: the next second after the export's clock and
    /// after the chain's latest version, whichever is later.
    Timestamped { next: chrono::NaiveDateTime },
    /// Alembic revisions, each following `parent`: the chain's head for the
    /// first, then the revision before.
    Linked { parent: Option<String> },
}

/// The Supabase CLI's version format.
const SUPABASE_VERSION: &str = "%Y%m%d%H%M%S";

/// The migration that runs `create extension if not exists vector`.
const CREATES_PGVECTOR: i32 = 1;

/// What the supabase target writes before migration 0001, which runs
/// `create extension if not exists vector`. Without it, pgvector lands in
/// the first schema on the migration role's search path, `public`, which
/// PostgREST exposes. Supabase keeps extensions in `extensions` instead.
const SUPABASE_EXTENSIONS: &str = "\
-- Exported for Supabase by `gnomon migrations export --target supabase`.
-- Supabase keeps extensions in the `extensions` schema, which PostgREST does
-- not expose, so pgvector is created there before migration 0001 asks for
-- it. Where pgvector is already installed, it stays where it is.
create schema if not exists extensions;
create extension if not exists vector with schema extensions;

";

impl Placement {
    /// The file name for `m`, the next migration in the chain, and for an
    /// Alembic revision the revision it follows (`None` for the chain's base).
    fn next(&mut self, m: &Migration) -> (String, Option<String>) {
        match self {
            Self::Numbered => (format!("{}.sql", m.name), None),
            Self::Timestamped { next } => {
                let name = format!("{}_gnomon_{}.sql", next.format(SUPABASE_VERSION), m.name);
                *next += chrono::Duration::seconds(1);
                (name, None)
            }
            Self::Linked { parent } => {
                let follows = parent.replace(alembic::revision_id(m));
                (alembic::file_name(m), follows)
            }
        }
    }
}

impl Target {
    /// Whether `file` in a chain holds migration `m`.
    fn holds(self, m: &Migration, file: &str) -> bool {
        match self {
            Self::Plain => file == format!("{}.sql", m.name),
            Self::Supabase => file
                .strip_suffix(&format!("_gnomon_{}.sql", m.name))
                .is_some_and(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit())),
            Self::Alembic => file == alembic::file_name(m),
        }
    }

    /// The file contents this release writes for `m`. Only an Alembic
    /// revision names a `parent`, the revision it follows.
    fn contents(
        self,
        m: &Migration,
        dimension: EmbeddingDimension,
        parent: Option<&str>,
    ) -> String {
        let sql = m
            .template
            .replace(DIMENSION_PLACEHOLDER, &dimension.get().to_string());
        match self {
            Self::Plain => sql,
            Self::Supabase if m.version == CREATES_PGVECTOR => {
                format!("{SUPABASE_EXTENSIONS}{sql}")
            }
            Self::Supabase => sql,
            Self::Alembic => alembic::revision(m, parent, &sql),
        }
    }

    fn placement(
        self,
        dir: &Path,
        files: &Files,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<Placement, ExportError> {
        Ok(match self {
            Self::Plain => Placement::Numbered,
            Self::Alembic => Placement::Linked {
                parent: alembic::head(dir, files)?,
            },
            Self::Supabase => {
                let now = now.naive_utc().trunc_subsecs(0);
                let latest = files
                    .names
                    .iter()
                    .filter_map(|f| f.strip_suffix(".sql")?.split_once('_'))
                    .filter_map(|(v, _)| {
                        chrono::NaiveDateTime::parse_from_str(v, SUPABASE_VERSION).ok()
                    })
                    .max();
                let next = match latest {
                    Some(l) if l >= now => l + chrono::Duration::seconds(1),
                    _ => now,
                };
                Placement::Timestamped { next }
            }
        })
    }
}

/// Alembic revision files: writing gnomon's, and reading enough of any
/// revision to find the chain's head.
mod alembic {
    use super::{io_error_at, ExportError, Files, Migration};
    use std::path::Path;

    pub(super) fn revision_id(m: &Migration) -> String {
        format!("gnomon_{:04}", m.version)
    }

    pub(super) fn file_name(m: &Migration) -> String {
        format!("gnomon_{}.py", m.name)
    }

    /// gnomon migration `m` as an Alembic revision following `parent`. The
    /// SQL goes to the driver as written, with no parameters, so neither
    /// Alembic nor the driver reads its `%` or `:` as a placeholder.
    pub(super) fn revision(m: &Migration, parent: Option<&str>, sql: &str) -> String {
        let down = parent.map_or("None".to_owned(), python_string);
        format!(
            r#""""gnomon migration {name}.

Exported by `gnomon migrations export --target alembic`. Released gnomon
migrations never change, so leave this revision as it is: a later gnomon
release adds new revisions after it.
"""

from alembic import op

revision = {revision}
down_revision = {down}
branch_labels = None
depends_on = None

SQL = {sql}


def upgrade() -> None:
    op.get_bind().exec_driver_sql(SQL, execution_options={{"no_parameters": True}})


def downgrade() -> None:
    raise NotImplementedError("gnomon migrations have no downgrade")
"#,
            name = m.name,
            revision = python_string(&revision_id(m)),
            sql = python_long_string(sql),
        )
    }

    /// `s` as a Python string literal.
    fn python_string(s: &str) -> String {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }

    /// `s` as a triple-quoted Python string literal.
    fn python_long_string(s: &str) -> String {
        format!(
            "\"\"\"{}\"\"\"",
            s.replace('\\', "\\\\").replace('"', "\\\"")
        )
    }

    /// The single head of the chain in `dir`, or `None` for a chain with no
    /// revisions yet.
    pub(super) fn head(dir: &Path, files: &Files) -> Result<Option<String>, ExportError> {
        let mut ids = Vec::new();
        let mut parents = Vec::new();
        for name in files.names.iter().filter(|n| n.ends_with(".py")) {
            let path = dir.join(name);
            let source = std::fs::read_to_string(&path).map_err(io_error_at(&path))?;
            let (revision, down) = revisions(&source);
            if let Some(r) = revision {
                ids.push(r);
                parents.extend(down);
            }
        }
        let mut heads: Vec<String> = ids.into_iter().filter(|r| !parents.contains(r)).collect();
        match heads.len() {
            0 | 1 => Ok(heads.pop()),
            _ => {
                heads.sort();
                Err(ExportError::SeveralHeads { heads })
            }
        }
    }

    /// A revision file's own `revision` id and the `down_revision` ids it
    /// follows, read from the module-level assignments in the forms `alembic
    /// revision` writes: `revision = "a"`, `revision: str = "a"`, and a
    /// `down_revision` of `None`, one string, or a tuple or list of strings
    /// for a merge.
    pub(super) fn revisions(source: &str) -> (Option<String>, Vec<String>) {
        let revision =
            assignment(source, "revision").and_then(|v| quoted_strings(v).into_iter().next());
        let down = assignment(source, "down_revision")
            .map(quoted_strings)
            .unwrap_or_default();
        (revision, down)
    }

    /// The right-hand side of the module-level assignment to `name`.
    fn assignment<'a>(source: &'a str, name: &str) -> Option<&'a str> {
        source.lines().find_map(|line| {
            let rest = line.strip_prefix(name)?.trim_start();
            let value = match rest.strip_prefix(':') {
                Some(annotated) => annotated.split_once('=')?.1,
                None => rest.strip_prefix('=')?,
            };
            Some(value.trim())
        })
    }

    /// The string literals in a Python expression, up to any comment.
    fn quoted_strings(expr: &str) -> Vec<String> {
        let mut found = Vec::new();
        let mut chars = expr.chars();
        while let Some(c) = chars.next() {
            match c {
                '#' => break,
                '\'' | '"' => found.push(chars.by_ref().take_while(|&d| d != c).collect()),
                _ => {}
            }
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A gnomon release one migration newer than this one.
    fn next_release() -> Vec<Migration> {
        let mut set = MIGRATIONS.to_vec();
        set.push(Migration {
            version: SCHEMA_VERSION + 1,
            name: "9999_a_later_release",
            template: "create table gnomon.later_release (id bigint primary key);\n",
        });
        set
    }

    fn chain(dir: &Path) -> Vec<(String, String)> {
        Files::read(dir)
            .unwrap()
            .names
            .into_iter()
            .map(|n| {
                let contents = std::fs::read_to_string(dir.join(&n)).unwrap();
                (n, contents)
            })
            .collect()
    }

    #[test]
    fn exporting_a_newer_release_adds_only_its_new_migration_on_every_target() {
        let dimension = EmbeddingDimension::new(1536).unwrap();
        let now = chrono::Utc::now();
        for target in [Target::Plain, Target::Supabase, Target::Alembic] {
            let out = tempfile::tempdir().unwrap();
            export_set(MIGRATIONS, target, dimension, out.path(), now).unwrap();
            let before = chain(out.path());

            // The next release runs a minute later.
            let later = now + chrono::Duration::minutes(1);
            let written = export_set(&next_release(), target, dimension, out.path(), later)
                .unwrap_or_else(|e| panic!("{target}: {e}"));

            let after = chain(out.path());
            for file in &before {
                assert!(after.contains(file), "{target}: {} changed", file.0);
            }
            assert_eq!(after.len(), before.len() + 1, "{target}");
            assert_eq!(written.len(), 1, "{target}: wrote {written:?}");
            let added = written[0].file_name().unwrap().to_str().unwrap();
            assert!(added.contains("9999_a_later_release"), "{target}: {added}");
            assert!(
                before.iter().all(|(n, _)| n.as_str() < added),
                "{target}: {added} sorts before a released migration"
            );
            if target == Target::Alembic {
                let source = std::fs::read_to_string(&written[0]).unwrap();
                let (revision, down) = alembic::revisions(&source);
                assert_eq!(
                    revision.as_deref(),
                    Some(format!("gnomon_{:04}", SCHEMA_VERSION + 1).as_str())
                );
                assert_eq!(down, vec![format!("gnomon_{:04}", SCHEMA_VERSION)]);
            }
        }
    }
}
