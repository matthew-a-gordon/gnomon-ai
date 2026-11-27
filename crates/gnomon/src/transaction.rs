//! A transaction on one pooled connection, and every Store call inside it.
//!
//! [`Caller::transaction`](crate::Caller::transaction) starts one, and each
//! single call on a [`Caller`](crate::Caller) runs in one of its own. The
//! transaction is
//!
//! ```sql
//! begin; set local role <authenticated | gnomon_worker>;   -- one round trip
//! <set request.jwt.claims locally, then gnomon.call_tenant(<the Tenant or null>)>;
//! <the calls>;
//! commit;
//! ```
//!
//! so a pooled server connection never carries one caller's role or claims
//! into another caller's call. Every statement is sent unnamed through
//! `query_typed`, which works on every transaction-mode pooler at its
//! default settings (ADR-0001).

use jiff::Timestamp;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::time::Duration;

use tokio_postgres::Row;
use tokio_postgres::types::{ToSql, Type};

use crate::timestamptz::Timestamptz;
use crate::{
    AsOf, Bound, BoundChange, BoundOutcome, BoundSide, BoundStatement, BranchRank, Citation,
    Community, CommunityId, DerivedCitation, Direction, EdgeId, EdgeVersion, EndOfValidity,
    Episode, EpisodeErasure, EpisodeId, EpisodeRetracted, EpisodeRetraction, Error,
    FullTextCandidate, FullTextNameMatch, Gap, GapCursor, GapPage, GraphErasure, Kind, KindOf,
    Mention, MergeRule, NewCommunity, NewEdge, NewEpisode, NewKind, NewNode, NewSummary,
    NodeEpisode, NodeId, NodeVersion, Patch, Precision, ResolutionCandidate, Retraction, Scope,
    SearchHit, SearchRequest, SearchResult, SignalRank, StageError, StageOutcome, StageRecord,
    StageStatus, StageUsage, StageWrite, StatementKind, Step, Summary, SummaryId, TenantId,
    Traversal, ValidTime, VectorCandidate, VectorNameMatch, VersionId, Versioned, WriteOutcome,
    WriteStatus,
};

/// The statement that sets a call's claims and checks its Tenant. The
/// materialized CTE sets the claims before the outer select reads them, so
/// the check adds no round trip.
pub(crate) const CLAIMS: &str = "with claims as materialized (select set_config('request.jwt.claims', $1, true)) \
     select gnomon.call_tenant($2) from claims";

/// One connection taken from the Store's pool.
///
/// While a transaction may be open on it, from the moment `begin` is sent
/// until COMMIT or ROLLBACK is answered, dropping it closes the connection
/// instead of returning it to the pool. The server then rolls the
/// transaction back. So a call cancelled at any point, or a transaction
/// dropped without a commit, leaves no role, claims or open transaction on a
/// connection that a later call takes.
pub(crate) struct Conn {
    object: Option<deadpool_postgres::Object>,
    open: bool,
}

impl Conn {
    pub(crate) fn new(object: deadpool_postgres::Object) -> Conn {
        Conn {
            object: Some(object),
            open: false,
        }
    }

    fn client(&self) -> &tokio_postgres::Client {
        self.object.as_ref().expect("held until the Conn drops")
    }

    /// Whether a transaction may be open on the connection.
    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    /// Opens a transaction as `role`, sending `begin` and `set local role`
    /// in one round trip.
    pub(crate) async fn begin(&mut self, role: &str) -> Result<(), Error> {
        self.open = true;
        self.client()
            .batch_execute(&format!("begin; set local role {role}"))
            .await?;
        Ok(())
    }

    /// Runs one unnamed statement.
    pub(crate) async fn query(
        &self,
        sql: &str,
        params: &[(&(dyn ToSql + Sync), Type)],
    ) -> Result<Vec<Row>, tokio_postgres::Error> {
        self.client().query_typed(sql, params).await
    }

    /// Commits. A connection lost during COMMIT leaves the outcome unknown,
    /// and the error says so. After any failed COMMIT the connection is
    /// closed when this drops.
    pub(crate) async fn commit(&mut self) -> Result<(), Error> {
        // A connection the client already saw close cannot send COMMIT, so
        // that transaction is known not to have committed.
        let sent = !self.client().is_closed();
        match self.client().batch_execute("commit").await {
            Ok(()) => {
                self.open = false;
                Ok(())
            }
            Err(e) => Err(Error::from_statement(e, sent)),
        }
    }

    /// Rolls back, so the connection can go back to the pool. If this
    /// fails, the connection is closed when this drops.
    pub(crate) async fn rollback(&mut self) -> Result<(), Error> {
        self.client().batch_execute("rollback").await?;
        self.open = false;
        Ok(())
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        if self.open
            && let Some(object) = self.object.take()
        {
            // Out of the pool for good: dropping the client closes the
            // connection.
            drop(deadpool_postgres::Object::take(object));
        }
    }
}

/// One transaction of one caller in one Tenant, on a connection of its own
/// from the Store's pool.
///
/// [`Caller::transaction`](crate::Caller::transaction) starts it. Every Store
/// call is a method on it, as on the [`Caller`](crate::Caller), without the
/// Tenant argument, and nothing it writes is saved until
/// [`commit`](Self::commit) succeeds. A stage writes its output and its
/// stage records (ADR-0011) in one transaction:
///
/// ```no_run
/// # async fn stage(store: &gnomon::Store, tenant: gnomon::TenantId,
/// #     version: gnomon::VersionId, hash: &str, embedding: &[f32],
/// #     finish: gnomon::StageWrite) -> Result<(), gnomon::Error> {
/// let worker = store.as_worker(tenant, gnomon::Scope::new("ingest"), None);
/// let mut tx = worker.transaction(tenant).await?;
/// tx.store_embedding(version, hash, embedding).await?;
/// tx.write_stages(&[finish]).await?;
/// tx.commit().await?;
/// # Ok(()) }
/// ```
///
/// - The transaction holds its pooled connection until it commits, rolls
///   back or drops. A single call made on the Store meanwhile takes another
///   connection, so with [`max_connections`](crate::ConnectOptions::max_connections)
///   of them open no other call can start until one is free.
/// - No model call runs while a transaction is open. A stage calls its
///   model first, then opens the transaction that writes the output, so a
///   connection is held only for the short transaction (ADR-0016).
/// - Dropping it without a commit saves nothing. The Store closes its
///   connection rather than return it to the pool with a transaction open.
/// - Once a call in it fails, or is cancelled before it returns, Postgres
///   has rolled the transaction back or may have done only part of the
///   call, so [`commit`](Self::commit) rolls back and fails with
///   [`Error::RolledBack`].
/// - Calls in a transaction never run again by themselves. After an
///   [`Error::Conflict`], the whole transaction rolled back and can be run
///   again from the start, by whoever holds it; [`Error::can_run_again`]
///   says when that is possible.
pub struct Transaction {
    conn: Conn,
    tenant: TenantId,
    failed: bool,
}

impl std::fmt::Debug for Transaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transaction")
            .field("tenant", &self.tenant)
            .field("failed", &self.failed)
            .finish_non_exhaustive()
    }
}

impl Transaction {
    pub(crate) fn new(conn: Conn, tenant: TenantId) -> Transaction {
        Transaction {
            conn,
            tenant,
            failed: false,
        }
    }

    /// The transaction's Tenant, as `gnomon.call_tenant` checked it when the
    /// transaction started: the Tenant it was started for, or the default
    /// Tenant in a single-tenant deployment.
    pub fn tenant(&self) -> TenantId {
        self.tenant
    }

    /// Commits everything the transaction's calls wrote.
    ///
    /// It fails with [`Error::RolledBack`], and saves nothing, when a call in
    /// the transaction failed or was cancelled. The Store then closes the
    /// connection instead of sending ROLLBACK, which would wait behind a
    /// cancelled statement the server is still running; the server rolls the
    /// transaction back. It fails with
    /// [`Error::Unavailable`] and
    /// [`UnavailableReason::LostDuringCommit`](crate::UnavailableReason::LostDuringCommit)
    /// when the connection is lost before COMMIT is answered: the
    /// transaction may or may not have committed.
    pub async fn commit(mut self) -> Result<(), Error> {
        if self.failed {
            // Dropping the connection with its transaction open closes it.
            return Err(Error::RolledBack);
        }
        self.conn.commit().await
    }

    /// Rolls the transaction back, saving nothing, and returns its
    /// connection to the pool. After a call in it failed or was cancelled,
    /// the Store closes the connection instead, as [`commit`](Self::commit)
    /// does.
    pub async fn rollback(mut self) -> Result<(), Error> {
        if self.failed {
            return Ok(());
        }
        self.conn.rollback().await
    }

    /// Commits after a call that succeeded, or rolls back after one that
    /// failed and returns its error.
    pub(crate) async fn finish<T>(mut self, done: Result<T, Error>) -> Result<T, Error> {
        match done {
            Ok(value) => self.commit().await.map(|()| value),
            Err(e) => {
                let _ = self.conn.rollback().await;
                Err(e)
            }
        }
    }

    /// Marks the start of a call. Until [`done`](Self::done) the transaction
    /// counts as failed, so a call that returns an error or is cancelled
    /// leaves it failed. Returns what to restore.
    fn call(&mut self) -> bool {
        std::mem::replace(&mut self.failed, true)
    }

    /// Marks the end of a call that succeeded.
    fn done<T>(&mut self, before: bool, value: T) -> Result<T, Error> {
        self.failed = before;
        Ok(value)
    }

    /// Runs one statement, converting its error.
    async fn query(
        &self,
        sql: &str,
        params: &[(&(dyn ToSql + Sync), Type)],
    ) -> Result<Vec<Row>, Error> {
        Ok(self.conn.query(sql, params).await?)
    }

    /// Refuses a call that names another Tenant than the transaction's.
    fn in_tenant(&self, named: Option<TenantId>) -> Result<(), Error> {
        match named {
            Some(t) if t != self.tenant => Err(Error::TenantMismatch(format!(
                "the call names Tenant {t}, but the transaction is in Tenant {}",
                self.tenant
            ))),
            _ => Ok(()),
        }
    }
}

/// Episodes and Kinds.
impl Transaction {
    /// As [`Caller::record_episode`](crate::Caller::record_episode), in this
    /// transaction. An Episode that names another Tenant than the
    /// transaction's fails with [`Error::TenantMismatch`].
    pub async fn record_episode(&mut self, episode: &NewEpisode) -> Result<EpisodeId, Error> {
        let call = self.call();
        self.in_tenant(episode.tenant)?;
        let rows = self
            .query(
                "select gnomon.record_episode($1, $2, $3, $4, $5)",
                &[
                    (&self.tenant.0, Type::UUID),
                    (&episode.scope.as_str(), Type::TEXT),
                    (&episode.content, Type::TEXT),
                    (&Timestamptz(episode.reference_time), Type::TIMESTAMPTZ),
                    (&episode.authority, Type::INT4),
                ],
            )
            .await?;
        self.done(call, EpisodeId(rows[0].get(0)))
    }

    /// As [`Caller::read_episode`](crate::Caller::read_episode), in this
    /// transaction.
    pub async fn read_episode(&mut self, id: EpisodeId) -> Result<Option<Episode>, Error> {
        let call = self.call();
        let rows = self
            .query(
                "select tenant_id, scope, id, content, reference_time, authority, recorded_at,
                        recorded_by, retracted_at, retracted_by
                 from gnomon.read_episode($1, $2)",
                &[(&self.tenant.0, Type::UUID), (&id.0, Type::UUID)],
            )
            .await?;
        let episode = rows.first().map(episode_record).transpose()?;
        self.done(call, episode)
    }

    /// As [`Caller::register_kind`](crate::Caller::register_kind), in this
    /// transaction. A Kind that names another Tenant than the transaction's
    /// fails with [`Error::TenantMismatch`].
    pub async fn register_kind(&mut self, kind: &NewKind) -> Result<Kind, Error> {
        let call = self.call();
        self.in_tenant(kind.tenant)?;
        let (source, target) = match &kind.of {
            KindOf::Node => (None, None),
            KindOf::Edge { source, target } => (Some(source), Some(target)),
        };
        let kind_of = kind.of.sql_name();
        self.query(
            "select gnomon.register_kind($1, $2, $3, $4, $5, $6, $7, $8)",
            &[
                (&self.tenant.0, Type::UUID),
                (&kind.name, Type::TEXT),
                (&kind_of, Type::TEXT),
                (&kind.parent, Type::TEXT),
                (&kind.properties, Type::JSONB),
                (&kind.search, Type::TEXT_ARRAY),
                (&source, Type::TEXT_ARRAY),
                (&target, Type::TEXT_ARRAY),
            ],
        )
        .await?;
        let registered = read_kind(self, &kind.name).await?;
        self.done(
            call,
            registered.expect("a Kind just registered is visible to its registrant"),
        )
    }

    /// As [`Caller::set_merge_rule`](crate::Caller::set_merge_rule), in this
    /// transaction.
    pub async fn set_merge_rule(&mut self, kind: &str, rule: MergeRule) -> Result<(), Error> {
        let call = self.call();
        self.query(
            "select gnomon.set_merge_rule($1, $2, $3)",
            &[
                (&self.tenant.0, Type::UUID),
                (&kind, Type::TEXT),
                (&rule.as_sql(), Type::TEXT),
            ],
        )
        .await?;
        self.done(call, ())
    }

    /// As [`Caller::merge_rule`](crate::Caller::merge_rule), in this
    /// transaction.
    pub async fn merge_rule(&mut self, kind: &str) -> Result<MergeRule, Error> {
        let call = self.call();
        let rows = self
            .query(
                "select gnomon.kind_merge_rule($1, $2)",
                &[(&self.tenant.0, Type::UUID), (&kind, Type::TEXT)],
            )
            .await?;
        self.done(call, MergeRule::from_sql(rows[0].get(0)))
    }

    /// As [`Caller::read_kind`](crate::Caller::read_kind), in this
    /// transaction.
    pub async fn read_kind(&mut self, name: &str) -> Result<Option<Kind>, Error> {
        let call = self.call();
        let kind = read_kind(self, name).await?;
        self.done(call, kind)
    }
}

/// One write of a batch: to a new Node or Edge, or to an existing one. Every
/// write of a batch is in the batch's one Tenant.
enum Target<'a> {
    NewNode(&'a NewNode),
    Node(NodeId),
    NewEdge(&'a NewEdge),
    Edge(EdgeId),
}

/// Graph writes and statements.
impl Transaction {
    /// As [`Caller::create_node`](crate::Caller::create_node), in this
    /// transaction. A Node that names another Tenant than the transaction's
    /// fails with [`Error::TenantMismatch`].
    pub async fn create_node(
        &mut self,
        node: &NewNode,
        patch: &Patch,
    ) -> Result<(NodeId, WriteOutcome), Error> {
        let (id, outcome) = self
            .writes(node.tenant, &[(Target::NewNode(node), patch)], false)
            .await?
            .remove(0);
        match id {
            Versioned::Node(node) => Ok((node, outcome)),
            Versioned::Edge(_) => unreachable!("a new Node's write is about the Node"),
        }
    }

    /// As [`Caller::write_node`](crate::Caller::write_node), in this
    /// transaction.
    pub async fn write_node(&mut self, node: NodeId, patch: &Patch) -> Result<WriteOutcome, Error> {
        let mut done = self
            .writes(None, &[(Target::Node(node), patch)], false)
            .await?;
        Ok(done.remove(0).1)
    }

    /// As [`Caller::write_nodes`](crate::Caller::write_nodes), in this
    /// transaction.
    pub async fn write_nodes(
        &mut self,
        writes: &[(NodeId, Patch)],
    ) -> Result<Vec<WriteOutcome>, Error> {
        let targets: Vec<_> = writes
            .iter()
            .map(|(node, patch)| (Target::Node(*node), patch))
            .collect();
        Ok(self
            .writes(None, &targets, false)
            .await?
            .into_iter()
            .map(|(_, outcome)| outcome)
            .collect())
    }

    /// As [`Caller::create_edge`](crate::Caller::create_edge), in this
    /// transaction. An Edge that names another Tenant than the transaction's
    /// fails with [`Error::TenantMismatch`].
    pub async fn create_edge(
        &mut self,
        edge: &NewEdge,
        patch: &Patch,
    ) -> Result<(EdgeId, WriteOutcome), Error> {
        let (id, outcome) = self
            .writes(edge.tenant, &[(Target::NewEdge(edge), patch)], false)
            .await?
            .remove(0);
        match id {
            Versioned::Edge(edge) => Ok((edge, outcome)),
            Versioned::Node(_) => unreachable!("a new Edge's write is about the Edge"),
        }
    }

    /// As [`Caller::write_edge`](crate::Caller::write_edge), in this
    /// transaction.
    pub async fn write_edge(&mut self, edge: EdgeId, patch: &Patch) -> Result<WriteOutcome, Error> {
        let mut done = self
            .writes(None, &[(Target::Edge(edge), patch)], false)
            .await?;
        Ok(done.remove(0).1)
    }

    /// As [`Caller::write_edges`](crate::Caller::write_edges), in this
    /// transaction.
    pub async fn write_edges(
        &mut self,
        writes: &[(EdgeId, Patch)],
    ) -> Result<Vec<WriteOutcome>, Error> {
        let targets: Vec<_> = writes
            .iter()
            .map(|(edge, patch)| (Target::Edge(*edge), patch))
            .collect();
        Ok(self
            .writes(None, &targets, false)
            .await?
            .into_iter()
            .map(|(_, outcome)| outcome)
            .collect())
    }

    /// As [`Caller::correct_node`](crate::Caller::correct_node), in this
    /// transaction.
    pub async fn correct_node(
        &mut self,
        node: NodeId,
        patch: &Patch,
    ) -> Result<WriteOutcome, Error> {
        let mut done = self
            .writes(None, &[(Target::Node(node), patch)], true)
            .await?;
        Ok(done.remove(0).1)
    }

    /// As [`Caller::correct_edge`](crate::Caller::correct_edge), in this
    /// transaction.
    pub async fn correct_edge(
        &mut self,
        edge: EdgeId,
        patch: &Patch,
    ) -> Result<WriteOutcome, Error> {
        let mut done = self
            .writes(None, &[(Target::Edge(edge), patch)], true)
            .await?;
        Ok(done.remove(0).1)
    }

    /// Runs a batch of writes, in order.
    async fn writes(
        &mut self,
        named: Option<TenantId>,
        writes: &[(Target<'_>, &Patch)],
        correct: bool,
    ) -> Result<Vec<(Versioned, WriteOutcome)>, Error> {
        let call = self.call();
        self.in_tenant(named)?;
        let tenant = self.tenant;
        let mut done = Vec::with_capacity(writes.len());
        for (target, patch) in writes {
            let of = match target {
                Target::Node(node) => Versioned::Node(*node),
                Target::Edge(edge) => Versioned::Edge(*edge),
                Target::NewNode(new) => {
                    let rows = self
                        .query(
                            "select gnomon.create_node($1, $2, $3)",
                            &[
                                (&tenant.0, Type::UUID),
                                (&new.scope.as_str(), Type::TEXT),
                                (&new.kind, Type::TEXT),
                            ],
                        )
                        .await?;
                    Versioned::Node(NodeId(rows[0].get(0)))
                }
                Target::NewEdge(new) => {
                    let rows = self
                        .query(
                            "select gnomon.create_edge($1, $2, $3, $4, $5)",
                            &[
                                (&tenant.0, Type::UUID),
                                (&new.scope.as_str(), Type::TEXT),
                                (&new.kind, Type::TEXT),
                                (&new.source.0, Type::UUID),
                                (&new.target.0, Type::UUID),
                            ],
                        )
                        .await?;
                    Versioned::Edge(EdgeId(rows[0].get(0)))
                }
            };
            done.push((of, write_patch(self, of, patch, correct).await?));
        }
        self.done(call, done)
    }

    /// As [`Caller::state_bound`](crate::Caller::state_bound), in this
    /// transaction.
    pub async fn state_bound(
        &mut self,
        node: NodeId,
        statement: &BoundStatement,
    ) -> Result<BoundOutcome, Error> {
        self.state_bound_of(Versioned::Node(node), statement).await
    }

    /// As [`Caller::state_edge_bound`](crate::Caller::state_edge_bound), in
    /// this transaction.
    pub async fn state_edge_bound(
        &mut self,
        edge: EdgeId,
        statement: &BoundStatement,
    ) -> Result<BoundOutcome, Error> {
        self.state_bound_of(Versioned::Edge(edge), statement).await
    }

    async fn state_bound_of(
        &mut self,
        of: Versioned,
        statement: &BoundStatement,
    ) -> Result<BoundOutcome, Error> {
        let call = self.call();
        let id = of.id();
        let (at, precision) = statement.bound.parts();
        let side = match statement.side {
            BoundSide::Start => "from",
            BoundSide::End => "to",
        };
        let rows = self
            .query(
                of.state_bound_sql(),
                &[
                    (&self.tenant.0, Type::UUID),
                    (&id, Type::UUID),
                    (&statement.episode.0, Type::UUID),
                    (&statement.version.0, Type::UUID),
                    (&side, Type::TEXT),
                    (&at.map(Timestamptz), Type::TIMESTAMPTZ),
                    (&precision, Type::TEXT),
                ],
            )
            .await?;
        let r = &rows[0];
        let change = match r.get::<_, &str>(0) {
            "refinement" => BoundChange::Refinement,
            "correction" => BoundChange::Correction,
            "unchanged" => BoundChange::Unchanged,
            other => panic!("gnomon.state_bound returned change {other:?}"),
        };
        let opened: Vec<uuid::Uuid> = r.get(2);
        let outcome = BoundOutcome {
            change,
            recorded_at: time(r, 1)?,
            opened: opened.into_iter().map(VersionId).collect(),
        };
        self.done(call, outcome)
    }

    /// As [`Caller::end_node_validity`](crate::Caller::end_node_validity), in
    /// this transaction.
    pub async fn end_node_validity(
        &mut self,
        node: NodeId,
        end: &EndOfValidity,
    ) -> Result<WriteOutcome, Error> {
        self.write_ending(Versioned::Node(node), Ending::End(end))
            .await
    }

    /// As [`Caller::end_edge_validity`](crate::Caller::end_edge_validity), in
    /// this transaction.
    pub async fn end_edge_validity(
        &mut self,
        edge: EdgeId,
        end: &EndOfValidity,
    ) -> Result<WriteOutcome, Error> {
        self.write_ending(Versioned::Edge(edge), Ending::End(end))
            .await
    }

    /// As [`Caller::retract_node_version`](crate::Caller::retract_node_version),
    /// in this transaction.
    pub async fn retract_node_version(
        &mut self,
        node: NodeId,
        retraction: &Retraction,
    ) -> Result<WriteOutcome, Error> {
        self.write_ending(Versioned::Node(node), Ending::Retract(retraction))
            .await
    }

    /// As [`Caller::retract_edge_version`](crate::Caller::retract_edge_version),
    /// in this transaction.
    pub async fn retract_edge_version(
        &mut self,
        edge: EdgeId,
        retraction: &Retraction,
    ) -> Result<WriteOutcome, Error> {
        self.write_ending(Versioned::Edge(edge), Ending::Retract(retraction))
            .await
    }

    async fn write_ending(
        &mut self,
        of: Versioned,
        ending: Ending<'_>,
    ) -> Result<WriteOutcome, Error> {
        let call = self.call();
        let id = of.id();
        let is_edge = of.is_edge();
        let rows = match ending {
            Ending::End(end) => {
                let (at, precision) = end.at.parts();
                self.query(
                    "select status, recorded_at, versions
                     from gnomon.end_validity($1, $2, $3, $4, $5, $6)",
                    &[
                        (&self.tenant.0, Type::UUID),
                        (&id, Type::UUID),
                        (&is_edge, Type::BOOL),
                        (&end.episode.0, Type::UUID),
                        (&at.map(Timestamptz), Type::TIMESTAMPTZ),
                        (&precision, Type::TEXT),
                    ],
                )
                .await?
            }
            Ending::Retract(r) => {
                self.query(
                    "select status, recorded_at, versions
                     from gnomon.retract_version($1, $2, $3, $4, $5)",
                    &[
                        (&self.tenant.0, Type::UUID),
                        (&id, Type::UUID),
                        (&is_edge, Type::BOOL),
                        (&r.episode.0, Type::UUID),
                        (&r.version.0, Type::UUID),
                    ],
                )
                .await?
            }
        };
        let outcome = write_outcome(&rows[0])?;
        self.done(call, outcome)
    }

    /// As [`Caller::retract_episode`](crate::Caller::retract_episode), in
    /// this transaction.
    pub async fn retract_episode(
        &mut self,
        episode: EpisodeId,
    ) -> Result<EpisodeRetraction, Error> {
        let call = self.call();
        let rows = self
            .query(
                "select recorded_at, versions from gnomon.retract_episode($1, $2)",
                &[(&self.tenant.0, Type::UUID), (&episode.0, Type::UUID)],
            )
            .await?;
        let row = &rows[0];
        let retraction = EpisodeRetraction {
            recorded_at: time(row, 0)?,
            retracted: row
                .get::<_, Vec<uuid::Uuid>>(1)
                .into_iter()
                .map(VersionId)
                .collect(),
        };
        self.done(call, retraction)
    }

    /// As [`Caller::erase_episode`](crate::Caller::erase_episode), in this
    /// transaction.
    pub async fn erase_episode(&mut self, episode: EpisodeId) -> Result<EpisodeErasure, Error> {
        let call = self.call();
        let rows = self
            .query(
                "select erased_at, recomputed_nodes, recomputed_edges, erased_nodes, erased_edges
                 from gnomon.erase_episode($1, $2)",
                &[(&self.tenant.0, Type::UUID), (&episode.0, Type::UUID)],
            )
            .await?;
        let row = &rows[0];
        let ids = |i: usize| row.get::<_, Vec<uuid::Uuid>>(i);
        let erasure = EpisodeErasure {
            erased_at: time(row, 0)?,
            recomputed_nodes: ids(1).into_iter().map(NodeId).collect(),
            recomputed_edges: ids(2).into_iter().map(EdgeId).collect(),
            erased_nodes: ids(3).into_iter().map(NodeId).collect(),
            erased_edges: ids(4).into_iter().map(EdgeId).collect(),
        };
        self.done(call, erasure)
    }

    /// As [`Caller::erase_node`](crate::Caller::erase_node), in this
    /// transaction.
    pub async fn erase_node(&mut self, node: NodeId) -> Result<GraphErasure, Error> {
        self.erase_graph(
            "select erased_at, erased_episodes, erased_nodes, erased_edges
             from gnomon.erase_node($1, $2)",
            &node.0,
            Type::UUID,
        )
        .await
    }

    /// As [`Caller::erase_edge`](crate::Caller::erase_edge), in this
    /// transaction.
    pub async fn erase_edge(&mut self, edge: EdgeId) -> Result<GraphErasure, Error> {
        self.erase_graph(
            "select erased_at, erased_episodes, erased_nodes, erased_edges
             from gnomon.erase_edge($1, $2)",
            &edge.0,
            Type::UUID,
        )
        .await
    }

    /// As [`Caller::erase_scope`](crate::Caller::erase_scope), in this
    /// transaction.
    pub async fn erase_scope(&mut self, scope: &Scope) -> Result<GraphErasure, Error> {
        self.erase_graph(
            "select erased_at, erased_episodes, erased_nodes, erased_edges
             from gnomon.erase_scope($1, $2)",
            &scope.as_str(),
            Type::TEXT,
        )
        .await
    }

    /// Runs one of the Erasures of a Node, an Edge or a Scope.
    async fn erase_graph(
        &mut self,
        sql: &'static str,
        target: &(dyn ToSql + Sync),
        target_type: Type,
    ) -> Result<GraphErasure, Error> {
        let call = self.call();
        let rows = self
            .query(sql, &[(&self.tenant.0, Type::UUID), (target, target_type)])
            .await?;
        let row = &rows[0];
        let ids = |i: usize| row.get::<_, Vec<uuid::Uuid>>(i);
        let erasure = GraphErasure {
            erased_at: time(row, 0)?,
            erased_episodes: ids(1).into_iter().map(EpisodeId).collect(),
            erased_nodes: ids(2).into_iter().map(NodeId).collect(),
            erased_edges: ids(3).into_iter().map(EdgeId).collect(),
        };
        self.done(call, erasure)
    }
}

/// Reads of the graph.
impl Transaction {
    /// As [`Caller::read_node`](crate::Caller::read_node), in this
    /// transaction.
    pub async fn read_node(
        &mut self,
        node: NodeId,
        as_of: AsOf,
    ) -> Result<Option<NodeVersion>, Error> {
        let call = self.call();
        let rows = self
            .query(
                "select id, node_id, scope, kind, props,
                        nullif(valid_from, '-infinity'), valid_from_precision::text,
                        nullif(valid_to, 'infinity'), valid_to_precision::text,
                        recorded_from, nullif(recorded_to, 'infinity'), uncertain, text_hash,
                        merge_rule::text
                 from gnomon.read_node($1, $2, $3, $4)",
                &[
                    (&self.tenant.0, Type::UUID),
                    (&node.0, Type::UUID),
                    (&Timestamptz(as_of.valid_at), Type::TIMESTAMPTZ),
                    (&as_of.known_at.map(Timestamptz), Type::TIMESTAMPTZ),
                ],
            )
            .await?;
        let version = rows.first().map(|r| node_version(r, 0)).transpose()?;
        self.done(call, version)
    }

    /// As [`Caller::read_nodes`](crate::Caller::read_nodes), in this
    /// transaction.
    pub async fn read_nodes(
        &mut self,
        nodes: &[NodeId],
        as_of: AsOf,
    ) -> Result<Vec<NodeVersion>, Error> {
        let call = self.call();
        let ids: Vec<uuid::Uuid> = nodes.iter().map(|n| n.0).collect();
        let rows = self
            .query(
                "select id, node_id, scope, kind, props,
                        nullif(valid_from, '-infinity'), valid_from_precision::text,
                        nullif(valid_to, 'infinity'), valid_to_precision::text,
                        recorded_from, nullif(recorded_to, 'infinity'), uncertain, text_hash,
                        merge_rule::text
                 from gnomon.read_nodes($1, $2, $3, $4)",
                &[
                    (&self.tenant.0, Type::UUID),
                    (&ids, Type::UUID_ARRAY),
                    (&Timestamptz(as_of.valid_at), Type::TIMESTAMPTZ),
                    (&as_of.known_at.map(Timestamptz), Type::TIMESTAMPTZ),
                ],
            )
            .await?;
        let versions = rows
            .iter()
            .map(|r| node_version(r, 0))
            .collect::<Result<_, _>>()?;
        self.done(call, versions)
    }

    /// As [`Caller::read_edge`](crate::Caller::read_edge), in this
    /// transaction.
    pub async fn read_edge(
        &mut self,
        edge: EdgeId,
        as_of: AsOf,
    ) -> Result<Option<EdgeVersion>, Error> {
        let call = self.call();
        let rows = self
            .query(
                "select id, edge_id, scope, kind, source_id, target_id, props,
                        nullif(valid_from, '-infinity'), valid_from_precision::text,
                        nullif(valid_to, 'infinity'), valid_to_precision::text,
                        recorded_from, nullif(recorded_to, 'infinity'), uncertain, text_hash,
                        merge_rule::text, sentence
                 from gnomon.read_edge($1, $2, $3, $4)",
                &[
                    (&self.tenant.0, Type::UUID),
                    (&edge.0, Type::UUID),
                    (&Timestamptz(as_of.valid_at), Type::TIMESTAMPTZ),
                    (&as_of.known_at.map(Timestamptz), Type::TIMESTAMPTZ),
                ],
            )
            .await?;
        let version = rows.first().map(|r| edge_version(r, 0)).transpose()?;
        self.done(call, version)
    }

    /// As [`Caller::traverse`](crate::Caller::traverse), in this transaction.
    pub async fn traverse(
        &mut self,
        node: NodeId,
        as_of: AsOf,
        traversal: &Traversal,
    ) -> Result<Vec<Step>, Error> {
        let call = self.call();
        let kinds = (!traversal.kinds.is_empty()).then_some(&traversal.kinds);
        let direction = traversal.direction.as_sql();
        let rows = self
            .query(
                "select id, edge_id, scope, kind, source_id, target_id, props,
                        nullif(valid_from, '-infinity'), valid_from_precision::text,
                        nullif(valid_to, 'infinity'), valid_to_precision::text,
                        recorded_from, nullif(recorded_to, 'infinity'), uncertain, text_hash,
                        merge_rule::text, sentence,
                        neighbour_version_id, neighbour_id, neighbour_scope, neighbour_kind,
                        neighbour_props,
                        nullif(neighbour_valid_from, '-infinity'),
                        neighbour_valid_from_precision::text,
                        nullif(neighbour_valid_to, 'infinity'),
                        neighbour_valid_to_precision::text,
                        neighbour_recorded_from, nullif(neighbour_recorded_to, 'infinity'),
                        neighbour_uncertain, neighbour_text_hash, neighbour_merge_rule::text,
                        direction
                 from gnomon.traverse($1, $2, $3, $4, $5, $6)",
                &[
                    (&self.tenant.0, Type::UUID),
                    (&node.0, Type::UUID),
                    (&Timestamptz(as_of.valid_at), Type::TIMESTAMPTZ),
                    (&as_of.known_at.map(Timestamptz), Type::TIMESTAMPTZ),
                    (&direction, Type::TEXT),
                    (&kinds, Type::TEXT_ARRAY),
                ],
            )
            .await?;
        let steps = rows
            .iter()
            .map(|r| {
                Ok(Step {
                    direction: Direction::from_sql(r.get(31)),
                    edge: edge_version(r, 0)?,
                    neighbour: NodeId(r.get(18)),
                    neighbour_version: r
                        .get::<_, Option<uuid::Uuid>>(17)
                        .map(|_| node_version(r, 17))
                        .transpose()?,
                })
            })
            .collect::<Result<_, Error>>()?;
        self.done(call, steps)
    }

    /// As [`Caller::list_node_statements`](crate::Caller::list_node_statements),
    /// in this transaction.
    pub async fn list_node_statements(
        &mut self,
        node: NodeId,
    ) -> Result<Vec<crate::Statement>, Error> {
        self.list_statements(node.0).await
    }

    /// As [`Caller::list_edge_statements`](crate::Caller::list_edge_statements),
    /// in this transaction.
    pub async fn list_edge_statements(
        &mut self,
        edge: EdgeId,
    ) -> Result<Vec<crate::Statement>, Error> {
        self.list_statements(edge.0).await
    }

    async fn list_statements(&mut self, id: uuid::Uuid) -> Result<Vec<crate::Statement>, Error> {
        let call = self.call();
        let rows = self
            .query(
                "select kind, episode_id, version_id, nullif(valid_from, '-infinity'),
                        valid_from_precision::text, nullif(valid_to, 'infinity'),
                        valid_to_precision::text, recorded_at
                 from gnomon.list_statements($1, $2)",
                &[(&self.tenant.0, Type::UUID), (&id, Type::UUID)],
            )
            .await?;
        let statements = rows
            .iter()
            .map(|r| {
                Ok(crate::Statement {
                    kind: match r.get::<_, &str>(0) {
                        "correction" => StatementKind::Correction,
                        "end_of_validity" => StatementKind::EndOfValidity,
                        "retraction" => StatementKind::Retraction,
                        other => panic!("gnomon.list_statements returned kind {other:?}"),
                    },
                    episode: EpisodeId(r.get(1)),
                    version: r.get::<_, Option<uuid::Uuid>>(2).map(VersionId),
                    valid_time: ValidTime {
                        from: bound(optional_time(r, 3)?, r.get(4)),
                        to: bound(optional_time(r, 5)?, r.get(6)),
                    },
                    recorded_at: time(r, 7)?,
                })
            })
            .collect::<Result<_, Error>>()?;
        self.done(call, statements)
    }

    /// As [`Caller::list_node_episodes`](crate::Caller::list_node_episodes),
    /// in this transaction.
    pub async fn list_node_episodes(&mut self, node: NodeId) -> Result<Vec<NodeEpisode>, Error> {
        let call = self.call();
        let rows = self
            .query(
                "select episode_id, scope from gnomon.list_node_episodes($1, $2)",
                &[(&self.tenant.0, Type::UUID), (&node.0, Type::UUID)],
            )
            .await?;
        let episodes = rows
            .iter()
            .map(|r| NodeEpisode {
                episode: EpisodeId(r.get(0)),
                scope: Scope::new(r.get::<_, String>(1)),
            })
            .collect();
        self.done(call, episodes)
    }

    /// As [`Caller::list_citations`](crate::Caller::list_citations), in this
    /// transaction.
    pub async fn list_citations(&mut self, version: VersionId) -> Result<Vec<Citation>, Error> {
        let call = self.call();
        let rows = self
            .query(
                "select episode_id, property, place, recorded_at, episode_recorded_at
                 from gnomon.list_citations($1, $2)",
                &[(&self.tenant.0, Type::UUID), (&version.0, Type::UUID)],
            )
            .await?;
        let citations = rows
            .iter()
            .map(|r| {
                Ok(Citation {
                    episode: EpisodeId(r.get(0)),
                    property: r.get(1),
                    place: r.get(2),
                    recorded_at: time(r, 3)?,
                    episode_recorded_at: time(r, 4)?,
                })
            })
            .collect::<Result<_, Error>>()?;
        self.done(call, citations)
    }
}

/// Search, Resolution, embeddings, Sentences and the gaps.
impl Transaction {
    /// As [`Caller::search_full_text`](crate::Caller::search_full_text), in
    /// this transaction.
    pub async fn search_full_text(
        &mut self,
        query: &str,
        as_of: AsOf,
        limit: i32,
    ) -> Result<Vec<FullTextCandidate>, Error> {
        let call = self.call();
        let rows = self
            .query(
                "select version_id, node_id, edge_id, scope, score, rank
                 from gnomon.search_full_text($1, $2, $3, $4, $5)",
                &[
                    (&self.tenant.0, Type::UUID),
                    (&query, Type::TEXT),
                    (&Timestamptz(as_of.valid_at), Type::TIMESTAMPTZ),
                    (&as_of.known_at.map(Timestamptz), Type::TIMESTAMPTZ),
                    (&limit, Type::INT4),
                ],
            )
            .await?;
        let found = rows
            .iter()
            .map(|r| FullTextCandidate {
                version: VersionId(r.get(0)),
                of: match r.get::<_, Option<uuid::Uuid>>(1) {
                    Some(node) => Versioned::Node(NodeId(node)),
                    None => Versioned::Edge(EdgeId(r.get(2))),
                },
                scope: Scope::new(r.get::<_, String>(3)),
                score: r.get(4),
                rank: r.get(5),
            })
            .collect();
        self.done(call, found)
    }

    /// As [`Caller::store_embedding`](crate::Caller::store_embedding), in
    /// this transaction.
    pub async fn store_embedding(
        &mut self,
        version: VersionId,
        text_hash: &str,
        embedding: &[f32],
    ) -> Result<(), Error> {
        let call = self.call();
        self.query(
            "select gnomon.store_embedding($1, $2, $3, $4)",
            &[
                (&self.tenant.0, Type::UUID),
                (&version.0, Type::UUID),
                (&text_hash, Type::TEXT),
                (&embedding, Type::FLOAT4_ARRAY),
            ],
        )
        .await?;
        self.done(call, ())
    }

    /// As [`Caller::store_name_embedding`](crate::Caller::store_name_embedding),
    /// in this transaction.
    pub async fn store_name_embedding(
        &mut self,
        node: NodeId,
        name: &str,
        embedding: &[f32],
    ) -> Result<(), Error> {
        let call = self.call();
        self.query(
            "select gnomon.store_name_embedding($1, $2, $3, $4)",
            &[
                (&self.tenant.0, Type::UUID),
                (&node.0, Type::UUID),
                (&name, Type::TEXT),
                (&embedding, Type::FLOAT4_ARRAY),
            ],
        )
        .await?;
        self.done(call, ())
    }

    /// As [`Caller::search_vector`](crate::Caller::search_vector), in this
    /// transaction.
    pub async fn search_vector(
        &mut self,
        query: &[f32],
        as_of: AsOf,
        limit: i32,
    ) -> Result<Vec<VectorCandidate>, Error> {
        let call = self.call();
        let rows = self
            .query(
                "select version_id, node_id, edge_id, scope, distance, rank
                 from gnomon.search_vector($1, $2, $3, $4, $5)",
                &[
                    (&self.tenant.0, Type::UUID),
                    (&query, Type::FLOAT4_ARRAY),
                    (&Timestamptz(as_of.valid_at), Type::TIMESTAMPTZ),
                    (&as_of.known_at.map(Timestamptz), Type::TIMESTAMPTZ),
                    (&limit, Type::INT4),
                ],
            )
            .await?;
        let found = rows
            .iter()
            .map(|r| VectorCandidate {
                version: VersionId(r.get(0)),
                of: match r.get::<_, Option<uuid::Uuid>>(1) {
                    Some(node) => Versioned::Node(NodeId(node)),
                    None => Versioned::Edge(EdgeId(r.get(2))),
                },
                scope: Scope::new(r.get::<_, String>(3)),
                distance: r.get(4),
                rank: r.get(5),
            })
            .collect();
        self.done(call, found)
    }

    /// As [`Caller::resolve`](crate::Caller::resolve), in this transaction.
    pub async fn resolve(
        &mut self,
        mention: &Mention<'_>,
        limit: i32,
    ) -> Result<Vec<ResolutionCandidate>, Error> {
        let call = self.call();
        let embedding: Option<Vec<f32>> = mention.embedding.map(<[f32]>::to_vec);
        let rows = self
            .query(
                "select node_id, scope, name, alias, score, rank,
                        full_text_version_id, full_text_name, full_text_alias,
                        full_text_score, full_text_rank,
                        vector_version_id, vector_name, vector_alias, vector_distance,
                        vector_rank
                 from gnomon.resolve($1, $2, $3, $4, $5)",
                &[
                    (&self.tenant.0, Type::UUID),
                    (&mention.text, Type::TEXT),
                    (&embedding, Type::FLOAT4_ARRAY),
                    (&mention.valid_at.map(Timestamptz), Type::TIMESTAMPTZ),
                    (&limit, Type::INT4),
                ],
            )
            .await?;
        let candidates = rows
            .iter()
            .map(|r| ResolutionCandidate {
                node: NodeId(r.get(0)),
                scope: Scope::new(r.get::<_, String>(1)),
                name: r.get(2),
                alias: r.get(3),
                score: r.get(4),
                rank: r.get(5),
                full_text: r
                    .get::<_, Option<uuid::Uuid>>(6)
                    .map(|version| FullTextNameMatch {
                        version: VersionId(version),
                        name: r.get(7),
                        alias: r.get(8),
                        score: r.get(9),
                        rank: r.get(10),
                    }),
                vector: r
                    .get::<_, Option<uuid::Uuid>>(11)
                    .map(|version| VectorNameMatch {
                        version: VersionId(version),
                        name: r.get(12),
                        alias: r.get(13),
                        distance: r.get(14),
                        rank: r.get(15),
                    }),
            })
            .collect();
        self.done(call, candidates)
    }

    /// As [`Caller::search`](crate::Caller::search), in this transaction.
    pub async fn search(
        &mut self,
        request: &SearchRequest<'_>,
        limit: i32,
    ) -> Result<Vec<SearchResult>, Error> {
        let call = self.call();
        let embedding: Option<Vec<f32>> = request.embedding.map(<[f32]>::to_vec);
        let seeds: Vec<uuid::Uuid> = request.seeds.iter().map(|n| n.0).collect();
        let w = &request.weights;
        let rows = self
            .query(
                "select kind, id, node_id, edge_id, scope, score, rank,
                        vector_distance, vector_rank, full_text_score, full_text_rank,
                        neighbourhood_depth, neighbourhood_rank, mentions, mentions_rank,
                        connectedness, connectedness_rank
                 from gnomon.search($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
                &[
                    (&self.tenant.0, Type::UUID),
                    (&request.text, Type::TEXT),
                    (&embedding, Type::FLOAT4_ARRAY),
                    (&Timestamptz(request.as_of.valid_at), Type::TIMESTAMPTZ),
                    (&request.as_of.known_at.map(Timestamptz), Type::TIMESTAMPTZ),
                    (&seeds, Type::UUID_ARRAY),
                    (&request.depth, Type::INT4),
                    (&w.vector, Type::FLOAT8),
                    (&w.full_text, Type::FLOAT8),
                    (&w.neighbourhood, Type::FLOAT8),
                    (&w.mentions, Type::FLOAT8),
                    (&w.connectedness, Type::FLOAT8),
                    (&limit, Type::INT4),
                ],
            )
            .await?;
        self.done(call, rows.iter().map(search_result).collect())
    }

    /// As [`Caller::store_summary_embedding`](crate::Caller::store_summary_embedding),
    /// in this transaction.
    pub async fn store_summary_embedding(
        &mut self,
        summary: SummaryId,
        embedding: &[f32],
    ) -> Result<(), Error> {
        self.store_derived_embedding(
            "select gnomon.store_summary_embedding($1, $2, $3)",
            summary.0,
            embedding,
        )
        .await
    }

    /// As [`Caller::store_community_embedding`](crate::Caller::store_community_embedding),
    /// in this transaction.
    pub async fn store_community_embedding(
        &mut self,
        community: CommunityId,
        embedding: &[f32],
    ) -> Result<(), Error> {
        self.store_derived_embedding(
            "select gnomon.store_community_embedding($1, $2, $3)",
            community.0,
            embedding,
        )
        .await
    }

    async fn store_derived_embedding(
        &mut self,
        sql: &'static str,
        id: uuid::Uuid,
        embedding: &[f32],
    ) -> Result<(), Error> {
        let call = self.call();
        self.query(
            sql,
            &[
                (&self.tenant.0, Type::UUID),
                (&id, Type::UUID),
                (&embedding, Type::FLOAT4_ARRAY),
            ],
        )
        .await?;
        self.done(call, ())
    }

    /// As [`Caller::store_sentence`](crate::Caller::store_sentence), in this
    /// transaction.
    pub async fn store_sentence(
        &mut self,
        version: VersionId,
        sentence: &str,
    ) -> Result<(), Error> {
        let call = self.call();
        self.query(
            "select gnomon.store_sentence($1, $2, $3)",
            &[
                (&self.tenant.0, Type::UUID),
                (&version.0, Type::UUID),
                (&sentence, Type::TEXT),
            ],
        )
        .await?;
        self.done(call, ())
    }

    /// As [`Caller::list_gaps`](crate::Caller::list_gaps), in this
    /// transaction.
    pub async fn list_gaps(
        &mut self,
        scope: &Scope,
        after: Option<&GapCursor>,
        limit: i32,
    ) -> Result<GapPage, Error> {
        let call = self.call();
        let rows = self
            .query(
                "select kind, id, text_hash, node_id, edge_id, search_text
                 from gnomon.list_gaps($1, $2, $3, $4, $5, $6)",
                &[
                    (&self.tenant.0, Type::UUID),
                    (&scope.as_str(), Type::TEXT),
                    (&after.map(|a| a.kind), Type::TEXT),
                    (&after.map(|a| a.id), Type::UUID),
                    (&after.and_then(|a| a.text_hash.as_deref()), Type::TEXT),
                    (&limit, Type::INT4),
                ],
            )
            .await?;
        let gaps: Vec<Gap> = rows.iter().map(gap).collect();
        let next = match gaps.last() {
            Some(last) if gaps.len() == limit as usize => Some(last.cursor()),
            _ => None,
        };
        self.done(call, GapPage { gaps, next })
    }
}

/// Stage records (ADR-0011).
impl Transaction {
    /// Writes stage records in this transaction (ADR-0011), such as the
    /// finish of the stage whose output the transaction writes and the
    /// pending record of the next stage, so that they commit with the output
    /// or not at all. Otherwise as
    /// [`Caller::write_stages`](crate::Caller::write_stages). A finish
    /// refused because another claim took the record fails with
    /// [`Error::StageClaimLost`], and the commit then saves nothing.
    pub async fn write_stages(&mut self, writes: &[StageWrite]) -> Result<(), Error> {
        let call = self.call();
        for write in writes {
            write_stage(self, write).await?;
        }
        self.done(call, ())
    }

    /// As [`Caller::claim_stage`](crate::Caller::claim_stage), in this
    /// transaction. The claim holds the record's row lock until the
    /// transaction ends, and its lease counts from the claim.
    pub async fn claim_stage(
        &mut self,
        episode: Option<EpisodeId>,
        lease: Duration,
    ) -> Result<Option<StageRecord>, Error> {
        let call = self.call();
        let rows = self
            .query(
                "select tenant_id, scope, episode_id, stage, status::text, attempts,
                        last_error_code, last_error_status, retry_at, lease_until, created_at,
                        updated_at
                 from gnomon.claim_stage($1, $2, $3 * interval '1 second')",
                &[
                    (&self.tenant.0, Type::UUID),
                    (&episode.map(|e| e.0), Type::UUID),
                    (&lease.as_secs_f64(), Type::FLOAT8),
                ],
            )
            .await?;
        let claimed = match rows.first() {
            None => None,
            Some(row) => {
                let episode = EpisodeId(row.get(2));
                let usage = read_stage_usage(self, episode).await?;
                Some(stage_record(
                    row,
                    usage
                        .get(&row.get::<_, String>(3))
                        .cloned()
                        .unwrap_or_default(),
                )?)
            }
        };
        self.done(call, claimed)
    }

    /// As [`Caller::stage_records`](crate::Caller::stage_records), in this
    /// transaction.
    pub async fn stage_records(&mut self, episode: EpisodeId) -> Result<Vec<StageRecord>, Error> {
        let call = self.call();
        let rows = self
            .query(
                "select tenant_id, scope, episode_id, stage, status::text, attempts,
                        last_error_code, last_error_status, retry_at, lease_until, created_at,
                        updated_at
                 from gnomon.read_stage_records($1, $2)",
                &[(&self.tenant.0, Type::UUID), (&episode.0, Type::UUID)],
            )
            .await?;
        let mut usage = read_stage_usage(self, episode).await?;
        let records = rows
            .iter()
            .map(|row| {
                let of_stage = usage.remove(&row.get::<_, String>(3)).unwrap_or_default();
                stage_record(row, of_stage)
            })
            .collect::<Result<_, _>>()?;
        self.done(call, records)
    }
}

/// Summaries and Communities (ADR-0014, ADR-0015).
impl Transaction {
    /// As [`Caller::write_summary`](crate::Caller::write_summary), in this
    /// transaction.
    pub async fn write_summary(&mut self, summary: &NewSummary) -> Result<SummaryId, Error> {
        let (episodes, properties, places) = citation_columns(&summary.citations);
        let params: [(&(dyn ToSql + Sync), Type); 7] = [
            (&summary.scope.as_str(), Type::TEXT),
            (&summary.node.0, Type::UUID),
            (&summary.content, Type::TEXT),
            (&Timestamptz(summary.as_of_valid), Type::TIMESTAMPTZ),
            (&episodes, Type::UUID_ARRAY),
            (&properties, Type::TEXT_ARRAY),
            (&places, Type::TEXT_ARRAY),
        ];
        let id = self
            .write_derived(
                "select gnomon.write_summary($1, $2, $3, $4, $5, $6, $7, $8)",
                &params,
            )
            .await?;
        Ok(SummaryId(id))
    }

    /// As [`Caller::write_community`](crate::Caller::write_community), in
    /// this transaction.
    pub async fn write_community(
        &mut self,
        community: &NewCommunity,
    ) -> Result<CommunityId, Error> {
        let (episodes, properties, places) = citation_columns(&community.citations);
        let replaces: Vec<uuid::Uuid> = community.replaces.iter().map(|c| c.0).collect();
        let members: Vec<uuid::Uuid> = community.members.iter().map(|n| n.0).collect();
        let params: [(&(dyn ToSql + Sync), Type); 8] = [
            (&community.scope.as_str(), Type::TEXT),
            (&replaces, Type::UUID_ARRAY),
            (&members, Type::UUID_ARRAY),
            (&community.content, Type::TEXT),
            (&Timestamptz(community.as_of_valid), Type::TIMESTAMPTZ),
            (&episodes, Type::UUID_ARRAY),
            (&properties, Type::TEXT_ARRAY),
            (&places, Type::TEXT_ARRAY),
        ];
        let id = self
            .write_derived(
                "select gnomon.write_community($1, $2, $3, $4, $5, $6, $7, $8, $9)",
                &params,
            )
            .await?;
        Ok(CommunityId(id))
    }

    /// Runs one Summary or Community write, with the Tenant first.
    async fn write_derived(
        &mut self,
        sql: &'static str,
        params: &[(&(dyn ToSql + Sync), Type)],
    ) -> Result<uuid::Uuid, Error> {
        let call = self.call();
        let tenant = self.tenant;
        let mut all: Vec<(&(dyn ToSql + Sync), Type)> = vec![(&tenant.0, Type::UUID)];
        all.extend(params.iter().cloned());
        let rows = self.query(sql, &all).await?;
        self.done(call, rows[0].get(0))
    }

    /// As [`Caller::read_summaries`](crate::Caller::read_summaries), in this
    /// transaction.
    pub async fn read_summaries(
        &mut self,
        node: NodeId,
        as_of: AsOf,
    ) -> Result<Vec<Summary>, Error> {
        let call = self.call();
        let rows = self
            .query(
                "select id, scope, node_id, content, as_of_valid, recorded_from, recorded_to,
                        cited_episodes, cited_properties, cited_places
                 from gnomon.read_summaries($1, $2, $3, $4)",
                &[
                    (&self.tenant.0, Type::UUID),
                    (&node.0, Type::UUID),
                    (&Timestamptz(as_of.valid_at), Type::TIMESTAMPTZ),
                    (&as_of.known_at.map(Timestamptz), Type::TIMESTAMPTZ),
                ],
            )
            .await?;
        let summaries = rows
            .iter()
            .map(|r| {
                Ok(Summary {
                    id: SummaryId(r.get(0)),
                    scope: Scope::new(r.get::<_, String>(1)),
                    node: NodeId(r.get(2)),
                    content: r.get(3),
                    as_of_valid: time(r, 4)?,
                    recorded_from: time(r, 5)?,
                    recorded_to: optional_time(r, 6)?,
                    citations: derived_citations(r, 7),
                })
            })
            .collect::<Result<_, Error>>()?;
        self.done(call, summaries)
    }

    /// As [`Caller::read_communities`](crate::Caller::read_communities), in
    /// this transaction.
    pub async fn read_communities(
        &mut self,
        node: NodeId,
        as_of: AsOf,
    ) -> Result<Vec<Community>, Error> {
        let call = self.call();
        let rows = self
            .query(
                "select id, scope, members, content, as_of_valid, recorded_from, recorded_to,
                        cited_episodes, cited_properties, cited_places
                 from gnomon.read_communities($1, $2, $3, $4)",
                &[
                    (&self.tenant.0, Type::UUID),
                    (&node.0, Type::UUID),
                    (&Timestamptz(as_of.valid_at), Type::TIMESTAMPTZ),
                    (&as_of.known_at.map(Timestamptz), Type::TIMESTAMPTZ),
                ],
            )
            .await?;
        let communities = rows
            .iter()
            .map(|r| {
                Ok(Community {
                    id: CommunityId(r.get(0)),
                    scope: Scope::new(r.get::<_, String>(1)),
                    members: r
                        .get::<_, Vec<uuid::Uuid>>(2)
                        .into_iter()
                        .map(NodeId)
                        .collect(),
                    content: r.get(3),
                    as_of_valid: time(r, 4)?,
                    recorded_from: time(r, 5)?,
                    recorded_to: optional_time(r, 6)?,
                    citations: derived_citations(r, 7),
                })
            })
            .collect::<Result<_, Error>>()?;
        self.done(call, communities)
    }
}

/// What an End of validity or a Retraction is made of.
#[derive(Clone, Copy)]
enum Ending<'a> {
    End(&'a EndOfValidity),
    Retract(&'a Retraction),
}

/// The Store functions that write the Node or Edge a write or a bound
/// statement is about.
impl Versioned {
    fn id(self) -> uuid::Uuid {
        match self {
            Versioned::Node(node) => node.0,
            Versioned::Edge(edge) => edge.0,
        }
    }

    fn write_sql(self) -> &'static str {
        match self {
            Versioned::Node(_) => {
                "select status, recorded_at, versions
                 from gnomon.write_node($1, $2, $3, $4, $5, $6, $7, $8, $9)"
            }
            Versioned::Edge(_) => {
                "select status, recorded_at, versions
                 from gnomon.write_edge($1, $2, $3, $4, $5, $6, $7, $8, $9)"
            }
        }
    }

    fn correct_sql(self) -> &'static str {
        match self {
            Versioned::Node(_) => {
                "select status, recorded_at, versions
                 from gnomon.correct_node($1, $2, $3, $4, $5, $6, $7, $8, $9)"
            }
            Versioned::Edge(_) => {
                "select status, recorded_at, versions
                 from gnomon.correct_edge($1, $2, $3, $4, $5, $6, $7, $8, $9)"
            }
        }
    }

    fn is_edge(self) -> bool {
        matches!(self, Versioned::Edge(_))
    }

    fn state_bound_sql(self) -> &'static str {
        match self {
            Versioned::Node(_) => {
                "select change, recorded_at, versions
                 from gnomon.state_bound($1, $2, $3, $4, $5, $6, $7)"
            }
            Versioned::Edge(_) => {
                "select change, recorded_at, versions
                 from gnomon.state_edge_bound($1, $2, $3, $4, $5, $6, $7)"
            }
        }
    }
}

async fn write_patch(
    tx: &Transaction,
    of: Versioned,
    patch: &Patch,
    correct: bool,
) -> Result<WriteOutcome, Error> {
    let id = of.id();
    let (from, to) = patch.valid_time.bounds();
    let ((from, from_precision), (to, to_precision)) = (from.parts(), to.parts());
    let values = Value::Object(patch.values.clone());
    let places = (!patch.places.is_empty()).then(|| {
        Value::Object(
            patch
                .places
                .iter()
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect(),
        )
    });
    let sql = if correct {
        of.correct_sql()
    } else {
        of.write_sql()
    };
    let rows = tx
        .query(
            sql,
            &[
                (&tx.tenant.0, Type::UUID),
                (&id, Type::UUID),
                (&patch.episode.0, Type::UUID),
                (&from.map(Timestamptz), Type::TIMESTAMPTZ),
                (&from_precision, Type::TEXT),
                (&to.map(Timestamptz), Type::TIMESTAMPTZ),
                (&to_precision, Type::TEXT),
                (&values, Type::JSONB),
                (&places, Type::JSONB),
            ],
        )
        .await?;
    write_outcome(&rows[0])
}

fn write_outcome(r: &Row) -> Result<WriteOutcome, Error> {
    let status = match r.get::<_, &str>(0) {
        "written" => WriteStatus::Written,
        "unchanged" => WriteStatus::Unchanged,
        other => panic!("the Store's write returned status {other:?}"),
    };
    let opened: Vec<uuid::Uuid> = r.get(2);
    Ok(WriteOutcome {
        status,
        recorded_at: time(r, 1)?,
        opened: opened.into_iter().map(VersionId).collect(),
    })
}

/// A Node Version from the 14 columns of `row` from `at` on: ID, Node,
/// Scope, Kind, values, the four bounds with their precisions, the two
/// Recorded-time bounds, the `uncertain` marker, the text hash and the merge
/// rule, with open bounds as nulls.
fn node_version(row: &Row, at: usize) -> Result<NodeVersion, Error> {
    Ok(NodeVersion {
        id: VersionId(row.get(at)),
        node: NodeId(row.get(at + 1)),
        scope: Scope::new(row.get::<_, String>(at + 2)),
        kind: row.get(at + 3),
        values: object(row.get(at + 4)),
        valid_time: ValidTime {
            from: bound(optional_time(row, at + 5)?, row.get(at + 6)),
            to: bound(optional_time(row, at + 7)?, row.get(at + 8)),
        },
        recorded_from: time(row, at + 9)?,
        recorded_to: optional_time(row, at + 10)?,
        uncertain: row.get(at + 11),
        text_hash: row.get(at + 12),
        merge_rule: MergeRule::from_sql(row.get(at + 13)),
    })
}

/// An Edge Version from the 17 columns of `row` from `at` on: ID, Edge,
/// Scope, Kind, source, target, then as [`node_version`] from the values on,
/// and last the Sentence.
fn edge_version(row: &Row, at: usize) -> Result<EdgeVersion, Error> {
    Ok(EdgeVersion {
        id: VersionId(row.get(at)),
        edge: EdgeId(row.get(at + 1)),
        scope: Scope::new(row.get::<_, String>(at + 2)),
        kind: row.get(at + 3),
        source: NodeId(row.get(at + 4)),
        target: NodeId(row.get(at + 5)),
        values: object(row.get(at + 6)),
        valid_time: ValidTime {
            from: bound(optional_time(row, at + 7)?, row.get(at + 8)),
            to: bound(optional_time(row, at + 9)?, row.get(at + 10)),
        },
        recorded_from: time(row, at + 11)?,
        recorded_to: optional_time(row, at + 12)?,
        uncertain: row.get(at + 13),
        text_hash: row.get(at + 14),
        merge_rule: MergeRule::from_sql(row.get(at + 15)),
        sentence: row.get(at + 16),
    })
}

fn object(values: Value) -> Map<String, Value> {
    match values {
        Value::Object(m) => m,
        other => panic!("the Store returned values {other}"),
    }
}

/// A column that holds a time. `timestamptz` holds times outside the range
/// of a `Timestamp`, and `Row::get` panics on a value it cannot convert, so
/// every time is decoded here or in [`optional_time`].
fn time(row: &Row, at: usize) -> Result<Timestamp, Error> {
    Ok(row.try_get::<_, Timestamptz>(at)?.0)
}

/// A column that holds a time or a null, as [`time`] decodes one.
fn optional_time(row: &Row, at: usize) -> Result<Option<Timestamp>, Error> {
    Ok(row
        .try_get::<_, Option<Timestamptz>>(at)?
        .map(|stored| stored.0))
}

/// A Valid-time bound as the Store returns it: unknown, or an instant.
fn bound(at: Option<Timestamp>, precision: &str) -> Bound {
    match (Precision::from_sql(precision), at) {
        (None, _) | (_, None) => Bound::Unknown,
        (Some(Precision::Instant), Some(t)) => Bound::At(t),
        (Some(precision), Some(at)) => Bound::Period { at, precision },
    }
}

async fn read_kind(tx: &Transaction, name: &str) -> Result<Option<Kind>, Error> {
    let tenant = tx.tenant;
    let rows = tx
        .query(
            "select name, kind_of, parent, ancestors, properties, rules, search,
                    source_kinds, target_kinds, is_symmetric
             from gnomon.read_kind($1, $2)",
            &[(&tenant.0, Type::UUID), (&name, Type::TEXT)],
        )
        .await?;
    Ok(rows.first().map(|r| {
        let of = match r.get::<_, &str>(1) {
            "node" => KindOf::Node,
            "edge" => KindOf::Edge {
                source: r.get(7),
                target: r.get(8),
            },
            other => panic!("gnomon.read_kind returned kind_of {other:?}"),
        };
        let rules = match r.get::<_, Value>(5) {
            Value::Object(m) => m
                .into_iter()
                .map(|(property, rule)| match rule {
                    Value::String(rule) => (property, rule),
                    other => panic!("gnomon.read_kind returned rule {other} for {property}"),
                })
                .collect(),
            other => panic!("gnomon.read_kind returned rules {other}"),
        };
        Kind {
            tenant,
            name: r.get(0),
            of,
            parent: r.get(2),
            ancestors: r.get(3),
            properties: r.get(4),
            rules,
            search: r.get(6),
            symmetric: r.get(9),
        }
    }))
}

/// An Episode from a row of `gnomon.read_episode`'s columns, in their order.
fn episode_record(r: &Row) -> Result<Episode, Error> {
    let retracted_at: Option<Timestamp> = optional_time(r, 8)?;
    Ok(Episode {
        tenant: TenantId(r.get(0)),
        scope: Scope::new(r.get::<_, String>(1)),
        id: EpisodeId(r.get(2)),
        content: r.get(3),
        reference_time: time(r, 4)?,
        authority: r.get(5),
        recorded_at: time(r, 6)?,
        recorded_by: r.get(7),
        retracted: retracted_at.map(|recorded_at| EpisodeRetracted {
            recorded_at,
            recorded_by: r.get(9),
        }),
    })
}

/// A stage record from a row of `tenant_id, scope, episode_id, stage,
/// status, attempts, last_error_code, last_error_status, retry_at,
/// lease_until, created_at, updated_at`, as `gnomon.claim_stage` and
/// `gnomon.read_stage_records` return them.
fn stage_record(row: &Row, usage: Vec<StageUsage>) -> Result<StageRecord, Error> {
    let code: Option<String> = row.get(6);
    Ok(StageRecord {
        tenant: TenantId(row.get(0)),
        scope: Scope::new(row.get::<_, String>(1)),
        episode: EpisodeId(row.get(2)),
        stage: row.get(3),
        status: StageStatus::from_sql(row.get(4)),
        attempts: row.get(5),
        last_error: code.map(|code| StageError {
            code,
            status: row.get(7),
        }),
        retry_at: optional_time(row, 8)?,
        lease_until: optional_time(row, 9)?,
        created_at: time(row, 10)?,
        updated_at: time(row, 11)?,
        usage,
    })
}

/// The usage counts of an Episode's stage records, by stage, each in order
/// of model.
async fn read_stage_usage(
    tx: &Transaction,
    episode: EpisodeId,
) -> Result<HashMap<String, Vec<StageUsage>>, Error> {
    let rows = tx
        .query(
            "select stage, model, calls, input_tokens, output_tokens, cost_usd::float8
             from gnomon.read_stage_usage($1, $2)",
            &[(&tx.tenant.0, Type::UUID), (&episode.0, Type::UUID)],
        )
        .await?;
    let mut by_stage: HashMap<String, Vec<StageUsage>> = HashMap::new();
    for row in &rows {
        by_stage.entry(row.get(0)).or_default().push(StageUsage {
            model: row.get(1),
            calls: row.get(2),
            input_tokens: row.get(3),
            output_tokens: row.get(4),
            cost_usd: row.get(5),
        });
    }
    Ok(by_stage)
}

/// Writes one stage record in `tx`.
async fn write_stage(tx: &Transaction, write: &StageWrite) -> Result<(), Error> {
    let tenant = tx.tenant;
    match write {
        StageWrite::Pending { episode, stage } => {
            tx.query(
                "select gnomon.stage_pending($1, $2, $3)",
                &[
                    (&tenant.0, Type::UUID),
                    (&episode.0, Type::UUID),
                    (stage, Type::TEXT),
                ],
            )
            .await?;
        }
        StageWrite::Finish {
            episode,
            stage,
            attempt,
            outcome,
            usage,
        } => {
            // `retry_at` is read back as a `Timestamp`, so a delay that
            // ends past one would leave the Episode's records unreadable.
            if let StageOutcome::Retry { after, .. } = outcome
                && Timestamp::now().checked_add(*after).is_err()
            {
                return Err(Error::InvalidWrite(format!(
                    "a retry delay of {} seconds ends after {}, the latest time gnomon can read",
                    after.as_secs(),
                    Timestamp::MAX
                )));
            }
            let (status, error, retry_after) = match outcome {
                StageOutcome::Succeeded => (StageStatus::Succeeded, None, None),
                StageOutcome::Retry { error, after } => {
                    (StageStatus::Retry, Some(error), Some(after.as_secs_f64()))
                }
                StageOutcome::Failed { error } => (StageStatus::Failed, Some(error), None),
            };
            tx.query(
                "select gnomon.finish_stage($1, $2, $3, $4, $5::text::gnomon.stage_status,
                                            $6, $7, $8 * interval '1 second')",
                &[
                    (&tenant.0, Type::UUID),
                    (&episode.0, Type::UUID),
                    (stage, Type::TEXT),
                    (attempt, Type::INT4),
                    (&status.as_sql(), Type::TEXT),
                    (&error.map(|e| e.code.as_str()), Type::TEXT),
                    (&error.and_then(|e| e.status), Type::INT4),
                    (&retry_after, Type::FLOAT8),
                ],
            )
            .await?;
            for u in usage {
                tx.query(
                    "select gnomon.add_stage_usage($1, $2, $3, $4, $5, $6, $7, $8::numeric)",
                    &[
                        (&tenant.0, Type::UUID),
                        (&episode.0, Type::UUID),
                        (stage, Type::TEXT),
                        (&u.model, Type::TEXT),
                        (&u.calls, Type::INT4),
                        (&u.input_tokens, Type::INT8),
                        (&u.output_tokens, Type::INT8),
                        (&u.cost_usd, Type::FLOAT8),
                    ],
                )
                .await?;
            }
        }
    }
    Ok(())
}

/// The three parallel arrays the write functions take for Citations.
type CitationColumns = (Vec<uuid::Uuid>, Vec<Option<String>>, Vec<Option<String>>);

fn citation_columns(citations: &[DerivedCitation]) -> CitationColumns {
    (
        citations.iter().map(|c| c.episode.0).collect(),
        citations.iter().map(|c| c.property.clone()).collect(),
        citations.iter().map(|c| c.place.clone()).collect(),
    )
}

/// The Citations in the three array columns of `row` from `at` on: the
/// Episodes, the properties and the places.
fn derived_citations(row: &Row, at: usize) -> Vec<DerivedCitation> {
    let episodes: Vec<uuid::Uuid> = row.get(at);
    let properties: Vec<Option<String>> = row.get(at + 1);
    let places: Vec<Option<String>> = row.get(at + 2);
    episodes
        .into_iter()
        .zip(properties)
        .zip(places)
        .map(|((episode, property), place)| DerivedCitation {
            episode: EpisodeId(episode),
            property,
            place,
        })
        .collect()
}

fn search_result(r: &Row) -> SearchResult {
    let id: uuid::Uuid = r.get(1);
    let node: Option<uuid::Uuid> = r.get(2);
    let hit = match r.get::<_, &str>(0) {
        "summary" => SearchHit::Summary {
            summary: SummaryId(id),
            node: NodeId(node.expect("a Summary describes a Node")),
        },
        "community" => SearchHit::Community(CommunityId(id)),
        "version" => SearchHit::Version {
            version: VersionId(id),
            of: match node {
                Some(node) => Versioned::Node(NodeId(node)),
                None => Versioned::Edge(EdgeId(r.get(3))),
            },
        },
        other => unreachable!("gnomon.search returns no {other} results"),
    };
    let branch = |value: usize, rank: usize| {
        r.get::<_, Option<i64>>(rank).map(|rank| BranchRank {
            value: r.get::<_, f64>(value),
            rank,
        })
    };
    SearchResult {
        hit,
        scope: Scope::new(r.get::<_, String>(4)),
        score: r.get(5),
        rank: r.get(6),
        vector: branch(7, 8),
        full_text: branch(9, 10),
        neighbourhood: r.get::<_, Option<i64>>(12).map(|rank| BranchRank {
            value: r.get(11),
            rank,
        }),
        mentions: SignalRank {
            count: r.get(13),
            rank: r.get(14),
        },
        connectedness: SignalRank {
            count: r.get(15),
            rank: r.get(16),
        },
    }
}

fn gap(r: &Row) -> Gap {
    let id: uuid::Uuid = r.get(1);
    let hash = || r.get::<_, String>(2);
    let node = || NodeId(r.get(3));
    let edge = || EdgeId(r.get(4));
    let text = || r.get::<_, String>(5);
    match r.get::<_, &str>(0) {
        "sentence" => Gap::Sentence {
            version: VersionId(id),
            edge: edge(),
        },
        "sentence_embedding" => Gap::SentenceEmbedding {
            version: VersionId(id),
            edge: edge(),
            text_hash: hash(),
            sentence: text(),
        },
        "node_embedding" => Gap::NodeEmbedding {
            version: VersionId(id),
            node: node(),
            text_hash: hash(),
        },
        "name_embedding" => Gap::NameEmbedding {
            node: NodeId(id),
            text_hash: hash(),
            name: text(),
        },
        "summary_embedding" => Gap::SummaryEmbedding {
            summary: SummaryId(id),
            node: node(),
        },
        "community_embedding" => Gap::CommunityEmbedding {
            community: CommunityId(id),
        },
        "summary" => Gap::Summary { node: NodeId(id) },
        "community" => Gap::Community {
            community: CommunityId(id),
        },
        other => unreachable!("gnomon.list_gaps returns no {other} gaps"),
    }
}
