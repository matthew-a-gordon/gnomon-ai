//! Synthetic embeddings for the vector Search suites, following prototype 5
//! (`prototype/exact-search`, common.py): unit vectors near the centres of
//! two-level clusters, so each subtopic holds many near-equidistant
//! neighbours. Every draw is seeded, so a run is repeatable.

use std::collections::HashMap;

use jiff::{SignedDuration, Timestamp, civil::date, tz::TimeZone};
use tokio_postgres::types::Type;
use uuid::Uuid;

use super::erasure::World;

/// The embedding dimension the suites use.
pub const DIM: usize = super::DEFAULT_DIMENSION as usize;

/// A small seeded generator (SplitMix64), so the suites need no crate for it.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in [0, 1).
    pub fn uniform(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform in 0..n.
    pub fn below(&mut self, n: usize) -> usize {
        (self.uniform() * n as f64) as usize
    }

    /// Standard normal, by Box-Muller.
    pub fn normal(&mut self) -> f64 {
        let u = self.uniform().max(f64::MIN_POSITIVE);
        let v = self.uniform();
        (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()
    }

    /// A random unit vector.
    pub fn unit(&mut self) -> Vec<f32> {
        unit((0..DIM).map(|_| self.normal() as f32).collect())
    }

    /// A unit vector near `centre`, with noise of norm about `noise`.
    pub fn near(&mut self, centre: &[f32], noise: f64) -> Vec<f32> {
        let scale = noise / (DIM as f64).sqrt();
        unit(
            centre
                .iter()
                .map(|c| c + (self.normal() * scale) as f32)
                .collect(),
        )
    }
}

pub fn unit(mut v: Vec<f32>) -> Vec<f32> {
    let norm = v
        .iter()
        .map(|x| (*x as f64) * (*x as f64))
        .sum::<f64>()
        .sqrt();
    for x in &mut v {
        *x = (*x as f64 / norm) as f32;
    }
    v
}

/// Cosine distance at full precision, as pgvector's `<=>` computes it.
pub fn cosine_distance(a: &[f32], b: &[f32]) -> f64 {
    let (mut dot, mut na, mut nb) = (0.0f64, 0.0f64, 0.0f64);
    for (x, y) in a.iter().zip(b) {
        let (x, y) = (*x as f64, *y as f64);
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    1.0 - dot / (na.sqrt() * nb.sqrt())
}

/// The subtopic centres of prototype 5: `topics` random topics, each with
/// `subs` subtopics at relative distance 0.6 from it.
pub fn subtopics(rng: &mut Rng, topics: usize, subs: usize) -> Vec<Vec<f32>> {
    let mut out = Vec::with_capacity(topics * subs);
    for _ in 0..topics {
        let topic = rng.unit();
        for _ in 0..subs {
            let offset = rng.unit();
            out.push(unit(
                topic
                    .iter()
                    .zip(&offset)
                    .map(|(t, o)| t + 0.6 * o)
                    .collect(),
            ));
        }
    }
    out
}

/// One Version of the synthetic corpus, and which text it holds.
#[derive(Debug, Clone)]
pub struct SyntheticVersion {
    pub id: Uuid,
    pub scope: String,
    pub text: usize,
    pub valid_from: Timestamp,
    pub valid_to: Option<Timestamp>,
    pub recorded_from: Timestamp,
    pub recorded_to: Option<Timestamp>,
}

impl SyntheticVersion {
    /// Whether an As-of read at `valid_at` and `known_at` (`None`: the
    /// current belief) sees this Version.
    pub fn visible(&self, valid_at: Timestamp, known_at: Option<Timestamp>) -> bool {
        let valid = self.valid_from <= valid_at && self.valid_to.is_none_or(|t| valid_at < t);
        let known = match known_at {
            None => self.recorded_to.is_none(),
            Some(k) => self.recorded_from <= k && self.recorded_to.is_none_or(|t| k < t),
        };
        valid && known
    }
}

/// The texts and Versions of the synthetic corpus a test loaded.
#[derive(Debug, Default)]
pub struct Corpus {
    pub texts: Vec<Vec<f32>>,
    pub versions: Vec<SyntheticVersion>,
}

impl Corpus {
    /// The exact top `k` by cosine distance at full precision, over the
    /// Versions of `scopes` an As-of read sees, ties to the lower Version
    /// ID: the ground truth recall is measured against.
    pub fn exact(
        &self,
        query: &[f32],
        scopes: &[&str],
        valid_at: Timestamp,
        known_at: Option<Timestamp>,
        k: usize,
    ) -> Vec<Uuid> {
        let mut d: Vec<(f64, Uuid)> = self
            .versions
            .iter()
            .filter(|v| scopes.contains(&v.scope.as_str()) && v.visible(valid_at, known_at))
            .map(|v| (cosine_distance(query, &self.texts[v.text]), v.id))
            .collect();
        d.sort_by(|a, b| a.partial_cmp(b).unwrap());
        d.into_iter().take(k).map(|(_, id)| id).collect()
    }

    pub fn visible(
        &self,
        scopes: &[&str],
        valid_at: Timestamp,
        known_at: Option<Timestamp>,
    ) -> usize {
        self.versions
            .iter()
            .filter(|v| scopes.contains(&v.scope.as_str()) && v.visible(valid_at, known_at))
            .count()
    }
}

fn days(n: i64) -> SignedDuration {
    SignedDuration::from_hours(24 * n)
}

fn day(rng: &mut Rng, from: Timestamp, to: Timestamp) -> Timestamp {
    let between = to.duration_since(from).as_hours() as usize / 24;
    from + days(rng.below(between) as i64)
}

/// Loads `n` texts into `scope` of `tenant`, as prototype 5's generator
/// lays them out: each Node holds a chain of one to three texts over its
/// Recorded time (60%, 30%, 10%), and 30% of texts are held by two Versions
/// in turn, as a Refinement leaves them. Each text is a vector near one of
/// `centres`. Valid time starts unknown for a fifth of the Nodes and is open
/// for 70%. Recorded times run from 2024-01-01 to 2026-11-01.
///
/// The Nodes and Versions are inserted as the superuser with triggers off,
/// because only that can give a Version a past Recorded time. Every
/// embedding is then stored through `gnomon.store_embedding` as the Scope's
/// worker, as the Pipeline stores one.
pub async fn load(
    db: &super::TestDb,
    corpus: &mut Corpus,
    tenant: gnomon::TenantId,
    scope: &str,
    n: usize,
    centres: &[Vec<f32>],
    rng: &mut Rng,
) {
    let utc = |y, m, d| {
        date(y, m, d)
            .at(0, 0, 0, 0)
            .to_zoned(TimeZone::UTC)
            .unwrap()
            .timestamp()
    };
    let (r_start, r_end) = (utc(2024, 1, 1), utc(2026, 11, 1));
    let (mut node_ids, mut ids, mut node_of, mut props) = (vec![], vec![], vec![], vec![]);
    let (mut vf, mut vt, mut rf, mut rt) = (vec![], vec![], vec![], vec![]);
    let mut made = 0;
    while made < n {
        let chain = match rng.uniform() {
            x if x < 0.6 => 1,
            x if x < 0.9 => 2,
            _ => 3,
        }
        .min(n - made);
        let node = Uuid::new_v4();
        node_ids.push(node);
        let (valid_from, valid_to) = if rng.uniform() < 0.2 {
            (None, None)
        } else {
            let from = day(rng, utc(2020, 1, 1), utc(2025, 6, 1));
            let to = (rng.uniform() >= 0.7).then(|| from + days(100 + rng.below(1400) as i64));
            (Some(from), to)
        };
        // The Versions of the chain, in Recorded time.
        let mut held = vec![];
        for _ in 0..chain {
            let text = corpus.texts.len();
            let centre = &centres[rng.below(centres.len())];
            corpus.texts.push(rng.near(centre, 0.6));
            held.push(text);
            if rng.uniform() < 0.3 {
                held.push(text);
            }
            made += 1;
        }
        let mut at = day(rng, r_start, r_end - days(30));
        for (i, text) in held.iter().enumerate() {
            // Each Version but the last is closed by the next, from one to
            // 200 days later and before the end of the range.
            let next = (i + 1 < held.len()).then(|| {
                let later = at + days(1 + rng.below(200) as i64);
                if later < r_end {
                    later
                } else {
                    at + SignedDuration::from_hours(1)
                }
            });
            let id = Uuid::new_v4();
            corpus.versions.push(SyntheticVersion {
                id,
                scope: scope.to_owned(),
                text: *text,
                valid_from: valid_from.unwrap_or(Timestamp::MIN),
                valid_to,
                recorded_from: at,
                recorded_to: next,
            });
            ids.push(id);
            node_of.push(node);
            props.push(serde_json::json!({ "name": format!("{scope} text {text}") }).to_string());
            vf.push(valid_from);
            vt.push(valid_to);
            rf.push(at);
            rt.push(next);
            if let Some(n) = next {
                at = n;
            }
        }
    }

    let mut admin = db.admin().await;
    let tx = admin.transaction().await.unwrap();
    tx.batch_execute("set local session_replication_role = replica")
        .await
        .unwrap();
    tx.execute_typed(
        "insert into gnomon.node (tenant_id, scope, id, kind)
         select $1, $2, x, 'company' from unnest($3::uuid[]) x",
        &[
            (&tenant.0, Type::UUID),
            (&scope, Type::TEXT),
            (&node_ids, Type::UUID_ARRAY),
        ],
    )
    .await
    .unwrap();
    tx.execute_typed(
        "insert into gnomon.version (tenant_id, scope, id, node_id, props,
                                     valid_from, valid_from_precision, valid_to, valid_to_precision,
                                     recorded_from, recorded_to, merge_rule, closed_in)
         select $1, $2, x.id, x.node, x.props::jsonb,
                coalesce(x.vf, '-infinity'),
                case when x.vf is null then 'unknown' else 'instant' end::gnomon.time_precision,
                coalesce(x.vt, 'infinity'),
                case when x.vt is null then 'unknown' else 'instant' end::gnomon.time_precision,
                x.rf, coalesce(x.rt, 'infinity'), 'recency',
                case when x.rt is null then null else '0'::xid8 end
         from unnest($3::uuid[], $4::uuid[], $5::text[], $6::timestamptz[], $7::timestamptz[],
                     $8::timestamptz[], $9::timestamptz[]) x(id, node, props, vf, vt, rf, rt)",
        &[
            (&tenant.0, Type::UUID),
            (&scope, Type::TEXT),
            (&ids, Type::UUID_ARRAY),
            (&node_of, Type::UUID_ARRAY),
            (&props, Type::TEXT_ARRAY),
            (&vf, Type::TIMESTAMPTZ_ARRAY),
            (&vt, Type::TIMESTAMPTZ_ARRAY),
            (&rf, Type::TIMESTAMPTZ_ARRAY),
            (&rt, Type::TIMESTAMPTZ_ARRAY),
        ],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    // The embeddings, through the Store's function, as the worker.
    let claims = serde_json::json!({
        "role": "gnomon_worker",
        "app_metadata": { "tenant_id": tenant.0.to_string(), "scopes": [scope] }
    })
    .to_string();
    let first = corpus.versions.len() - ids.len();
    for chunk in (first..corpus.versions.len())
        .collect::<Vec<_>>()
        .chunks(500)
    {
        let batch: Vec<Uuid> = chunk.iter().map(|i| corpus.versions[*i].id).collect();
        let flat: Vec<f32> = chunk
            .iter()
            .flat_map(|i| corpus.texts[corpus.versions[*i].text].iter().copied())
            .collect();
        let tx = admin.transaction().await.unwrap();
        tx.batch_execute("set local role gnomon_worker")
            .await
            .unwrap();
        tx.query_typed(
            "select set_config('request.jwt.claims', $1, true)",
            &[(&claims, Type::TEXT)],
        )
        .await
        .unwrap();
        tx.query_typed(
            "select gnomon.store_embedding($1, v.id, v.text_hash,
                                           ($3::real[])[(x.i - 1) * $4 + 1 : x.i * $4])
             from unnest($2::uuid[]) with ordinality x(id, i)
             join gnomon.version v on v.tenant_id = $1 and v.id = x.id",
            &[
                (&tenant.0, Type::UUID),
                (&batch, Type::UUID_ARRAY),
                (&flat, Type::FLOAT4_ARRAY),
                (&(DIM as i32), Type::INT4),
            ],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }
}

/// The hash of a Sentence's text, as the Store computes it.
pub async fn sentence_hash(w: &World, sentence: &str) -> String {
    w.db.admin()
        .await
        .query_typed(
            "select gnomon.sentence_text_hash($1)",
            &[(&sentence, Type::TEXT)],
        )
        .await
        .unwrap()[0]
        .get(0)
}

/// Stores an embedding of the values of every Version of `scope`, current
/// and closed, as that Scope's worker, one vector per text, so Versions with
/// the same text pass the same vector. Returns the vector of each text hash.
pub async fn embed_every_version(
    w: &World,
    scope: &str,
    rng: &mut Rng,
) -> HashMap<String, Vec<f32>> {
    let rows =
        w.db.admin()
            .await
            .query_typed(
                "select id, text_hash from gnomon.version where scope = $1 order by id",
                &[(&scope, Type::TEXT)],
            )
            .await
            .unwrap();
    let mut vectors = HashMap::new();
    for r in rows {
        let (id, hash): (Uuid, String) = (r.get(0), r.get(1));
        let v = vectors
            .entry(hash.clone())
            .or_insert_with(|| rng.unit())
            .clone();
        w.worker_in(scope)
            .store_embedding(w.tenant, gnomon::VersionId(id), &hash, &v)
            .await
            .unwrap();
    }
    vectors
}

/// One name or Alias of the synthetic name corpus, and which text it holds.
#[derive(Debug, Clone)]
pub struct SyntheticName {
    pub node: Uuid,
    pub scope: String,
    pub text: usize,
    /// Whether its Version is the current belief, or one a later Version
    /// replaced.
    pub current: bool,
    pub valid_from: Timestamp,
    pub valid_to: Option<Timestamp>,
}

/// The texts and names of the synthetic name corpus a test loaded.
#[derive(Debug, Default)]
pub struct NameCorpus {
    pub texts: Vec<Vec<f32>>,
    pub names: Vec<SyntheticName>,
}

impl NameCorpus {
    /// The exact top `k` Nodes by the cosine distance of their nearest
    /// current name or Alias at full precision, over `scopes`, valid at
    /// `valid_at` when it is set, ties to the lower Node ID: the ground
    /// truth Resolution's recall is measured against.
    pub fn exact(
        &self,
        query: &[f32],
        scopes: &[&str],
        valid_at: Option<Timestamp>,
        k: usize,
    ) -> Vec<Uuid> {
        let mut best: HashMap<Uuid, f64> = HashMap::new();
        for n in self.names.iter().filter(|n| {
            n.current
                && scopes.contains(&n.scope.as_str())
                && valid_at
                    .is_none_or(|at| n.valid_from <= at && n.valid_to.is_none_or(|to| at < to))
        }) {
            let d = cosine_distance(query, &self.texts[n.text]);
            let e = best.entry(n.node).or_insert(f64::INFINITY);
            if d < *e {
                *e = d;
            }
        }
        let mut d: Vec<(f64, Uuid)> = best.into_iter().map(|(n, d)| (d, n)).collect();
        d.sort_by(|a, b| a.partial_cmp(b).unwrap());
        d.into_iter().take(k).map(|(_, n)| n).collect()
    }

    /// How many current names and Aliases of `scopes` there are.
    pub fn current(&self, scopes: &[&str]) -> usize {
        self.names
            .iter()
            .filter(|n| n.current && scopes.contains(&n.scope.as_str()))
            .count()
    }
}

/// Loads `nodes` Nodes of Kind `company` into `scope` of `tenant`, each
/// with a current Version holding a name and up to two Aliases (60%, 30%,
/// 10% hold one, two or three names). A third of the Nodes also have an
/// earlier Version, replaced in Recorded time, with a name of its own. Each
/// text is a vector near one of `centres`. Valid time starts unknown for a
/// fifth of the Nodes, and ends for 30% of the rest.
///
/// The Nodes and Versions are inserted as the superuser with triggers off,
/// because only that can give a Version a past Recorded time, and their name
/// rows are then derived by `gnomon.index_node_names`, as the search index
/// trigger derives them. The embeddings of current names are stored through
/// `gnomon.store_name_embedding` as the Scope's worker, as the Pipeline
/// stores one. Those of replaced names, which that call refuses, are stored
/// as the superuser, so a search that read replaced names would find them.
pub async fn load_names(
    db: &super::TestDb,
    corpus: &mut NameCorpus,
    tenant: gnomon::TenantId,
    scope: &str,
    nodes: usize,
    centres: &[Vec<f32>],
    rng: &mut Rng,
) {
    let utc = |y, m, d| {
        date(y, m, d)
            .at(0, 0, 0, 0)
            .to_zoned(TimeZone::UTC)
            .unwrap()
            .timestamp()
    };
    let (mut node_ids, mut ids, mut node_of, mut props) = (vec![], vec![], vec![], vec![]);
    let (mut vf, mut vt, mut rf, mut rt) = (vec![], vec![], vec![], vec![]);
    let (mut current, mut replaced) = (vec![], vec![]);
    let text = |corpus: &mut NameCorpus, rng: &mut Rng| {
        let i = corpus.texts.len();
        let centre = &centres[rng.below(centres.len())];
        corpus.texts.push(rng.near(centre, 0.6));
        i
    };
    for _ in 0..nodes {
        let node = Uuid::new_v4();
        node_ids.push(node);
        let (valid_from, valid_to) = if rng.uniform() < 0.2 {
            (None, None)
        } else {
            let from = day(rng, utc(2020, 1, 1), utc(2025, 6, 1));
            let to = (rng.uniform() >= 0.7).then(|| from + days(100 + rng.below(1400) as i64));
            (Some(from), to)
        };
        let replaced_at = day(rng, utc(2024, 1, 1), utc(2026, 1, 1));
        let mut versions = vec![];
        if rng.uniform() < 1.0 / 3.0 {
            versions.push((false, vec![text(corpus, rng)]));
        }
        let held = match rng.uniform() {
            x if x < 0.6 => 1,
            x if x < 0.9 => 2,
            _ => 3,
        };
        versions.push((true, (0..held).map(|_| text(corpus, rng)).collect()));
        let only = versions.len() == 1;
        for (is_current, texts) in versions {
            let name_of = |t: &usize| format!("{scope} name {t}");
            for t in &texts {
                corpus.names.push(SyntheticName {
                    node,
                    scope: scope.to_owned(),
                    text: *t,
                    current: is_current,
                    valid_from: valid_from.unwrap_or(Timestamp::MIN),
                    valid_to,
                });
                if is_current {
                    current.push((node, name_of(t), *t));
                } else {
                    replaced.push((name_of(t), *t));
                }
            }
            ids.push(Uuid::new_v4());
            node_of.push(node);
            props.push(
                serde_json::json!({
                    "name": name_of(&texts[0]),
                    "aliases": texts[1..].iter().map(name_of).collect::<Vec<_>>(),
                })
                .to_string(),
            );
            vf.push(valid_from);
            vt.push(valid_to);
            if is_current {
                rf.push(if only { utc(2024, 1, 1) } else { replaced_at });
                rt.push(None);
            } else {
                rf.push(utc(2023, 6, 1));
                rt.push(Some(replaced_at));
            }
        }
    }

    let mut admin = db.admin().await;
    let tx = admin.transaction().await.unwrap();
    tx.batch_execute("set local session_replication_role = replica")
        .await
        .unwrap();
    tx.execute_typed(
        "insert into gnomon.node (tenant_id, scope, id, kind)
         select $1, $2, x, 'company' from unnest($3::uuid[]) x",
        &[
            (&tenant.0, Type::UUID),
            (&scope, Type::TEXT),
            (&node_ids, Type::UUID_ARRAY),
        ],
    )
    .await
    .unwrap();
    tx.execute_typed(
        "insert into gnomon.version (tenant_id, scope, id, node_id, props,
                                     valid_from, valid_from_precision, valid_to, valid_to_precision,
                                     recorded_from, recorded_to, merge_rule, closed_in)
         select $1, $2, x.id, x.node, x.props::jsonb,
                coalesce(x.vf, '-infinity'),
                case when x.vf is null then 'unknown' else 'instant' end::gnomon.time_precision,
                coalesce(x.vt, 'infinity'),
                case when x.vt is null then 'unknown' else 'instant' end::gnomon.time_precision,
                x.rf, coalesce(x.rt, 'infinity'), 'recency',
                case when x.rt is null then null else '0'::xid8 end
         from unnest($3::uuid[], $4::uuid[], $5::text[], $6::timestamptz[], $7::timestamptz[],
                     $8::timestamptz[], $9::timestamptz[]) x(id, node, props, vf, vt, rf, rt)",
        &[
            (&tenant.0, Type::UUID),
            (&scope, Type::TEXT),
            (&ids, Type::UUID_ARRAY),
            (&node_of, Type::UUID_ARRAY),
            (&props, Type::TEXT_ARRAY),
            (&vf, Type::TIMESTAMPTZ_ARRAY),
            (&vt, Type::TIMESTAMPTZ_ARRAY),
            (&rf, Type::TIMESTAMPTZ_ARRAY),
            (&rt, Type::TIMESTAMPTZ_ARRAY),
        ],
    )
    .await
    .unwrap();
    tx.batch_execute("set local session_replication_role = origin")
        .await
        .unwrap();
    tx.execute_typed(
        "select gnomon.index_node_names($1, $2)",
        &[(&tenant.0, Type::UUID), (&ids, Type::UUID_ARRAY)],
    )
    .await
    .unwrap();
    // The replaced names' embeddings, past the Store's function.
    let flat: Vec<f32> = replaced
        .iter()
        .flat_map(|(_, t)| corpus.texts[*t].iter().copied())
        .collect();
    let names: Vec<String> = replaced.iter().map(|(n, _)| n.clone()).collect();
    tx.execute_typed(
        "insert into gnomon.embedding (tenant_id, scope, text_hash, embedding)
         select $1, $2, gnomon.sentence_text_hash(x.name),
                gnomon.embedding_vector(($4::real[])[(x.i - 1) * $5 + 1 : x.i * $5])
         from unnest($3::text[]) with ordinality x(name, i)",
        &[
            (&tenant.0, Type::UUID),
            (&scope, Type::TEXT),
            (&names, Type::TEXT_ARRAY),
            (&flat, Type::FLOAT4_ARRAY),
            (&(DIM as i32), Type::INT4),
        ],
    )
    .await
    .unwrap();
    tx.execute_typed(
        "update gnomon.node_name set q = null
         where tenant_id = $1 and scope = $2 and name = any($3)",
        &[
            (&tenant.0, Type::UUID),
            (&scope, Type::TEXT),
            (&names, Type::TEXT_ARRAY),
        ],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    // The current names' embeddings, through the Store's function, as the
    // worker.
    let claims = serde_json::json!({
        "role": "gnomon_worker",
        "app_metadata": { "tenant_id": tenant.0.to_string(), "scopes": [scope] }
    })
    .to_string();
    for chunk in current.chunks(500) {
        let nodes: Vec<Uuid> = chunk.iter().map(|(n, _, _)| *n).collect();
        let names: Vec<String> = chunk.iter().map(|(_, n, _)| n.clone()).collect();
        let flat: Vec<f32> = chunk
            .iter()
            .flat_map(|(_, _, t)| corpus.texts[*t].iter().copied())
            .collect();
        let tx = admin.transaction().await.unwrap();
        tx.batch_execute("set local role gnomon_worker")
            .await
            .unwrap();
        tx.query_typed(
            "select set_config('request.jwt.claims', $1, true)",
            &[(&claims, Type::TEXT)],
        )
        .await
        .unwrap();
        tx.query_typed(
            "select gnomon.store_name_embedding($1, x.node, x.name,
                                                ($4::real[])[(x.i - 1) * $5 + 1 : x.i * $5])
             from unnest($2::uuid[], $3::text[]) with ordinality x(node, name, i)",
            &[
                (&tenant.0, Type::UUID),
                (&nodes, Type::UUID_ARRAY),
                (&names, Type::TEXT_ARRAY),
                (&flat, Type::FLOAT4_ARRAY),
                (&(DIM as i32), Type::INT4),
            ],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }
}

/// Stores an embedding of every current name and Alias of `scope`, as that
/// Scope's worker, one vector per text. Returns the vector of each text.
pub async fn embed_every_name(w: &World, scope: &str, rng: &mut Rng) -> HashMap<String, Vec<f32>> {
    let rows =
        w.db.admin()
            .await
            .query_typed(
                "select distinct node_id, name from gnomon.node_name
             where scope = $1 and recorded_to = 'infinity' order by 1, 2",
                &[(&scope, Type::TEXT)],
            )
            .await
            .unwrap();
    let mut vectors = HashMap::new();
    for r in rows {
        let (node, name): (Uuid, String) = (r.get(0), r.get(1));
        let v = vectors
            .entry(name.clone())
            .or_insert_with(|| rng.unit())
            .clone();
        w.worker_in(scope)
            .store_name_embedding(w.tenant, gnomon::NodeId(node), &name, &v)
            .await
            .unwrap();
    }
    vectors
}
