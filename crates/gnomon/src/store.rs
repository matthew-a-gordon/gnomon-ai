//! The Store API: every call runs as a caller, under RLS, in one explicit
//! transaction (ADR-0001, ADR-0009).
//!
//! Each call is
//!
//! ```sql
//! begin;
//! set local role <authenticated | gnomon_worker>;
//! <set request.jwt.claims locally, then gnomon.call_tenant(<the Tenant or null>)>;
//! <the call>;
//! commit;
//! ```
//!
//! so a pooled server connection never carries one caller's role or claims
//! into another caller's call. `gnomon.call_tenant` checks the call's Tenant
//! against the claims, or picks the default Tenant for a call that names
//! none in single-tenant mode, and the call goes on with the Tenant it
//! returns (ADR-0005). By default every parameterized statement goes through
//! `query_typed`, which sends the unnamed statement, because named statements
//! fail on some transaction-mode poolers at their default settings (the
//! `prototype/pooler-prepared-statements` branch, ADR-0001). Named
//! statements are sent only when the deployment turns them on with
//! [`ConnectOptions::named_statements`].

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use std::collections::HashMap;
use tokio::sync::Mutex;
use tokio_postgres::config::SslMode;
use tokio_postgres::error::SqlState;

use tokio_postgres::types::{ToSql, Type};
use tokio_postgres::{Client, Config, NoTls, Row, Statement, Transaction};

use crate::connect::ConnectOptions;
use crate::migrations::{EmbeddingDimension, SCHEMA_VERSION};
use crate::{
    AsOf, Bound, BoundChange, BoundOutcome, BoundSide, BoundStatement, Citation, Direction, EdgeId,
    EdgeVersion, EndOfValidity, Episode, EpisodeErasure, EpisodeId, EpisodeRetraction, Error,
    GraphErasure, Kind, KindOf, MergeRule, NewEdge, NewEpisode, NewKind, NewNode, NodeId,
    NodeVersion, Patch, Precision, Retraction, Scope, StatementKind, Step, TenantId, Traversal,
    VacuumMode, VacuumReport, ValidTime, VersionId, WriteOutcome, WriteStatus,
};

/// How many times a write is retried when Postgres fails it with a
/// serialization failure or a deadlock, which a write to a Node that a
/// concurrent transaction wrote first can raise.
const WRITE_RETRIES: usize = 8;

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

/// A connection to a gnomon Store.
///
/// The Store holds one connection and runs one call at a time on it.
pub struct Store {
    client: Mutex<Client>,
    statements: Statements,
    embedding_dimension: EmbeddingDimension,
}

impl Store {
    /// Connects as the login role and checks the schema version.
    ///
    /// `connection` is a libpq-style URL or key=value string for the login
    /// role, which must be able to `SET ROLE` to `authenticated` and
    /// `gnomon_worker`. `options` decide TLS and the statement mode. By
    /// default the Store connects with TLS and verifies the server's
    /// certificate, and the connection string's `sslmode` cannot turn that
    /// off: `sslmode=disable` with TLS on is refused, and so is
    /// `sslmode=require` with [`Tls::plaintext`](crate::Tls::plaintext).
    ///
    /// The check runs once, here. A Store that connected before a
    /// migration was applied keeps the version it checked until it
    /// reconnects. Behind a transaction-mode pooler each call may run on
    /// another server connection, but every one of them is to the same
    /// database, so the version holds for all of them.
    pub async fn connect(connection: &str, options: &ConnectOptions) -> Result<Store, Error> {
        let mut config: Config = connection.parse()?;
        let ssl_mode = config.get_ssl_mode();
        let mut client = match options.tls.connector()? {
            None => {
                if matches!(ssl_mode, SslMode::Require) {
                    return Err(Error::InvalidConnection(
                        "the connection string sets sslmode=require, but the Store's TLS \
                         setting is Tls::plaintext()"
                            .into(),
                    ));
                }
                config.ssl_mode(SslMode::Disable);
                spawn_connection(config.connect(NoTls).await?)
            }
            Some(tls) => {
                if matches!(ssl_mode, SslMode::Disable) {
                    return Err(Error::InvalidConnection(
                        "the connection string sets sslmode=disable, which does not turn TLS \
                         off; connect with Tls::plaintext() to connect without TLS"
                            .into(),
                    ));
                }
                // Never fall back to plain text, as libpq's default `prefer` does.
                config.ssl_mode(SslMode::Require);
                spawn_connection(config.connect(tls).await?)
            }
        };
        let statements = Statements::new(options.named_statements);
        let embedding_dimension = check_schema(&mut client, &statements).await?;
        Ok(Store {
            client: Mutex::new(client),
            statements,
            embedding_dimension,
        })
    }

    /// The embedding dimension the migrations were exported with.
    pub fn embedding_dimension(&self) -> EmbeddingDimension {
        self.embedding_dimension
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
    /// The claims are
    /// `{"role": "gnomon_worker", "app_metadata": {"tenant_id": <tenant>, "scopes": [<scope>]}}`,
    /// without `"tenant_id"` for `None`. A deployment that replaces
    /// `gnomon.tenant_id()` and `gnomon.allowed_scopes()` must read them.
    pub fn as_worker(&self, tenant: impl Into<Option<TenantId>>, scope: Scope) -> Caller<'_> {
        let mut app_metadata = Map::new();
        if let Some(tenant) = tenant.into() {
            app_metadata.insert("tenant_id".into(), Value::String(tenant.0.to_string()));
        }
        app_metadata.insert("scopes".into(), serde_json::json!([scope.as_str()]));
        let claims = serde_json::json!({
            "role": CallerRole::Worker.name(),
            "app_metadata": app_metadata,
        });
        Caller {
            store: self,
            role: CallerRole::Worker,
            claims: claims.to_string(),
        }
    }
}

impl Store {
    /// Vacuums the tables a given Erasure touched, its
    /// [`touched_tables`](crate::EpisodeErasure::touched_tables), and
    /// returns once every one is vacuumed (ADR-0003). Until then copies of
    /// the erased bytes stay in their dead tuples and index pages.
    ///
    /// The call is not a Store function and runs as no caller role:
    /// `VACUUM` cannot run inside a transaction block, so each table's
    /// `VACUUM` goes as a simple query of one statement, outside any
    /// transaction, on this Store's connection, as the login role it
    /// connected as. Through a transaction-mode pooler that is a transaction
    /// of its own. A deployment connects a Store for the call as a login
    /// role of its own. It grants that role `gnomon_maintainer` with
    /// `with inherit true, set false`, and `authenticated` for the schema
    /// check [`Store::connect`] runs. Migration 0013 grants
    /// `gnomon_maintainer` `MAINTAIN` on every table an Erasure touches.
    ///
    /// It refuses with [`Error::VacuumRefused`], and vacuums nothing, when a
    /// table is not one an Erasure touches or the login role may not vacuum
    /// it. `VACUUM` itself would skip such a table with only a warning.
    /// `VACUUM` removes only dead tuples no open transaction can still see,
    /// so a transaction that began before the Erasure committed keeps them.
    pub async fn vacuum_erasure(
        &self,
        tables: &[String],
        mode: VacuumMode,
    ) -> Result<VacuumReport, Error> {
        let mut client = self.client.lock().await;
        let erasure_tables = self.erasure_vacuum_tables(&mut client).await?;
        let unknown: Vec<&str> = tables
            .iter()
            .filter(|t| !erasure_tables.iter().any(|e| &e.name == *t))
            .map(String::as_str)
            .collect();
        if !unknown.is_empty() {
            return Err(Error::VacuumRefused(format!(
                "{} {} not a table an Erasure touches",
                unknown.join(", "),
                if unknown.len() == 1 { "is" } else { "are" }
            )));
        }
        let chosen = erasure_tables
            .into_iter()
            .filter(|e| tables.contains(&e.name))
            .collect();
        vacuum_tables(&client, chosen, mode).await
    }

    /// Vacuums every table any Erasure touches, and returns once every one
    /// is vacuumed. That covers every table touched by every Erasure since
    /// the call last ran, without a record of past Erasures, which would be
    /// a trace of them. It runs and refuses as
    /// [`vacuum_erasure`](Self::vacuum_erasure) does. The tables come from
    /// the catalog (`gnomon.erasure_vacuum_tables()`), so a table a later
    /// migration adds is among them.
    pub async fn vacuum_erasures(&self, mode: VacuumMode) -> Result<VacuumReport, Error> {
        let mut client = self.client.lock().await;
        let erasure_tables = self.erasure_vacuum_tables(&mut client).await?;
        vacuum_tables(&client, erasure_tables, mode).await
    }

    /// Every table an Erasure touches, and whether the login role may
    /// vacuum it, read in a transaction of its own.
    async fn erasure_vacuum_tables(&self, client: &mut Client) -> Result<Vec<ErasureTable>, Error> {
        let tx = client.transaction().await?;
        let rows = self
            .statements
            .query(
                &tx,
                "select table_name, maintainable from gnomon.erasure_vacuum_tables()",
                &[],
            )
            .await?;
        tx.commit().await?;
        Ok(rows
            .iter()
            .map(|r| ErasureTable {
                name: r.get(0),
                maintainable: r.get(1),
            })
            .collect())
    }
}

/// A table an Erasure touches, as `gnomon.erasure_vacuum_tables()` lists it.
struct ErasureTable {
    /// Schema-qualified and quoted by the catalog's `format('%I.%I')`, so it
    /// can go into a `VACUUM` statement as it is.
    name: String,
    /// Whether the login role may vacuum it.
    maintainable: bool,
}

/// Vacuums `tables`, each with its own `VACUUM` outside any transaction,
/// after checking that the login role may vacuum every one.
async fn vacuum_tables(
    client: &Client,
    tables: Vec<ErasureTable>,
    mode: VacuumMode,
) -> Result<VacuumReport, Error> {
    let refused: Vec<&str> = tables
        .iter()
        .filter(|t| !t.maintainable)
        .map(|t| t.name.as_str())
        .collect();
    if !refused.is_empty() {
        return Err(Error::VacuumRefused(format!(
            "the login role may not vacuum {}: grant it gnomon_maintainer, and MAINTAIN on \
             each table to gnomon_maintainer",
            refused.join(", ")
        )));
    }
    let tables: Vec<String> = tables.into_iter().map(|t| t.name).collect();
    for table in &tables {
        client
            .batch_execute(&format!("vacuum ({}) {table}", mode.options()))
            .await?;
    }
    // Outside a transaction too, so not through `Statements`, which runs in
    // one. `query_typed` sends the unnamed statement.
    let row = client.query_typed("select clock_timestamp()", &[]).await?;
    Ok(VacuumReport {
        tables,
        mode,
        finished_at: row[0].get(0),
    })
}

/// How the Store sends its parameterized statements: unnamed through
/// `query_typed` by default, or, with named statements turned on, prepared
/// once per connection under a name and reused.
struct Statements {
    named: Option<std::sync::Mutex<HashMap<&'static str, Statement>>>,
}

impl Statements {
    fn new(named: bool) -> Statements {
        Statements {
            named: named.then(Default::default),
        }
    }

    /// Runs `sql` with `params` in `tx`.
    async fn query(
        &self,
        tx: &Transaction<'_>,
        sql: &'static str,
        params: &[(&(dyn ToSql + Sync), Type)],
    ) -> Result<Vec<Row>, tokio_postgres::Error> {
        let Some(named) = &self.named else {
            return tx.query_typed(sql, params).await;
        };
        let cached = named.lock().unwrap().get(sql).cloned();
        let statement = match cached {
            Some(statement) => statement,
            None => {
                let types: Vec<Type> = params.iter().map(|(_, t)| t.clone()).collect();
                let statement = tx.prepare_typed(sql, &types).await?;
                named.lock().unwrap().insert(sql, statement.clone());
                statement
            }
        };
        let values: Vec<&(dyn ToSql + Sync)> = params.iter().map(|(v, _)| *v).collect();
        tx.query(&statement, &values).await
    }
}

/// Drives a new connection in the background and returns its client. The
/// connection ends when the Store drops its client. An error on it surfaces
/// on the Store's next call.
fn spawn_connection<S, T>(
    (client, connection): (Client, tokio_postgres::Connection<S, T>),
) -> Client
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

/// Reads the ledger and the embedding dimension, as an end user would, and
/// refuses any version but the one this build expects.
async fn check_schema(
    client: &mut Client,
    statements: &Statements,
) -> Result<EmbeddingDimension, Error> {
    let tx = client.transaction().await?;
    tx.batch_execute(&format!("set local role {}", CallerRole::User.name()))
        .await?;
    let row = match statements
        .query(
            &tx,
            "select (select max(version) from gnomon.schema_version), gnomon.embedding_dimension()",
            &[],
        )
        .await
    {
        Ok(rows) => rows.into_iter().next().expect("one row"),
        Err(e) if is_missing_schema(&e) => {
            return Err(Error::SchemaVersion {
                expected: SCHEMA_VERSION,
                found: None,
            })
        }
        Err(e) => return Err(e.into()),
    };
    tx.commit().await?;

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

/// The gnomon tables the current transaction deleted rows from or updated
/// rows in, which an Erasure leaves dead tuples in, from the transaction's
/// own statistics. They count the rows a trigger or a foreign key changed
/// too, so a table a later migration adds is never missed.
async fn touched_tables(
    statements: &Statements,
    tx: &Transaction<'_>,
) -> Result<Vec<String>, Error> {
    let rows = statements
        .query(
            tx,
            "select coalesce(array_agg(format('%I.%I', schemaname, relname) order by relname), '{}')
             from pg_catalog.pg_stat_xact_user_tables
             where schemaname = 'gnomon' and n_tup_upd + n_tup_del > 0",
            &[],
        )
        .await?;
    Ok(rows[0].get(0))
}

/// One caller's view of the Store. Every method is one transaction.
pub struct Caller<'s> {
    store: &'s Store,
    role: CallerRole,
    claims: String,
}

impl Caller<'_> {
    /// Opens the call's transaction, sets its role and claims locally, and
    /// returns the call's Tenant as `gnomon.call_tenant` checked it.
    ///
    /// The claims and the check share one statement, so the check adds no
    /// round trip. The materialized CTE sets the claims before the outer
    /// select reads them.
    async fn begin<'c>(
        &self,
        client: &'c mut Client,
        tenant: Option<TenantId>,
    ) -> Result<(Transaction<'c>, TenantId), Error> {
        let tx = client.transaction().await?;
        tx.batch_execute(&format!("set local role {}", self.role.name()))
            .await?;
        let rows = self
            .store
            .statements
            .query(
                &tx,
                "with claims as materialized (select set_config('request.jwt.claims', $1, true)) \
                 select gnomon.call_tenant($2) from claims",
                &[
                    (&self.claims, Type::TEXT),
                    (&tenant.map(|t| t.0), Type::UUID),
                ],
            )
            .await
            .map_err(Error::from_store)?;
        Ok((tx, TenantId(rows[0].get(0))))
    }

    /// Records an Episode and returns its new ID, a UUID v7.
    pub async fn record_episode(&self, episode: &NewEpisode) -> Result<EpisodeId, Error> {
        let mut client = self.store.client.lock().await;
        let (tx, tenant) = self.begin(&mut client, episode.tenant).await?;
        let rows = self
            .store
            .statements
            .query(
                &tx,
                "select gnomon.record_episode($1, $2, $3, $4, $5)",
                &[
                    (&tenant.0, Type::UUID),
                    (&episode.scope.as_str(), Type::TEXT),
                    (&episode.content, Type::TEXT),
                    (&episode.reference_time, Type::TIMESTAMPTZ),
                    (&episode.authority, Type::INT4),
                ],
            )
            .await?;
        tx.commit().await?;
        Ok(EpisodeId(rows[0].get(0)))
    }

    /// Reads an Episode, or `None` if this caller cannot see it.
    pub async fn read_episode(
        &self,
        tenant: impl Into<Option<TenantId>>,
        id: EpisodeId,
    ) -> Result<Option<Episode>, Error> {
        let mut client = self.store.client.lock().await;
        let (tx, tenant) = self.begin(&mut client, tenant.into()).await?;
        let rows = self
            .store
            .statements
            .query(
                &tx,
                "select tenant_id, scope, id, content, reference_time, authority, recorded_at
                 from gnomon.read_episode($1, $2)",
                &[(&tenant.0, Type::UUID), (&id.0, Type::UUID)],
            )
            .await?;
        tx.commit().await?;
        Ok(rows.first().map(|r| Episode {
            tenant: TenantId(r.get(0)),
            scope: Scope::new(r.get::<_, String>(1)),
            id: EpisodeId(r.get(2)),
            content: r.get(3),
            reference_time: r.get::<_, DateTime<Utc>>(4),
            authority: r.get(5),
            recorded_at: r.get::<_, DateTime<Utc>>(6),
        }))
    }

    /// Registers a Kind in the Tenant's Ontology and returns it flattened
    /// with its ancestors and with its compiled rules.
    ///
    /// Registering the same Kind again changes nothing and returns it. The
    /// deployment decides who may register Kinds: without its EXECUTE grant
    /// on `gnomon.register_kind`, the call fails with
    /// `insufficient_privilege`.
    pub async fn register_kind(&self, kind: &NewKind) -> Result<Kind, Error> {
        let (source, target) = match &kind.of {
            KindOf::Node => (None, None),
            KindOf::Edge { source, target } => (Some(source), Some(target)),
        };
        let kind_of = kind.of.sql_name();
        let mut client = self.store.client.lock().await;
        let (tx, tenant) = self.begin(&mut client, kind.tenant).await?;
        self.store
            .statements
            .query(
                &tx,
                "select gnomon.register_kind($1, $2, $3, $4, $5, $6, $7, $8)",
                &[
                    (&tenant.0, Type::UUID),
                    (&kind.name, Type::TEXT),
                    (&kind_of, Type::TEXT),
                    (&kind.parent, Type::TEXT),
                    (&kind.properties, Type::JSONB),
                    (&kind.search, Type::TEXT_ARRAY),
                    (&source, Type::TEXT_ARRAY),
                    (&target, Type::TEXT_ARRAY),
                ],
            )
            .await
            .map_err(Error::from_store)?;
        let registered = read_kind(&tx, &self.store.statements, tenant, &kind.name).await?;
        tx.commit().await?;
        Ok(registered.expect("a Kind just registered is visible to its registrant"))
    }

    /// Sets a Kind's own merge rule. Ranked merge orders a Node's values by
    /// Authority, then Reference time, then Recorded time (ADR-0007). A
    /// Kind below a Kind that ranks inherits the rule, and cannot turn it
    /// off: the call fails with [`Error::InvalidKind`]. A change applies to
    /// later writes only. It changes no existing Version.
    ///
    /// The deployment decides who may set it: without its EXECUTE grant on
    /// `gnomon.set_merge_rule`, the call fails with `insufficient_privilege`.
    pub async fn set_merge_rule(
        &self,
        tenant: impl Into<Option<TenantId>>,
        kind: &str,
        rule: MergeRule,
    ) -> Result<(), Error> {
        let mut client = self.store.client.lock().await;
        let (tx, tenant) = self.begin(&mut client, tenant.into()).await?;
        self.store
            .statements
            .query(
                &tx,
                "select gnomon.set_merge_rule($1, $2, $3)",
                &[
                    (&tenant.0, Type::UUID),
                    (&kind, Type::TEXT),
                    (&rule.as_sql(), Type::TEXT),
                ],
            )
            .await
            .map_err(Error::from_store)?;
        tx.commit().await?;
        Ok(())
    }

    /// The merge rule a Kind has in the Tenant's Ontology: ranked if it or
    /// any ancestor ranks, else recency.
    pub async fn merge_rule(
        &self,
        tenant: impl Into<Option<TenantId>>,
        kind: &str,
    ) -> Result<MergeRule, Error> {
        let mut client = self.store.client.lock().await;
        let (tx, tenant) = self.begin(&mut client, tenant.into()).await?;
        let rows = self
            .store
            .statements
            .query(
                &tx,
                "select gnomon.kind_merge_rule($1, $2)",
                &[(&tenant.0, Type::UUID), (&kind, Type::TEXT)],
            )
            .await?;
        tx.commit().await?;
        Ok(MergeRule::from_sql(rows[0].get(0)))
    }

    /// Reads a Kind of the Tenant's Ontology, a root Kind included, or `None`
    /// if this caller's Tenant has no such Kind.
    pub async fn read_kind(
        &self,
        tenant: impl Into<Option<TenantId>>,
        name: &str,
    ) -> Result<Option<Kind>, Error> {
        let mut client = self.store.client.lock().await;
        let (tx, tenant) = self.begin(&mut client, tenant.into()).await?;
        let kind = read_kind(&tx, &self.store.statements, tenant, name).await?;
        tx.commit().await?;
        Ok(kind)
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

impl Caller<'_> {
    /// Creates a Node and writes its first patch, in one transaction.
    pub async fn create_node(
        &self,
        node: &NewNode,
        patch: &Patch,
    ) -> Result<(NodeId, WriteOutcome), Error> {
        let (id, outcome) = self
            .run_writes(node.tenant, &[(Target::NewNode(node), patch)], false)
            .await?
            .remove(0);
        match id {
            Versioned::Node(node) => Ok((node, outcome)),
            Versioned::Edge(_) => unreachable!("a new Node's write is about the Node"),
        }
    }

    /// Writes a patch of property values for a Node, citing the patch's
    /// Episode, and recomputes the Node's Versions over its Valid time.
    pub async fn write_node(
        &self,
        tenant: impl Into<Option<TenantId>>,
        node: NodeId,
        patch: &Patch,
    ) -> Result<WriteOutcome, Error> {
        let mut done = self
            .run_writes(tenant.into(), &[(Target::Node(node), patch)], false)
            .await?;
        Ok(done.remove(0).1)
    }

    /// Writes several patches in one transaction, in order. A second write
    /// to the same Node replaces the Versions the first opened, in place, so
    /// the batch leaves no Version that no read could have seen (ADR-0008).
    pub async fn write_nodes(
        &self,
        tenant: impl Into<Option<TenantId>>,
        writes: &[(NodeId, Patch)],
    ) -> Result<Vec<WriteOutcome>, Error> {
        let targets: Vec<_> = writes
            .iter()
            .map(|(node, patch)| (Target::Node(*node), patch))
            .collect();
        Ok(self
            .run_writes(tenant.into(), &targets, false)
            .await?
            .into_iter()
            .map(|(_, outcome)| outcome)
            .collect())
    }

    /// Creates an Edge and writes its first patch, in one transaction.
    ///
    /// The Store refuses an Edge whose Kind is not an Edge Kind of the
    /// Tenant, whose endpoint is not a Node of the Tenant that the caller
    /// can see, or whose endpoint's Kind the Edge Kind does not allow, with
    /// [`Error::InvalidWrite`]. The Edge holds over the patch's Valid time,
    /// and the patch may state no property value.
    pub async fn create_edge(
        &self,
        edge: &NewEdge,
        patch: &Patch,
    ) -> Result<(EdgeId, WriteOutcome), Error> {
        let (id, outcome) = self
            .run_writes(edge.tenant, &[(Target::NewEdge(edge), patch)], false)
            .await?
            .remove(0);
        match id {
            Versioned::Edge(edge) => Ok((edge, outcome)),
            Versioned::Node(_) => unreachable!("a new Edge's write is about the Edge"),
        }
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
        let mut done = self
            .run_writes(tenant.into(), &[(Target::Edge(edge), patch)], false)
            .await?;
        Ok(done.remove(0).1)
    }

    /// Writes several patches to Edges in one transaction, in order, as
    /// [`write_nodes`](Self::write_nodes) does for Nodes.
    pub async fn write_edges(
        &self,
        tenant: impl Into<Option<TenantId>>,
        writes: &[(EdgeId, Patch)],
    ) -> Result<Vec<WriteOutcome>, Error> {
        let targets: Vec<_> = writes
            .iter()
            .map(|(edge, patch)| (Target::Edge(*edge), patch))
            .collect();
        Ok(self
            .run_writes(tenant.into(), &targets, false)
            .await?
            .into_iter()
            .map(|(_, outcome)| outcome)
            .collect())
    }

    /// Runs a batch of writes in one transaction, retrying the whole
    /// transaction when Postgres asks for a retry.
    async fn run_writes(
        &self,
        tenant: Option<TenantId>,
        writes: &[(Target<'_>, &Patch)],
        correct: bool,
    ) -> Result<Vec<(Versioned, WriteOutcome)>, Error> {
        let mut client = self.store.client.lock().await;
        let mut attempt = 0;
        loop {
            match self.try_writes(&mut client, tenant, writes, correct).await {
                Err(e) if attempt < WRITE_RETRIES && is_retryable(&e) => attempt += 1,
                done => return done,
            }
        }
    }

    async fn try_writes(
        &self,
        client: &mut Client,
        tenant: Option<TenantId>,
        writes: &[(Target<'_>, &Patch)],
        correct: bool,
    ) -> Result<Vec<(Versioned, WriteOutcome)>, Error> {
        let (tx, tenant) = self.begin(client, tenant).await?;
        let statements = &self.store.statements;
        let mut done = Vec::with_capacity(writes.len());
        for (target, patch) in writes {
            let of = match target {
                Target::Node(node) => Versioned::Node(*node),
                Target::Edge(edge) => Versioned::Edge(*edge),
                Target::NewNode(new) => {
                    let rows = statements
                        .query(
                            &tx,
                            "select gnomon.create_node($1, $2, $3)",
                            &[
                                (&tenant.0, Type::UUID),
                                (&new.scope.as_str(), Type::TEXT),
                                (&new.kind, Type::TEXT),
                            ],
                        )
                        .await
                        .map_err(Error::from_store)?;
                    Versioned::Node(NodeId(rows[0].get(0)))
                }
                Target::NewEdge(new) => {
                    let rows = statements
                        .query(
                            &tx,
                            "select gnomon.create_edge($1, $2, $3, $4, $5)",
                            &[
                                (&tenant.0, Type::UUID),
                                (&new.scope.as_str(), Type::TEXT),
                                (&new.kind, Type::TEXT),
                                (&new.source.0, Type::UUID),
                                (&new.target.0, Type::UUID),
                            ],
                        )
                        .await
                        .map_err(Error::from_store)?;
                    Versioned::Edge(EdgeId(rows[0].get(0)))
                }
            };
            done.push((
                of,
                write_patch(&tx, statements, tenant, of, patch, correct).await?,
            ));
        }
        tx.commit().await?;
        Ok(done)
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
        let mut client = self.store.client.lock().await;
        let (tx, tenant) = self.begin(&mut client, tenant.into()).await?;
        let rows = self
            .store
            .statements
            .query(
                &tx,
                "select id, node_id, scope, kind, props,
                        nullif(valid_from, '-infinity'), valid_from_precision::text,
                        nullif(valid_to, 'infinity'), valid_to_precision::text,
                        recorded_from, nullif(recorded_to, 'infinity'), uncertain, text_hash,
                        merge_rule::text
                 from gnomon.read_node($1, $2, $3, $4)",
                &[
                    (&tenant.0, Type::UUID),
                    (&node.0, Type::UUID),
                    (&as_of.valid_at, Type::TIMESTAMPTZ),
                    (&as_of.known_at, Type::TIMESTAMPTZ),
                ],
            )
            .await?;
        tx.commit().await?;
        Ok(rows.first().map(|r| node_version(r, 0)))
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
        let ids: Vec<uuid::Uuid> = nodes.iter().map(|n| n.0).collect();
        let mut client = self.store.client.lock().await;
        let (tx, tenant) = self.begin(&mut client, tenant.into()).await?;
        let rows = self
            .store
            .statements
            .query(
                &tx,
                "select id, node_id, scope, kind, props,
                        nullif(valid_from, '-infinity'), valid_from_precision::text,
                        nullif(valid_to, 'infinity'), valid_to_precision::text,
                        recorded_from, nullif(recorded_to, 'infinity'), uncertain, text_hash,
                        merge_rule::text
                 from gnomon.read_nodes($1, $2, $3, $4)",
                &[
                    (&tenant.0, Type::UUID),
                    (&ids, Type::UUID_ARRAY),
                    (&as_of.valid_at, Type::TIMESTAMPTZ),
                    (&as_of.known_at, Type::TIMESTAMPTZ),
                ],
            )
            .await?;
        tx.commit().await?;
        Ok(rows.iter().map(|r| node_version(r, 0)).collect())
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
        let mut client = self.store.client.lock().await;
        let (tx, tenant) = self.begin(&mut client, tenant.into()).await?;
        let rows = self
            .store
            .statements
            .query(
                &tx,
                "select id, edge_id, scope, kind, source_id, target_id, props,
                        nullif(valid_from, '-infinity'), valid_from_precision::text,
                        nullif(valid_to, 'infinity'), valid_to_precision::text,
                        recorded_from, nullif(recorded_to, 'infinity'), uncertain, text_hash,
                        merge_rule::text, sentence
                 from gnomon.read_edge($1, $2, $3, $4)",
                &[
                    (&tenant.0, Type::UUID),
                    (&edge.0, Type::UUID),
                    (&as_of.valid_at, Type::TIMESTAMPTZ),
                    (&as_of.known_at, Type::TIMESTAMPTZ),
                ],
            )
            .await?;
        tx.commit().await?;
        Ok(rows.first().map(|r| edge_version(r, 0)))
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
        let kinds = (!traversal.kinds.is_empty()).then_some(&traversal.kinds);
        let direction = traversal.direction.as_sql();
        let mut client = self.store.client.lock().await;
        let (tx, tenant) = self.begin(&mut client, tenant.into()).await?;
        let rows = self
            .store
            .statements
            .query(
                &tx,
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
                    (&tenant.0, Type::UUID),
                    (&node.0, Type::UUID),
                    (&as_of.valid_at, Type::TIMESTAMPTZ),
                    (&as_of.known_at, Type::TIMESTAMPTZ),
                    (&direction, Type::TEXT),
                    (&kinds, Type::TEXT_ARRAY),
                ],
            )
            .await?;
        tx.commit().await?;
        Ok(rows
            .iter()
            .map(|r| Step {
                direction: Direction::from_sql(r.get(31)),
                edge: edge_version(r, 0),
                neighbour: NodeId(r.get(18)),
                neighbour_version: r
                    .get::<_, Option<uuid::Uuid>>(17)
                    .map(|_| node_version(r, 17)),
            })
            .collect())
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
        self.state_bound_of(tenant.into(), Versioned::Node(node), statement)
            .await
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
        self.state_bound_of(tenant.into(), Versioned::Edge(edge), statement)
            .await
    }

    async fn state_bound_of(
        &self,
        tenant: Option<TenantId>,
        of: Versioned,
        statement: &BoundStatement,
    ) -> Result<BoundOutcome, Error> {
        let id = of.id();
        let (at, precision) = statement.bound.parts();
        let side = match statement.side {
            BoundSide::Start => "from",
            BoundSide::End => "to",
        };
        let sql = of.state_bound_sql();
        let mut client = self.store.client.lock().await;
        let mut attempt = 0;
        loop {
            let (tx, tenant) = self.begin(&mut client, tenant).await?;
            let rows = self
                .store
                .statements
                .query(
                    &tx,
                    sql,
                    &[
                        (&tenant.0, Type::UUID),
                        (&id, Type::UUID),
                        (&statement.episode.0, Type::UUID),
                        (&statement.version.0, Type::UUID),
                        (&side, Type::TEXT),
                        (&at, Type::TIMESTAMPTZ),
                        (&precision, Type::TEXT),
                    ],
                )
                .await
                .map_err(Error::from_store);
            let rows = match rows {
                Err(e) if attempt < WRITE_RETRIES && is_retryable(&e) => {
                    attempt += 1;
                    continue;
                }
                other => other?,
            };
            tx.commit().await?;
            let r = &rows[0];
            let change = match r.get::<_, &str>(0) {
                "refinement" => BoundChange::Refinement,
                "correction" => BoundChange::Correction,
                "unchanged" => BoundChange::Unchanged,
                other => panic!("gnomon.state_bound returned change {other:?}"),
            };
            let opened: Vec<uuid::Uuid> = r.get(2);
            return Ok(BoundOutcome {
                change,
                recorded_at: r.get(1),
                opened: opened.into_iter().map(VersionId).collect(),
            });
        }
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
        let mut done = self
            .run_writes(tenant.into(), &[(Target::Node(node), patch)], true)
            .await?;
        Ok(done.remove(0).1)
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
        let mut done = self
            .run_writes(tenant.into(), &[(Target::Edge(edge), patch)], true)
            .await?;
        Ok(done.remove(0).1)
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
        self.write_ending(tenant.into(), Versioned::Node(node), Ending::End(end))
            .await
    }

    /// Records an End of validity of an Edge: the Edge holds no Version from
    /// the stated instant, whatever property values remain.
    pub async fn end_edge_validity(
        &self,
        tenant: impl Into<Option<TenantId>>,
        edge: EdgeId,
        end: &EndOfValidity,
    ) -> Result<WriteOutcome, Error> {
        self.write_ending(tenant.into(), Versioned::Edge(edge), Ending::End(end))
            .await
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
        self.write_ending(
            tenant.into(),
            Versioned::Node(node),
            Ending::Retract(retraction),
        )
        .await
    }

    /// Records a Retraction of a current Version of an Edge.
    pub async fn retract_edge_version(
        &self,
        tenant: impl Into<Option<TenantId>>,
        edge: EdgeId,
        retraction: &Retraction,
    ) -> Result<WriteOutcome, Error> {
        self.write_ending(
            tenant.into(),
            Versioned::Edge(edge),
            Ending::Retract(retraction),
        )
        .await
    }

    async fn write_ending(
        &self,
        tenant: Option<TenantId>,
        of: Versioned,
        ending: Ending<'_>,
    ) -> Result<WriteOutcome, Error> {
        let id = of.id();
        let is_edge = of.is_edge();
        let mut client = self.store.client.lock().await;
        let mut attempt = 0;
        loop {
            let (tx, tenant) = self.begin(&mut client, tenant).await?;
            let rows = match ending {
                Ending::End(end) => {
                    let (at, precision) = end.at.parts();
                    self.store
                        .statements
                        .query(
                            &tx,
                            "select status, recorded_at, versions
                             from gnomon.end_validity($1, $2, $3, $4, $5, $6)",
                            &[
                                (&tenant.0, Type::UUID),
                                (&id, Type::UUID),
                                (&is_edge, Type::BOOL),
                                (&end.episode.0, Type::UUID),
                                (&at, Type::TIMESTAMPTZ),
                                (&precision, Type::TEXT),
                            ],
                        )
                        .await
                }
                Ending::Retract(r) => {
                    self.store
                        .statements
                        .query(
                            &tx,
                            "select status, recorded_at, versions
                             from gnomon.retract_version($1, $2, $3, $4, $5)",
                            &[
                                (&tenant.0, Type::UUID),
                                (&id, Type::UUID),
                                (&is_edge, Type::BOOL),
                                (&r.episode.0, Type::UUID),
                                (&r.version.0, Type::UUID),
                            ],
                        )
                        .await
                }
            }
            .map_err(Error::from_store);
            let rows = match rows {
                Err(e) if attempt < WRITE_RETRIES && is_retryable(&e) => {
                    attempt += 1;
                    continue;
                }
                other => other?,
            };
            tx.commit().await?;
            return Ok(write_outcome(&rows[0]));
        }
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
        let mut client = self.store.client.lock().await;
        let mut attempt = 0;
        let tenant = tenant.into();
        loop {
            let (tx, tenant) = self.begin(&mut client, tenant).await?;
            let rows = self
                .store
                .statements
                .query(
                    &tx,
                    "select recorded_at, versions from gnomon.retract_episode($1, $2)",
                    &[(&tenant.0, Type::UUID), (&episode.0, Type::UUID)],
                )
                .await
                .map_err(Error::from_store);
            let rows = match rows {
                Err(e) if attempt < WRITE_RETRIES && is_retryable(&e) => {
                    attempt += 1;
                    continue;
                }
                other => other?,
            };
            tx.commit().await?;
            let row = &rows[0];
            return Ok(EpisodeRetraction {
                recorded_at: row.get(0),
                retracted: row
                    .get::<_, Vec<uuid::Uuid>>(1)
                    .into_iter()
                    .map(VersionId)
                    .collect(),
            });
        }
    }

    /// Erases an Episode (ADR-0003, ADR-0007). In one transaction it deletes
    /// the Episode, its Contributions, its statements, its Citations and
    /// every Version derived from it alone. It recomputes every Version the
    /// Episode touched, current and closed, from the Contributions that
    /// remain, each under the merge rule it recorded. As-of reads afterwards
    /// return the history as if the Episode had never existed. A Node or
    /// Edge left with nothing is erased too, and a Node only once no Edge
    /// joins it.
    ///
    /// Erasure is a definer function that the migrations grant to no caller
    /// role, so a call fails with [`Error::Database`] carrying
    /// `insufficient_privilege` until the deployment grants EXECUTE on
    /// `gnomon.erase_episode` (ADR-0009). Writes to the Nodes and Edges the
    /// Episode touched wait for it to commit.
    pub async fn erase_episode(
        &self,
        tenant: impl Into<Option<TenantId>>,
        episode: EpisodeId,
    ) -> Result<EpisodeErasure, Error> {
        let mut client = self.store.client.lock().await;
        let mut attempt = 0;
        let tenant = tenant.into();
        loop {
            let (tx, tenant) = self.begin(&mut client, tenant).await?;
            let rows = self
                .store
                .statements
                .query(
                    &tx,
                    "select erased_at, recomputed_nodes, recomputed_edges, erased_nodes, erased_edges
                     from gnomon.erase_episode($1, $2)",
                    &[(&tenant.0, Type::UUID), (&episode.0, Type::UUID)],
                )
                .await
                .map_err(Error::from_store);
            let rows = match rows {
                Err(e) if attempt < WRITE_RETRIES && is_retryable(&e) => {
                    attempt += 1;
                    continue;
                }
                other => other?,
            };
            let touched_tables = touched_tables(&self.store.statements, &tx).await?;
            tx.commit().await?;
            let row = &rows[0];
            let ids = |i: usize| row.get::<_, Vec<uuid::Uuid>>(i);
            return Ok(EpisodeErasure {
                erased_at: row.get(0),
                recomputed_nodes: ids(1).into_iter().map(NodeId).collect(),
                recomputed_edges: ids(2).into_iter().map(EdgeId).collect(),
                erased_nodes: ids(3).into_iter().map(NodeId).collect(),
                erased_edges: ids(4).into_iter().map(EdgeId).collect(),
                touched_tables,
            });
        }
    }

    /// Erases a Node (ADR-0003): its Versions, Contributions, statements and
    /// Citations, and every Edge that joins it, whatever the Edge's Scope,
    /// with that Edge's own rows. An endpoint of such an Edge left with
    /// nothing is erased too when no other Edge joins it and the caller sees
    /// its Scope. The Episodes stay. The Node must be one the caller sees.
    ///
    /// Erasure is granted to no caller role by the migrations, so a call
    /// fails with [`Error::Database`] carrying `insufficient_privilege` until
    /// the deployment grants EXECUTE on `gnomon.erase_node` (ADR-0009).
    pub async fn erase_node(
        &self,
        tenant: impl Into<Option<TenantId>>,
        node: NodeId,
    ) -> Result<GraphErasure, Error> {
        self.erase_graph(
            tenant.into(),
            "select erased_at, erased_episodes, erased_nodes, erased_edges
             from gnomon.erase_node($1, $2)",
            &node.0,
            Type::UUID,
        )
        .await
    }

    /// Erases an Edge (ADR-0003): its Versions, Contributions, statements,
    /// Citations and Sentences. Each endpoint left with nothing is erased too
    /// when no other Edge joins it and the caller sees its Scope. The Edge
    /// must be one the caller sees. It is granted as
    /// [`erase_node`](Self::erase_node) is, on `gnomon.erase_edge`.
    pub async fn erase_edge(
        &self,
        tenant: impl Into<Option<TenantId>>,
        edge: EdgeId,
    ) -> Result<GraphErasure, Error> {
        self.erase_graph(
            tenant.into(),
            "select erased_at, erased_episodes, erased_nodes, erased_edges
             from gnomon.erase_edge($1, $2)",
            &edge.0,
            Type::UUID,
        )
        .await
    }

    /// Erases a Scope (ADR-0003): every row in it, its Episodes among them,
    /// and every Edge of another Scope that joins one of its Nodes, with that
    /// Edge's own rows. An endpoint outside the Scope left with nothing is
    /// erased too when no other Edge joins it and the caller sees its Scope.
    /// Nothing else outside the Scope changes. The caller must see the Scope.
    /// It is granted as [`erase_node`](Self::erase_node) is, on
    /// `gnomon.erase_scope`.
    pub async fn erase_scope(
        &self,
        tenant: impl Into<Option<TenantId>>,
        scope: &Scope,
    ) -> Result<GraphErasure, Error> {
        self.erase_graph(
            tenant.into(),
            "select erased_at, erased_episodes, erased_nodes, erased_edges
             from gnomon.erase_scope($1, $2)",
            &scope.as_str(),
            Type::TEXT,
        )
        .await
    }

    /// Runs one of the Erasures of a Node, an Edge or a Scope, retrying a
    /// serialization failure or a deadlock as a write does.
    async fn erase_graph(
        &self,
        tenant: Option<TenantId>,
        sql: &'static str,
        target: &(dyn ToSql + Sync),
        target_type: Type,
    ) -> Result<GraphErasure, Error> {
        let mut client = self.store.client.lock().await;
        let mut attempt = 0;
        loop {
            let (tx, tenant) = self.begin(&mut client, tenant).await?;
            let rows = self
                .store
                .statements
                .query(
                    &tx,
                    sql,
                    &[(&tenant.0, Type::UUID), (target, target_type.clone())],
                )
                .await
                .map_err(Error::from_store);
            let rows = match rows {
                Err(e) if attempt < WRITE_RETRIES && is_retryable(&e) => {
                    attempt += 1;
                    continue;
                }
                other => other?,
            };
            let touched_tables = touched_tables(&self.store.statements, &tx).await?;
            tx.commit().await?;
            let row = &rows[0];
            let ids = |i: usize| row.get::<_, Vec<uuid::Uuid>>(i);
            return Ok(GraphErasure {
                erased_at: row.get(0),
                erased_episodes: ids(1).into_iter().map(EpisodeId).collect(),
                erased_nodes: ids(2).into_iter().map(NodeId).collect(),
                erased_edges: ids(3).into_iter().map(EdgeId).collect(),
                touched_tables,
            });
        }
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
        self.list_statements(tenant.into(), node.0).await
    }

    /// Lists the statements about an Edge, as
    /// [`list_node_statements`](Self::list_node_statements) does for a Node.
    pub async fn list_edge_statements(
        &self,
        tenant: impl Into<Option<TenantId>>,
        edge: EdgeId,
    ) -> Result<Vec<crate::Statement>, Error> {
        self.list_statements(tenant.into(), edge.0).await
    }

    async fn list_statements(
        &self,
        tenant: Option<TenantId>,
        id: uuid::Uuid,
    ) -> Result<Vec<crate::Statement>, Error> {
        let mut client = self.store.client.lock().await;
        let (tx, tenant) = self.begin(&mut client, tenant).await?;
        let rows = self
            .store
            .statements
            .query(
                &tx,
                "select kind, episode_id, version_id, nullif(valid_from, '-infinity'),
                        valid_from_precision::text, nullif(valid_to, 'infinity'),
                        valid_to_precision::text, recorded_at
                 from gnomon.list_statements($1, $2)",
                &[(&tenant.0, Type::UUID), (&id, Type::UUID)],
            )
            .await?;
        tx.commit().await?;
        Ok(rows
            .iter()
            .map(|r| crate::Statement {
                kind: match r.get::<_, &str>(0) {
                    "correction" => StatementKind::Correction,
                    "end_of_validity" => StatementKind::EndOfValidity,
                    "retraction" => StatementKind::Retraction,
                    other => panic!("gnomon.list_statements returned kind {other:?}"),
                },
                episode: EpisodeId(r.get(1)),
                version: r.get::<_, Option<uuid::Uuid>>(2).map(VersionId),
                valid_time: ValidTime {
                    from: bound(r.get(3), r.get(4)),
                    to: bound(r.get(5), r.get(6)),
                },
                recorded_at: r.get(7),
            })
            .collect())
    }

    /// Lists the Citations of a Version: each Episode it was derived from,
    /// with the property and the place they name.
    pub async fn list_citations(
        &self,
        tenant: impl Into<Option<TenantId>>,
        version: VersionId,
    ) -> Result<Vec<Citation>, Error> {
        let mut client = self.store.client.lock().await;
        let (tx, tenant) = self.begin(&mut client, tenant.into()).await?;
        let rows = self
            .store
            .statements
            .query(
                &tx,
                "select episode_id, property, place, recorded_at, episode_recorded_at
                 from gnomon.list_citations($1, $2)",
                &[(&tenant.0, Type::UUID), (&version.0, Type::UUID)],
            )
            .await?;
        tx.commit().await?;
        Ok(rows
            .iter()
            .map(|r| Citation {
                episode: EpisodeId(r.get(0)),
                property: r.get(1),
                place: r.get(2),
                recorded_at: r.get(3),
                episode_recorded_at: r.get(4),
            })
            .collect())
    }
}

/// What an End of validity or a Retraction is made of.
#[derive(Clone, Copy)]
enum Ending<'a> {
    End(&'a EndOfValidity),
    Retract(&'a Retraction),
}

/// The Node or Edge a write or a bound statement is about, and the Store
/// functions that write it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Versioned {
    Node(NodeId),
    Edge(EdgeId),
}

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
    tx: &Transaction<'_>,
    statements: &Statements,
    tenant: TenantId,
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
    let rows = statements
        .query(
            tx,
            sql,
            &[
                (&tenant.0, Type::UUID),
                (&id, Type::UUID),
                (&patch.episode.0, Type::UUID),
                (&from, Type::TIMESTAMPTZ),
                (&from_precision, Type::TEXT),
                (&to, Type::TIMESTAMPTZ),
                (&to_precision, Type::TEXT),
                (&values, Type::JSONB),
                (&places, Type::JSONB),
            ],
        )
        .await
        .map_err(Error::from_store)?;
    Ok(write_outcome(&rows[0]))
}

fn write_outcome(r: &Row) -> WriteOutcome {
    let status = match r.get::<_, &str>(0) {
        "written" => WriteStatus::Written,
        "unchanged" => WriteStatus::Unchanged,
        other => panic!("the Store's write returned status {other:?}"),
    };
    let opened: Vec<uuid::Uuid> = r.get(2);
    WriteOutcome {
        status,
        recorded_at: r.get(1),
        opened: opened.into_iter().map(VersionId).collect(),
    }
}

/// A Node Version from the 14 columns of `row` from `at` on: ID, Node,
/// Scope, Kind, values, the four bounds with their precisions, the two
/// Recorded-time bounds, the `uncertain` marker, the text hash and the merge
/// rule, with open bounds as nulls.
fn node_version(row: &Row, at: usize) -> NodeVersion {
    NodeVersion {
        id: VersionId(row.get(at)),
        node: NodeId(row.get(at + 1)),
        scope: Scope::new(row.get::<_, String>(at + 2)),
        kind: row.get(at + 3),
        values: object(row.get(at + 4)),
        valid_time: ValidTime {
            from: bound(row.get(at + 5), row.get(at + 6)),
            to: bound(row.get(at + 7), row.get(at + 8)),
        },
        recorded_from: row.get(at + 9),
        recorded_to: row.get(at + 10),
        uncertain: row.get(at + 11),
        text_hash: row.get(at + 12),
        merge_rule: MergeRule::from_sql(row.get(at + 13)),
    }
}

/// An Edge Version from the 17 columns of `row` from `at` on: ID, Edge,
/// Scope, Kind, source, target, then as [`node_version`] from the values on,
/// and last the Sentence.
fn edge_version(row: &Row, at: usize) -> EdgeVersion {
    EdgeVersion {
        id: VersionId(row.get(at)),
        edge: EdgeId(row.get(at + 1)),
        scope: Scope::new(row.get::<_, String>(at + 2)),
        kind: row.get(at + 3),
        source: NodeId(row.get(at + 4)),
        target: NodeId(row.get(at + 5)),
        values: object(row.get(at + 6)),
        valid_time: ValidTime {
            from: bound(row.get(at + 7), row.get(at + 8)),
            to: bound(row.get(at + 9), row.get(at + 10)),
        },
        recorded_from: row.get(at + 11),
        recorded_to: row.get(at + 12),
        uncertain: row.get(at + 13),
        text_hash: row.get(at + 14),
        merge_rule: MergeRule::from_sql(row.get(at + 15)),
        sentence: row.get(at + 16),
    }
}

fn object(values: Value) -> Map<String, Value> {
    match values {
        Value::Object(m) => m,
        other => panic!("the Store returned values {other}"),
    }
}

fn is_retryable(e: &Error) -> bool {
    matches!(
        e.sqlstate(),
        Some(c) if *c == SqlState::T_R_SERIALIZATION_FAILURE || *c == SqlState::T_R_DEADLOCK_DETECTED
    )
}

/// A Valid-time bound as the Store returns it: unknown, or an instant.
fn bound(at: Option<DateTime<Utc>>, precision: &str) -> Bound {
    match (Precision::from_sql(precision), at) {
        (None, _) | (_, None) => Bound::Unknown,
        (Some(Precision::Instant), Some(t)) => Bound::At(t),
        (Some(precision), Some(at)) => Bound::Period { at, precision },
    }
}

async fn read_kind(
    tx: &Transaction<'_>,
    statements: &Statements,
    tenant: TenantId,
    name: &str,
) -> Result<Option<Kind>, Error> {
    let rows = statements
        .query(
            tx,
            "select name, kind_of, parent, ancestors, properties, rules, search,
                    source_kinds, target_kinds
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
        }
    }))
}
