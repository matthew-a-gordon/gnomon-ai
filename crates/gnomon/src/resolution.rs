//! Resolution (ADR-0010, ADR-0012, `0004_search.sql`): matching a mention to the
//! Nodes the graph already has.
//!
//! [`Caller::resolve`](crate::Caller::resolve) searches the current names and
//! Aliases of the Nodes the caller can see, by full text and, when the
//! mention carries an embedding, by vector, and returns candidate Nodes,
//! ranked, each with the name or Alias it matched. A name is current while it
//! belongs to a Version of the Store's current belief, whatever its Valid
//! time, unless the mention names a valid-at instant. A Node is found by a
//! new name through full text as soon as the write that gave it commits,
//! before the Pipeline embeds the name.
//!
//! Resolution only proposes. It never merges two Nodes, and the finding that
//! two are one thing, or are not, is a `same_as` or `different_from` Edge the
//! Pipeline writes (ADR-0012).

use jiff::Timestamp;

use crate::{NodeId, Scope, VersionId};

/// A mention to resolve: its text, an embedding of it, or both.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mention<'a> {
    /// Searched by full text against each name and Alias. Empty text
    /// matches nothing.
    pub text: &'a str,
    /// Searched by vector against the stored embeddings of the names and
    /// Aliases.
    pub embedding: Option<&'a [f32]>,
    /// When set, only the names and Aliases of Versions valid at this
    /// instant match. When `None`, those of every Valid time do.
    pub valid_at: Option<Timestamp>,
}

impl<'a> Mention<'a> {
    /// A mention searched by its text alone, over every Valid time.
    pub fn text(text: &'a str) -> Mention<'a> {
        Mention {
            text,
            embedding: None,
            valid_at: None,
        }
    }

    /// A mention searched by its embedding alone, over every Valid time.
    pub fn embedding(embedding: &'a [f32]) -> Mention<'a> {
        Mention {
            text: "",
            embedding: Some(embedding),
            valid_at: None,
        }
    }

    /// The same mention, also searched by `embedding`.
    pub fn with_embedding(self, embedding: &'a [f32]) -> Mention<'a> {
        Mention {
            embedding: Some(embedding),
            ..self
        }
    }

    /// The same mention, matching only names valid at `at`.
    pub fn valid_at(self, at: Timestamp) -> Mention<'a> {
        Mention {
            valid_at: Some(at),
            ..self
        }
    }
}

/// The name or Alias the full-text half matched for a candidate.
#[derive(Debug, Clone, PartialEq)]
pub struct FullTextNameMatch {
    /// The Version the name belongs to.
    pub version: VersionId,
    pub name: String,
    pub alias: bool,
    /// The name's BM25 score for the mention, from the word counts of the
    /// caller's allowed Scopes only and the name's own length.
    pub score: f64,
    /// The candidate's place in the half's list, from 1.
    pub rank: i64,
}

/// The name or Alias the vector half matched for a candidate.
#[derive(Debug, Clone, PartialEq)]
pub struct VectorNameMatch {
    /// The Version the name belongs to.
    pub version: VersionId,
    pub name: String,
    pub alias: bool,
    /// The cosine distance from the mention's embedding to the name's, at
    /// full precision.
    pub distance: f64,
    /// The candidate's place in the half's list, from 1.
    pub rank: i64,
}

/// A Node a Resolution proposes for a mention.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolutionCandidate {
    pub node: NodeId,
    pub scope: Scope,
    /// The name or Alias that matched, from the half that ranked the Node
    /// higher, the full-text half's on a tie.
    pub name: String,
    /// Whether [`name`](Self::name) is an Alias rather than the Node's name.
    pub alias: bool,
    /// The reciprocal rank fusion score of the two halves, with k = 60.
    pub score: f64,
    /// Its place in the fused list, from 1, by score and then by Node ID.
    pub rank: i64,
    /// What the full-text half found, if it found the Node.
    pub full_text: Option<FullTextNameMatch>,
    /// What the vector half found, if it found the Node.
    pub vector: Option<VectorNameMatch>,
}
