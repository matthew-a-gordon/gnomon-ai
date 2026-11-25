//! The gaps `maintain` repairs (migration 0021).
//!
//! For one Scope, the Store lists what the Pipeline has still to write: the
//! Edge Versions without a Sentence, the search texts without an embedding
//! (Node Versions' texts, current names and Aliases, Sentences, Summaries and
//! Communities), and the Nodes and Communities without a current Summary. A
//! Sentence is held per Edge Version, so a Refinement or an Erasure that
//! recomputes an Edge Version leaves that Version with no Sentence, and it
//! is listed again.
//!
//! The listing is paged by key. Its order is fixed: by kind, in the order of
//! the variants of [`Gap`], then by ID and text hash. A [`GapCursor`], the
//! page token, is the key of the last gap of a page, and the next page
//! starts after it. A gap's key never changes while it is listed, so a gap
//! that is there on every call is listed exactly once however the set
//! changes between pages.

use uuid::Uuid;

use crate::{CommunityId, EdgeId, NodeId, SummaryId, VersionId};

/// One thing `maintain` repairs in a Scope.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Gap {
    /// An Edge Version, open or closed, with no Sentence.
    Sentence { version: VersionId, edge: EdgeId },
    /// An Edge Version with a Sentence and no embedding of it. The
    /// embedding is stored with `text_hash`, the Sentence's hash.
    SentenceEmbedding {
        version: VersionId,
        edge: EdgeId,
        text_hash: String,
        sentence: String,
    },
    /// A Node Version, open or closed, with no embedding of its text, whose
    /// hash is `text_hash`.
    NodeEmbedding {
        version: VersionId,
        node: NodeId,
        text_hash: String,
    },
    /// A current name or Alias of a Node with no embedding, for Resolution.
    NameEmbedding {
        node: NodeId,
        text_hash: String,
        name: String,
    },
    /// A Summary, open or closed, with no embedding.
    SummaryEmbedding { summary: SummaryId, node: NodeId },
    /// A Community, open or closed, with no embedding.
    CommunityEmbedding { community: CommunityId },
    /// A Node of the Scope with a current Version and no current Summary
    /// in the Scope.
    Summary { node: NodeId },
    /// A closed Community that lost its replacement: an Erasure removed
    /// every Community written when it closed, and no current Community
    /// holds any of its Nodes. The Pipeline writes a Community of them
    /// again.
    Community { community: CommunityId },
}

impl Gap {
    /// The page token that starts a page after this gap.
    pub fn cursor(&self) -> GapCursor {
        let (kind, id, hash) = match self {
            Gap::Sentence { version, .. } => ("sentence", version.0, None),
            Gap::SentenceEmbedding {
                version, text_hash, ..
            } => ("sentence_embedding", version.0, Some(text_hash)),
            Gap::NodeEmbedding {
                version, text_hash, ..
            } => ("node_embedding", version.0, Some(text_hash)),
            Gap::NameEmbedding {
                node, text_hash, ..
            } => ("name_embedding", node.0, Some(text_hash)),
            Gap::SummaryEmbedding { summary, .. } => ("summary_embedding", summary.0, None),
            Gap::CommunityEmbedding { community } => ("community_embedding", community.0, None),
            Gap::Summary { node } => ("summary", node.0, None),
            Gap::Community { community } => ("community", community.0, None),
        };
        GapCursor {
            kind,
            id,
            text_hash: hash.cloned(),
        }
    }
}

/// The page token of the gap listing: the key of a gap, after which the
/// next page starts. It is the gap's kind, ID and text hash.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GapCursor {
    pub(crate) kind: &'static str,
    pub(crate) id: Uuid,
    pub(crate) text_hash: Option<String>,
}

/// One page of the gap listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GapPage {
    pub gaps: Vec<Gap>,
    /// The token of the next page, when this page is full. A full last page
    /// has one too, and the page after it is empty.
    pub next: Option<GapCursor>,
}
