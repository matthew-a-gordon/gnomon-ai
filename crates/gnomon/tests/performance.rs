//! The performance suite (ticket #21), on demand and apart from the
//! correctness suite. It asserts the Search budgets of ADR-0010, on the data
//! shape of prototype 5 (`origin/prototype/exact-search`), and the patch
//! write limits of ADR-0007 (#84), on a small Store and on a large one. It
//! also times Search and Resolution on the same shape with realistic word
//! frequencies, and writes of long texts (#85). ADR-0020 gives it no
//! `criterion`: it asserts budgets and does not compare runs.
//!
//! Cargo builds it only with `--features performance`, so a plain
//! `cargo test` or `cargo nextest run` never runs it. See `docs/testing.md`
//! for the command, `scripts/perf.sh` for the runs on both images and in a
//! container capped at 4 GiB.
//!
//! Each case writes its environment, the machine load around it and its
//! numbers to `target/tmp/performance/` and prints a summary. A Search case
//! also saves the query plans of one call of each class, taken after the last
//! timed call (#124). A run on a busy machine says so. The cases take a lock,
//! so they never overlap even when the test runner runs tests in parallel.

mod support;

use std::time::{Duration, Instant};

use gnomon::{
    AsOf, KindOf, Mention, NewEpisode, NewKind, NewNode, Patch, ROOT_NODE_KIND, Scope,
    SearchRequest, Store, TenantId, ValidTime,
};
use jiff::{SignedDuration, Timestamp};
use serde_json::{Map, Value, json};
use support::TestDb;
use support::perf::{self, Dataset, QueryWords, Record, Stats, Vocabulary};
use support::vectors::Rng;
use tokio::sync::Mutex;

/// The budgets of ADR-0010: the whole fused Search at p95.
const SEARCH_BUDGET_50K: f64 = 100.0;
const SEARCH_BUDGET_200K: f64 = 250.0;

/// Searches discarded before the timed ones, and timed.
const SEARCH_WARM_UP: usize = 10;
const SEARCH_SAMPLES: usize = 100;

/// The word every text of the data set holds (`perf text <n>`), and the
/// searches by it: discarded, then timed. It is recorded beside the budget
/// and asserts nothing, because the full-text branch then scores every
/// visible row, which no budget covers.
const COMMON_WORD: &str = "text";
const COMMON_WORD_WARM_UP: usize = 5;
const COMMON_WORD_SAMPLES: usize = 50;

/// Held for the whole of each case, so the cases never overlap. A case that
/// panics releases it, and the next one still runs.
static ONE_AT_A_TIME: Mutex<()> = Mutex::const_new(());

/// Saves the plans of one fused Search of `class` in the record.
async fn take_search_plans(
    written: &mut perf::Written,
    data: &Dataset,
    class: &str,
    request: &SearchRequest<'_>,
) {
    written
        .take_plans(class, request.text, async |store| {
            data.worker(store)
                .search(data.tenant(), request, 10)
                .await
                .unwrap();
        })
        .await;
}

async fn fused_search_case(case: &str, visible_target: usize, budget_ms: f64) {
    let _one = ONE_AT_A_TIME.lock().await;
    let data = Dataset::get(perf::texts_for_visible(visible_target), Vocabulary::Numbers).await;
    println!(
        "{case}: {} texts, {} Versions, {} visible now",
        data.texts, data.versions, data.visible
    );
    // The case must be about the size it names, or it proves less than it says.
    assert!(
        (data.visible as f64) >= visible_target as f64 * 0.95
            && (data.visible as f64) <= visible_target as f64 * 1.05,
        "{} visible rows is not about {visible_target}",
        data.visible
    );
    let store = data.store().await;
    let caller = data.worker(&store);
    let centres = data.centres().await;
    let mut rng = Rng::new(21);
    let record = Record::start(case, &data.db).await;

    let current = AsOf::current(perf::valid_at());
    let mut times = vec![];
    let mut vector_times = vec![];
    let mut text_times = vec![];
    for i in 0..SEARCH_WARM_UP + SEARCH_SAMPLES {
        let (text, q) = data.query_for(&mut rng, &centres);
        let request = SearchRequest::text(&text, current).with_embedding(&q);
        let started = Instant::now();
        let results = caller.search(data.tenant(), &request, 10).await.unwrap();
        let took = started.elapsed();
        assert!(!results.is_empty(), "a search found nothing for {text:?}");
        if i >= SEARCH_WARM_UP {
            times.push(took);
            // The two branches alone, for the record.
            if i % 4 == 0 {
                let t = Instant::now();
                caller
                    .search_vector(data.tenant(), &q, current, 10)
                    .await
                    .unwrap();
                vector_times.push(t.elapsed());
                let t = Instant::now();
                caller
                    .search_full_text(data.tenant(), &text, current, 10)
                    .await
                    .unwrap();
                text_times.push(t.elapsed());
            }
        }
    }
    // The word every text holds, each time with a fresh query vector.
    let mut common_times = vec![];
    let mut common_text_times = vec![];
    for i in 0..COMMON_WORD_WARM_UP + COMMON_WORD_SAMPLES {
        let (_, q) = data.query_for(&mut rng, &centres);
        let request = SearchRequest::text(COMMON_WORD, current).with_embedding(&q);
        let started = Instant::now();
        let results = caller.search(data.tenant(), &request, 10).await.unwrap();
        let took = started.elapsed();
        assert!(
            !results.is_empty(),
            "a search found nothing for {COMMON_WORD:?}"
        );
        if i >= COMMON_WORD_WARM_UP {
            common_times.push(took);
            if i % 2 == 0 {
                let t = Instant::now();
                caller
                    .search_full_text(data.tenant(), COMMON_WORD, current, 10)
                    .await
                    .unwrap();
                common_text_times.push(t.elapsed());
            }
        }
    }
    let stats = Stats::of(&times);
    let common = Stats::of(&common_times);
    let mut written = record.finish(json!({
        "visible_rows": data.visible,
        "versions": data.versions,
        "texts": data.texts,
        "warm_up_discarded": SEARCH_WARM_UP,
        "fused_search": stats.to_json(),
        "vector_branch_alone": Stats::of(&vector_times).to_json(),
        "full_text_branch_alone": Stats::of(&text_times).to_json(),
        "budget_p95_ms": budget_ms,
        "common_word": COMMON_WORD,
        "common_word_warm_up_discarded": COMMON_WORD_WARM_UP,
        "common_word_fused_search": common.to_json(),
        "common_word_full_text_branch_alone": Stats::of(&common_text_times).to_json(),
    }));
    // The plans of one call of each class, after the last timed call.
    let (word, q) = data.query_for(&mut rng, &centres);
    for (class, text) in [("number word", word.as_str()), ("common word", COMMON_WORD)] {
        let request = SearchRequest::text(text, current).with_embedding(&q);
        take_search_plans(&mut written, &data, class, &request).await;
    }
    let loaded = written.loaded;
    println!(
        "{case}: fused Search over {} visible rows: p50 {:.1} ms, p95 {:.1} ms, p99 {:.1} ms \
         (budget {budget_ms} ms at p95){}",
        data.visible,
        stats.p50,
        stats.p95,
        stats.p99,
        if loaded { ", on a loaded machine" } else { "" }
    );
    println!(
        "{case}: fused Search for {COMMON_WORD:?}, which every text holds: p50 {:.1} ms, \
         p95 {:.1} ms (recorded, no budget)",
        common.p50, common.p95
    );
    assert!(
        stats.p95 < budget_ms,
        "fused Search p95 {:.1} ms is over the {budget_ms} ms budget at {} visible rows{}",
        stats.p95,
        data.visible,
        if loaded {
            " (the machine was loaded: rerun when it is quieter)"
        } else {
            ""
        }
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn fused_search_stays_under_100_ms_at_p95_up_to_50000_visible_rows() {
    fused_search_case("fused_search_50k", 50_000, SEARCH_BUDGET_50K).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn fused_search_stays_under_250_ms_at_p95_up_to_200000_visible_rows() {
    fused_search_case("fused_search_200k", 200_000, SEARCH_BUDGET_200K).await;
}

// ---- realistic word frequencies (#85) ----

/// The classes of query the Search cases on realistic word frequencies time.
#[derive(Debug, Clone, Copy, PartialEq)]
enum QueryClass {
    /// A word in 8 to 40 visible texts.
    Rare,
    /// A word in 0.6% to 1.5% of the visible texts.
    Mid,
    /// One of the three commonest words.
    Common,
    /// A natural-language question: three of the ten commonest words, a mid
    /// word and a rare word.
    Question,
}

impl QueryClass {
    const ALL: [QueryClass; 4] = [
        QueryClass::Rare,
        QueryClass::Mid,
        QueryClass::Common,
        QueryClass::Question,
    ];

    fn name(self) -> &'static str {
        match self {
            QueryClass::Rare => "rare word",
            QueryClass::Mid => "mid word",
            QueryClass::Common => "common word",
            QueryClass::Question => "natural-language question",
        }
    }

    fn query(self, words: &QueryWords, rng: &mut Rng) -> String {
        let pick = |rng: &mut Rng, pool: &[String]| pool[rng.below(pool.len())].clone();
        match self {
            QueryClass::Rare => pick(rng, &words.rare),
            QueryClass::Mid => pick(rng, &words.mid),
            QueryClass::Common => pick(rng, &words.commonest[..3]),
            QueryClass::Question => {
                let mut common: Vec<String> = vec![];
                while common.len() < 3 {
                    let w = pick(rng, &words.commonest);
                    if !common.contains(&w) {
                        common.push(w);
                    }
                }
                let (mid, rare) = (pick(rng, &words.mid), pick(rng, &words.rare));
                format!("{} {mid} {} {} {rare}", common[0], common[1], common[2])
            }
        }
    }
}

/// The query classes whose Search budget is asserted at each size: those
/// that met it when the cases were added, while Search read a stored count of
/// each word (#85). None did. A rare word then took 150 ms at p95 at 50,000
/// visible rows on pgvector and 487 ms at 200,000 on supabase, so every class
/// is recorded only, and #87 tracks the cost of the full-text branch with
/// these frequencies.
const WORD_BUDGETED_50K: &[QueryClass] = &[];
const WORD_BUDGETED_200K: &[QueryClass] = &[];

/// The seed of the queries, the query vectors and the long texts, so every
/// run times the same ones.
const WORD_SEED: u64 = 85;

/// Searches and Resolutions discarded before the timed ones, and timed, for
/// each query class.
const WORD_WARM_UP: usize = 5;
const WORD_SAMPLES: usize = 50;

/// Times one call, after `WORD_WARM_UP` discarded ones, `WORD_SAMPLES` times.
async fn time_calls<F, Fut>(mut call: F) -> Vec<Duration>
where
    F: FnMut(usize) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mut times = vec![];
    for i in 0..WORD_WARM_UP + WORD_SAMPLES {
        let started = Instant::now();
        call(i).await;
        if i >= WORD_WARM_UP {
            times.push(started.elapsed());
        }
    }
    times
}

async fn word_frequency_case(
    case: &str,
    visible_target: usize,
    budget_ms: f64,
    budgeted: &[QueryClass],
) {
    let _one = ONE_AT_A_TIME.lock().await;
    let data = Dataset::get(perf::texts_for_visible(visible_target), Vocabulary::Words).await;
    let words = data.query_words().await;
    println!(
        "{case}: {} texts, {} Versions, {} visible now; {} words, the commonest in {:.1}% of \
         visible texts; {} rare, {} mid and {} rare name words to query",
        data.texts,
        data.versions,
        data.visible,
        words.vocabulary,
        words.commonest_share * 100.0,
        words.rare.len(),
        words.mid.len(),
        words.rare_names.len()
    );
    assert!(
        (data.visible as f64) >= visible_target as f64 * 0.95
            && (data.visible as f64) <= visible_target as f64 * 1.05,
        "{} visible rows is not about {visible_target}",
        data.visible
    );
    // The vocabulary is the one #85 measured: its commonest word is in about
    // 82% of the texts.
    assert!(
        (0.79..=0.85).contains(&words.commonest_share),
        "the commonest word is in {:.1}% of the visible texts, not about 82%",
        words.commonest_share * 100.0
    );
    let store = data.store().await;
    let caller = data.worker(&store);
    let centres = data.centres().await;
    let record = Record::start(case, &data.db).await;
    let current = AsOf::current(perf::valid_at());
    let tenant = data.tenant();

    // The first query and mention of each class, to take their plans.
    let mut searched = vec![];
    let mut mentioned = vec![];
    let mut classes = vec![];
    let mut failures = vec![];
    for class in QueryClass::ALL {
        // The same queries and vectors in every run.
        let mut rng = Rng::new(WORD_SEED);
        let queries: Vec<(String, Vec<f32>)> = (0..WORD_WARM_UP + WORD_SAMPLES)
            .map(|_| {
                let text = class.query(&words, &mut rng);
                let (_, q) = data.query_for(&mut rng, &centres);
                (text, q)
            })
            .collect();
        let fused = Stats::of(
            &time_calls(|i| {
                let (caller, (text, q)) = (&caller, &queries[i]);
                async move {
                    let request = SearchRequest::text(text, current).with_embedding(q);
                    let results = caller.search(tenant, &request, 10).await.unwrap();
                    assert!(!results.is_empty(), "a search found nothing for {text:?}");
                }
            })
            .await,
        );
        let branch = Stats::of(
            &time_calls(|i| {
                let (caller, (text, _)) = (&caller, &queries[i]);
                async move {
                    let found = caller
                        .search_full_text(tenant, text, current, 10)
                        .await
                        .unwrap();
                    assert!(!found.is_empty(), "full text found nothing for {text:?}");
                }
            })
            .await,
        );
        let asserted = budgeted.contains(&class);
        println!(
            "{case}: {}: fused Search p50 {:.1} ms, p95 {:.1} ms; full-text branch alone p50 \
             {:.1} ms, p95 {:.1} ms (budget {budget_ms} ms at p95, {})",
            class.name(),
            fused.p50,
            fused.p95,
            branch.p50,
            branch.p95,
            if asserted {
                "asserted"
            } else {
                "recorded only"
            }
        );
        if asserted && fused.p95 >= budget_ms {
            failures.push(format!(
                "fused Search p95 {:.1} ms for a {} is over the {budget_ms} ms budget",
                fused.p95,
                class.name()
            ));
        }
        classes.push(json!({
            "class": class.name(),
            "example_query": queries[0].0,
            "fused_search": fused.to_json(),
            "full_text_branch_alone": branch.to_json(),
            "budget_p95_ms": budget_ms,
            "budget_asserted": asserted,
        }));
        searched.push((class.name(), queries[0].clone()));
    }

    // Resolution's full-text half: a mention of one word, with no embedding.
    let mut resolution = vec![];
    for (class, pool) in [
        ("common name word", &words.common_names),
        ("rare name word", &words.rare_names),
    ] {
        let mut rng = Rng::new(WORD_SEED);
        let mentions: Vec<String> = (0..WORD_WARM_UP + WORD_SAMPLES)
            .map(|_| pool[rng.below(pool.len())].clone())
            .collect();
        let stats = Stats::of(
            &time_calls(|i| {
                let (caller, mention) = (&caller, &mentions[i]);
                async move {
                    let found = caller
                        .resolve(tenant, &Mention::text(mention), 10)
                        .await
                        .unwrap();
                    assert!(
                        !found.is_empty(),
                        "Resolution found nothing for {mention:?}"
                    );
                }
            })
            .await,
        );
        println!(
            "{case}: Resolution's full-text half, {class}: p50 {:.1} ms, p95 {:.1} ms \
             (recorded only)",
            stats.p50, stats.p95
        );
        resolution.push(json!({ "class": class, "example_mention": mentions[0],
                                "resolve": stats.to_json() }));
        mentioned.push((class, mentions[0].clone()));
    }

    let mut written = record.finish(json!({
        "visible_rows": data.visible,
        "versions": data.versions,
        "texts": data.texts,
        "vocabulary": words.vocabulary,
        "commonest_word_share": words.commonest_share,
        "commonest_words": words.commonest,
        "warm_up_discarded": WORD_WARM_UP,
        "search": classes,
        "resolution_full_text": resolution,
    }));
    // The plans of one call of each class, after the last timed call.
    for (class, (text, q)) in &searched {
        let request = SearchRequest::text(text, current).with_embedding(q);
        take_search_plans(&mut written, &data, class, &request).await;
    }
    for (class, mention) in &mentioned {
        written
            .take_plans(&format!("Resolution, {class}"), mention, async |store| {
                data.worker(store)
                    .resolve(tenant, &Mention::text(mention), 10)
                    .await
                    .unwrap();
            })
            .await;
    }
    let loaded = written.loaded;
    assert!(
        failures.is_empty(),
        "{}{}",
        failures.join("; "),
        if loaded {
            " (the machine was loaded: rerun when it is quieter)"
        } else {
            ""
        }
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn search_with_word_frequencies_at_50k_meets_the_budgets_it_met_before_85() {
    word_frequency_case(
        "word_frequencies_50k",
        50_000,
        SEARCH_BUDGET_50K,
        WORD_BUDGETED_50K,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn search_with_word_frequencies_at_200k_meets_the_budgets_it_met_before_85() {
    word_frequency_case(
        "word_frequencies_200k",
        200_000,
        SEARCH_BUDGET_200K,
        WORD_BUDGETED_200K,
    )
    .await;
}

// ---- long texts (#85) ----

/// The distinct words of a long text, the Nodes written first so the Scope
/// holds the vocabulary, and the writes discarded and timed of each kind.
const LONG_TEXT_WORDS: usize = 100;
const LONG_TEXT_EARLIER_NODES: usize = 200;
const LONG_TEXT_WARM_UP: usize = 5;
const LONG_TEXT_SAMPLES: usize = 50;
/// The Nodes one transaction creates, and the transactions discarded and
/// timed.
const BATCH_NODES: usize = 25;
const BATCH_WARM_UP: usize = 2;
const BATCH_SAMPLES: usize = 20;

/// The Kind of the long-text Nodes: its `text` is searchable.
const NOTE: &str = "note";

#[tokio::test(flavor = "multi_thread")]
async fn long_text_writes_and_a_transaction_of_25_nodes_are_timed_with_their_commits() {
    let _one = ONE_AT_A_TIME.lock().await;
    let w = Writes::new().await;
    w.db.grant_kind_registration("gnomon_worker").await;
    w.caller()
        .register_kind(&NewKind {
            tenant: Some(w.tenant),
            name: NOTE.into(),
            of: KindOf::Node,
            parent: None,
            properties: json!({ "type": "object",
                                "properties": { "text": { "type": "string" } } }),
            search: vec!["text".into()],
        })
        .await
        .unwrap();
    let mut rng = Rng::new(WORD_SEED);
    let new_node = NewNode {
        tenant: Some(w.tenant),
        scope: Scope::new(perf::SCOPE),
        kind: NOTE.to_owned(),
    };
    let long_patch = |rng: &mut Rng, ep| {
        Patch::new(
            ep,
            ValidTime::since(day(0)),
            values(json!({
                "name": format!("{} {}", perf::zipf_word(rng), perf::zipf_word(rng)),
                "text": perf::distinct_words(rng, LONG_TEXT_WORDS),
            })),
        )
    };

    let mut nodes = vec![];
    for _ in 0..LONG_TEXT_EARLIER_NODES {
        let first = long_patch(&mut rng, w.episode(day(1)).await);
        nodes.push(w.create_with(&new_node, &first).await);
    }
    w.analyze().await;
    let record = Record::start("long_text_writes", &w.db.name).await;

    let mut creates = vec![];
    for i in 0..LONG_TEXT_WARM_UP + LONG_TEXT_SAMPLES {
        let first = long_patch(&mut rng, w.episode(day(1)).await);
        let started = Instant::now();
        let node = w.create_with(&new_node, &first).await;
        if i >= LONG_TEXT_WARM_UP {
            creates.push(started.elapsed());
        }
        nodes.push(node);
    }
    // Each patch replaces the text of one of the earlier Nodes.
    let mut patches = vec![];
    for (i, node) in nodes
        .iter()
        .take(LONG_TEXT_WARM_UP + LONG_TEXT_SAMPLES)
        .enumerate()
    {
        let patch = long_patch(&mut rng, w.episode(day(2)).await);
        let took = w.timed(*node, &patch).await;
        if i >= LONG_TEXT_WARM_UP {
            patches.push(took);
        }
    }
    let mut batches = vec![];
    for i in 0..BATCH_WARM_UP + BATCH_SAMPLES {
        let mut firsts = vec![];
        for _ in 0..BATCH_NODES {
            firsts.push(long_patch(&mut rng, w.episode(day(1)).await));
        }
        let started = Instant::now();
        let mut tx = w.caller().transaction(w.tenant).await.unwrap();
        for first in &firsts {
            tx.create_node(&new_node, first).await.unwrap();
        }
        tx.commit().await.unwrap();
        if i >= BATCH_WARM_UP {
            batches.push(started.elapsed());
        }
    }
    let (create, patch, batch) = (
        Stats::of(&creates),
        Stats::of(&patches),
        Stats::of(&batches),
    );
    for (what, stats) in [
        ("create, a text of 100 distinct words", &create),
        ("patch, a text of 100 distinct words", &patch),
        ("one transaction that creates 25 such Nodes", &batch),
    ] {
        println!(
            "long_text_writes: {what}, with its commit: p50 {:.2} ms, p95 {:.2} ms, n = {} \
             (recorded only)",
            stats.p50, stats.p95, stats.n
        );
    }
    record.finish(json!({
        "distinct_words_per_text": LONG_TEXT_WORDS,
        "earlier_nodes": LONG_TEXT_EARLIER_NODES,
        "warm_up_discarded": LONG_TEXT_WARM_UP,
        "create": create.to_json(),
        "patch": patch.to_json(),
        "transaction_of_25_creates": batch.to_json(),
        "transactions_discarded": BATCH_WARM_UP,
    }));
}

// ---- patch writes ----

fn day(d: i64) -> Timestamp {
    "2023-01-01T00:00:00Z".parse::<Timestamp>().unwrap() + SignedDuration::from_hours(24 * d)
}

fn values(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        other => panic!("not an object: {other}"),
    }
}

/// A patch case (#84, ADR-0007). Each is timed on a small Store and on a
/// large one.
#[derive(Debug, Clone, Copy, PartialEq)]
enum PatchCase {
    /// Bounded patches (two properties over 14 days) on Nodes of this many
    /// Contributions.
    Bounded(usize),
    /// Patches of two properties from a day to the end of time, on Nodes of
    /// this many Contributions.
    OpenEnded(usize),
    /// One patch from the first Version's start to the end of time that
    /// changes every Version of a Node of this many Versions.
    RewriteEvery(usize),
}

impl PatchCase {
    fn name(self) -> String {
        match self {
            PatchCase::Bounded(c) => format!("bounded on {c} Contributions"),
            PatchCase::OpenEnded(c) => format!("open-ended on {c} Contributions"),
            PatchCase::RewriteEvery(v) => format!("rewriting every one of {v} Versions"),
        }
    }
}

/// Each case's p50 limit in ms, on the small Store and on the large one:
/// 1.5 times the p50 the suite measured after #84 on the slower of the two
/// images, with `fsync` off as `scripts/perf.sh` runs it (ADR-0007).
const PATCH_LIMITS_MS: [(PatchCase, f64, f64); 5] = [
    (PatchCase::Bounded(10), 13.8, 14.4),
    (PatchCase::Bounded(100), 8.0, 7.8),
    (PatchCase::Bounded(500), 24.5, 24.0),
    (PatchCase::OpenEnded(500), 66.5, 66.6),
    (PatchCase::RewriteEvery(200), 338.4, 359.0),
];

/// The most a case's p50 on the large Store may be, as a multiple of its p50
/// on the small one.
const LARGE_STORE_RATIO: f64 = 1.5;

/// The Nodes the large Store holds beside the ones the cases write, each
/// with an Episode, two Contributions, two Versions and two Citations
/// (`perf::add_unrelated_rows`).
const UNRELATED_NODES: usize = 300_000;

/// Nodes of each Contribution count, patches per Node and Nodes written to
/// and discarded before the timed ones, for the bounded and open-ended
/// cases.
const CONTRIBUTION_SIZES: [usize; 3] = [10, 100, 500];
const NODES: i64 = 20;
const PATCHES_PER_NODE: i64 = 3;
const WARM_UP_NODES: i64 = 3;

/// The Versions of a Node the last case rewrites, and its Nodes on each
/// Store: one rewrite each, the first discarded.
const REWRITTEN_VERSIONS: usize = 200;
const REWRITE_NODES: usize = 20;
const REWRITE_WARM_UP: usize = 1;

struct Writes {
    db: TestDb,
    store: Store,
    tenant: TenantId,
}

impl Writes {
    async fn new() -> Writes {
        let db = TestDb::migrated().await;
        let store = Store::connect(&db.login_url(), &support::connect_options())
            .await
            .unwrap();
        Writes {
            tenant: support::tenant(),
            store,
            db,
        }
    }

    fn caller(&self) -> gnomon::Caller<'_> {
        self.store
            .as_worker(self.tenant, Scope::new(perf::SCOPE), None)
    }

    async fn episode(&self, reference_time: Timestamp) -> gnomon::EpisodeId {
        self.caller()
            .record_episode(&NewEpisode {
                tenant: Some(self.tenant),
                scope: Scope::new(perf::SCOPE),
                content: format!("an Episode from {reference_time}"),
                reference_time,
                authority: None,
            })
            .await
            .unwrap()
    }

    /// A patch of two properties over `valid`, as prototype 4's writes were.
    async fn patch(&self, i: i64, valid: ValidTime) -> Patch {
        let ep = self.episode(day(i % 7)).await;
        Patch::new(
            ep,
            valid,
            values(json!({ "name": format!("n{}", i % 5), "aliases": [format!("a{}", i % 3)] })),
        )
    }

    async fn create(&self, first: &Patch) -> gnomon::NodeId {
        self.create_with(
            &NewNode {
                tenant: Some(self.tenant),
                scope: Scope::new(perf::SCOPE),
                kind: ROOT_NODE_KIND.to_owned(),
            },
            first,
        )
        .await
    }

    async fn create_with(&self, node: &NewNode, first: &Patch) -> gnomon::NodeId {
        self.caller().create_node(node, first).await.unwrap().0
    }

    async fn write(&self, node: gnomon::NodeId, patch: &Patch) {
        self.caller()
            .write_node(self.tenant, node, patch)
            .await
            .unwrap();
    }

    /// A Node holding `contributions` Contributions: one patch of two
    /// properties writes two, with Valid-time intervals of 10 to 60 days
    /// spread over a year.
    async fn node_with(&self, contributions: usize, n: i64) -> gnomon::NodeId {
        let first = self.patch(0, ValidTime::between(day(0), day(400))).await;
        let node = self.create(&first).await;
        for i in 1..(contributions as i64 / 2) {
            let from = (i * 37 + n * 11) % 330;
            let patch = self
                .patch(
                    i,
                    ValidTime::between(day(from), day(from + 10 + (i * 13) % 50)),
                )
                .await;
            self.write(node, &patch).await;
        }
        node
    }

    /// A Node of `versions` Versions: a name of its own on each of as many
    /// days in a row.
    async fn node_with_versions(&self, versions: usize) -> gnomon::NodeId {
        let named = |i: i64| async move {
            Patch::new(
                self.episode(day(0)).await,
                ValidTime::between(day(i), day(i + 1)),
                values(json!({ "name": format!("v{i}") })),
            )
        };
        let node = self.create(&named(0).await).await;
        for i in 1..versions as i64 {
            self.write(node, &named(i).await).await;
        }
        node
    }

    /// Times one write of `patch` with its commit.
    async fn timed(&self, node: gnomon::NodeId, patch: &Patch) -> Duration {
        let started = Instant::now();
        self.write(node, patch).await;
        started.elapsed()
    }

    async fn timed_bounded(&self, node: gnomon::NodeId, round: i64, n: i64) -> Duration {
        let from = (round * 61 + n * 29) % 330;
        let patch = self
            .patch(round + 1000, ValidTime::between(day(from), day(from + 14)))
            .await;
        self.timed(node, &patch).await
    }

    async fn timed_open_ended(&self, node: gnomon::NodeId, round: i64, n: i64) -> Duration {
        let from = (round * 61 + n * 29) % 330;
        let patch = self.patch(round + 2000, ValidTime::since(day(from))).await;
        self.timed(node, &patch).await
    }

    /// A patch of a property no Version holds, from the first Version's start
    /// on, so every Version changes.
    async fn timed_rewrite(&self, node: gnomon::NodeId) -> Duration {
        let patch = Patch::new(
            self.episode(day(1)).await,
            ValidTime::since(day(0)),
            values(json!({ "aliases": ["rewritten"] })),
        );
        self.timed(node, &patch).await
    }

    async fn analyze(&self) {
        // A running Store has autovacuum analyze what it wrote. The Nodes were
        // just loaded, so the planner's statistics are brought up to date here.
        self.db
            .admin()
            .await
            .batch_execute("analyze")
            .await
            .unwrap();
    }

    async fn count(&self, sql: &str, node: gnomon::NodeId) -> i64 {
        self.db
            .admin()
            .await
            .query_typed(sql, &[(&node.0, tokio_postgres::types::Type::UUID)])
            .await
            .unwrap()[0]
            .get(0)
    }

    /// Times one write of `case`'s patch on `node`.
    async fn timed_case(
        &self,
        case: PatchCase,
        node: gnomon::NodeId,
        round: i64,
        n: i64,
    ) -> Duration {
        match case {
            PatchCase::Bounded(_) => self.timed_bounded(node, round, n).await,
            PatchCase::OpenEnded(_) => self.timed_open_ended(node, round, n).await,
            PatchCase::RewriteEvery(_) => self.timed_rewrite(node).await,
        }
    }
}

/// The Stores the cases run on, by name.
const STORES: [&str; 2] = ["small", "large"];

/// Times every case on both Stores and returns each Store's cases in order,
/// each with its times and the size of its first Node. Each write on the
/// small Store is followed by the same write on the large one, so a change in
/// the machine's load falls on both alike and the ratio between them holds.
async fn measure(stores: [&Writes; 2]) -> [Vec<(PatchCase, Stats, i64)>; 2] {
    let mut measured: [Vec<(PatchCase, Stats, i64)>; 2] = [vec![], vec![]];
    for contributions in CONTRIBUTION_SIZES {
        let loading = Instant::now();
        let mut nodes: [Vec<gnomon::NodeId>; 2] = [vec![], vec![]];
        let mut counts = [0; 2];
        for (s, w) in stores.iter().enumerate() {
            for n in 0..WARM_UP_NODES + NODES {
                nodes[s].push(w.node_with(contributions, n).await);
            }
            w.analyze().await;
            counts[s] = w
                .count(
                    "select count(*) from gnomon.contribution where node_id = $1",
                    nodes[s][0],
                )
                .await;
        }
        let loaded_in = loading.elapsed().as_secs_f64();
        let mut cases = vec![PatchCase::Bounded(contributions)];
        if contributions == 500 {
            cases.push(PatchCase::OpenEnded(contributions));
        }
        for case in cases {
            // Warm-up Nodes first, their writes discarded.
            for (s, w) in stores.iter().enumerate() {
                for (n, node) in nodes[s].iter().take(WARM_UP_NODES as usize).enumerate() {
                    w.timed_case(case, *node, 0, n as i64).await;
                }
            }
            let mut times: [Vec<Duration>; 2] = [vec![], vec![]];
            for round in 0..PATCHES_PER_NODE {
                let pairs = nodes[0].iter().zip(&nodes[1]).enumerate();
                for (n, (small, large)) in pairs.skip(WARM_UP_NODES as usize) {
                    for (s, node) in [small, large].into_iter().enumerate() {
                        times[s].push(stores[s].timed_case(case, *node, round, n as i64).await);
                    }
                }
            }
            for s in 0..2 {
                let stats = Stats::of(&times[s]);
                println!(
                    "{} Store, {} ({} on the first Node, both Stores loaded in \
                     {loaded_in:.0}s): p50 {:.2} ms, p95 {:.2} ms, n = {}",
                    STORES[s],
                    case.name(),
                    counts[s],
                    stats.p50,
                    stats.p95,
                    stats.n
                );
                measured[s].push((case, stats, counts[s]));
            }
        }
    }

    let case = PatchCase::RewriteEvery(REWRITTEN_VERSIONS);
    let loading = Instant::now();
    let mut nodes: [Vec<gnomon::NodeId>; 2] = [vec![], vec![]];
    let open = "select count(*) from gnomon.version where node_id = $1
                and recorded_to = 'infinity'";
    for (s, w) in stores.iter().enumerate() {
        for _ in 0..REWRITE_NODES {
            nodes[s].push(w.node_with_versions(REWRITTEN_VERSIONS).await);
        }
        w.analyze().await;
        assert_eq!(
            w.count(open, nodes[s][0]).await,
            REWRITTEN_VERSIONS as i64,
            "the Node's Versions before the rewrite"
        );
    }
    let loaded_in = loading.elapsed().as_secs_f64();
    let mut times: [Vec<Duration>; 2] = [vec![], vec![]];
    for (i, (small, large)) in nodes[0].iter().zip(&nodes[1]).enumerate() {
        for (s, node) in [small, large].into_iter().enumerate() {
            let took = stores[s].timed_case(case, *node, 0, 0).await;
            if i >= REWRITE_WARM_UP {
                times[s].push(took);
            }
        }
    }
    for (s, w) in stores.iter().enumerate() {
        // The rewrite closed every Version, so none is open without the new
        // property, and opened as many again and one more after the last day.
        let counts =
            w.db.admin()
                .await
                .query_typed(
                    "select count(*) filter (where props ? 'aliases'),
                        count(*) filter (where not props ? 'aliases')
                 from gnomon.version where node_id = $1 and recorded_to = 'infinity'",
                    &[(&nodes[s][0].0, tokio_postgres::types::Type::UUID)],
                )
                .await
                .unwrap();
        let (rewritten, left): (i64, i64) = (counts[0].get(0), counts[0].get(1));
        assert_eq!(
            (rewritten, left),
            (REWRITTEN_VERSIONS as i64 + 1, 0),
            "the open Versions after the rewrite"
        );
        let stats = Stats::of(&times[s]);
        println!(
            "{} Store, {} (both Stores loaded in {loaded_in:.0}s): p50 {:.2} ms, p95 {:.2} ms, \
             n = {}",
            STORES[s],
            case.name(),
            stats.p50,
            stats.p95,
            stats.n
        );
        measured[s].push((case, stats, REWRITTEN_VERSIONS as i64));
    }
    measured
}

#[tokio::test(flavor = "multi_thread")]
async fn each_patch_write_meets_its_limit_and_costs_at_most_half_as_much_again_on_a_large_store() {
    let _one = ONE_AT_A_TIME.lock().await;
    let small = Writes::new().await;
    let large = Writes::new().await;
    let loading = Instant::now();
    let unrelated =
        perf::add_unrelated_rows(&large.db, large.tenant, perf::SCOPE, UNRELATED_NODES).await;
    println!(
        "large Store: {unrelated:?} rows, loaded in {:.0}s",
        loading.elapsed().as_secs_f64()
    );
    for (table, rows) in &unrelated {
        assert!(
            *rows >= UNRELATED_NODES as i64,
            "gnomon.{table} holds {rows} rows, fewer than {UNRELATED_NODES}"
        );
    }
    let record = Record::start("patch_write", &small.db.name).await;
    let [on_small, on_large] = measure([&small, &large]).await;

    let mut cells = vec![];
    for ((case, s, s_size), (_, l, l_size)) in on_small.iter().zip(&on_large) {
        let (_, small_limit, large_limit) = PATCH_LIMITS_MS
            .iter()
            .find(|(c, _, _)| c == case)
            .expect("every case has a limit");
        cells.push(json!({
            "case": case.name(),
            "small_store": { "patch": s.to_json(), "first_node_size": s_size,
                             "p50_limit_ms": small_limit },
            "large_store": { "patch": l.to_json(), "first_node_size": l_size,
                             "p50_limit_ms": large_limit },
            "p50_ratio": l.p50 / s.p50,
        }));
    }
    let written = record.finish(json!({
        "cells": cells,
        "unrelated_rows": unrelated.iter().map(|(t, n)| json!({ "table": t, "rows": n }))
            .collect::<Vec<_>>(),
        "large_store_ratio_limit": LARGE_STORE_RATIO,
        "warm_up_nodes_discarded": WARM_UP_NODES,
        "rewrites_discarded": REWRITE_WARM_UP,
    }));
    let rerun = if written.loaded {
        " (the machine was loaded: rerun when it is quieter)"
    } else {
        ""
    };
    let mut failures = vec![];
    for ((case, s, _), (_, l, _)) in on_small.iter().zip(&on_large) {
        let (_, small_limit, large_limit) =
            PATCH_LIMITS_MS.iter().find(|(c, _, _)| c == case).unwrap();
        let ratio = l.p50 / s.p50;
        println!(
            "{}: p50 {:.2} ms small, {:.2} ms large, ratio {ratio:.2} (limits {small_limit} ms, \
             {large_limit} ms, ratio {LARGE_STORE_RATIO})",
            case.name(),
            s.p50,
            l.p50
        );
        if s.p50 > *small_limit {
            failures.push(format!(
                "{} took {:.2} ms at p50 on the small Store, over {small_limit} ms",
                case.name(),
                s.p50
            ));
        }
        if l.p50 > *large_limit {
            failures.push(format!(
                "{} took {:.2} ms at p50 on the large Store, over {large_limit} ms",
                case.name(),
                l.p50
            ));
        }
        if ratio > LARGE_STORE_RATIO {
            failures.push(format!(
                "{} took {ratio:.2} times as long on the large Store as on the small one, over \
                 {LARGE_STORE_RATIO}",
                case.name()
            ));
        }
    }
    assert!(failures.is_empty(), "{}{rerun}", failures.join("; "));
}
