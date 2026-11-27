//! What the Erasure suites share (ADR-0003, ADR-0009): a Tenant with Kinds
//! to write and the grants to erase, and the checks that read every gnomon
//! table from the catalog, so that a table a later migration adds fails them
//! until Erasure deletes from it.

use chrono::{DateTime, TimeZone, Utc};
use gnomon::{
    AsOf, EdgeId, EpisodeErasure, EpisodeId, GraphErasure, KindOf, NewEdge, NewEpisode, NewKind,
    NewNode, NodeId, NodeVersion, Patch, Scope, StageOutcome, StageRecord, StageUsage, StageWrite,
    Store, TenantId, ValidTime, VersionId,
};
use serde_json::{Map, Value, json};
use tokio_postgres::types::Type;
use tokio_postgres::{Client, GenericClient};
use uuid::Uuid;

use super::TestDb;

/// The Scope the worker writes in.
pub const SCOPE: &str = "support";

/// Every Scope the Erasure suites write in.
pub const ALL_SCOPES: &[&str] = &[SCOPE, "billing", "hr"];

pub fn at(y: i32, m: u32, d: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, m, d, 0, 0, 0).unwrap()
}

pub fn values(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        other => panic!("not an object: {other}"),
    }
}

pub struct World {
    pub db: TestDb,
    pub store: Store,
    pub tenant: TenantId,
}

impl World {
    /// A Tenant with the Node Kinds `account`, which declares `plan`, and
    /// `company`, and the Edge Kind `owns`, which declares `share`. The
    /// worker may register Kinds and erase Episodes, Nodes, Edges and
    /// Scopes.
    pub async fn new() -> World {
        let db = TestDb::migrated().await;
        db.grant_kind_registration("gnomon_worker").await;
        db.grant_erasure("gnomon_worker").await;
        let store = Store::connect(&db.login_url(), &super::connect_options())
            .await
            .unwrap();
        let w = World {
            db,
            store,
            tenant: super::tenant(),
        };
        for (name, of, properties) in [
            (
                "account",
                KindOf::Node,
                json!({ "plan": { "type": "string" } }),
            ),
            ("company", KindOf::Node, json!({})),
            (
                "owns",
                KindOf::Edge {
                    source: vec!["account".into()],
                    target: vec!["company".into()],
                },
                json!({ "share": { "type": "integer" } }),
            ),
        ] {
            w.caller()
                .register_kind(&NewKind {
                    tenant: Some(w.tenant),
                    name: name.into(),
                    of,
                    parent: None,
                    properties: json!({ "type": "object", "properties": properties }),
                    search: vec![],
                })
                .await
                .unwrap();
        }
        w
    }

    pub fn caller(&self) -> gnomon::Caller<'_> {
        self.store.as_worker(self.tenant, Scope::new(SCOPE))
    }

    pub fn claims(&self) -> String {
        json!({
            "role": "gnomon_worker",
            "app_metadata": { "tenant_id": self.tenant.0.to_string(), "scopes": [SCOPE] }
        })
        .to_string()
    }

    /// A raw transaction as the worker, as the Store opens one.
    pub async fn raw<'c>(&self, client: &'c mut Client) -> tokio_postgres::Transaction<'c> {
        let tx = client.transaction().await.unwrap();
        tx.batch_execute("set local role gnomon_worker")
            .await
            .unwrap();
        tx.query_typed(
            "select set_config('request.jwt.claims', $1, true)",
            &[(&self.claims(), Type::TEXT)],
        )
        .await
        .unwrap();
        tx
    }

    pub async fn episode(
        &self,
        reference_time: DateTime<Utc>,
        authority: Option<i32>,
    ) -> EpisodeId {
        self.episode_saying(reference_time, authority, "an Episode")
            .await
    }

    pub async fn episode_saying(
        &self,
        reference_time: DateTime<Utc>,
        authority: Option<i32>,
        content: &str,
    ) -> EpisodeId {
        self.caller()
            .record_episode(&NewEpisode {
                tenant: Some(self.tenant),
                scope: Scope::new(SCOPE),
                content: format!("{content} from {reference_time}"),
                reference_time,
                authority,
            })
            .await
            .unwrap()
    }

    pub async fn node(&self, kind: &str, episode: EpisodeId, vt: ValidTime, v: Value) -> NodeId {
        self.caller()
            .create_node(
                &NewNode {
                    tenant: Some(self.tenant),
                    scope: Scope::new(SCOPE),
                    kind: kind.into(),
                },
                &Patch::new(episode, vt, values(v)),
            )
            .await
            .unwrap()
            .0
    }

    pub async fn edge(
        &self,
        source: NodeId,
        target: NodeId,
        episode: EpisodeId,
        vt: ValidTime,
        v: Value,
    ) -> EdgeId {
        self.caller()
            .create_edge(
                &NewEdge {
                    tenant: Some(self.tenant),
                    scope: Scope::new(SCOPE),
                    kind: "owns".into(),
                    source,
                    target,
                },
                &Patch::new(episode, vt, values(v)),
            )
            .await
            .unwrap()
            .0
    }

    pub async fn write(
        &self,
        node: NodeId,
        episode: EpisodeId,
        vt: ValidTime,
        v: Value,
    ) -> DateTime<Utc> {
        self.caller()
            .write_node(self.tenant, node, &Patch::new(episode, vt, values(v)))
            .await
            .unwrap()
            .recorded_at
    }

    pub async fn write_edge(&self, edge: EdgeId, episode: EpisodeId, vt: ValidTime, v: Value) {
        self.caller()
            .write_edge(self.tenant, edge, &Patch::new(episode, vt, values(v)))
            .await
            .unwrap();
    }

    pub async fn erase(&self, episode: EpisodeId) -> EpisodeErasure {
        self.caller()
            .erase_episode(self.tenant, episode)
            .await
            .unwrap()
    }

    /// A user who sees these Scopes of the Tenant. Erasure is not granted to
    /// users, so this caller writes and reads but does not erase.
    pub fn seeing(&self, scopes: &[&str]) -> gnomon::Caller<'_> {
        self.store.as_user(values(json!({
            "role": "authenticated",
            "app_metadata": { "tenant_id": self.tenant.0.to_string(), "scopes": scopes }
        })))
    }

    /// The worker of a Scope, which may erase in that Scope only.
    pub fn worker_in(&self, scope: &str) -> gnomon::Caller<'_> {
        self.store.as_worker(self.tenant, Scope::new(scope))
    }

    pub async fn episode_in(
        &self,
        scope: &str,
        reference_time: DateTime<Utc>,
        content: &str,
    ) -> EpisodeId {
        self.worker_in(scope)
            .record_episode(&NewEpisode {
                tenant: Some(self.tenant),
                scope: Scope::new(scope),
                content: content.into(),
                reference_time,
                authority: None,
            })
            .await
            .unwrap()
    }

    pub async fn node_in(&self, scope: &str, kind: &str, episode: EpisodeId, v: Value) -> NodeId {
        self.worker_in(scope)
            .create_node(
                &NewNode {
                    tenant: Some(self.tenant),
                    scope: Scope::new(scope),
                    kind: kind.into(),
                },
                &Patch::new(episode, ValidTime::UNKNOWN, values(v)),
            )
            .await
            .unwrap()
            .0
    }

    /// An Edge of `scope` written by a user who sees every Scope the suites
    /// use, so it may join a Node of any of them.
    pub async fn edge_in(
        &self,
        scope: &str,
        source: NodeId,
        target: NodeId,
        episode: EpisodeId,
        v: Value,
    ) -> EdgeId {
        self.seeing(ALL_SCOPES)
            .create_edge(
                &NewEdge {
                    tenant: Some(self.tenant),
                    scope: Scope::new(scope),
                    kind: "owns".into(),
                    source,
                    target,
                },
                &Patch::new(episode, ValidTime::UNKNOWN, values(v)),
            )
            .await
            .unwrap()
            .0
    }

    /// A Node no Episode describes, created by the worker of `scope`.
    pub async fn bare_node_in(&self, scope: &str, kind: &str) -> NodeId {
        let claims = json!({
            "role": "gnomon_worker",
            "app_metadata": { "tenant_id": self.tenant.0.to_string(), "scopes": [scope] }
        })
        .to_string();
        let mut client = self.db.login().await;
        let tx = client.transaction().await.unwrap();
        tx.batch_execute("set local role gnomon_worker")
            .await
            .unwrap();
        tx.query_typed(
            "select set_config('request.jwt.claims', $1, true)",
            &[(&claims, Type::TEXT)],
        )
        .await
        .unwrap();
        let id: Uuid = tx
            .query_typed(
                "select gnomon.create_node($1, $2, $3)",
                &[
                    (&self.tenant.0, Type::UUID),
                    (&scope, Type::TEXT),
                    (&kind, Type::TEXT),
                ],
            )
            .await
            .unwrap()[0]
            .get(0);
        tx.commit().await.unwrap();
        NodeId(id)
    }

    pub async fn erase_node(&self, node: NodeId) -> GraphErasure {
        self.caller().erase_node(self.tenant, node).await.unwrap()
    }

    pub async fn erase_edge(&self, edge: EdgeId) -> GraphErasure {
        self.caller().erase_edge(self.tenant, edge).await.unwrap()
    }

    /// Erases a Scope as that Scope's worker.
    pub async fn erase_scope(&self, scope: &str) -> GraphErasure {
        self.worker_in(scope)
            .erase_scope(self.tenant, &Scope::new(scope))
            .await
            .unwrap()
    }

    /// The IDs of every Version of these Nodes and Edges, read past RLS.
    pub async fn versions_of(&self, ids: &[Uuid]) -> Vec<Uuid> {
        let ids = ids.to_vec();
        self.db
            .admin()
            .await
            .query_typed(
                "select id from gnomon.version where versioned_id = any($1) order by id",
                &[(&ids, Type::UUID_ARRAY)],
            )
            .await
            .unwrap()
            .iter()
            .map(|r| r.get(0))
            .collect()
    }

    pub async fn read(&self, node: NodeId, valid_at: DateTime<Utc>) -> Option<NodeVersion> {
        self.caller()
            .read_node(self.tenant, node, AsOf::current(valid_at))
            .await
            .unwrap()
    }

    pub async fn read_as_of(
        &self,
        node: NodeId,
        valid_at: DateTime<Utc>,
        known_at: DateTime<Utc>,
    ) -> Option<NodeVersion> {
        self.caller()
            .read_node(self.tenant, node, AsOf::at(valid_at, known_at))
            .await
            .unwrap()
    }

    pub async fn current_values(&self, node: NodeId, valid_at: DateTime<Utc>) -> Option<Value> {
        self.read(node, valid_at)
            .await
            .map(|v| Value::Object(v.values))
    }

    pub async fn values_as_of(
        &self,
        node: NodeId,
        valid_at: DateTime<Utc>,
        known_at: DateTime<Utc>,
    ) -> Option<Value> {
        self.read_as_of(node, valid_at, known_at)
            .await
            .map(|v| Value::Object(v.values))
    }

    /// Writes what the Pipeline would for an Episode of `scope` (ADR-0011):
    /// its graph stage claimed and finished with `model`'s usage, and its
    /// sentences stage pending. Returns the Episode's stage records.
    pub async fn write_stage_records(
        &self,
        scope: &str,
        episode: EpisodeId,
        model: &str,
    ) -> Vec<StageRecord> {
        let caller = self.worker_in(scope);
        caller
            .write_stages(self.tenant, &[StageWrite::pending(episode, "graph")])
            .await
            .unwrap();
        let claim = caller
            .claim_stage(
                self.tenant,
                Some(episode),
                std::time::Duration::from_secs(3600),
            )
            .await
            .unwrap()
            .expect("the graph stage is due");
        caller
            .write_stages(
                self.tenant,
                &[
                    StageWrite::finish(
                        &claim,
                        StageOutcome::Succeeded,
                        vec![StageUsage {
                            model: model.into(),
                            calls: 2,
                            input_tokens: 800,
                            output_tokens: 90,
                            cost_usd: Some(0.004),
                        }],
                    ),
                    StageWrite::pending(episode, "sentences"),
                ],
            )
            .await
            .unwrap();
        let records = caller.stage_records(self.tenant, episode).await.unwrap();
        assert_eq!(records.len(), 2);
        records
    }

    /// An Episode's stage records, as the worker of `scope` reads them.
    pub async fn stage_records_of(&self, scope: &str, episode: EpisodeId) -> Vec<StageRecord> {
        self.worker_in(scope)
            .stage_records(self.tenant, episode)
            .await
            .unwrap()
    }

    /// Whether a Node or Edge row exists, read past RLS.
    pub async fn exists(&self, table: &str, id: Uuid) -> bool {
        self.db
            .admin()
            .await
            .query_typed(
                &format!("select from gnomon.{table} where id = $1"),
                &[(&id, Type::UUID)],
            )
            .await
            .unwrap()
            .len()
            == 1
    }

    pub async fn store_sentence(&self, version: VersionId, sentence: &str) {
        self.store_sentence_in(SCOPE, version.0, sentence).await;
    }

    /// Stores a Sentence of an Edge Version of `scope` as that Scope's
    /// worker, as the Pipeline does.
    pub async fn store_sentence_in(&self, scope: &str, version: Uuid, sentence: &str) {
        self.worker_in(scope)
            .store_sentence(self.tenant, gnomon::VersionId(version), sentence)
            .await
            .unwrap();
    }

    pub async fn sentence_of(&self, version: VersionId) -> Option<String> {
        self.db
            .admin()
            .await
            .query_typed(
                "select sentence from gnomon.sentence where version_id = $1",
                &[(&version.0, Type::UUID)],
            )
            .await
            .unwrap()
            .first()
            .map(|r| r.get(0))
    }

    pub async fn current_edge_version(&self, edge: EdgeId, valid_at: DateTime<Utc>) -> VersionId {
        self.caller()
            .read_edge(self.tenant, edge, AsOf::current(valid_at))
            .await
            .unwrap()
            .expect("the Edge holds")
            .id
    }
}

/// Every gnomon table, from the catalog.
pub async fn gnomon_tables(admin: &Client) -> Vec<String> {
    admin
        .query_typed(
            "select c.relname::text from pg_class c join pg_namespace n on n.oid = c.relnamespace
             where n.nspname = 'gnomon' and c.relkind in ('r', 'p') order by 1",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect()
}

/// The gnomon tables that reference an Episode: a foreign key to
/// `gnomon.episode`, or an `episode_id` column.
pub async fn tables_with_an_episode_reference(admin: &Client) -> Vec<String> {
    admin
        .query_typed(
            "select distinct c.relname::text
             from pg_class c join pg_namespace n on n.oid = c.relnamespace
             where n.nspname = 'gnomon' and c.relkind in ('r', 'p') and c.relname <> 'episode'
               and (exists (select from pg_constraint k
                            where k.conrelid = c.oid and k.contype = 'f'
                              and k.confrelid = 'gnomon.episode'::regclass)
                    or exists (select from pg_attribute a
                               where a.attrelid = c.oid and a.attname = 'episode_id'
                                 and not a.attisdropped))
             order by 1",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect()
}

/// The gnomon tables whose rows are derived from a Version's text: a foreign
/// key to `gnomon.version`, apart from the Citations, which name an Episode
/// and are checked as such. Each names its Version in `version_id`.
pub async fn tables_derived_from_version_text(admin: &Client) -> Vec<String> {
    admin
        .query_typed(
            "select distinct c.relname::text
             from pg_class c join pg_namespace n on n.oid = c.relnamespace
             join pg_constraint k on k.conrelid = c.oid and k.contype = 'f'
             where n.nspname = 'gnomon' and k.confrelid = 'gnomon.version'::regclass
               and c.relname <> 'citation'
             order by 1",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect()
}

/// Every row of these gnomon tables that the client sees, as text, sorted.
pub async fn rows_of(client: &impl GenericClient, tables: &[String]) -> Vec<String> {
    let mut rows = Vec::new();
    for table in tables {
        for r in client
            .query_typed(
                &format!("select '{table} ' || to_jsonb(x)::text from gnomon.{table} x"),
                &[],
            )
            .await
            .unwrap()
        {
            rows.push(r.get(0));
        }
    }
    rows.sort();
    rows
}

/// Every row of every gnomon table the caller sees, as text.
pub async fn snapshot(tx: &tokio_postgres::Transaction<'_>, tables: &[String]) -> Vec<String> {
    rows_of(tx, tables).await
}

/// The columns of gnomon tables that name a row of `gnomon.{table}` by its
/// ID: each foreign key column that references its `id`, and each
/// `{table}_id` column without a foreign key, such as `statement.version_id`.
/// The table itself is listed with its `id`.
pub async fn columns_naming(admin: &Client, table: &str) -> Vec<(String, String)> {
    admin
        .query_typed(
            "select $1::text, 'id'
             union
             select c.relname::text, a.attname::text
             from pg_constraint k
             join pg_class c on c.oid = k.conrelid
             join pg_namespace n on n.oid = c.relnamespace
             cross join lateral unnest(k.conkey, k.confkey) u(col, fcol)
             join pg_attribute a on a.attrelid = k.conrelid and a.attnum = u.col
             join pg_attribute f on f.attrelid = k.confrelid and f.attnum = u.fcol
             where n.nspname = 'gnomon' and k.contype = 'f'
               and k.confrelid = to_regclass('gnomon.' || $1) and f.attname = 'id'
             union
             select c.relname::text, a.attname::text
             from pg_attribute a
             join pg_class c on c.oid = a.attrelid
             join pg_namespace n on n.oid = c.relnamespace
             where n.nspname = 'gnomon' and c.relkind in ('r', 'p')
               and a.attname = $1 || '_id' and not a.attisdropped
             order by 1, 2",
            &[(&table, Type::TEXT)],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect()
}

/// What an Erasure must leave no trace of.
#[derive(Debug, Default)]
pub struct Residue<'a> {
    /// The Episodes, Nodes, Edges and Versions it erased.
    pub episodes: &'a [Uuid],
    pub nodes: &'a [Uuid],
    pub edges: &'a [Uuid],
    pub versions: &'a [Uuid],
    /// The surviving Versions it recomputed, whose derived rows must go.
    pub recomputed: &'a [Uuid],
    /// Text only the erased rows held.
    pub markers: &'a [&'a str],
}

/// Checks that no row of any gnomon table, read past RLS, holds the text of
/// an erased ID or a marker, that no column naming an Episode, Node, Edge or
/// Version names an erased one, and that no row derived from the text of an
/// erased or recomputed Version is left. Every table and column comes from
/// the catalog.
pub async fn assert_no_residue(admin: &Client, residue: &Residue<'_>) {
    let mut needles: Vec<String> = [
        residue.episodes,
        residue.nodes,
        residue.edges,
        residue.versions,
    ]
    .concat()
    .iter()
    .map(Uuid::to_string)
    .collect();
    needles.extend(residue.markers.iter().map(|m| m.to_string()));
    for table in gnomon_tables(admin).await {
        let n: i64 = admin
            .query_typed(
                &format!(
                    "select count(*) from gnomon.{table} x
                     where exists (select from unnest($1::text[]) s where strpos(x::text, s) > 0)"
                ),
                &[(&needles, Type::TEXT_ARRAY)],
            )
            .await
            .unwrap()[0]
            .get(0);
        assert_eq!(n, 0, "gnomon.{table} still holds an erased ID or value");
    }
    for (target, ids) in [
        ("episode", residue.episodes),
        ("node", residue.nodes),
        ("edge", residue.edges),
        ("version", residue.versions),
    ] {
        let ids = ids.to_vec();
        for (table, column) in columns_naming(admin, target).await {
            let n: i64 = admin
                .query_typed(
                    &format!("select count(*) from gnomon.{table} where {column} = any($1)"),
                    &[(&ids, Type::UUID_ARRAY)],
                )
                .await
                .unwrap()[0]
                .get(0);
            assert_eq!(
                n, 0,
                "gnomon.{table}.{column} still names an erased {target}"
            );
        }
    }
    assert_no_unused_full_vector(admin).await;
    let gone = [residue.versions, residue.recomputed].concat();
    for table in tables_derived_from_version_text(admin).await {
        if table == SEARCH_TOKEN {
            assert_tokens_derived_anew(admin, residue).await;
            continue;
        }
        if table == NAME_ROWS {
            assert_names_derived_anew(admin, residue).await;
            continue;
        }
        let n: i64 = admin
            .query_typed(
                &format!("select count(*) from gnomon.{table} where version_id = any($1)"),
                &[(&gone, Type::UUID_ARRAY)],
            )
            .await
            .unwrap()[0]
            .get(0);
        assert_eq!(
            n, 0,
            "gnomon.{table} still holds rows of erased or recomputed Versions"
        );
    }
}

/// The full vectors of vector Search (migration 0017), each the embedding of
/// one text of a Scope. A row names no Version, so an Erasure that deletes
/// one leaves no ID behind in the row's text; [`assert_no_unused_full_vector`]
/// checks it instead.
pub const FULL_VECTORS: &str = "embedding";

/// Checks, past RLS, that every full vector left is of a text some row of
/// its Scope still uses: a quantized row of a Version, a Version's text, a
/// Sentence, or a name or Alias of a Node Version. A full vector of erased
/// text has none of them, unless a surviving Version, Sentence or name holds
/// the same text.
pub async fn assert_no_unused_full_vector(admin: &Client) {
    let unused: Vec<String> = admin
        .query_typed(
            "select format('%s %s', e.scope, e.text_hash) from gnomon.embedding e
             where not exists (select from gnomon.embedding_bit b
                               where b.tenant_id = e.tenant_id and b.scope = e.scope
                                 and b.text_hash = e.text_hash)
               and not exists (select from gnomon.version v
                               where v.tenant_id = e.tenant_id and v.scope = e.scope
                                 and v.text_hash = e.text_hash)
               and not exists (select from gnomon.sentence s
                               where s.tenant_id = e.tenant_id and s.scope = e.scope
                                 and gnomon.sentence_text_hash(s.sentence) = e.text_hash)
               and not exists (select from gnomon.node_name n
                               where n.tenant_id = e.tenant_id and n.scope = e.scope
                                 and n.text_hash = e.text_hash)
             order by 1",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert!(
        unused.is_empty(),
        "gnomon.embedding holds full vectors of text no row uses: {unused:?}"
    );
}

/// The token table of full-text Search (migration 0016), whose rows the
/// Store derives from each Version's text.
pub const SEARCH_TOKEN: &str = "search_token";

/// The name table of Resolution (migration 0019), whose rows the Store
/// derives from each Node Version's name and Aliases.
pub const NAME_ROWS: &str = "node_name";

/// The name rows an Erasure leaves: none of a Version it deleted, and for
/// each recomputed Version that survives, exactly the rows its name and
/// Aliases as they stand now give, so none holds a name or Alias only the
/// erased text held. Their bits are not compared: a recomputed Version's
/// name keeps bits only while the full vector of its text survives.
async fn assert_names_derived_anew(admin: &Client, residue: &Residue<'_>) {
    let n: i64 = admin
        .query_typed(
            "select count(*) from gnomon.node_name where version_id = any($1)",
            &[(&residue.versions, Type::UUID_ARRAY)],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(n, 0, "gnomon.node_name still holds rows of erased Versions");
    let stale: Vec<String> = admin
        .query_typed(
            "with kept as (
               select tenant_id, scope, version_id, node_id, text_hash, name, alias,
                      valid_from, valid_to, recorded_to
               from gnomon.node_name where version_id = any($1)),
             anew as (
               select t.* from gnomon.version v
               cross join lateral gnomon.node_names(v.tenant_id, array[v.id]) t
               where v.id = any($1))
             select format('%s %s', version_id, name)
             from ((table kept except table anew) union all (table anew except table kept)) d
             order by 1",
            &[(&residue.recomputed, Type::UUID_ARRAY)],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert!(
        stale.is_empty(),
        "gnomon.node_name holds rows of recomputed Versions not derived from their names \
         and Aliases as they stand: {stale:?}"
    );
}

/// The BM25 counts of each Scope, which an Erasure lowers rather than
/// deletes while the Scope holds other Versions, and the table of each
/// transaction's changes to them, which its commit empties.
pub const BM25_COUNT_TABLES: &[&str] = &[
    "search_token_frequency",
    "search_scope_length",
    "search_count_delta",
];

/// The token rows an Erasure leaves: none of a Version it deleted, and for
/// each recomputed Version that survives, exactly the rows its text as it
/// stands now gives, so none holds a word only the erased text held.
async fn assert_tokens_derived_anew(admin: &Client, residue: &Residue<'_>) {
    let n: i64 = admin
        .query_typed(
            "select count(*) from gnomon.search_token where version_id = any($1)",
            &[(&residue.versions, Type::UUID_ARRAY)],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(
        n, 0,
        "gnomon.search_token still holds rows of erased Versions"
    );
    let stale: Vec<String> = admin
        .query_typed(
            "with kept as (
               select tenant_id, scope, token, version_id, tf, dl
               from gnomon.search_token where version_id = any($1)),
             anew as (
               select t.* from gnomon.version v
               cross join lateral gnomon.search_text_tokens(v.tenant_id, array[v.id]) t
               where v.id = any($1))
             select format('%s %s', version_id, token)
             from ((table kept except table anew) union all (table anew except table kept)) d
             order by 1",
            &[(&residue.recomputed, Type::UUID_ARRAY)],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert!(
        stale.is_empty(),
        "gnomon.search_token holds rows of recomputed Versions not derived from their text \
         as it stands: {stale:?}"
    );
}

/// Checks that the BM25 counts, read past RLS, are those the token rows give
/// when counted again: each Scope's texts per word, and its texts and words.
/// The texts are Versions' and, since migration 0020, Summaries' and
/// Communities'. No committed transaction leaves a change to them behind.
pub async fn assert_search_counts_match_the_token_rows(admin: &Client) {
    let left: i64 = admin
        .query_typed("select count(*) from gnomon.search_count_delta", &[])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(left, 0, "committed changes to the counts are left");
    for (stored, recounted, what) in [
        (
            "select scope, token, versions from gnomon.search_token_frequency",
            "select scope, token, count(*)
             from (select scope, token from gnomon.search_token
                   union all
                   select scope, token from gnomon.derived_search_token) t
             group by 1, 2",
            "the document frequencies",
        ),
        (
            "select scope, versions::text, words from gnomon.search_scope_length",
            "select scope, count(*)::text, sum(dl)::bigint
             from (select distinct scope, version_id, dl from gnomon.search_token
                   union all
                   select distinct scope, derived_id, dl from gnomon.derived_search_token) d
             group by 1",
            "the Scope totals",
        ),
    ] {
        let read = |sql: &'static str| async move {
            let mut rows: Vec<String> = admin
                .query_typed(
                    &format!("select format('%s %s %s', x.a, x.b, x.c) from ({sql}) x(a, b, c)"),
                    &[],
                )
                .await
                .unwrap()
                .iter()
                .map(|r| r.get(0))
                .collect();
            rows.sort();
            rows
        };
        assert_eq!(read(stored).await, read(recounted).await, "{what}");
    }
}

/// Every row of every gnomon table, read past RLS, as text, but for the two
/// tables no Erasure touches, the BM25 counts, which [`assert_only_removed`]
/// checks against the token rows instead, and the full vectors, which it
/// checks with [`assert_no_unused_full_vector`].
pub async fn every_row(admin: &Client) -> Vec<String> {
    let tables: Vec<String> = gnomon_tables(admin)
        .await
        .into_iter()
        .filter(|t| {
            t != "schema_version"
                && t != "tenancy"
                && t != FULL_VECTORS
                && !BM25_COUNT_TABLES.contains(&t.as_str())
        })
        .collect();
    rows_of(admin, &tables).await
}

/// Checks that an Erasure removed exactly the rows that name one of `ids` in
/// their text, from `before`, a read of [`every_row`] taken before it, and
/// changed nothing else, that the BM25 counts went down with the token rows
/// it removed, and that it left no full vector of text no row uses.
pub async fn assert_only_removed(admin: &Client, before: &[String], ids: &[Uuid]) {
    assert_search_counts_match_the_token_rows(admin).await;
    assert_no_unused_full_vector(admin).await;
    let needles: Vec<String> = ids.iter().map(Uuid::to_string).collect();
    let kept: Vec<String> = before
        .iter()
        .filter(|r| !needles.iter().any(|n| r.contains(n.as_str())))
        .cloned()
        .collect();
    assert!(kept.len() < before.len(), "the Erasure removed nothing");
    let after = every_row(admin).await;
    let gone: Vec<&String> = kept
        .iter()
        .filter(|r| after.binary_search(r).is_err())
        .collect();
    let new: Vec<&String> = after
        .iter()
        .filter(|r| kept.binary_search(r).is_err())
        .collect();
    assert!(
        gone.is_empty(),
        "the Erasure removed rows outside it: {gone:#?}"
    );
    assert!(
        new.is_empty(),
        "the Erasure left or wrote rows it should not: {new:#?}"
    );
}

/// The gnomon tables with a `scope` column, from the catalog: every table
/// that holds rows of a Scope.
pub async fn tables_with_a_scope(admin: &Client) -> Vec<String> {
    admin
        .query_typed(
            "select c.relname::text
             from pg_attribute a
             join pg_class c on c.oid = a.attrelid
             join pg_namespace n on n.oid = c.relnamespace
             where n.nspname = 'gnomon' and c.relkind in ('r', 'p')
               and a.attname = 'scope' and not a.attisdropped
             order by 1",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect()
}
