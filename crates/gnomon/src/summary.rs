//! Summaries and Communities (ADR-0014, ADR-0015, `0003_reads_and_writes.sql`).
//!
//! The Pipeline writes a [`Summary`] of a Node and a [`Community`] of Nodes
//! with its Summary, each with Citations to the Episodes it was derived from.
//! The Store holds the Pipeline's text and makes no model call.
//!
//! Each belongs to one Scope and cites only Episodes of that Scope. A Summary
//! may describe a Node of another Scope the writer sees, so a Node cited from
//! two Scopes has one Summary in each. A Community holds only Nodes of its own
//! Scope. Each row carries a Recorded time and the valid-at instant it was
//! written for, `as_of_valid`. Regenerating one closes the old row and adds a
//! new one, so a read at an earlier known-at instant still finds the old row
//! (ADR-0015). Erasure removes every row, closed or open, that cites an erased
//! Episode, and every Summary of an erased Node and Community that holds one
//! (ADR-0003).

use chrono::{DateTime, Utc};

use crate::{EpisodeId, NodeId, Scope};

/// The ID of a Summary row, a UUID v7.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SummaryId(pub uuid::Uuid);

/// The ID of a Community row, a UUID v7.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CommunityId(pub uuid::Uuid);

/// A Citation of a Summary or Community: an Episode of its Scope it was
/// derived from, optionally naming a property and a place in the Episode.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DerivedCitation {
    pub episode: EpisodeId,
    pub property: Option<String>,
    pub place: Option<String>,
}

impl DerivedCitation {
    /// A Citation of `episode` that names no property and no place.
    pub fn of(episode: EpisodeId) -> DerivedCitation {
        DerivedCitation {
            episode,
            property: None,
            place: None,
        }
    }
}

/// A Summary to write: prose describing one Node, in one of the writer's
/// Scopes, citing at least one Episode of that Scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSummary {
    pub scope: Scope,
    pub node: NodeId,
    pub content: String,
    /// The valid-at instant the Pipeline wrote it for.
    pub as_of_valid: DateTime<Utc>,
    pub citations: Vec<DerivedCitation>,
}

/// A Summary as the Store holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub id: SummaryId,
    pub scope: Scope,
    pub node: NodeId,
    pub content: String,
    pub as_of_valid: DateTime<Utc>,
    /// When gnomon began to hold it.
    pub recorded_from: DateTime<Utc>,
    /// When a regeneration closed it, or `None` while it is current.
    pub recorded_to: Option<DateTime<Utc>>,
    /// In order of Episode ID, then property and place.
    pub citations: Vec<DerivedCitation>,
}

/// A Community to write: Nodes of one of the writer's Scopes with a
/// generated Summary of the group, citing at least one Episode of that Scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewCommunity {
    pub scope: Scope,
    /// The current Communities of the Scope this one regenerates, which the
    /// write closes.
    pub replaces: Vec<CommunityId>,
    /// At least one Node, each in `scope`.
    pub members: Vec<NodeId>,
    /// The generated Summary of the group.
    pub content: String,
    pub as_of_valid: DateTime<Utc>,
    pub citations: Vec<DerivedCitation>,
}

/// A Community as the Store holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Community {
    pub id: CommunityId,
    pub scope: Scope,
    /// In order of Node ID.
    pub members: Vec<NodeId>,
    pub content: String,
    pub as_of_valid: DateTime<Utc>,
    pub recorded_from: DateTime<Utc>,
    pub recorded_to: Option<DateTime<Utc>>,
    /// In order of Episode ID, then property and place.
    pub citations: Vec<DerivedCitation>,
}
