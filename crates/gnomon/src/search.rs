//! The full-text and vector branches of Search (ADR-0010).
//!
//! [`Caller::search_full_text`](crate::Caller::search_full_text) returns the
//! Versions whose searchable text matches the query text, ranked by BM25 and
//! filtered by Tenant, Scope and the As-of instants. The text it searches is
//! what each Kind marks as searchable: the properties a Kind names, a Node's
//! name and Aliases, and an Edge Version's Sentence once the Pipeline has
//! stored one (ADR-0013). Migration 0016 says how the token table behind it
//! is kept.
//!
//! [`Caller::search_vector`](crate::Caller::search_vector) returns the
//! Versions whose stored embeddings are nearest a query embedding, by cosine
//! distance at full precision, filtered the same way. The Pipeline stores
//! each embedding after the write commits, with
//! [`Caller::store_embedding`](crate::Caller::store_embedding). Migration 0017
//! says how the search stays exact over the rows the caller can see.

use crate::{EdgeId, NodeId, Scope, VersionId};

/// What a Version is a Version of: a Node or an Edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Versioned {
    Node(NodeId),
    Edge(EdgeId),
}

/// A Version the full-text branch found.
#[derive(Debug, Clone, PartialEq)]
pub struct FullTextCandidate {
    pub version: VersionId,
    pub of: Versioned,
    pub scope: Scope,
    /// The Version's BM25 score for the query, from the word counts of the
    /// caller's allowed Scopes only.
    pub score: f64,
    /// Its place in the branch's list, from 1, by score and then by Version
    /// ID.
    pub rank: i64,
}

/// A Version the vector branch found.
#[derive(Debug, Clone, PartialEq)]
pub struct VectorCandidate {
    pub version: VersionId,
    pub of: Versioned,
    pub scope: Scope,
    /// The cosine distance from the query to the Version's stored
    /// embedding, at full precision: 0 for the same direction, up to 2.
    pub distance: f64,
    /// Its place in the branch's list, from 1, by distance and then by
    /// Version ID.
    pub rank: i64,
}
