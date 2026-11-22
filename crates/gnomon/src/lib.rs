//! gnomon's Rust core: the Store API.
//!
//! A [`Store`] runs every call as a caller under RLS: [`Store::as_user`] for
//! an end user with verified JWT claims, and [`Store::as_worker`] for
//! background work in one Tenant and Scope. [`migrations`] exports the schema
//! the Store expects into a consumer's migration chain.

pub mod migrations;
mod store;

use std::fmt;

use chrono::{DateTime, Utc};
use tokio_postgres::error::SqlState;
use uuid::Uuid;

pub use store::{Caller, Store};

/// The unit of isolation: one customer's graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TenantId(pub Uuid);

impl fmt::Display for TenantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// An opaque key below the Tenant that partitions its graph. gnomon gives it
/// no meaning of its own.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Scope(String);

impl Scope {
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The ID of an Episode, a UUID v7.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EpisodeId(pub Uuid);

impl fmt::Display for EpisodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// An Episode to record: an immutable source, such as a message or a
/// document, that Versions will be derived from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewEpisode {
    pub tenant: TenantId,
    pub scope: Scope,
    pub content: String,
    /// When the source was produced, not when gnomon recorded it.
    pub reference_time: DateTime<Utc>,
    /// The rank the consumer gives this Episode, if any.
    pub authority: Option<i32>,
}

/// A recorded Episode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Episode {
    pub id: EpisodeId,
    pub tenant: TenantId,
    pub scope: Scope,
    pub content: String,
    pub reference_time: DateTime<Utc>,
    pub authority: Option<i32>,
    /// When gnomon recorded the Episode: its transaction's time.
    pub recorded_at: DateTime<Utc>,
}

/// Why a Store call failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The database's `gnomon.schema_version` ledger is not at the version
    /// this build expects, or there is no gnomon schema at all (`found` is
    /// `None`).
    #[error(
        "the gnomon schema is at version {found:?}, but this build of gnomon expects version \
         {expected}; apply the migrations from `gnomon migrations export`"
    )]
    SchemaVersion { expected: i32, found: Option<i32> },
    /// Postgres refused or failed the call. RLS refusals arrive here.
    #[error(transparent)]
    Database(#[from] tokio_postgres::Error),
}

impl Error {
    /// The SQLSTATE Postgres returned, if this is a database error.
    pub fn sqlstate(&self) -> Option<&SqlState> {
        match self {
            Error::Database(e) => e.code(),
            Error::SchemaVersion { .. } => None,
        }
    }
}
