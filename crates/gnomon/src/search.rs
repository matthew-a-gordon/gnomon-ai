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
//!
//! [`Caller::search`](crate::Caller::search) is the one Search call a
//! consumer runs. It fuses the vector, full-text and neighbourhood branches
//! over Versions, Summaries and Communities by reciprocal rank fusion, then
//! reorders the fused candidates by two graph signals, mentions and
//! connectedness, counted over the rows the caller can see. A signal never
//! adds a candidate. Each result reports where each branch and signal ranked
//! it. Migration 0020 says how.

use crate::{AsOf, CommunityId, EdgeId, NodeId, Scope, SummaryId, VersionId};

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

/// What a Search result is: a Version, a Summary or a Community.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SearchHit {
    /// A Version of a Node or an Edge.
    Version {
        version: VersionId,
        of: Versioned,
    },
    /// A Summary, and the Node it describes.
    Summary {
        summary: SummaryId,
        node: NodeId,
    },
    Community(CommunityId),
}

/// The weights reciprocal rank fusion gives each ranked list of a Search:
/// each list that ranks a result at `r` adds `weight / (60 + r)` to its
/// score. A branch weighted 0 adds no result, though the vector and
/// full-text branches still give the neighbourhood its seeds when the
/// request names none. A signal weighted 0 adds nothing to a score, and its
/// count is still reported.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SearchWeights {
    pub vector: f64,
    pub full_text: f64,
    pub neighbourhood: f64,
    /// The graph signal that counts the Episodes citing a result.
    pub mentions: f64,
    /// The graph signal that counts the Edges joining the Nodes a result is
    /// about.
    pub connectedness: f64,
}

impl Default for SearchWeights {
    /// 1 for each branch, and 0.5 for each graph signal, which leaves the
    /// signals on at a lower weight.
    fn default() -> SearchWeights {
        SearchWeights {
            vector: 1.0,
            full_text: 1.0,
            neighbourhood: 1.0,
            mentions: 0.5,
            connectedness: 0.5,
        }
    }
}

/// What one Search matches (ADR-0010): text, an embedding, or both, As-of a
/// valid-at and a known-at instant.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchRequest<'a> {
    /// Searched by full text. Empty text matches nothing.
    pub text: &'a str,
    /// Searched by vector. `None` runs no vector branch.
    pub embedding: Option<&'a [f32]>,
    pub as_of: AsOf,
    /// The Nodes the neighbourhood branch expands from. Empty takes the ten
    /// Nodes the vector and full-text branches rank best together.
    pub seeds: &'a [NodeId],
    /// How far the neighbourhood branch expands: 1 or 2.
    pub depth: i32,
    pub weights: SearchWeights,
}

impl<'a> SearchRequest<'a> {
    /// A Search by text alone at `as_of`, with seeds from the top Node hits,
    /// depth 1 and the default weights.
    pub fn text(text: &'a str, as_of: AsOf) -> SearchRequest<'a> {
        SearchRequest {
            text,
            embedding: None,
            as_of,
            seeds: &[],
            depth: 1,
            weights: SearchWeights::default(),
        }
    }

    /// The same Search, also by `embedding`.
    pub fn with_embedding(self, embedding: &'a [f32]) -> SearchRequest<'a> {
        SearchRequest {
            embedding: Some(embedding),
            ..self
        }
    }

    /// The same Search, expanding the neighbourhood from `seeds`.
    pub fn seeds(self, seeds: &'a [NodeId]) -> SearchRequest<'a> {
        SearchRequest { seeds, ..self }
    }

    /// The same Search, expanding the neighbourhood to `depth`.
    pub fn depth(self, depth: i32) -> SearchRequest<'a> {
        SearchRequest { depth, ..self }
    }

    /// The same Search, with these weights.
    pub fn weights(self, weights: SearchWeights) -> SearchRequest<'a> {
        SearchRequest { weights, ..self }
    }
}

/// Where a branch ranked a result.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BranchRank<T> {
    /// The branch's own measure: a cosine distance, a BM25 score, or a depth.
    pub value: T,
    /// The result's place in the branch's list, from 1.
    pub rank: i64,
}

/// What a graph signal counted for a result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignalRank {
    /// The Episodes or Edges it counted, among the rows the caller sees at
    /// the As-of instants.
    pub count: i64,
    /// The result's place in the signal's list, from 1, with equal counts
    /// sharing a place, or `None` when the count is 0.
    pub rank: Option<i64>,
}

/// One result of a Search.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchResult {
    pub hit: SearchHit,
    pub scope: Scope,
    /// The reciprocal rank fusion score, with k = 60 and the query's weights.
    pub score: f64,
    /// Its place in the fused list, from 1, by score and then by ID.
    pub rank: i64,
    /// The cosine distance and rank the vector branch gave it, if it found
    /// it.
    pub vector: Option<BranchRank<f64>>,
    /// The BM25 score and rank the full-text branch gave it.
    pub full_text: Option<BranchRank<f64>>,
    /// The depth, 1 or 2, at which the neighbourhood branch reached it, and
    /// its rank.
    pub neighbourhood: Option<BranchRank<i32>>,
    pub mentions: SignalRank,
    pub connectedness: SignalRank,
}
