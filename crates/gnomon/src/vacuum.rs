//! The vacuum call after Erasure (ADR-0003).
//!
//! An Erasure is complete at commit, but copies of the erased bytes stay in
//! dead tuples and index pages until the tables it touched are vacuumed.
//! `VACUUM` cannot run inside a transaction block, so the call is not a Store
//! function: [`Store::vacuum_erasure`](crate::Store::vacuum_erasure) and
//! [`Store::vacuum_erasures`](crate::Store::vacuum_erasures) send each
//! `VACUUM` as a simple query of one statement, outside any transaction. A
//! transaction-mode pooler runs it as a transaction of its own, and it sets
//! nothing that outlives it. Migration 0013 says the rest.
//!
//! A table an Erasure touched is named in its result, as
//! [`EpisodeErasure::touched_tables`](crate::EpisodeErasure::touched_tables).
//! It names tables, never rows, so it gives the caller no ID from a Scope it
//! cannot see.

use chrono::{DateTime, Utc};

/// How the vacuum call vacuums each table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VacuumMode {
    /// `VACUUM (INDEX_CLEANUP ON, ANALYZE)`. It removes the dead tuples an
    /// Erasure left and their index entries, and refreshes the planner's
    /// statistics, without blocking reads or writes. The space it frees on
    /// a page is not overwritten, so the bytes stay on the page until
    /// Postgres reuses the space.
    Plain,
    /// `VACUUM (FULL, ANALYZE)`. It writes each table and its indexes anew
    /// and drops the old files, so no page of the table holds the erased
    /// bytes afterwards. Each table is locked against reads and writes
    /// while it is rewritten.
    Full,
}

impl VacuumMode {
    /// The options of the `VACUUM` statement.
    pub(crate) fn options(self) -> &'static str {
        match self {
            VacuumMode::Plain => "index_cleanup on, analyze",
            VacuumMode::Full => "full, analyze",
        }
    }
}

/// What the vacuum call did. It is returned once every table has been
/// vacuumed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VacuumReport {
    /// The tables vacuumed, schema-qualified, in order of name.
    pub tables: Vec<String>,
    pub mode: VacuumMode,
    /// The server's clock when the last table's `VACUUM` had finished.
    pub finished_at: DateTime<Utc>,
}
