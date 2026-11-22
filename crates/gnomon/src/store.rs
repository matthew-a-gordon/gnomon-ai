//! The Store API: every call runs as a caller, under RLS, in one explicit
//! transaction (ADR-0001, ADR-0009).
//!
//! Each call is
//!
//! ```sql
//! begin;
//! set local role <authenticated | gnomon_worker>;
//! select set_config('request.jwt.claims', $1, true);
//! <the call>;
//! commit;
//! ```
//!
//! so a pooled server connection never carries one caller's role or claims
//! into another caller's call. Every parameterized statement goes through
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
use crate::{Episode, EpisodeId, Error, NewEpisode, Scope, TenantId};

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
    /// bypass RLS.
    pub fn as_worker(&self, tenant: TenantId, scope: Scope) -> Caller<'_> {
        let claims = serde_json::json!({
            "role": CallerRole::Worker.name(),
            "app_metadata": {
                "tenant_id": tenant.0.to_string(),
                "scopes": [scope.as_str()],
            },
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
    /// Opens the call's transaction and sets its role and claims locally.
    async fn begin<'c>(&self, client: &'c mut Client) -> Result<Transaction<'c>, Error> {
        let tx = client.transaction().await?;
        tx.batch_execute(&format!("set local role {}", self.role.name()))
            .await?;
        tx.query_typed(
            "select set_config('request.jwt.claims', $1, true)",
            &[(&self.claims, Type::TEXT)],
        )
        .await?;
        Ok(tx)
    }

    /// Records an Episode and returns its new ID, a UUID v7.
    pub async fn record_episode(&self, episode: &NewEpisode) -> Result<EpisodeId, Error> {
        let mut client = self.store.client.lock().await;
        let tx = self.begin(&mut client).await?;
        let rows = tx
            .query_typed(
                "select gnomon.record_episode($1, $2, $3, $4, $5)",
                &[
                    (&episode.tenant.0, Type::UUID),
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
        tenant: TenantId,
        id: EpisodeId,
    ) -> Result<Option<Episode>, Error> {
        let mut client = self.store.client.lock().await;
        let tx = self.begin(&mut client).await?;
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
}
