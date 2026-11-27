//! Seam 1, bound precision, Refinement and the `uncertain` marker: writing a
//! Valid-time bound at a precision, reading it back, and stating a bound that
//! an earlier Version left unknown or coarse, through the Rust core's Store
//! API against real Postgres (ADR-0004, ADR-0007, ADR-0008). The adversarial
//! cases live in sql_boundary.rs.

mod support;

use gnomon::{
    AsOf, Bound, BoundChange, BoundSide, BoundStatement, EpisodeId, NewEpisode, NewNode, NodeId,
    NodeVersion, Patch, Precision, ROOT_NODE_KIND, Scope, Store, TenantId, ValidTime,
};
use jiff::{SignedDuration, Timestamp, civil::date, tz::TimeZone};
use serde_json::{Map, Value, json};
use support::TestDb;

const SCOPE: &str = "support";

fn at(y: i16, m: i8, d: i8, h: i8) -> Timestamp {
    date(y, m, d)
        .at(h, 0, 0, 0)
        .to_zoned(TimeZone::UTC)
        .unwrap()
        .timestamp()
}

fn values(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        other => panic!("not an object: {other}"),
    }
}

struct World {
    _db: TestDb,
    store: Store,
    tenant: TenantId,
}

impl World {
    async fn new() -> World {
        let db = TestDb::migrated().await;
        let store = Store::connect(&db.login_url(), &support::connect_options())
            .await
            .unwrap();
        World {
            _db: db,
            store,
            tenant: support::tenant(),
        }
    }

    fn caller(&self) -> gnomon::Caller<'_> {
        self.store.as_worker(self.tenant, Scope::new(SCOPE), None)
    }

    async fn episode(&self, reference_time: Timestamp) -> EpisodeId {
        self.caller()
            .record_episode(&NewEpisode {
                tenant: Some(self.tenant),
                scope: Scope::new(SCOPE),
                content: format!("an Episode from {reference_time}"),
                reference_time,
                authority: None,
            })
            .await
            .unwrap()
    }

    /// A Node named `name` over `valid_time`, written by a fresh Episode.
    async fn node(&self, name: &str, valid_time: ValidTime) -> (NodeId, gnomon::WriteOutcome) {
        let ep = self.episode(at(2024, 1, 1, 0)).await;
        self.caller()
            .create_node(
                &NewNode {
                    tenant: Some(self.tenant),
                    scope: Scope::new(SCOPE),
                    kind: ROOT_NODE_KIND.to_owned(),
                },
                &Patch::new(ep, valid_time, values(json!({ "name": name }))),
            )
            .await
            .unwrap()
    }

    async fn read(&self, node: NodeId, as_of: AsOf) -> Option<NodeVersion> {
        self.caller()
            .read_node(self.tenant, node, as_of)
            .await
            .unwrap()
    }

    /// States `bound` for one side of `version`, citing a new Episode.
    async fn state(
        &self,
        node: NodeId,
        version: gnomon::VersionId,
        side: BoundSide,
        bound: Bound,
    ) -> gnomon::BoundOutcome {
        let episode = self.episode(at(2024, 6, 1, 0)).await;
        self.caller()
            .state_bound(
                self.tenant,
                node,
                &BoundStatement {
                    episode,
                    version,
                    side,
                    bound,
                },
            )
            .await
            .unwrap()
    }
}

fn period(precision: Precision, when: Timestamp) -> Bound {
    Bound::Period {
        at: when,
        precision,
    }
}

#[tokio::test]
async fn a_bound_stated_at_a_precision_is_stored_as_the_start_of_its_period_in_utc() {
    let w = World::new().await;
    let cases = [
        (Precision::Year, at(2023, 8, 17, 13), at(2023, 1, 1, 0)),
        (Precision::Month, at(2023, 8, 17, 13), at(2023, 8, 1, 0)),
        (Precision::Day, at(2023, 8, 17, 13), at(2023, 8, 17, 0)),
        (Precision::Instant, at(2023, 8, 17, 13), at(2023, 8, 17, 13)),
    ];
    for (precision, stated, start) in cases {
        let (node, _) = w
            .node("Acme", ValidTime::since_bound(period(precision, stated)))
            .await;
        let read = w
            .read(node, AsOf::current(at(2030, 1, 1, 0)))
            .await
            .expect("the Version holds in 2030");
        let expected = match precision {
            Precision::Instant => Bound::At(start),
            _ => period(precision, start),
        };
        assert_eq!(read.valid_time.from, expected, "{precision:?}");
        assert_eq!(read.valid_time.to, Bound::Unknown, "{precision:?}");
    }
}

#[tokio::test]
async fn an_unknown_start_reads_as_open_and_returns_its_precision() {
    let w = World::new().await;
    let (node, _) = w.node("Acme", ValidTime::UNKNOWN).await;
    for when in [at(1066, 10, 14, 0), at(2024, 6, 1, 0), at(2999, 1, 1, 0)] {
        let read = w
            .read(node, AsOf::current(when))
            .await
            .unwrap_or_else(|| panic!("an unknown start reads as open at {when}"));
        assert_eq!(read.valid_time, ValidTime::UNKNOWN);
        assert!(!read.uncertain, "an open bound is not imprecise");
    }
}

#[tokio::test]
async fn a_read_inside_an_imprecise_bounds_period_is_uncertain_and_a_read_outside_it_is_not() {
    let w = World::new().await;
    // "In March 2024" through "in 2026".
    let (node, _) = w
        .node(
            "Acme",
            ValidTime {
                from: period(Precision::Month, at(2024, 3, 17, 0)),
                to: period(Precision::Year, at(2026, 5, 5, 0)),
            },
        )
        .await;

    let uncertain = |v: Option<NodeVersion>| v.expect("the Version holds").uncertain;
    // Inside March 2024: the start is not pinned down.
    assert!(uncertain(
        w.read(node, AsOf::current(at(2024, 3, 1, 0))).await
    ));
    assert!(uncertain(
        w.read(node, AsOf::current(at(2024, 3, 31, 23))).await
    ));
    // From April 2024 the Version certainly holds.
    assert!(!uncertain(
        w.read(node, AsOf::current(at(2024, 4, 1, 0))).await
    ));
    assert!(!uncertain(
        w.read(node, AsOf::current(at(2025, 6, 1, 0))).await
    ));
    // Before the period there is no Version at all, certain or not.
    assert_eq!(w.read(node, AsOf::current(at(2024, 2, 28, 23))).await, None);
}

#[tokio::test]
async fn a_bound_at_the_precision_of_an_instant_is_never_uncertain() {
    let w = World::new().await;
    let (node, _) = w
        .node(
            "Acme",
            ValidTime::between(at(2024, 3, 1, 0), at(2024, 4, 1, 0)),
        )
        .await;
    for when in [at(2024, 3, 1, 0), at(2024, 3, 15, 0), at(2024, 3, 31, 23)] {
        assert!(!w.read(node, AsOf::current(when)).await.unwrap().uncertain);
    }
}

#[tokio::test]
async fn stating_an_unknown_start_is_a_refinement_that_keeps_the_text_hash_and_the_old_version_readable()
 {
    let w = World::new().await;
    let (node, first) = w.node("Acme", ValidTime::UNKNOWN).await;
    let earlier = w
        .read(node, AsOf::current(at(2024, 3, 15, 0)))
        .await
        .unwrap();

    let outcome = w
        .state(
            node,
            first.opened[0],
            BoundSide::Start,
            period(Precision::Month, at(2024, 3, 1, 0)),
        )
        .await;
    assert_eq!(outcome.change, BoundChange::Refinement);
    assert_eq!(outcome.opened.len(), 1);

    let now = w
        .read(node, AsOf::current(at(2024, 3, 15, 0)))
        .await
        .unwrap();
    assert_eq!(now.id, outcome.opened[0]);
    assert_eq!(now.values, earlier.values);
    assert_eq!(
        now.valid_time,
        ValidTime {
            from: period(Precision::Month, at(2024, 3, 1, 0)),
            to: Bound::Unknown
        }
    );
    assert!(
        now.uncertain,
        "March 15 is inside the month the start names"
    );
    // The Refinement states no new text: its Version shares the earlier
    // Version's text hash, so one embedding serves both (ADR-0007).
    assert_eq!(now.text_hash, earlier.text_hash);
    assert!(!now.text_hash.is_empty());
    let other = w.node("Other", ValidTime::UNKNOWN).await.0;
    let other = w
        .read(other, AsOf::current(at(2024, 3, 15, 0)))
        .await
        .unwrap();
    assert_ne!(
        other.text_hash, now.text_hash,
        "different text, different hash"
    );

    // Before the Refinement the earlier Version is still what gnomon knew:
    // it was incomplete, not wrong, and it is not rewritten.
    let known = outcome.recorded_at - SignedDuration::from_micros(1);
    let then = w
        .read(node, AsOf::at(at(2024, 3, 15, 0), known))
        .await
        .unwrap();
    assert_eq!(then.id, earlier.id);
    assert_eq!(then.valid_time, ValidTime::UNKNOWN);
    assert_eq!(then.recorded_to, Some(outcome.recorded_at));
    // And the Version before the stated start no longer holds.
    assert_eq!(w.read(node, AsOf::current(at(2024, 2, 1, 0))).await, None);
}

#[tokio::test]
async fn narrowing_a_precision_within_the_same_period_is_a_refinement() {
    let w = World::new().await;
    let (node, first) = w
        .node(
            "Acme",
            ValidTime::since_bound(period(Precision::Year, at(2024, 1, 1, 0))),
        )
        .await;
    let to_month = w
        .state(
            node,
            first.opened[0],
            BoundSide::Start,
            period(Precision::Month, at(2024, 3, 9, 0)),
        )
        .await;
    assert_eq!(to_month.change, BoundChange::Refinement);
    let to_day = w
        .state(
            node,
            to_month.opened[0],
            BoundSide::Start,
            period(Precision::Day, at(2024, 3, 14, 0)),
        )
        .await;
    assert_eq!(to_day.change, BoundChange::Refinement);
    let read = w
        .read(node, AsOf::current(at(2024, 3, 14, 12)))
        .await
        .unwrap();
    assert_eq!(
        read.valid_time.from,
        period(Precision::Day, at(2024, 3, 14, 0))
    );
    assert!(read.uncertain);
    assert!(
        !w.read(node, AsOf::current(at(2024, 3, 15, 0)))
            .await
            .unwrap()
            .uncertain
    );
}

#[tokio::test]
async fn an_end_that_was_unknown_can_be_refined_too() {
    let w = World::new().await;
    let (node, first) = w
        .node(
            "Acme",
            ValidTime::since_bound(period(Precision::Instant, at(2020, 1, 1, 0))),
        )
        .await;
    let outcome = w
        .state(
            node,
            first.opened[0],
            BoundSide::End,
            period(Precision::Year, at(2024, 6, 1, 0)),
        )
        .await;
    assert_eq!(outcome.change, BoundChange::Refinement);
    let read = w
        .read(node, AsOf::current(at(2023, 12, 31, 0)))
        .await
        .unwrap();
    assert_eq!(
        read.valid_time.to,
        period(Precision::Year, at(2024, 1, 1, 0))
    );
    assert_eq!(w.read(node, AsOf::current(at(2024, 1, 1, 0))).await, None);
}

#[tokio::test]
async fn a_bound_outside_the_earlier_period_is_recorded_as_a_correction_not_a_refinement() {
    let w = World::new().await;
    let (node, first) = w
        .node(
            "Acme",
            ValidTime::since_bound(period(Precision::Month, at(2024, 3, 1, 0))),
        )
        .await;
    // April is not within March: the source contradicts the earlier Version.
    let outcome = w
        .state(
            node,
            first.opened[0],
            BoundSide::Start,
            period(Precision::Day, at(2024, 4, 2, 0)),
        )
        .await;
    assert_eq!(outcome.change, BoundChange::Correction);
    assert_eq!(outcome.opened.len(), 1);

    // The wrong Version stays readable as-of before the Correction.
    let known = outcome.recorded_at - SignedDuration::from_micros(1);
    let wrong = w
        .read(node, AsOf::at(at(2024, 3, 15, 0), known))
        .await
        .expect("the earlier belief is still readable");
    assert_eq!(wrong.id, first.opened[0]);
    assert_eq!(
        wrong.valid_time.from,
        period(Precision::Month, at(2024, 3, 1, 0))
    );
    assert_eq!(w.read(node, AsOf::current(at(2024, 3, 15, 0))).await, None);
    let now = w
        .read(node, AsOf::current(at(2024, 4, 2, 0)))
        .await
        .unwrap();
    assert_eq!(
        now.valid_time.from,
        period(Precision::Day, at(2024, 4, 2, 0))
    );

    // A coarser statement is no narrowing either.
    let coarser = w
        .state(
            node,
            now.id,
            BoundSide::Start,
            period(Precision::Year, at(2024, 1, 1, 0)),
        )
        .await;
    assert_eq!(coarser.change, BoundChange::Correction);
}

#[tokio::test]
async fn restating_the_same_bound_is_unchanged() {
    let w = World::new().await;
    let (node, first) = w
        .node(
            "Acme",
            ValidTime::since_bound(period(Precision::Month, at(2024, 3, 1, 0))),
        )
        .await;
    let outcome = w
        .state(
            node,
            first.opened[0],
            BoundSide::Start,
            period(Precision::Month, at(2024, 3, 20, 0)),
        )
        .await;
    assert_eq!(outcome.change, BoundChange::Unchanged);
    assert!(outcome.opened.is_empty());
    let read = w
        .read(node, AsOf::current(at(2024, 5, 1, 0)))
        .await
        .unwrap();
    assert_eq!(read.id, first.opened[0]);
}

#[tokio::test]
async fn a_later_write_to_the_node_keeps_a_refined_bound() {
    let w = World::new().await;
    let (node, first) = w.node("Acme", ValidTime::UNKNOWN).await;
    w.state(
        node,
        first.opened[0],
        BoundSide::Start,
        period(Precision::Month, at(2024, 3, 1, 0)),
    )
    .await;
    // Another Episode states a second property over a window inside it.
    let ep = w.episode(at(2024, 7, 1, 0)).await;
    w.caller()
        .write_node(
            w.tenant,
            node,
            &Patch::new(
                ep,
                ValidTime::between(at(2024, 5, 1, 0), at(2024, 6, 1, 0)),
                values(json!({ "aliases": ["ACME"] })),
            ),
        )
        .await
        .unwrap();
    let before = w
        .read(node, AsOf::current(at(2024, 4, 1, 0)))
        .await
        .unwrap();
    assert_eq!(
        before.valid_time.from,
        period(Precision::Month, at(2024, 3, 1, 0))
    );
    assert_eq!(w.read(node, AsOf::current(at(2024, 2, 1, 0))).await, None);
}

#[tokio::test]
async fn a_statement_that_would_empty_the_valid_time_is_refused() {
    let w = World::new().await;
    let (node, first) = w
        .node(
            "Acme",
            ValidTime::between(at(2024, 3, 1, 0), at(2024, 4, 1, 0)),
        )
        .await;
    let ep = w.episode(at(2024, 6, 1, 0)).await;
    let err = w
        .caller()
        .state_bound(
            w.tenant,
            node,
            &BoundStatement {
                episode: ep,
                version: first.opened[0],
                side: BoundSide::Start,
                bound: period(Precision::Year, at(2025, 1, 1, 0)),
            },
        )
        .await
        .unwrap_err();
    match err {
        gnomon::Error::InvalidWrite(m) => assert!(m.contains("is empty"), "{m}"),
        other => panic!("expected a refused write, got {other:?}"),
    }
}
