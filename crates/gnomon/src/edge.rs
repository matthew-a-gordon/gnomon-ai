//! Edges, traversal and the batch As-of read (ADR-0007, ADR-0008,
//! ADR-0013).
//!
//! An Edge is a stable identity for a directed relationship between two
//! Nodes, of an Edge Kind, in one Scope. A write to an Edge goes through the
//! same Contribution and Version model as a write to a Node, and also states
//! that the Edge holds over the write's Valid time, so a write may state no
//! property value at all. Each Edge Version may carry a Sentence, which the
//! Pipeline stores after the Version commits.

use std::fmt;

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::{MergeRule, NodeId, NodeVersion, Scope, TenantId, ValidTime, VersionId};

/// The ID of an Edge, a UUID v7.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EdgeId(pub Uuid);

impl fmt::Display for EdgeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// An Edge to create: a stable identity of an Edge Kind, in one Scope, from
/// a source Node to a target Node.
///
/// Both Nodes must be in the Edge's Tenant and visible to the caller, in any
/// of the caller's Scopes, and the Edge Kind must allow their Kinds at each
/// end. The Store checks nothing about the Nodes' Valid times (ADR-0008).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewEdge {
    /// `None` names no Tenant: the default Tenant in single-tenant mode.
    pub tenant: Option<TenantId>,
    pub scope: Scope,
    /// The root Kind [`ROOT_EDGE_KIND`](crate::ROOT_EDGE_KIND) or an Edge
    /// Kind the Tenant registered.
    pub kind: String,
    pub source: NodeId,
    pub target: NodeId,
}

/// One Version of an Edge.
#[derive(Debug, Clone, PartialEq)]
pub struct EdgeVersion {
    pub id: VersionId,
    pub edge: EdgeId,
    pub scope: Scope,
    pub kind: String,
    pub source: NodeId,
    pub target: NodeId,
    /// `{property: value}` of the Edge Kind's properties, with no nulls. It
    /// may be empty: an Edge holds over its Valid time whether or not any
    /// write stated a property value.
    pub values: Map<String, Value>,
    pub valid_time: ValidTime,
    /// When gnomon began to hold this Version as its belief.
    pub recorded_from: DateTime<Utc>,
    /// When it stopped, or `None` while it is current.
    pub recorded_to: Option<DateTime<Utc>>,
    /// Whether the read's valid-at instant falls inside the period of this
    /// Version's imprecise start bound (ADR-0008).
    pub uncertain: bool,
    /// The hash of the Version's values.
    pub text_hash: String,
    /// The rule that chose this Version's values (ADR-0007).
    pub merge_rule: MergeRule,
    /// What this Version says in plain language, or `None` until the
    /// Pipeline stores it after the Version commits (ADR-0013).
    pub sentence: Option<String>,
}

/// Which Edges of a Node a traversal follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Edges whose source is the Node.
    Outgoing,
    /// Edges whose target is the Node.
    Incoming,
    /// Both. An Edge from the Node to itself is followed once, outgoing.
    Both,
}

impl Direction {
    pub(crate) fn as_sql(self) -> &'static str {
        match self {
            Direction::Outgoing => "out",
            Direction::Incoming => "in",
            Direction::Both => "both",
        }
    }

    pub(crate) fn from_sql(s: &str) -> Direction {
        match s {
            "out" => Direction::Outgoing,
            "in" => Direction::Incoming,
            other => panic!("gnomon.traverse returned direction {other:?}"),
        }
    }
}

/// What a traversal follows from a Node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Traversal {
    pub direction: Direction,
    /// Only Edges of these Edge Kinds or of Kinds below them. Empty follows
    /// every Edge.
    pub kinds: Vec<String>,
}

impl Traversal {
    /// Every Edge from or to the Node.
    pub fn all() -> Traversal {
        Traversal {
            direction: Direction::Both,
            kinds: Vec::new(),
        }
    }
}

/// One Edge a traversal followed, with the Node at its other end.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    /// [`Direction::Outgoing`] or [`Direction::Incoming`]: how the Edge was
    /// followed from the Node.
    pub direction: Direction,
    /// The Edge's Version at the traversal's As-of instants.
    pub edge: EdgeVersion,
    /// The Node at the Edge's other end.
    pub neighbour: NodeId,
    /// The neighbour's Version at the same instants, or `None` where it held
    /// none, as a Node with no Versions does. Temporal referential integrity
    /// is not enforced, so an Edge may hold where its neighbour does not
    /// (ADR-0008).
    pub neighbour_version: Option<NodeVersion>,
}

/// What Erase Episode removed and recomputed (ADR-0003, ADR-0007).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpisodeErasure {
    /// When the Erasure ran: when the call's transaction started. History is
    /// rewritten up to this instant as if the Episode had never existed.
    pub erased_at: DateTime<Utc>,
    /// The Nodes the Episode touched whose Versions were recomputed from the
    /// Contributions that remain, in order of ID.
    pub recomputed_nodes: Vec<NodeId>,
    /// The Edges the Episode touched whose Versions were recomputed, in order
    /// of ID.
    pub recomputed_edges: Vec<EdgeId>,
    /// The Nodes erased because nothing was left of them and no Edge joined
    /// them, in order of ID.
    pub erased_nodes: Vec<NodeId>,
    /// The Edges erased because nothing was left of them, in order of ID.
    pub erased_edges: Vec<EdgeId>,
}
