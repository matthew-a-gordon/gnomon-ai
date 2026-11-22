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
//! returns (ADR-0005). Every parameterized statement goes through
//! `query_typed`, which sends the unnamed statement, because named statements
//! fail on transaction-mode poolers at their default settings (the
//! `prototype/pooler-prepared-statements` branch, ADR-0001).
//! Named statements are never sent.

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use tokio::sync::Mutex;
use tokio_postgres::error::SqlState;
use tokio_postgres::types::Type;
use tokio_postgres::{Client, NoTls, Transaction};

use crate::migrations::{EmbeddingDimension, SCHEMA_VERSION};
use crate::{
    AsOf, Bound, Citation, Episode, EpisodeId, Error, Kind, KindOf, NewEpisode, NewKind, NewNode,
    NodeId, NodeVersion, Patch, Scope, TenantId, ValidTime, VersionId, WriteOutcome, WriteStatus,
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
    embedding_dimension: EmbeddingDimension,
}

impl Store {
    /// Connects without TLS and checks the schema version.
    ///
    /// The check runs once, here. A Store that connected before a
    /// migration was applied keeps the version it checked until it
    /// reconnects.
    ///
    /// `connection` is a libpq-style URL or key=value string for the login
    /// role, which must be able to `SET ROLE` to `authenticated` and
    /// `gnomon_worker`. TLS with certificate verification is not built yet,
    /// so this connects in plain text.
    pub async fn connect_no_tls(connection: &str) -> Result<Store, Error> {
        let (mut client, conn) = tokio_postgres::connect(connection, NoTls).await?;
        tokio::spawn(async move {
            // The connection ends when the Store drops its client. An error
            // here surfaces on the Store's next call.
            let _ = conn.await;
        });
        let embedding_dimension = check_schema(&mut client).await?;
        Ok(Store {
            client: Mutex::new(client),
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
    /// `gnomon.tenant_id()` and `gnomon.allowed_scopes()` must resolve them.
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

/// Reads the ledger and the embedding dimension, as an end user would, and
/// refuses any version but the one this build expects.
async fn check_schema(client: &mut Client) -> Result<EmbeddingDimension, Error> {
    let tx = client.transaction().await?;
    tx.batch_execute(&format!("set local role {}", CallerRole::User.name()))
        .await?;
    let row = match tx
        .query_typed(
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
    /// The claims and the check share one statement, so a call costs no
    /// more round trips than it did before tenancy modes. The materialized
    /// CTE sets the claims before the outer select reads them.
    async fn begin<'c>(
        &self,
        client: &'c mut Client,
        tenant: Option<TenantId>,
    ) -> Result<(Transaction<'c>, TenantId), Error> {
        let tx = client.transaction().await?;
        tx.batch_execute(&format!("set local role {}", self.role.name()))
            .await?;
        let rows = tx
            .query_typed(
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
        let rows = tx
            .query_typed(
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
        let rows = tx
            .query_typed(
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
        tx.query_typed(
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
        let registered = read_kind(&tx, tenant, &kind.name).await?;
        tx.commit().await?;
        Ok(registered.expect("a Kind just registered is visible to its registrant"))
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
        let kind = read_kind(&tx, tenant, name).await?;
        tx.commit().await?;
        Ok(kind)
    }
}

/// One write of a batch: to a new Node, or to an existing one. Every write
/// of a batch is in the batch's one Tenant.
enum Target<'a> {
    New(&'a NewNode),
    Existing(NodeId),
}

impl Caller<'_> {
    /// Creates a Node and writes its first patch, in one transaction.
    pub async fn create_node(
        &self,
        node: &NewNode,
        patch: &Patch,
    ) -> Result<(NodeId, WriteOutcome), Error> {
        let mut done = self
            .run_writes(node.tenant, &[(Target::New(node), patch)])
            .await?;
        Ok(done.remove(0))
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
            .run_writes(tenant.into(), &[(Target::Existing(node), patch)])
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
            .map(|(node, patch)| (Target::Existing(*node), patch))
            .collect();
        Ok(self
            .run_writes(tenant.into(), &targets)
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
    ) -> Result<Vec<(NodeId, WriteOutcome)>, Error> {
        let mut client = self.store.client.lock().await;
        let mut attempt = 0;
        loop {
            match self.try_writes(&mut client, tenant, writes).await {
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
    ) -> Result<Vec<(NodeId, WriteOutcome)>, Error> {
        let (tx, tenant) = self.begin(client, tenant).await?;
        let mut done = Vec::with_capacity(writes.len());
        for (target, patch) in writes {
            let node = match target {
                Target::Existing(node) => *node,
                Target::New(new) => {
                    let rows = tx
                        .query_typed(
                            "select gnomon.create_node($1, $2, $3)",
                            &[
                                (&tenant.0, Type::UUID),
                                (&new.scope.as_str(), Type::TEXT),
                                (&new.kind, Type::TEXT),
                            ],
                        )
                        .await
                        .map_err(Error::from_store)?;
                    NodeId(rows[0].get(0))
                }
            };
            done.push((node, write_patch(&tx, tenant, node, patch).await?));
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
        let rows = tx
            .query_typed(
                "select id, node_id, scope, kind, props,
                        nullif(valid_from, '-infinity'), valid_from_precision::text,
                        nullif(valid_to, 'infinity'), valid_to_precision::text,
                        recorded_from, nullif(recorded_to, 'infinity')
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
        Ok(rows.first().map(|r| {
            let values = match r.get::<_, Value>(4) {
                Value::Object(m) => m,
                other => panic!("gnomon.read_node returned values {other}"),
            };
            NodeVersion {
                id: VersionId(r.get(0)),
                node: NodeId(r.get(1)),
                scope: Scope::new(r.get::<_, String>(2)),
                kind: r.get(3),
                values,
                valid_time: ValidTime {
                    from: bound(r.get(5), r.get(6)),
                    to: bound(r.get(7), r.get(8)),
                },
                recorded_from: r.get(9),
                recorded_to: r.get(10),
            }
        }))
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
        let rows = tx
            .query_typed(
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

async fn write_patch(
    tx: &Transaction<'_>,
    tenant: TenantId,
    node: NodeId,
    patch: &Patch,
) -> Result<WriteOutcome, Error> {
    let (from, to) = patch.valid_time.instants();
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
    let rows = tx
        .query_typed(
            "select status, recorded_at, versions from gnomon.write_node($1, $2, $3, $4, $5, $6, $7)",
            &[
                (&tenant.0, Type::UUID),
                (&node.0, Type::UUID),
                (&patch.episode.0, Type::UUID),
                (&from, Type::TIMESTAMPTZ),
                (&to, Type::TIMESTAMPTZ),
                (&values, Type::JSONB),
                (&places, Type::JSONB),
            ],
        )
        .await
        .map_err(Error::from_store)?;
    let r = &rows[0];
    let status = match r.get::<_, &str>(0) {
        "written" => WriteStatus::Written,
        "unchanged" => WriteStatus::Unchanged,
        other => panic!("gnomon.write_node returned status {other:?}"),
    };
    let opened: Vec<uuid::Uuid> = r.get(2);
    Ok(WriteOutcome {
        status,
        recorded_at: r.get(1),
        opened: opened.into_iter().map(VersionId).collect(),
    })
}

fn is_retryable(e: &Error) -> bool {
    matches!(
        e.sqlstate(),
        Some(c) if *c == SqlState::T_R_SERIALIZATION_FAILURE || *c == SqlState::T_R_DEADLOCK_DETECTED
    )
}

/// A Valid-time bound as the Store returns it: unknown, or an instant.
fn bound(at: Option<DateTime<Utc>>, precision: &str) -> Bound {
    match (precision, at) {
        ("unknown", _) | (_, None) => Bound::Unknown,
        (_, Some(t)) => Bound::At(t),
    }
}

async fn read_kind(
    tx: &Transaction<'_>,
    tenant: TenantId,
    name: &str,
) -> Result<Option<Kind>, Error> {
    let rows = tx
        .query_typed(
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
