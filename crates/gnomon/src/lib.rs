//! gnomon's Rust core: the Store API.
//!
//! A [`Store`] runs every call as a caller under RLS: [`Store::as_user`] for
//! an end user with verified JWT claims, and [`Store::as_worker`] for
//! background work in one Tenant and Scope. [`migrations`] exports the schema
//! the Store expects into a consumer's migration chain.

mod kind;
pub mod migrations;
mod store;

use std::fmt;

use chrono::{DateTime, Utc};
use tokio_postgres::error::SqlState;
use uuid::Uuid;

pub use kind::{Kind, KindOf, NewKind, ROOT_EDGE_KIND, ROOT_NODE_KIND};
pub use store::{Caller, Store};

/// The SQLSTATE Kind registration raises for a definition outside the
/// profile or the Ontology's rules.
pub const INVALID_KIND: &str = "GNK01";

/// The SQLSTATE Kind registration raises for a name already registered with
/// another definition.
pub const KIND_CONFLICT: &str = "GNK02";

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
    /// When gnomon recorded the Episode.
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
    /// `gnomon.embedding_dimension()` returned a width pgvector cannot
    /// store, so the schema was not exported by gnomon.
    #[error("the gnomon schema reports embedding dimension {0}, which is not 1 to 16000")]
    InvalidEmbeddingDimension(i32),
    /// Kind registration refused the definition: a keyword outside the JSON
    /// Schema profile, an unknown parent or endpoint Kind, or another rule of
    /// the Ontology. The message says which.
    #[error("invalid Kind: {0}")]
    InvalidKind(String),
    /// The Kind's name is already registered in the Tenant with another
    /// definition.
    #[error("Kind conflict: {0}")]
    KindConflict(String),
    /// Postgres refused or failed the call. RLS refusals and a missing grant
    /// arrive here.
    #[error(transparent)]
    Database(#[from] tokio_postgres::Error),
}

impl Error {
    /// The SQLSTATE Postgres returned, if this is a database error.
    pub fn sqlstate(&self) -> Option<&SqlState> {
        match self {
            Error::Database(e) => e.code(),
            Error::SchemaVersion { .. }
            | Error::InvalidEmbeddingDimension(_)
            | Error::InvalidKind(_)
            | Error::KindConflict(_) => None,
        }
    }

    /// Turns the SQLSTATEs Kind registration raises into their variants.
    pub(crate) fn from_kind_registration(e: tokio_postgres::Error) -> Error {
        let message = e.as_db_error().map(|d| d.message().to_owned());
        match (e.code().map(SqlState::code), message) {
            (Some(INVALID_KIND), Some(m)) => Error::InvalidKind(m),
            (Some(KIND_CONFLICT), Some(m)) => Error::KindConflict(m),
            _ => Error::Database(e),
        }
    }
}
