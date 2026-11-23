//! Synthetic embeddings for the vector Search suites, following prototype 5
//! (`prototype/exact-search`, common.py): unit vectors near the centres of
//! two-level clusters, so each subtopic holds many near-equidistant
//! neighbours. Every draw is seeded, so a run is repeatable.

/// The dimension the suites export the migrations with.
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

use chrono::{DateTime, Duration, TimeZone, Utc};
use tokio_postgres::types::Type;
use uuid::Uuid;

/// One Version of the synthetic corpus, and which text it holds.
#[derive(Debug, Clone)]
pub struct SyntheticVersion {
    pub id: Uuid,
    pub scope: String,
    pub text: usize,
    pub valid_from: DateTime<Utc>,
    pub valid_to: Option<DateTime<Utc>>,
    pub recorded_from: DateTime<Utc>,
    pub recorded_to: Option<DateTime<Utc>>,
}

impl SyntheticVersion {
    /// Whether an As-of read at `valid_at` and `known_at` (`None`: the
    /// current belief) sees this Version.
    pub fn visible(&self, valid_at: DateTime<Utc>, known_at: Option<DateTime<Utc>>) -> bool {
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
        valid_at: DateTime<Utc>,
        known_at: Option<DateTime<Utc>>,
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
        valid_at: DateTime<Utc>,
        known_at: Option<DateTime<Utc>>,
    ) -> usize {
        self.versions
            .iter()
            .filter(|v| scopes.contains(&v.scope.as_str()) && v.visible(valid_at, known_at))
            .count()
    }
}

fn day(rng: &mut Rng, from: DateTime<Utc>, to: DateTime<Utc>) -> DateTime<Utc> {
    let days = (to - from).num_days() as usize;
    from + Duration::days(rng.below(days) as i64)
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
    let utc = |y, m, d| Utc.with_ymd_and_hms(y, m, d, 0, 0, 0).unwrap();
    let (r_start, r_end) = (utc(2024, 1, 1), utc(2026, 11, 1));
    let (node_ids, mut ids, mut props, mut vf, mut vt, mut rf, mut rt) =
        (vec![], vec![], vec![], vec![], vec![], vec![], vec![]);
    let mut node_of = vec![];
    let mut made = 0;
    let mut node_ids = node_ids;
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
            let to =
                (rng.uniform() >= 0.7).then(|| from + Duration::days(100 + rng.below(1400) as i64));
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
        let mut at = day(rng, r_start, r_end - Duration::days(30));
        for (i, text) in held.iter().enumerate() {
            let next = (i + 1 < held.len()).then(|| {
                (at + Duration::days(1 + rng.below(200) as i64)).min(r_end - Duration::days(1))
            });
            let next = next
                .filter(|n| *n > at)
                .or_else(|| (i + 1 < held.len()).then(|| at + Duration::hours(1)));
            let id = Uuid::new_v4();
            corpus.versions.push(SyntheticVersion {
                id,
                scope: scope.to_owned(),
                text: *text,
                valid_from: valid_from.unwrap_or(DateTime::<Utc>::MIN_UTC),
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
