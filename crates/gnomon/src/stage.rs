//! Stage records (ADR-0011, migration 0014).
//!
//! The Pipeline's `add_episode` commits each stage's output together with the
//! stage's record, so a retry resumes at the first unfinished stage. A
//! [`Caller::recording`](crate::Caller::recording) Caller writes stage records
//! in the transaction of its next Store call, so the record and the output
//! commit or fail together. The optional server queue claims due records
//! with [`Caller::claim_stage`](crate::Caller::claim_stage).
//!
//! A stage record holds no model output and no text (ADR-0011). Its stage
//! name, model names and error code are identifiers, which the database
//! refuses unless they match a fixed pattern with no spaces and a fixed
//! maximum length of at most 128 characters. An error is a code and an optional number, never a
//! message, because a provider's or a parser's message often quotes its
//! input or the model's output.

use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::{EpisodeId, Scope, TenantId};

/// Where a stage record stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StageStatus {
    /// Written, and not yet claimed.
    Pending,
    /// Claimed, with a lease until [`StageRecord::lease_until`].
    Running,
    /// Failed, and due again at [`StageRecord::retry_at`].
    Retry,
    /// Finished, in the transaction that committed the stage's output.
    Succeeded,
    /// Failed for the last time. It stays visible as failed.
    Failed,
}

impl StageStatus {
    pub(crate) fn as_sql(self) -> &'static str {
        match self {
            StageStatus::Pending => "pending",
            StageStatus::Running => "running",
            StageStatus::Retry => "retry",
            StageStatus::Succeeded => "succeeded",
            StageStatus::Failed => "failed",
        }
    }

    pub(crate) fn from_sql(s: &str) -> StageStatus {
        match s {
            "pending" => StageStatus::Pending,
            "running" => StageStatus::Running,
            "retry" => StageStatus::Retry,
            "succeeded" => StageStatus::Succeeded,
            "failed" => StageStatus::Failed,
            other => unreachable!("gnomon.stage_status has no value {other:?}"),
        }
    }
}

/// The last error of a stage: a code and an optional number, and no text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageError {
    /// An identifier such as `rate_limited` or `output.invalid_json`: a
    /// lowercase letter, then up to 62 lowercase letters, digits, `_` or `.`.
    pub code: String,
    /// A number the error carries, such as the provider's HTTP status.
    pub status: Option<i32>,
}

/// One model's usage within a stage record.
#[derive(Debug, Clone, PartialEq)]
pub struct StageUsage {
    /// The model's name as the provider gives it, such as
    /// `claude-sonnet-5-5`: a letter or digit, then up to 127 letters,
    /// digits or `.`, `_`, `:`, `/`, `@`, `-`.
    pub model: String,
    pub calls: i32,
    pub input_tokens: i64,
    pub output_tokens: i64,
    /// The cost in US dollars the provider reported, or `None` when it
    /// reported none. Writing usage adds it to the record's total, which is
    /// the sum of the costs reported: a call without one adds nothing, so a
    /// total is `None` only when no call of the model reported a cost.
    pub cost_usd: Option<f64>,
}

/// A stage record, with its usage counts.
#[derive(Debug, Clone, PartialEq)]
pub struct StageRecord {
    pub tenant: TenantId,
    /// The Episode's Scope.
    pub scope: Scope,
    pub episode: EpisodeId,
    /// An identifier such as `graph`: a lowercase letter, then up to 62
    /// lowercase letters, digits or `_`.
    pub stage: String,
    pub status: StageStatus,
    /// How many times the record has been claimed.
    pub attempts: i32,
    pub last_error: Option<StageError>,
    /// When a [`StageStatus::Retry`] record is due, by the database's clock.
    pub retry_at: Option<DateTime<Utc>>,
    /// When a [`StageStatus::Running`] record's lease ends, by the
    /// database's clock. After it the record may be claimed again.
    pub lease_until: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// By model, in order of name.
    pub usage: Vec<StageUsage>,
}

/// How a claimed stage ended.
#[derive(Debug, Clone, PartialEq)]
pub enum StageOutcome {
    /// The stage's output commits in the same transaction.
    Succeeded,
    /// The stage failed and is due again `after` from now, by the database's
    /// clock.
    Retry { error: StageError, after: Duration },
    /// The stage failed for the last time.
    Failed { error: StageError },
}

/// A write of a stage record.
#[derive(Debug, Clone, PartialEq)]
pub enum StageWrite {
    /// A pending record of `stage`, unless the Episode has a record of that
    /// stage already, which stays as it is.
    Pending { episode: EpisodeId, stage: String },
    /// Finishes a record this caller claimed, at the attempt its claim
    /// returned, and adds the usage of the attempt. It fails with
    /// [`Error::StageClaimLost`](crate::Error::StageClaimLost), and the
    /// transaction rolls back, once another claim has taken the record.
    Finish {
        episode: EpisodeId,
        stage: String,
        attempt: i32,
        outcome: StageOutcome,
        usage: Vec<StageUsage>,
    },
}

impl StageWrite {
    pub fn pending(episode: EpisodeId, stage: impl Into<String>) -> StageWrite {
        StageWrite::Pending {
            episode,
            stage: stage.into(),
        }
    }

    /// Finishes the record `claim` returned.
    pub fn finish(
        claim: &StageRecord,
        outcome: StageOutcome,
        usage: Vec<StageUsage>,
    ) -> StageWrite {
        StageWrite::Finish {
            episode: claim.episode,
            stage: claim.stage.clone(),
            attempt: claim.attempts,
            outcome,
            usage,
        }
    }
}
