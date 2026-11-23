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

use crate::{EpisodeId, MergeRule, Scope, TenantId};

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

/// How exactly a source states a Valid-time bound (ADR-0008). `unknown`, the
/// coarsest, is [`Bound::Unknown`], so it is not listed here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Precision {
    Year,
    Month,
    Day,
    Instant,
}

impl Precision {
    pub(crate) fn as_sql(self) -> &'static str {
        match self {
            Precision::Year => "year",
            Precision::Month => "month",
            Precision::Day => "day",
            Precision::Instant => "instant",
        }
    }

    pub(crate) fn from_sql(s: &str) -> Option<Precision> {
        Some(match s {
            "year" => Precision::Year,
            "month" => Precision::Month,
            "day" => Precision::Day,
            "instant" => Precision::Instant,
            _ => return None,
        })
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
    /// A period the source names, such as "March 2024". A write may give any
    /// instant inside the period. The Store keeps the start of the period in
    /// UTC, and a read returns that start.
    Period {
        at: DateTime<Utc>,
        precision: Precision,
    },
}

impl Bound {
    /// The bound's instant and precision as the Store's SQL takes them: an
    /// unknown bound is `(None, None)`.
    pub(crate) fn parts(self) -> (Option<DateTime<Utc>>, Option<&'static str>) {
        match self {
            Bound::Unknown => (None, None),
            Bound::At(t) => (Some(t), Some(Precision::Instant.as_sql())),
            Bound::Period { at, precision } => (Some(at), Some(precision.as_sql())),
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

    /// From `from`, a bound of any precision, on, with an open end.
    pub fn since_bound(from: Bound) -> ValidTime {
        ValidTime {
            from,
            to: Bound::Unknown,
        }
    }

    /// From `from` on, with an open end.
    pub fn since(from: DateTime<Utc>) -> ValidTime {
        ValidTime {
            from: Bound::At(from),
            to: Bound::Unknown,
        }
    }

    /// The bounds, which the Store's SQL takes as an instant and a precision.
    pub(crate) fn bounds(self) -> (Bound, Bound) {
        (self.from, self.to)
    }
}

/// A Node to create: a stable identity of a Kind, in one Scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewNode {
    /// `None` names no Tenant: the default Tenant in single-tenant mode.
    pub tenant: Option<TenantId>,
    pub scope: Scope,
    /// The root Kind [`ROOT_NODE_KIND`](crate::ROOT_NODE_KIND) or a Node Kind
    /// the Tenant registered.
    pub kind: String,
}

/// What one Episode states about one Node or Edge over one Valid time: a
/// value for each of some properties.
#[derive(Debug, Clone, PartialEq)]
pub struct Patch {
    /// The Episode the write cites. It must be in the Scope of the Node or
    /// Edge.
    pub episode: EpisodeId,
    pub valid_time: ValidTime,
    /// `{property: value}`. A JSON null ends the property over the Valid
    /// time. Every other value must satisfy the Kind's rule for its
    /// property. Every Node also has `name`, a string, and `aliases`, a list
    /// of strings, and a write to a Node states at least one value. A write
    /// to an Edge may state none: it always states that the Edge holds over
    /// the Valid time, through the built-in `holds`, which the patch never
    /// names.
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
    /// The write's Recorded time: when its transaction started. Every write
    /// in one transaction shares it.
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
    /// Whether the read's valid-at instant falls inside the period of one of
    /// this Version's imprecise bounds, so that the Version may not yet have
    /// held (ADR-0008).
    pub uncertain: bool,
    /// The hash of the Version's text. Versions with the same text share it,
    /// so a Refinement shares the earlier Version's and needs no new
    /// embedding.
    pub text_hash: String,
    /// The rule that chose this Version's values. A change to a Kind's rule
    /// leaves it as it was (ADR-0007).
    pub merge_rule: MergeRule,
}

/// Which bound of a Version's Valid time a statement is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundSide {
    Start,
    End,
}

/// What one Episode states about one bound of one current Version: a bound
/// the Version left unknown or coarse, or one that contradicts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundStatement {
    /// The Episode the statement cites. It must be in the Node's Scope.
    pub episode: EpisodeId,
    /// The current Version whose bound is stated.
    pub version: VersionId,
    pub side: BoundSide,
    /// The stated bound, which must be [`Bound::At`] or [`Bound::Period`].
    pub bound: Bound,
}

/// How the Store recorded a [`BoundStatement`] (ADR-0008).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundChange {
    /// The bound was unknown, or the statement narrows its precision within
    /// the same period. The earlier Version was incomplete, not wrong.
    Refinement,
    /// The bound falls outside the earlier period, or is coarser than it:
    /// the earlier Version was wrong about its Valid time.
    Correction,
    /// The statement gives the bound the Version already has.
    Unchanged,
}

/// The result of one [`BoundStatement`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundOutcome {
    pub change: BoundChange,
    /// The statement's Recorded time: when its transaction started.
    pub recorded_at: DateTime<Utc>,
    /// The Versions the statement opened.
    pub opened: Vec<VersionId>,
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

/// What an Episode states when a fact stopped being true: from an instant
/// on, what a current Version said no longer holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EndOfValidity {
    /// The Episode the statement cites. It must be in the Scope of the Node
    /// or Edge.
    pub episode: EpisodeId,
    /// When the fact stopped being true, which must be [`Bound::At`] or
    /// [`Bound::Period`]. A period is stored as its start, and the ended
    /// Version's end then reads as an instant.
    pub at: Bound,
}

/// What an Episode states when a current Version was never true.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retraction {
    /// The Episode the statement cites. It must be in the Scope of the Node
    /// or Edge.
    pub episode: EpisodeId,
    /// The current Version that was never true.
    pub version: VersionId,
}

/// The kind of a statement about a Node or an Edge (GLOSSARY.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementKind {
    Correction,
    EndOfValidity,
    Retraction,
}

/// One Correction, End of validity or Retraction, with the Episode that made
/// it. A statement that lost on Authority is held and listed all the same.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    pub kind: StatementKind,
    pub episode: EpisodeId,
    /// The Version an End of validity ended or a Retraction retracted, as it
    /// stood. A Correction names none.
    pub version: Option<VersionId>,
    /// The Valid time the statement is about: the span a Correction states,
    /// from the stated instant to the end of the Version for an End of
    /// validity, and the whole Valid time of the Version for a Retraction.
    pub valid_time: ValidTime,
    /// When gnomon recorded the statement: when its transaction started.
    pub recorded_at: DateTime<Utc>,
}
