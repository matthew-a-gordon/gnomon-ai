//! Nodes, their Versions and the Citations behind them (ADR-0007, ADR-0008).
//!
//! A write states a patch of property values for one Node, citing one
//! Episode, over one Valid time. The Store keeps each value as a
//! Contribution and computes the Node's Versions from them. A read returns
//! the Version that held at a valid-at instant according to what gnomon
//! believed at a known-at instant.

use std::collections::BTreeMap;
use std::fmt;

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::{EpisodeId, Scope, TenantId};

/// The ID of a Node, a UUID v7.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub Uuid);

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The ID of a Version, a UUID v7.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VersionId(pub Uuid);

impl fmt::Display for VersionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// One bound of a Valid time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bound {
    /// The source states no bound. An unknown start reads as open, and so
    /// does an unknown end.
    Unknown,
    /// An instant, the bound's start in UTC.
    At(DateTime<Utc>),
}

impl Bound {
    fn instant(self) -> Option<DateTime<Utc>> {
        match self {
            Bound::Unknown => None,
            Bound::At(t) => Some(t),
        }
    }
}

/// The interval during which a Version was true in the world, half-open:
/// `[from, to)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValidTime {
    pub from: Bound,
    pub to: Bound,
}

impl ValidTime {
    /// The Valid time of a source that states none: an unknown start and an
    /// open end.
    pub const UNKNOWN: ValidTime = ValidTime {
        from: Bound::Unknown,
        to: Bound::Unknown,
    };

    /// `[from, to)`.
    pub fn between(from: DateTime<Utc>, to: DateTime<Utc>) -> ValidTime {
        ValidTime {
            from: Bound::At(from),
            to: Bound::At(to),
        }
    }

    /// From `from` on, with an open end.
    pub fn since(from: DateTime<Utc>) -> ValidTime {
        ValidTime {
            from: Bound::At(from),
            to: Bound::Unknown,
        }
    }

    pub(crate) fn sql(self) -> (Option<DateTime<Utc>>, Option<DateTime<Utc>>) {
        (self.from.instant(), self.to.instant())
    }
}

/// A Node to create: a stable identity of a Kind, in one Scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewNode {
    pub tenant: TenantId,
    pub scope: Scope,
    /// The root Kind [`ROOT_NODE_KIND`](crate::ROOT_NODE_KIND) or a Node Kind
    /// the Tenant registered.
    pub kind: String,
}

/// What one Episode states about one Node over one Valid time: a value for
/// each of some properties.
#[derive(Debug, Clone, PartialEq)]
pub struct Patch {
    /// The Episode the write cites. It must be in the Node's Scope.
    pub episode: EpisodeId,
    pub valid_time: ValidTime,
    /// `{property: value}`. A JSON null ends the property over the Valid
    /// time. Every other value must satisfy the Kind's rule for its
    /// property. Every Node also has `name`, a string, and `aliases`, a list
    /// of strings.
    pub values: Map<String, Value>,
    /// Where in the Episode each value was stated, for the properties whose
    /// place the consumer knows, in the consumer's own terms. The Citations
    /// carry it.
    pub places: BTreeMap<String, String>,
}

impl Patch {
    /// A patch with no places.
    pub fn new(episode: EpisodeId, valid_time: ValidTime, values: Map<String, Value>) -> Patch {
        Patch {
            episode,
            valid_time,
            values,
            places: BTreeMap::new(),
        }
    }
}

/// Whether a write changed the Node's Versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteStatus {
    /// The write opened or closed at least one Version.
    Written,
    /// The Node's Versions are as they were. The write's Episode is still
    /// cited from every Version whose value it states.
    Unchanged,
}

/// The result of one write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteOutcome {
    pub status: WriteStatus,
    /// The write's Recorded time: its transaction time.
    pub recorded_at: DateTime<Utc>,
    /// The Versions the write opened.
    pub opened: Vec<VersionId>,
}

/// The instants an As-of read reads at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AsOf {
    /// When in the world.
    pub valid_at: DateTime<Utc>,
    /// According to what gnomon believed when. `None` reads the current
    /// belief.
    pub known_at: Option<DateTime<Utc>>,
}

impl AsOf {
    /// What gnomon believes now about `valid_at`.
    pub fn current(valid_at: DateTime<Utc>) -> AsOf {
        AsOf {
            valid_at,
            known_at: None,
        }
    }

    /// What gnomon believed at `known_at` about `valid_at`.
    pub fn at(valid_at: DateTime<Utc>, known_at: DateTime<Utc>) -> AsOf {
        AsOf {
            valid_at,
            known_at: Some(known_at),
        }
    }
}

/// One Version of a Node.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeVersion {
    pub id: VersionId,
    pub node: NodeId,
    pub scope: Scope,
    pub kind: String,
    /// `{property: value}`, with no nulls.
    pub values: Map<String, Value>,
    pub valid_time: ValidTime,
    /// When gnomon began to hold this Version as its belief.
    pub recorded_from: DateTime<Utc>,
    /// When it stopped, or `None` while it is current.
    pub recorded_to: Option<DateTime<Utc>>,
}

/// The link from a Version to an Episode it was derived from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Citation {
    pub episode: EpisodeId,
    /// The property whose value the Episode stated.
    pub property: String,
    /// Where in the Episode, if the write said.
    pub place: Option<String>,
    /// When gnomon recorded the Citation.
    pub recorded_at: DateTime<Utc>,
    /// When gnomon recorded the Episode.
    pub episode_recorded_at: DateTime<Utc>,
}
