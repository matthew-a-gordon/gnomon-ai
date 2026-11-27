//! The Store's migration files (ADR-0006).
//!
//! gnomon never applies DDL itself. A consumer copies the files in
//! `crates/gnomon/migrations` into its own migration chain, and its tooling
//! applies them in order, each in one transaction. The Store refuses to run
//! against any schema version but [`SCHEMA_VERSION`].

/// One migration file. Once released, it never changes.
#[derive(Debug, Clone, Copy)]
pub struct Migration {
    /// The number the file records in `gnomon.schema_version`.
    pub version: i32,
    /// The file stem, such as `0001_schema`.
    pub name: &'static str,
    /// The file's SQL, as a consumer copies it.
    pub sql: &'static str,
}

/// Every migration file, in the order a chain applies them.
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "0001_schema",
        sql: include_str!("../migrations/0001_schema.sql"),
    },
    Migration {
        version: 2,
        name: "0002_kinds",
        sql: include_str!("../migrations/0002_kinds.sql"),
    },
    Migration {
        version: 3,
        name: "0003_reads_and_writes",
        sql: include_str!("../migrations/0003_reads_and_writes.sql"),
    },
    Migration {
        version: 4,
        name: "0004_search",
        sql: include_str!("../migrations/0004_search.sql"),
    },
    Migration {
        version: 5,
        name: "0005_erasure",
        sql: include_str!("../migrations/0005_erasure.sql"),
    },
    Migration {
        version: 6,
        name: "0006_grants",
        sql: include_str!("../migrations/0006_grants.sql"),
    },
];

/// The schema version this build of the Store expects: the last file's.
pub const SCHEMA_VERSION: i32 = MIGRATIONS[MIGRATIONS.len() - 1].version;

/// The width of every embedding the Store keeps, as
/// `gnomon.embedding_dimension()` states it.
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

/// An embedding dimension outside 1 to [`EmbeddingDimension::MAX`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("embedding dimension must be a whole number from 1 to 16000, got {0:?}")]
pub struct InvalidEmbeddingDimension(String);
