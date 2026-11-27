//! The Store API: every call runs as a caller, under RLS, in one explicit
//! transaction on a connection from the Store's pool (ADR-0001, ADR-0009,
//! ADR-0016).
//!
//! A single call on a [`Caller`] is one [`Transaction`] of its own, which
//! commits when the call succeeds. `gnomon.call_tenant` checks the call's
//! Tenant against the claims, or picks the default Tenant for a call that
//! names none in single-tenant mode, and the call goes on with the Tenant it
//! returns (ADR-0005). `transaction.rs` holds the calls themselves.

use std::sync::OnceLock;
use std::time::Duration;

use serde_json::{Map, Value};
use tokio_postgres::error::SqlState;
use tokio_postgres::types::Type;

use crate::connect::ConnectOptions;
use crate::migrations::{EmbeddingDimension, SCHEMA_VERSION};
use crate::transaction::{CLAIMS, Conn, Transaction};
use crate::{
    AsOf, BoundOutcome, BoundStatement, Citation, Community, CommunityId, EdgeId, EdgeVersion,
    EndOfValidity, Episode, EpisodeErasure, EpisodeId, EpisodeRetraction, Error, FullTextCandidate,
    GapCursor, GapPage, GraphErasure, Kind, Mention, MergeRule, NewCommunity, NewEdge, NewEpisode,
    NewKind, NewNode, NewSummary, NodeEpisode, NodeId, NodeVersion, Patch, ResolutionCandidate,
    Retraction, Scope, SearchRequest, SearchResult, StageRecord, StageWrite, Step, Summary,
    SummaryId, TenantId, Traversal, UnavailableReason, VectorCandidate, VersionId, WriteOutcome,
};

/// How many times a single call runs again when Postgres fails it with a
/// serialization failure or a deadlock, which a write to a Node that a
/// concurrent transaction wrote first can raise.
const CONFLICT_RETRIES: usize = 8;

/// The caller role a call runs as. Neither can bypass RLS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CallerRole {
    /// An end user, `authenticated`.
    User,
    /// A worker for one Tenant and Scope, `gnomon_worker`.
    Worker,
}

impl CallerRole {
    fn name(self) -> &'static str {
        match self {
            CallerRole::User => "authenticated",
            CallerRole::Worker => "gnomon_worker",
        }
    }
}

/// A gnomon Store: a small pool of connections to one database.
///
/// Calls run at once, each on a connection of its own. The pool opens a
/// connection when a call needs one and none is free, up to
/// [`ConnectOptions::max_connections`], keeps none open when it needs none,
/// and runs no background task (ADR-0016). A call waits at most
/// [`ConnectOptions::wait_timeout`] for a free connection, then fails with
/// [`Error::Unavailable`].
pub struct Store {
    pool: deadpool_postgres::Pool,
    /// What the schema check found, once one has passed.
    dimension: OnceLock<EmbeddingDimension>,
}

impl Store {
    /// Prepares a Store for a database. It opens no connection.
    ///
    /// `connection` is a libpq-style URL or key=value string for the login
    /// role, which must be able to `SET ROLE` to `authenticated` and
    /// `gnomon_worker`. `options` decide TLS and the pool. By default the
    /// Store connects with TLS and verifies the server's certificate and
    /// host name, and the connection string's `sslmode` cannot turn that
    /// off: `sslmode=disable` with TLS on is refused, and so are
    /// `sslmode=require` and `sslmode=verify-full` with
    /// [`Tls::plaintext`](crate::Tls::plaintext) and `sslmode=verify-full`
    /// with [`Tls::unverified`](crate::Tls::unverified). Settings that
    /// contradict each other fail here, with [`Error::InvalidConnection`].
    ///
    /// The first connection the Store opens reads the schema version and the
    /// embedding dimension before its call, and refuses any version but the
    /// one this build expects. A check that fails is not remembered, so a
    /// later call checks again. A server calls [`check`](Self::check) at
    /// startup to fail early. Behind a transaction-mode pooler each call may
    /// run on another server connection, but every one of them is to the
    /// same database, so the version holds for all of them.
    pub async fn connect(connection: &str, options: &ConnectOptions) -> Result<Store, Error> {
        Ok(Store {
            pool: options.pool(connection)?,
            dimension: OnceLock::new(),
        })
    }

    /// Connects and checks the schema version and the embedding dimension,
    /// as the first call would. A server calls it at startup.
    pub async fn check(&self) -> Result<(), Error> {
        let mut replaced = false;
        loop {
            let mut conn = self.connection().await?;
            match check_schema(&mut conn).await {
                Ok(dimension) => {
                    let _ = self.dimension.set(dimension);
                    return Ok(());
                }
                Err(e) if e.is_lost_connection() && !replaced => replaced = true,
                Err(e) => {
                    if conn.is_open() {
                        let _ = conn.rollback().await;
                    }
                    return Err(e);
                }
            }
        }
    }

    /// The embedding dimension `gnomon.embedding_dimension()` states. It
    /// connects and runs the schema check if no call has run it yet.
    pub async fn embedding_dimension(&self) -> Result<EmbeddingDimension, Error> {
        if let Some(dimension) = self.dimension.get() {
            return Ok(*dimension);
        }
        self.check().await?;
        Ok(*self.dimension.get().expect("the check passed"))
    }

    /// Acts as an end user. `claims` are the user's verified JWT claims; they
    /// go to `request.jwt.claims` unchanged, and the deployment's
    /// `gnomon.tenant_id()` and `gnomon.allowed_scopes()` read them.
    pub fn as_user(&self, claims: Map<String, Value>) -> Caller<'_> {
        Caller {
            store: self,
            role: CallerRole::User,
            claims: Value::Object(claims).to_string(),
        }
    }

    /// Acts as a worker for one Tenant and Scope, under a role that cannot
    /// bypass RLS. A worker in a single-tenant deployment may pass `None`
    /// for the default Tenant; its claims then bind no Tenant, and the
    /// reference `gnomon.tenant_id()` returns the default one.
    ///
    /// `user` is the person a background job works for, if any. The
    /// reference `gnomon.user_id()` reads it, so an Episode the worker
    /// records, or retracts, records that user in `recorded_by`. Without
    /// one, `recorded_by` is null.
    ///
    /// The claims are
    /// `{"role": "gnomon_worker", "sub": <user>, "app_metadata": {"tenant_id": <tenant>, "scopes": [<scope>]}}`,
    /// without `"sub"` for no user and without `"tenant_id"` for `None`. A
    /// deployment that replaces `gnomon.tenant_id()`,
    /// `gnomon.allowed_scopes()`, `gnomon.writable_scopes()` or
    /// `gnomon.user_id()` must read them, and tells a worker call by its
    /// `role`.
    pub fn as_worker(
        &self,
        tenant: impl Into<Option<TenantId>>,
        scope: Scope,
        user: Option<&str>,
    ) -> Caller<'_> {
        let mut app_metadata = Map::new();
        if let Some(tenant) = tenant.into() {
            app_metadata.insert("tenant_id".into(), Value::String(tenant.0.to_string()));
        }
        app_metadata.insert("scopes".into(), serde_json::json!([scope.as_str()]));
        let mut claims = Map::new();
        claims.insert(
            "role".into(),
            Value::String(CallerRole::Worker.name().into()),
        );
        if let Some(user) = user {
            claims.insert("sub".into(), Value::String(user.to_owned()));
        }
        claims.insert("app_metadata".into(), Value::Object(app_metadata));
        Caller {
            store: self,
            role: CallerRole::Worker,
            claims: Value::Object(claims).to_string(),
        }
    }

    /// Takes a free connection from the pool, or opens one.
    async fn connection(&self) -> Result<Conn, Error> {
        use deadpool_postgres::PoolError;
        match self.pool.get().await {
            Ok(object) => Ok(Conn::new(object)),
            Err(PoolError::Timeout(t)) => Err(Error::Unavailable {
                reason: UnavailableReason::PoolTimeout,
                source: Box::new(PoolError::Timeout(t)),
            }),
            Err(PoolError::Backend(e)) => Err(Error::Unavailable {
                reason: UnavailableReason::Connect,
                source: Box::new(e),
            }),
            Err(other) => Err(Error::Unavailable {
                reason: UnavailableReason::Connect,
                source: Box::new(other),
            }),
        }
    }

    /// Runs the schema check on `conn` unless one has passed.
    async fn check_once(&self, conn: &mut Conn) -> Result<(), Error> {
        if self.dimension.get().is_none() {
            let dimension = check_schema(conn).await?;
            let _ = self.dimension.set(dimension);
        }
        Ok(())
    }
}

/// Reads the ledger and the embedding dimension on `conn`, as an end user
/// would, in a transaction of its own, and refuses any version but the one
/// this build expects.
async fn check_schema(conn: &mut Conn) -> Result<EmbeddingDimension, Error> {
    conn.begin(CallerRole::User.name()).await?;
    let row = match conn
        .query(
            "select (select max(version) from gnomon.schema_version), gnomon.embedding_dimension()",
            &[],
        )
        .await
    {
        Ok(rows) => rows.into_iter().next().expect("one row"),
        Err(e) if is_missing_schema(&e) => {
            conn.rollback().await?;
            return Err(Error::SchemaVersion {
                expected: SCHEMA_VERSION,
                found: None,
            });
        }
        Err(e) => return Err(e.into()),
    };
    conn.commit().await?;

    let found: Option<i32> = row.get(0);
    if found != Some(SCHEMA_VERSION) {
        return Err(Error::SchemaVersion {
            expected: SCHEMA_VERSION,
            found,
        });
    }
    let dimension: i32 = row.get(1);
    u32::try_from(dimension)
        .ok()
        .and_then(|d| EmbeddingDimension::new(d).ok())
        .ok_or(Error::InvalidEmbeddingDimension(dimension))
}

fn is_missing_schema(e: &tokio_postgres::Error) -> bool {
    matches!(
        e.code(),
        Some(c) if *c == SqlState::INVALID_SCHEMA_NAME
            || *c == SqlState::UNDEFINED_TABLE
            || *c == SqlState::UNDEFINED_FUNCTION
    )
}

/// One caller's view of the Store.
///
/// Every method is a single call: one transaction of its own, which commits
/// when the call succeeds, on a connection of its own. A single call that
/// fails with a serialization failure or a deadlock runs again from the
/// start, a few times, before it returns [`Error::Conflict`]. To make several calls in
/// one transaction, such as a stage's output and its stage records, start a
/// [`Transaction`] with [`transaction`](Self::transaction).
pub struct Caller<'s> {
    store: &'s Store,
    role: CallerRole,
    claims: String,
}

/// Runs one call on a new transaction of `$caller` in `$tenant`, commits it
/// if the call succeeds and rolls it back if not, and runs it again from the
/// start after a conflict.
macro_rules! single {
    ($caller:expr, $tenant:expr, $tx:ident => $call:expr) => {{
        let caller: &Caller<'_> = $caller;
        let tenant: Option<TenantId> = $tenant;
        let mut attempt = 0;
        loop {
            let mut $tx = caller.begin(tenant).await?;
            let done = $call.await;
            match $tx.finish(done).await {
                Err(e) if attempt < CONFLICT_RETRIES && e.is_conflict() => attempt += 1,
                done => break done,
            }
        }
    }};
}

impl Caller<'_> {
    /// Starts a transaction of this caller in one Tenant, `None` naming the
    /// default Tenant of a single-tenant deployment. Every Store call is a
    /// method on it, and nothing it writes is saved until
    /// [`Transaction::commit`] succeeds.
    ///
    /// The transaction holds a connection of the pool until it ends, and a
    /// single call made meanwhile takes another one. No model call should
    /// run while it is open: a stage calls its model before it starts the
    /// transaction that writes the output. See [`Transaction`].
    pub async fn transaction(
        &self,
        tenant: impl Into<Option<TenantId>>,
    ) -> Result<Transaction, Error> {
        self.begin(tenant.into()).await
    }

    /// Takes a connection, opens a transaction on it with this caller's role
    /// and claims set locally, and checks the Tenant. A connection that
    /// turns out to be dead is closed, and the transaction starts once more
    /// on another.
    async fn begin(&self, tenant: Option<TenantId>) -> Result<Transaction, Error> {
        let mut replaced = false;
        loop {
            let mut conn = self.store.connection().await?;
            match self.start(&mut conn, tenant).await {
                Ok(tenant) => return Ok(Transaction::new(conn, tenant)),
                // Dropping `conn` closes it, since its transaction is open.
                Err(e) if e.is_lost_connection() && !replaced => replaced = true,
                Err(e) => {
                    if conn.is_open() {
                        let _ = conn.rollback().await;
                    }
                    return Err(e);
                }
            }
        }
    }

    async fn start(&self, conn: &mut Conn, tenant: Option<TenantId>) -> Result<TenantId, Error> {
        self.store.check_once(conn).await?;
        conn.begin(self.role.name()).await?;
        let rows = conn
            .query(
                CLAIMS,
                &[
                    (&self.claims, Type::TEXT),
                    (&tenant.map(|t| t.0), Type::UUID),
                ],
            )
            .await?;
        Ok(TenantId(rows[0].get(0)))
    }
}

/// The single calls.
impl Caller<'_> {
    /// Records an Episode and returns its new ID, a UUID v7.
    pub async fn record_episode(&self, episode: &NewEpisode) -> Result<EpisodeId, Error> {
        single!(self, episode.tenant, tx => tx.record_episode(episode))
    }

    /// Reads an Episode, or `None` if this caller cannot see it.
    pub async fn read_episode(
        &self,
        tenant: impl Into<Option<TenantId>>,
        id: EpisodeId,
    ) -> Result<Option<Episode>, Error> {
        single!(self, tenant.into(), tx => tx.read_episode(id))
    }

    /// Registers a Kind in the Tenant's Ontology and returns it flattened
    /// with its ancestors and with its compiled rules.
    ///
    /// Registering the same Kind again changes nothing and returns it. The
    /// deployment decides who may register Kinds: without its EXECUTE grant
    /// on `gnomon.register_kind`, the call fails with [`Error::Forbidden`].
    pub async fn register_kind(&self, kind: &NewKind) -> Result<Kind, Error> {
        single!(self, kind.tenant, tx => tx.register_kind(kind))
    }

    /// Sets a Kind's own merge rule. Ranked merge orders a Node's values by
    /// Authority, then Reference time, then Recorded time (ADR-0007). A
    /// Kind below a Kind that ranks inherits the rule, and cannot turn it
    /// off: the call fails with [`Error::InvalidKind`]. A change applies to
    /// later writes only. It changes no existing Version.
    ///
    /// The deployment decides who may set it: without its EXECUTE grant on
    /// `gnomon.set_merge_rule`, the call fails with [`Error::Forbidden`].
    pub async fn set_merge_rule(
        &self,
        tenant: impl Into<Option<TenantId>>,
        kind: &str,
        rule: MergeRule,
    ) -> Result<(), Error> {
        single!(self, tenant.into(), tx => tx.set_merge_rule(kind, rule))
    }

    /// The merge rule a Kind has in the Tenant's Ontology: ranked if it or
    /// any ancestor ranks, else recency.
    pub async fn merge_rule(
        &self,
        tenant: impl Into<Option<TenantId>>,
        kind: &str,
    ) -> Result<MergeRule, Error> {
        single!(self, tenant.into(), tx => tx.merge_rule(kind))
    }

    /// Reads a Kind of the Tenant's Ontology, a root Kind included, or `None`
    /// if this caller's Tenant has no such Kind.
    pub async fn read_kind(
        &self,
        tenant: impl Into<Option<TenantId>>,
        name: &str,
    ) -> Result<Option<Kind>, Error> {
        single!(self, tenant.into(), tx => tx.read_kind(name))
    }

    /// Creates a Node and writes its first patch, in one transaction.
    pub async fn create_node(
        &self,
        node: &NewNode,
        patch: &Patch,
    ) -> Result<(NodeId, WriteOutcome), Error> {
        single!(self, node.tenant, tx => tx.create_node(node, patch))
    }

    /// Writes a patch of property values for a Node, citing the patch's
    /// Episode, and recomputes the Node's Versions over its Valid time.
    pub async fn write_node(
        &self,
        tenant: impl Into<Option<TenantId>>,
        node: NodeId,
        patch: &Patch,
    ) -> Result<WriteOutcome, Error> {
        single!(self, tenant.into(), tx => tx.write_node(node, patch))
    }

    /// Writes several patches in one transaction, in order. A second write
    /// to the same Node replaces the Versions the first opened, in place, so
    /// the batch leaves no Version that no read could have seen (ADR-0008).
    pub async fn write_nodes(
        &self,
        tenant: impl Into<Option<TenantId>>,
        writes: &[(NodeId, Patch)],
    ) -> Result<Vec<WriteOutcome>, Error> {
        single!(self, tenant.into(), tx => tx.write_nodes(writes))
    }

    /// Creates an Edge and writes its first patch, in one transaction.
    ///
    /// The Store refuses an Edge whose Kind is not an Edge Kind of the
    /// Tenant, whose endpoint is not a Node of the Tenant that the caller
    /// can see, or whose endpoint's Kind the Edge Kind does not allow, with
    /// [`Error::InvalidWrite`]. The Edge holds over the patch's Valid time,
    /// and the patch may state no property value.
    ///
    /// For a symmetric Kind, [`SAME_AS_KIND`](crate::SAME_AS_KIND) or
    /// [`DIFFERENT_FROM_KIND`](crate::DIFFERENT_FROM_KIND), the call writes
    /// the patch to the Scope's Edge of that Kind between the two Nodes when
    /// there is one, in either order, and returns its ID. It refuses an Edge
    /// from a Node to itself.
    pub async fn create_edge(
        &self,
        edge: &NewEdge,
        patch: &Patch,
    ) -> Result<(EdgeId, WriteOutcome), Error> {
        single!(self, edge.tenant, tx => tx.create_edge(edge, patch))
    }

    /// Writes a patch of property values for an Edge, citing the patch's
    /// Episode, states that the Edge holds over the patch's Valid time, and
    /// recomputes the Edge's Versions over it. The patch may state no
    /// property value, and never states `holds`, which is built in.
    pub async fn write_edge(
        &self,
        tenant: impl Into<Option<TenantId>>,
        edge: EdgeId,
        patch: &Patch,
    ) -> Result<WriteOutcome, Error> {
        single!(self, tenant.into(), tx => tx.write_edge(edge, patch))
    }

    /// Writes several patches to Edges in one transaction, in order, as
    /// [`write_nodes`](Self::write_nodes) does for Nodes.
    pub async fn write_edges(
        &self,
        tenant: impl Into<Option<TenantId>>,
        writes: &[(EdgeId, Patch)],
    ) -> Result<Vec<WriteOutcome>, Error> {
        single!(self, tenant.into(), tx => tx.write_edges(writes))
    }

    /// Reads a Node As-of a valid-at and a known-at instant: the Version
    /// that held then, or `None` if none did or the caller cannot see the
    /// Node.
    pub async fn read_node(
        &self,
        tenant: impl Into<Option<TenantId>>,
        node: NodeId,
        as_of: AsOf,
    ) -> Result<Option<NodeVersion>, Error> {
        single!(self, tenant.into(), tx => tx.read_node(node, as_of))
    }

    /// Reads many Nodes As-of one valid-at and one known-at instant, in one
    /// call and one round trip: the Version of each Node that held then, in
    /// the order the Nodes are given, each Node once. A Node that held no
    /// Version then, or that the caller cannot see, has no entry.
    pub async fn read_nodes(
        &self,
        tenant: impl Into<Option<TenantId>>,
        nodes: &[NodeId],
        as_of: AsOf,
    ) -> Result<Vec<NodeVersion>, Error> {
        single!(self, tenant.into(), tx => tx.read_nodes(nodes, as_of))
    }

    /// Reads an Edge As-of a valid-at and a known-at instant: the Version
    /// that held then, with its Sentence if one is stored, or `None` if none
    /// held or the caller cannot see the Edge.
    pub async fn read_edge(
        &self,
        tenant: impl Into<Option<TenantId>>,
        edge: EdgeId,
        as_of: AsOf,
    ) -> Result<Option<EdgeVersion>, Error> {
        single!(self, tenant.into(), tx => tx.read_edge(edge, as_of))
    }

    /// Traverses the Edges of a Node As-of a valid-at and a known-at
    /// instant, in one call: each Edge the traversal follows that held a
    /// Version then, with the Node at its other end and that Node's Version
    /// then, if it held one. The steps come in Edge ID order.
    ///
    /// Only what the caller can see is returned. An Edge whose other end is
    /// a Node the caller cannot see is left out, and a Node the caller cannot
    /// see has no steps.
    pub async fn traverse(
        &self,
        tenant: impl Into<Option<TenantId>>,
        node: NodeId,
        as_of: AsOf,
        traversal: &Traversal,
    ) -> Result<Vec<Step>, Error> {
        single!(self, tenant.into(), tx => tx.traverse(node, as_of, traversal))
    }

    /// The full-text branch of Search (ADR-0010): the Versions whose
    /// searchable text holds a word of `query`, As-of `as_of`, best first,
    /// at most `limit` of them. Each is ranked by BM25 from the word counts
    /// of the caller's allowed Scopes only, so another Scope's text never
    /// changes the order. Words are lower-cased and split as Postgres's
    /// `simple` text search configuration splits them, with no stemming and
    /// no stop words. A Version is found as soon as the write that opened it
    /// commits, before any embedding is stored.
    ///
    /// The searchable text of a Version is what its Kind marks: the
    /// properties it names, a Node's name and Aliases, and an Edge
    /// Version's Sentence once one is stored. A `limit` below 1 fails with
    /// [`Error::Database`] and SQLSTATE 22023.
    pub async fn search_full_text(
        &self,
        tenant: impl Into<Option<TenantId>>,
        query: &str,
        as_of: AsOf,
        limit: i32,
    ) -> Result<Vec<FullTextCandidate>, Error> {
        single!(self, tenant.into(), tx => tx.search_full_text(query, as_of, limit))
    }

    /// Stores the embedding of a Version's text, after the write that opened
    /// the Version committed (ADR-0010). `text_hash` names the text that was
    /// embedded: the Version's [`text_hash`](NodeVersion::text_hash), or, for
    /// an Edge Version, the hash of its Sentence, which SQL's
    /// `gnomon.sentence_text_hash` gives. Versions with the same text in a
    /// Scope share one full vector: when one is stored already it stays, and
    /// `embedding` is not used, so a Refinement needs no new embedding. Each
    /// Version gets its own quantized row. Storing a Version's embedding again
    /// with another of its texts moves it to that text.
    ///
    /// An `embedding` of another length than the embedding dimension, of
    /// length zero, or holding a number that is not finite fails with
    /// [`Error::Database`] and SQLSTATE 22023. A Version the caller cannot
    /// see, or a hash of neither of its texts, fails with
    /// [`Error::InvalidWrite`].
    pub async fn store_embedding(
        &self,
        tenant: impl Into<Option<TenantId>>,
        version: VersionId,
        text_hash: &str,
        embedding: &[f32],
    ) -> Result<(), Error> {
        single!(self, tenant.into(), tx => tx.store_embedding(version, text_hash, embedding))
    }

    /// Stores the embedding of `name`, a current name or Alias of `node`,
    /// for Resolution (`0004_search.sql`). The full vector is stored once per
    /// text in the Node's Scope, the same row a Sentence or another name with
    /// that text uses: when one is stored already it stays, and `embedding`
    /// is not used. Every current name and Alias of the Scope with that text
    /// is then matched by vector, and so is every one written later, so a
    /// new Version that keeps its name needs no new embedding.
    ///
    /// An `embedding` that [`store_embedding`](Self::store_embedding) would
    /// refuse fails with [`Error::Database`] and SQLSTATE 22023. A Node the
    /// caller cannot see, or a text that is not one of its current names or
    /// Aliases, fails with [`Error::InvalidWrite`].
    pub async fn store_name_embedding(
        &self,
        tenant: impl Into<Option<TenantId>>,
        node: NodeId,
        name: &str,
        embedding: &[f32],
    ) -> Result<(), Error> {
        single!(self, tenant.into(), tx => tx.store_name_embedding(node, name, embedding))
    }

    /// The vector branch of Search (ADR-0010): the Versions whose stored
    /// embeddings are nearest `query` by cosine distance, As-of `as_of`,
    /// nearest first, at most `limit` of them. The search is exact over the
    /// Versions the caller can see, whatever the size of its Tenant and
    /// Scopes: the 400 nearest by the quantized embeddings, or `limit` when it
    /// is larger, are re-ranked at full precision. A Version whose embedding
    /// is not stored yet is not found here; the full-text branch finds it.
    ///
    /// A `limit` below 1, or a `query` that
    /// [`store_embedding`](Self::store_embedding) would refuse, fails with
    /// [`Error::Database`] and SQLSTATE 22023.
    pub async fn search_vector(
        &self,
        tenant: impl Into<Option<TenantId>>,
        query: &[f32],
        as_of: AsOf,
        limit: i32,
    ) -> Result<Vec<VectorCandidate>, Error> {
        single!(self, tenant.into(), tx => tx.search_vector(query, as_of, limit))
    }

    /// Resolution (`0004_search.sql`): the Nodes whose current names and
    /// Aliases match `mention`, best first, at most `limit` of them, each with
    /// the name or Alias it matched. The full-text half scores each name and
    /// Alias by BM25 from the word counts of the caller's allowed Scopes, and
    /// finds a name as soon as the write that gave it commits. The vector
    /// half runs when the mention carries an embedding. The halves are fused
    /// by reciprocal rank fusion, and each candidate says what each half
    /// found and at what rank.
    ///
    /// A name is current while its Version is the current belief. A name a
    /// later write, a Correction or a Retraction replaced is never matched. A
    /// name a Node had over an earlier Valid time is still matched, unless
    /// the mention names a valid-at instant. Resolution merges nothing
    /// (ADR-0012).
    ///
    /// A `limit` below 1, or an embedding that
    /// [`store_embedding`](Self::store_embedding) would refuse, fails with
    /// [`Error::Database`] and SQLSTATE 22023.
    pub async fn resolve(
        &self,
        tenant: impl Into<Option<TenantId>>,
        mention: &Mention<'_>,
        limit: i32,
    ) -> Result<Vec<ResolutionCandidate>, Error> {
        single!(self, tenant.into(), tx => tx.resolve(mention, limit))
    }

    /// States a bound of a current Version's Valid time, citing the
    /// statement's Episode (ADR-0008). A bound the Version left unknown, or a
    /// narrower precision within the same period, is a Refinement. A bound
    /// outside that period, or a coarser one, contradicts the Version and is
    /// recorded as a Correction. Either way the earlier Version stays
    /// readable As-of before the statement.
    pub async fn state_bound(
        &self,
        tenant: impl Into<Option<TenantId>>,
        node: NodeId,
        statement: &BoundStatement,
    ) -> Result<BoundOutcome, Error> {
        single!(self, tenant.into(), tx => tx.state_bound(node, statement))
    }

    /// States a bound of an Edge's current Version, as
    /// [`state_bound`](Self::state_bound) does for a Node. The Edge holds
    /// over the stated bound.
    pub async fn state_edge_bound(
        &self,
        tenant: impl Into<Option<TenantId>>,
        edge: EdgeId,
        statement: &BoundStatement,
    ) -> Result<BoundOutcome, Error> {
        single!(self, tenant.into(), tx => tx.state_edge_bound(edge, statement))
    }

    /// Records a Correction of a Node: a patch that replaces what the
    /// Node's earlier Version said over the same Valid time, because that
    /// Version was wrong. The Node must have a current Version over the
    /// patch's Valid time, or the Store refuses it with
    /// [`Error::InvalidWrite`]. The earlier Version is closed in Recorded
    /// time, so a read at an earlier known-at instant still returns it. The
    /// Correction is held like a Contribution: under ranked merge a
    /// Correction from a lower-Authority Episode does not replace values a
    /// higher-Authority Episode supplied where their Valid times overlap,
    /// and takes effect if that Episode is erased (ADR-0007).
    pub async fn correct_node(
        &self,
        tenant: impl Into<Option<TenantId>>,
        node: NodeId,
        patch: &Patch,
    ) -> Result<WriteOutcome, Error> {
        single!(self, tenant.into(), tx => tx.correct_node(node, patch))
    }

    /// Records a Correction of an Edge, as
    /// [`correct_node`](Self::correct_node) does for a Node. The patch may
    /// state no property value: it then corrects only that the Edge holds.
    pub async fn correct_edge(
        &self,
        tenant: impl Into<Option<TenantId>>,
        edge: EdgeId,
        patch: &Patch,
    ) -> Result<WriteOutcome, Error> {
        single!(self, tenant.into(), tx => tx.correct_edge(edge, patch))
    }

    /// Records an End of validity of a Node: what its current Version stated
    /// was true and stopped being true at the stated instant. The Version is
    /// replaced by one that ends there, and stays current As-of earlier
    /// valid-at instants. The instant must fall inside a current Version, not
    /// at its start, which is a Retraction.
    pub async fn end_node_validity(
        &self,
        tenant: impl Into<Option<TenantId>>,
        node: NodeId,
        end: &EndOfValidity,
    ) -> Result<WriteOutcome, Error> {
        single!(self, tenant.into(), tx => tx.end_node_validity(node, end))
    }

    /// Records an End of validity of an Edge: the Edge holds no Version from
    /// the stated instant, whatever property values remain.
    pub async fn end_edge_validity(
        &self,
        tenant: impl Into<Option<TenantId>>,
        edge: EdgeId,
        end: &EndOfValidity,
    ) -> Result<WriteOutcome, Error> {
        single!(self, tenant.into(), tx => tx.end_edge_validity(edge, end))
    }

    /// Records a Retraction of a current Version of a Node: what it stated
    /// was never true. The Version stops being current, and a read at an
    /// earlier known-at instant still returns it.
    pub async fn retract_node_version(
        &self,
        tenant: impl Into<Option<TenantId>>,
        node: NodeId,
        retraction: &Retraction,
    ) -> Result<WriteOutcome, Error> {
        single!(self, tenant.into(), tx => tx.retract_node_version(node, retraction))
    }

    /// Records a Retraction of a current Version of an Edge.
    pub async fn retract_edge_version(
        &self,
        tenant: impl Into<Option<TenantId>>,
        edge: EdgeId,
        retraction: &Retraction,
    ) -> Result<WriteOutcome, Error> {
        single!(self, tenant.into(), tx => tx.retract_edge_version(edge, retraction))
    }

    /// Retracts an Episode: records a Retraction, made by that Episode, of
    /// every current Version of a Node or an Edge that only that Episode
    /// cites, and returns those Versions. A Version another Episode also
    /// cites stays current. An As-of read at a known-at instant before the
    /// Retraction still returns each retracted Version, because nothing the
    /// Episode wrote is deleted: that is Erasure's job, not Retract Episode's
    /// (ADR-0003).
    ///
    /// The Episode's own statements, held or in effect, stop counting, so a
    /// held statement does not take effect if a higher-Authority Episode is
    /// erased later (ADR-0007). A Retraction that Retract Episode makes
    /// from a lower-Authority Episode does not end values a higher-Authority
    /// Episode supplied where their Valid times overlap, with ranked merge
    /// on. Retracting an Episode twice retracts nothing the second time.
    pub async fn retract_episode(
        &self,
        tenant: impl Into<Option<TenantId>>,
        episode: EpisodeId,
    ) -> Result<EpisodeRetraction, Error> {
        single!(self, tenant.into(), tx => tx.retract_episode(episode))
    }

    /// Erases an Episode (ADR-0003, ADR-0007). In one transaction it deletes
    /// the Episode, its Contributions, its statements, its Citations and
    /// every Version derived from it alone. It recomputes every Version the
    /// Episode touched, current and closed, from the Contributions that
    /// remain, each under the merge rule it recorded. As-of reads afterwards
    /// return the history as if the Episode had never existed. A Node or
    /// Edge left with nothing is erased too, and a Node only once no Edge
    /// joins it, in whichever Scope it is. The result names only the Nodes
    /// and Edges the caller can read.
    ///
    /// Erasure is a definer function that the migrations grant to no caller
    /// role, so a call fails with [`Error::Forbidden`] until the deployment
    /// grants EXECUTE on `gnomon.erase_episode` (ADR-0009). The Episode must
    /// be one the caller can read and write. Writes to the Nodes and Edges
    /// the Episode touched wait for it to commit.
    pub async fn erase_episode(
        &self,
        tenant: impl Into<Option<TenantId>>,
        episode: EpisodeId,
    ) -> Result<EpisodeErasure, Error> {
        single!(self, tenant.into(), tx => tx.erase_episode(episode))
    }

    /// Erases a Node (ADR-0003): its Versions, Contributions, statements and
    /// Citations, and every Edge that joins it, whatever the Edge's Scope,
    /// with that Edge's own rows, and every Summary of the Node in every
    /// Scope. An endpoint of such an Edge left with nothing is erased too, in
    /// whichever Scope it is, when no other Edge joins it and no Summary or
    /// Community names it. So the same request removes the same rows for
    /// every caller, and the result names only the Nodes and Edges the caller
    /// can read. The Episodes stay, and
    /// [`list_node_episodes`](Self::list_node_episodes) lists them. The Node must be one the caller
    /// can read and write, and a hidden one fails as an absent one does.
    ///
    /// Erasure is granted to no caller role by the migrations, so a call
    /// fails with [`Error::Forbidden`] until the deployment grants EXECUTE on
    /// `gnomon.erase_node` (ADR-0009).
    pub async fn erase_node(
        &self,
        tenant: impl Into<Option<TenantId>>,
        node: NodeId,
    ) -> Result<GraphErasure, Error> {
        single!(self, tenant.into(), tx => tx.erase_node(node))
    }

    /// Erases an Edge (ADR-0003): its Versions, Contributions, statements,
    /// Citations and Sentences. Each endpoint left with nothing is erased too,
    /// as [`erase_node`](Self::erase_node) erases one, and the Episodes stay.
    /// The Edge must be one the caller can read and write. It is granted as
    /// [`erase_node`](Self::erase_node) is, on `gnomon.erase_edge`.
    pub async fn erase_edge(
        &self,
        tenant: impl Into<Option<TenantId>>,
        edge: EdgeId,
    ) -> Result<GraphErasure, Error> {
        single!(self, tenant.into(), tx => tx.erase_edge(edge))
    }

    /// Erases a Scope (ADR-0003): every row in it, its Episodes among them,
    /// and every Edge of another Scope that joins one of its Nodes, with that
    /// Edge's own rows, and every Summary of one of its Nodes in another
    /// Scope. An endpoint outside the Scope left with nothing is erased too,
    /// as [`erase_node`](Self::erase_node) erases one. Nothing else outside
    /// the Scope changes. The caller must be able to read and write the Scope.
    /// It is granted as [`erase_node`](Self::erase_node) is, on
    /// `gnomon.erase_scope`.
    pub async fn erase_scope(
        &self,
        tenant: impl Into<Option<TenantId>>,
        scope: &Scope,
    ) -> Result<GraphErasure, Error> {
        single!(self, tenant.into(), tx => tx.erase_scope(scope))
    }

    /// Lists the statements about a Node: its Corrections, Ends of validity
    /// and Retractions, in the order they were recorded, with the Episode
    /// that made each. A statement that lost on Authority is listed too,
    /// because it stays held.
    pub async fn list_node_statements(
        &self,
        tenant: impl Into<Option<TenantId>>,
        node: NodeId,
    ) -> Result<Vec<crate::Statement>, Error> {
        single!(self, tenant.into(), tx => tx.list_node_statements(node))
    }

    /// Lists the statements about an Edge, as
    /// [`list_node_statements`](Self::list_node_statements) does for a Node.
    pub async fn list_edge_statements(
        &self,
        tenant: impl Into<Option<TenantId>>,
        edge: EdgeId,
    ) -> Result<Vec<crate::Statement>, Error> {
        single!(self, tenant.into(), tx => tx.list_edge_statements(edge))
    }

    /// Lists the Episodes behind a Node, each once, with its Scope, in order
    /// of ID: those that state a value, a statement or a bound of the Node or
    /// of an Edge that joins it, those that a current or closed Version of
    /// either cites, and those that a current or closed Summary of the Node
    /// cites. Communities are left out, because they cite Episodes about
    /// other Nodes too.
    ///
    /// Erase Node and Erase Edge keep these Episodes, so their text can still
    /// name what the Node was (ADR-0003). The README's data protection
    /// section gives the steps that erase them too.
    ///
    /// It runs as the caller and lists only Episodes the caller can read, so
    /// a caller who cannot read every Scope gets a shorter list. The Node
    /// need not be one the caller can read: a worker reads one Scope, so the
    /// call is repeated with the worker of each.
    pub async fn list_node_episodes(
        &self,
        tenant: impl Into<Option<TenantId>>,
        node: NodeId,
    ) -> Result<Vec<NodeEpisode>, Error> {
        single!(self, tenant.into(), tx => tx.list_node_episodes(node))
    }

    /// Lists the Citations of a Version: each Episode it was derived from,
    /// with the property and the place they name.
    pub async fn list_citations(
        &self,
        tenant: impl Into<Option<TenantId>>,
        version: VersionId,
    ) -> Result<Vec<Citation>, Error> {
        single!(self, tenant.into(), tx => tx.list_citations(version))
    }

    /// Writes stage records in one transaction of their own (ADR-0011), such
    /// as the pending record of an Episode's first stage, or the retry of a
    /// stage whose output rolled back. A stage that writes output writes its
    /// records in the [`Transaction`] that writes the output, with
    /// [`Transaction::write_stages`]. A pending record is refused with
    /// [`Error::InvalidWrite`] for an Episode the caller cannot see, and any
    /// write with a stage name, model name or error code that is not an
    /// identifier fails with a check violation.
    pub async fn write_stages(
        &self,
        tenant: impl Into<Option<TenantId>>,
        writes: &[StageWrite],
    ) -> Result<(), Error> {
        single!(self, tenant.into(), tx => tx.write_stages(writes))
    }

    /// Claims one due stage record in the caller's Scopes, or of `episode`
    /// only: one that is pending, retry-due, or running with an expired
    /// lease, the one due longest first. It sets the record running with a
    /// lease of `lease` from now, by the database's clock, counts the
    /// attempt, commits and returns the record, or `None` when nothing is
    /// due. Two concurrent claims never return the same record, because the
    /// claim skips a record another claim holds (`FOR UPDATE SKIP LOCKED`).
    /// A lease of zero expires at once, so the next claim may take the
    /// record again.
    pub async fn claim_stage(
        &self,
        tenant: impl Into<Option<TenantId>>,
        episode: Option<EpisodeId>,
        lease: Duration,
    ) -> Result<Option<StageRecord>, Error> {
        single!(self, tenant.into(), tx => tx.claim_stage(episode, lease))
    }

    /// Reads every stage record of an Episode, with its usage counts, in the
    /// order they were written. A caller that cannot see the Episode reads
    /// none.
    pub async fn stage_records(
        &self,
        tenant: impl Into<Option<TenantId>>,
        episode: EpisodeId,
    ) -> Result<Vec<StageRecord>, Error> {
        single!(self, tenant.into(), tx => tx.stage_records(episode))
    }

    /// Writes a Summary of a Node and returns its ID. The Node's current
    /// Summary in the same Scope, if it has one, is closed at the start of
    /// this call's transaction and stays readable As-of an earlier known-at
    /// instant (ADR-0015).
    ///
    /// A Summary is a stage's output, so a stage writes it and the stage's
    /// records in one [`Transaction`] (ADR-0011).
    /// A Scope or Node the caller cannot see, no Citation, or a Citation of an
    /// Episode the caller cannot see or of another Scope than the Summary's,
    /// fails with [`Error::InvalidWrite`] (ADR-0014).
    pub async fn write_summary(
        &self,
        tenant: impl Into<Option<TenantId>>,
        summary: &NewSummary,
    ) -> Result<SummaryId, Error> {
        single!(self, tenant.into(), tx => tx.write_summary(summary))
    }

    /// Writes a Community of Nodes with its Summary and returns its ID. The
    /// current Communities it names in `replaces` are closed, as
    /// [`write_summary`](Self::write_summary) closes a Summary. A stage
    /// writes it with its stage records in one [`Transaction`], as it writes a
    /// Summary.
    ///
    /// Every Node must be one the caller sees in the Community's own Scope,
    /// and a Node of another Scope is refused even when the caller sees it
    /// (ADR-0014). That, no Node, a Community in `replaces` that is not a
    /// current one of the Scope, and the refusals of `write_summary`, fail
    /// with [`Error::InvalidWrite`].
    pub async fn write_community(
        &self,
        tenant: impl Into<Option<TenantId>>,
        community: &NewCommunity,
    ) -> Result<CommunityId, Error> {
        single!(self, tenant.into(), tx => tx.write_community(community))
    }

    /// Reads the Summaries of a Node As-of `as_of`: in each of the caller's
    /// Scopes, the one held at the known-at instant, if it was written for a
    /// valid-at instant on or before `as_of.valid_at` (ADR-0015). They come in
    /// order of Scope. A Node the caller cannot see has none.
    pub async fn read_summaries(
        &self,
        tenant: impl Into<Option<TenantId>>,
        node: NodeId,
        as_of: AsOf,
    ) -> Result<Vec<Summary>, Error> {
        single!(self, tenant.into(), tx => tx.read_summaries(node, as_of))
    }

    /// Reads the Communities that hold a Node As-of `as_of`, as
    /// [`read_summaries`](Self::read_summaries) reads Summaries, in order of
    /// Scope and then of ID.
    pub async fn read_communities(
        &self,
        tenant: impl Into<Option<TenantId>>,
        node: NodeId,
        as_of: AsOf,
    ) -> Result<Vec<Community>, Error> {
        single!(self, tenant.into(), tx => tx.read_communities(node, as_of))
    }

    /// Search: one fused, ranked list of the Versions, Summaries and
    /// Communities that match `request` As-of its instants, best first, at most
    /// `limit` of them.
    ///
    /// Three branches find candidates: the vector branch, when the query
    /// carries an embedding, the full-text branch, and the neighbourhood
    /// branch, which follows the Edges of the request's seed Nodes, or of the
    /// ten Nodes the other two branches rank best when it names none, to
    /// depth 1 or 2, at most 50 Edges per Node, the most-mentioned first.
    /// Reciprocal rank fusion with k = 60 combines each branch's best 100, or
    /// `limit` when that is more. Two graph signals then reorder the fused
    /// candidates: mentions, the Episodes citing a candidate, and
    /// connectedness, the Edges joining the Nodes it is about. Both are
    /// counted at search time over the rows the caller can see at the As-of
    /// instants, and neither adds a candidate. A Summary or Community is
    /// found only when its Recorded time contains the known-at instant and
    /// the valid-at instant is on or after the one it was written for
    /// (ADR-0015).
    ///
    /// Each result reports where each branch and signal ranked it.
    ///
    /// A `limit` below 1, a depth other than 1 or 2, a weight below 0 or not
    /// finite, or an embedding that [`store_embedding`](Self::store_embedding)
    /// would refuse fails with [`Error::Database`] and SQLSTATE 22023.
    pub async fn search(
        &self,
        tenant: impl Into<Option<TenantId>>,
        request: &SearchRequest<'_>,
        limit: i32,
    ) -> Result<Vec<SearchResult>, Error> {
        single!(self, tenant.into(), tx => tx.search(request, limit))
    }

    /// Stores the embedding of a Summary's text, after the write that made
    /// it committed, for the vector branch of [`search`](Self::search). A
    /// Summary's text never changes, so its first embedding stays and a
    /// second call does not use `embedding`. A Summary the caller cannot see
    /// fails with [`Error::InvalidWrite`], and an `embedding` that
    /// [`store_embedding`](Self::store_embedding) would refuse with
    /// [`Error::Database`] and SQLSTATE 22023.
    pub async fn store_summary_embedding(
        &self,
        tenant: impl Into<Option<TenantId>>,
        summary: SummaryId,
        embedding: &[f32],
    ) -> Result<(), Error> {
        single!(self, tenant.into(), tx => tx.store_summary_embedding(summary, embedding))
    }

    /// Stores the embedding of a Community's text, as
    /// [`store_summary_embedding`](Self::store_summary_embedding) stores a
    /// Summary's.
    pub async fn store_community_embedding(
        &self,
        tenant: impl Into<Option<TenantId>>,
        community: CommunityId,
        embedding: &[f32],
    ) -> Result<(), Error> {
        single!(self, tenant.into(), tx => tx.store_community_embedding(community, embedding))
    }

    /// Stores the Sentence of an Edge Version, after the write that made the
    /// Version committed, replacing any stored before (ADR-0013). It writes
    /// no Version and changes no Recorded time. The Version may be closed: a
    /// Search at an earlier known-at instant finds it by its Sentence.
    ///
    /// The Sentence's words and BM25 counts are written in the same
    /// transaction, so full-text Search finds the Version by them once this
    /// call returns, before any embedding exists. The Pipeline then embeds
    /// it with [`store_embedding`](Self::store_embedding) and the hash
    /// `gnomon.sentence_text_hash` gives. A replaced Sentence's embedding is
    /// deleted with it, and its full vector unless another row uses it, so
    /// the replaced text is found by no Search.
    ///
    /// A Sentence is a stage's output, so a stage stores it and the stage's
    /// records in one [`Transaction`] (ADR-0011).
    /// A Version that is not an Edge Version the caller can see fails with
    /// [`Error::InvalidWrite`], and a Sentence with no word with
    /// [`Error::Database`] and SQLSTATE 22023.
    pub async fn store_sentence(
        &self,
        tenant: impl Into<Option<TenantId>>,
        version: VersionId,
        sentence: &str,
    ) -> Result<(), Error> {
        single!(self, tenant.into(), tx => tx.store_sentence(version, sentence))
    }

    /// One page of the gaps of `scope`, the Scope's rows the caller sees
    /// that `maintain` repairs: at most `limit`, after `after`, or from the
    /// first when it is `None`. See [`Gap`](crate::Gap) for each kind. The order is the
    /// same on every call: by kind, in the order of [`Gap`](crate::Gap)'s variants, then
    /// by ID and text hash. A gap that is listed on every call is listed once
    /// as the pages are read, however the gaps change between them.
    ///
    /// A Scope the caller cannot see fails with [`Error::InvalidWrite`], and
    /// a `limit` below 1 with [`Error::Database`] and SQLSTATE 22023.
    pub async fn list_gaps(
        &self,
        tenant: impl Into<Option<TenantId>>,
        scope: &Scope,
        after: Option<&GapCursor>,
        limit: i32,
    ) -> Result<GapPage, Error> {
        single!(self, tenant.into(), tx => tx.list_gaps(scope, after, limit))
    }
}
