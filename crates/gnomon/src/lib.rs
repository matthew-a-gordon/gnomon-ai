//! gnomon's Rust core: the Store API.
//!
//! A [`Store`] runs every call as a caller under RLS: [`Store::as_user`] for
//! an end user with verified JWT claims, and [`Store::as_worker`] for
//! background work in one Tenant and Scope. Each call on a [`Caller`] is one
//! transaction on a connection of the Store's pool, and
//! [`Caller::transaction`] starts a [`Transaction`] for several calls that
//! commit together. [`migrations`] lists the files of the schema the Store
//! expects, which a consumer copies into its own migration chain.

mod connect;
mod edge;
mod gap;
mod kind;
pub mod migrations;
mod node;
mod resolution;
mod search;
mod stage;
mod store;
mod summary;
mod transaction;

use std::fmt;

use chrono::{DateTime, Utc};
use tokio_postgres::error::SqlState;
use uuid::Uuid;

pub use connect::{ConnectOptions, DEFAULT_MAX_CONNECTIONS, DEFAULT_WAIT_TIMEOUT, Tls};
pub use edge::{
    Direction, EdgeId, EdgeVersion, EpisodeErasure, GraphErasure, NewEdge, Step, Traversal,
};
pub use gap::{Gap, GapCursor, GapPage};
pub use kind::{
    DIFFERENT_FROM_KIND, Kind, KindOf, MergeRule, NewKind, ROOT_EDGE_KIND, ROOT_NODE_KIND,
    SAME_AS_KIND,
};
pub use node::{
    AsOf, Bound, BoundChange, BoundOutcome, BoundSide, BoundStatement, Citation, EndOfValidity,
    EpisodeRetraction, NewNode, NodeEpisode, NodeId, NodeVersion, Patch, Precision, Retraction,
    Statement, StatementKind, ValidTime, VersionId, WriteOutcome, WriteStatus,
};
pub use resolution::{FullTextNameMatch, Mention, ResolutionCandidate, VectorNameMatch};
pub use search::{
    BranchRank, FullTextCandidate, SearchHit, SearchRequest, SearchResult, SearchWeights,
    SignalRank, VectorCandidate, Versioned,
};
pub use stage::{StageError, StageOutcome, StageRecord, StageStatus, StageUsage, StageWrite};
pub use store::{Caller, Store};
pub use summary::{
    Community, CommunityId, DerivedCitation, NewCommunity, NewSummary, Summary, SummaryId,
};
pub use transaction::Transaction;

/// The SQLSTATE Kind registration raises for a definition outside the
/// profile or the Ontology's rules.
pub const INVALID_KIND: &str = "GNK01";

/// The SQLSTATE Kind registration raises for a name already registered with
/// another definition.
pub const KIND_CONFLICT: &str = "GNK02";

/// The SQLSTATE a write raises when it breaks a rule of the Store: no
/// Episode, an empty Valid time, a value its Kind refuses, or a Node or
/// Episode the caller cannot see.
pub const INVALID_WRITE: &str = "GNW01";

/// The SQLSTATE a Store call raises when it names no Tenant in a
/// multi-tenant deployment.
pub const TENANT_REQUIRED: &str = "GNT01";

/// The SQLSTATE a Store call raises when the Tenant it names is not the
/// Tenant the caller's claims bind.
pub const TENANT_MISMATCH: &str = "GNT02";

/// The SQLSTATE a stage write raises when it finishes a stage record that is
/// no longer running at the attempt its claim returned, because another
/// claim took the record after its lease expired.
pub const STAGE_CLAIM_LOST: &str = "GNS01";

/// The unit of isolation: one customer's graph.
///
/// Every Store call takes an `Option<TenantId>`, and a `TenantId` converts
/// into one. A call that passes `None` uses the default Tenant in a
/// single-tenant deployment and fails with [`Error::TenantRequired`] in a
/// multi-tenant one (ADR-0005).
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
    /// `None` names no Tenant: the default Tenant in single-tenant mode.
    pub tenant: Option<TenantId>,
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
    /// Who recorded the Episode, as the deployment's `gnomon.user_id()`
    /// named them, or `None`. By default it is the end user's `sub` claim,
    /// or the user a worker was given. A person's edit is an Episode of its
    /// own, so a Version, Summary or Community leads to who stated it
    /// through its Citations.
    pub recorded_by: Option<String>,
    /// When the Episode was retracted and by whom, or `None` while it is
    /// not.
    pub retracted: Option<EpisodeRetracted>,
}

/// The Retraction of an Episode: when Retract Episode recorded it, and who.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpisodeRetracted {
    /// When Retract Episode ran: when its transaction started.
    pub recorded_at: DateTime<Utc>,
    /// As the Episode's own `recorded_by`.
    pub recorded_by: Option<String>,
}

/// Why a Store call failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The database's `gnomon.schema_version` ledger is not at the version
    /// this build expects, or there is no gnomon schema at all (`found` is
    /// `None`).
    #[error(
        "the gnomon schema is at version {found:?}, but this build of gnomon expects version \
         {expected}; apply the migration files in crates/gnomon/migrations that the database \
         lacks, in order"
    )]
    SchemaVersion { expected: i32, found: Option<i32> },
    /// `gnomon.embedding_dimension()` returned a width pgvector cannot
    /// store, so the line that states it was edited wrongly.
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
    /// The Store refused a write: it cites no Episode, its Valid time is
    /// empty, a value does not satisfy the Kind of the Node or Edge, an Edge
    /// Kind does not allow an endpoint's Kind, or a Node, Edge or Episode is
    /// not visible to the caller. The message says which.
    #[error("invalid write: {0}")]
    InvalidWrite(String),
    /// The call named no Tenant, and the deployment is multi-tenant, so it
    /// does not fall back to the default Tenant.
    #[error("Tenant required: {0}")]
    TenantRequired(String),
    /// The Tenant the call named is not the Tenant the caller's claims
    /// bind, or the claims bind none, or, in a [`Transaction`], not the
    /// transaction's Tenant. The message names both.
    #[error("Tenant mismatch: {0}")]
    TenantMismatch(String),
    /// The connection settings are invalid or contradict each other, such
    /// as a trusted CA PEM that does not parse, a connection string that does
    /// not parse, or one whose `sslmode` contradicts the Store's [`Tls`]
    /// setting.
    #[error("invalid connection settings: {0}")]
    InvalidConnection(String),
    /// A stage write finished a record that another claim has taken since
    /// this caller's lease expired, so the call's transaction rolled back,
    /// the stage's output with it. The message names the record.
    #[error("stage claim lost: {0}")]
    StageClaimLost(String),
    /// The caller lacks a privilege the call needs: Postgres raised
    /// `insufficient_privilege` (SQLSTATE 42501). A write to a Scope the
    /// caller may read and not write, a row RLS refuses, and a function the
    /// deployment has not granted, such as Erasure, arrive here.
    /// [`sqlstate`](Self::sqlstate) returns 42501, and the Postgres error is
    /// the source.
    #[error("forbidden: {}", db_message(.0))]
    Forbidden(#[source] tokio_postgres::Error),
    /// A concurrent transaction conflicted with this one: Postgres raised a
    /// serialization failure (40001) or a deadlock (40P01) and rolled this
    /// transaction back, so it saved nothing. A single call on a
    /// [`Caller`] has already run again before it returns this. Whoever
    /// holds a [`Transaction`] runs the whole transaction again themselves;
    /// [`can_run_again`](Self::can_run_again) is true.
    #[error(
        "a concurrent transaction conflicted with this one, which rolled back and saved \
         nothing; run the transaction again: {}",
        db_message(.0)
    )]
    Conflict(#[source] tokio_postgres::Error),
    /// The Store could not reach the database: no free connection within
    /// the wait timeout, no connection could be opened, or the connection
    /// was lost. `reason` says which, and `source` is the cause, such as the
    /// pool's timeout, the refused TLS certificate or the I/O error.
    #[error("{}", reason.message())]
    Unavailable {
        reason: UnavailableReason,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// [`Transaction::commit`] found that a call in the transaction failed
    /// or was cancelled, so it rolled the transaction back. Nothing in it
    /// was saved. Postgres answers a COMMIT of a failed transaction by
    /// rolling it back without an error, so the Store reports it here.
    #[error(
        "the transaction rolled back and saved nothing, because a call in it failed or was \
         cancelled"
    )]
    RolledBack,
    /// Postgres refused or failed the call for any other reason, such as a
    /// check violation or an invalid argument (SQLSTATE 22023).
    #[error(transparent)]
    Database(tokio_postgres::Error),
}

/// Why the Store could not reach the database, in [`Error::Unavailable`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum UnavailableReason {
    /// Every connection of the pool was in use for the whole wait timeout
    /// ([`ConnectOptions::wait_timeout`]). Nothing was sent.
    PoolTimeout,
    /// A new connection could not be opened: the server could not be
    /// reached, refused the login, or failed the TLS handshake. Nothing was
    /// sent.
    Connect,
    /// The connection was lost before COMMIT was sent. The transaction did
    /// not commit, so nothing was saved.
    Lost,
    /// The connection was lost after COMMIT was sent and before it was
    /// answered. Whether the transaction committed is unknown, and nothing
    /// runs again automatically.
    LostDuringCommit,
}

impl UnavailableReason {
    fn message(self) -> &'static str {
        match self {
            UnavailableReason::PoolTimeout => {
                "the database is unavailable: no connection was free within the wait timeout"
            }
            UnavailableReason::Connect => {
                "the database is unavailable: a connection could not be opened"
            }
            UnavailableReason::Lost => {
                "the database is unavailable: the connection was lost, and the transaction saved \
                 nothing"
            }
            UnavailableReason::LostDuringCommit => {
                "the connection was lost during COMMIT, so whether the transaction committed is \
                 unknown; nothing ran again"
            }
        }
    }
}

/// Postgres's own message for a database error, or the error itself.
fn db_message(e: &tokio_postgres::Error) -> String {
    match e.as_db_error() {
        Some(db) => db.message().to_owned(),
        None => e.to_string(),
    }
}

impl Error {
    /// The SQLSTATE Postgres returned, if this is a database error or a
    /// refused connection that carries one.
    pub fn sqlstate(&self) -> Option<&SqlState> {
        match self {
            Error::Database(e) | Error::Forbidden(e) | Error::Conflict(e) => e.code(),
            Error::Unavailable { source, .. } => source
                .downcast_ref::<tokio_postgres::Error>()
                .and_then(tokio_postgres::Error::code),
            Error::SchemaVersion { .. }
            | Error::InvalidEmbeddingDimension(_)
            | Error::InvalidConnection(_)
            | Error::InvalidKind(_)
            | Error::KindConflict(_)
            | Error::InvalidWrite(_)
            | Error::TenantRequired(_)
            | Error::TenantMismatch(_)
            | Error::StageClaimLost(_)
            | Error::RolledBack => None,
        }
    }

    /// Whether the call or the transaction saved nothing and running it
    /// again as it was can succeed: after a [`Conflict`](Error::Conflict),
    /// and after an [`Unavailable`](Error::Unavailable) because no
    /// connection was free or the connection was lost before COMMIT.
    ///
    /// It is false after the connection was lost during COMMIT, because the
    /// first run may have committed, and after a connection could not be
    /// opened, because a refused login or certificate fails the same way
    /// every time.
    pub fn can_run_again(&self) -> bool {
        match self {
            Error::Conflict(_) => true,
            Error::Unavailable { reason, .. } => matches!(
                reason,
                UnavailableReason::PoolTimeout | UnavailableReason::Lost
            ),
            _ => false,
        }
    }

    /// Whether this is a serialization failure or a deadlock.
    pub(crate) fn is_conflict(&self) -> bool {
        matches!(self, Error::Conflict(_))
    }

    /// Whether the connection was lost under the call.
    pub(crate) fn is_lost_connection(&self) -> bool {
        matches!(
            self,
            Error::Unavailable {
                reason: UnavailableReason::Lost | UnavailableReason::LostDuringCommit,
                ..
            }
        )
    }

    /// The one conversion every statement's error goes through, COMMIT's
    /// included (`during_commit`): a lost connection, a refused privilege, a
    /// conflict and the SQLSTATEs the Store's functions raise for a refused
    /// Kind, write or Tenant each become their variant.
    pub(crate) fn from_statement(e: tokio_postgres::Error, during_commit: bool) -> Error {
        if is_lost(&e) {
            return Error::Unavailable {
                reason: if during_commit {
                    UnavailableReason::LostDuringCommit
                } else {
                    UnavailableReason::Lost
                },
                source: Box::new(e),
            };
        }
        let message = e.as_db_error().map(|d| d.message().to_owned());
        let code = e.code().cloned();
        match (code.as_ref().map(SqlState::code), message) {
            (Some(INVALID_KIND), Some(m)) => Error::InvalidKind(m),
            (Some(KIND_CONFLICT), Some(m)) => Error::KindConflict(m),
            (Some(INVALID_WRITE), Some(m)) => Error::InvalidWrite(m),
            (Some(TENANT_REQUIRED), Some(m)) => Error::TenantRequired(m),
            (Some(TENANT_MISMATCH), Some(m)) => Error::TenantMismatch(m),
            (Some(STAGE_CLAIM_LOST), Some(m)) => Error::StageClaimLost(m),
            _ if code == Some(SqlState::INSUFFICIENT_PRIVILEGE) => Error::Forbidden(e),
            _ if code == Some(SqlState::T_R_SERIALIZATION_FAILURE)
                || code == Some(SqlState::T_R_DEADLOCK_DETECTED) =>
            {
                Error::Conflict(e)
            }
            _ => Error::Database(e),
        }
    }
}

impl From<tokio_postgres::Error> for Error {
    fn from(e: tokio_postgres::Error) -> Error {
        Error::from_statement(e, false)
    }
}

/// Whether an error means the connection is gone: the client saw it close,
/// an I/O error broke it, or the server ended the session (a connection
/// exception, class 08, or an operator or crash shutdown, 57P01 to 57P03).
fn is_lost(e: &tokio_postgres::Error) -> bool {
    if e.is_closed() {
        return true;
    }
    if let Some(code) = e.code() {
        // Class 08 has no single SqlState constant to compare with.
        return code.code().starts_with("08")
            || *code == SqlState::ADMIN_SHUTDOWN
            || *code == SqlState::CRASH_SHUTDOWN
            || *code == SqlState::CANNOT_CONNECT_NOW;
    }
    std::error::Error::source(e).is_some_and(|s| s.is::<std::io::Error>())
}
