use community_warm_start_proto::graph::{BATCHES, Scenario, scenario};
use community_warm_start_proto::{SIZE_CAP, Strategy, run};

/// Two runs on the same input with the same seed give the same partition, on
/// every refresh.
fn assert_repeats(strategy: Strategy) {
    let first = run(&scenario(), strategy);
    let second = run(&scenario(), strategy);

    assert_eq!(first.partitions.len(), BATCHES + 1);
    for (refresh, (a, b)) in first.partitions.iter().zip(&second.partitions).enumerate() {
        assert_eq!(a, b, "{strategy:?}, refresh {refresh}");
    }
}

#[test]
fn two_warm_started_runs_give_the_same_partitions() {
    assert_repeats(Strategy::Warm);
}

#[test]
fn two_cold_runs_give_the_same_partitions() {
    assert_repeats(Strategy::Cold);
}

#[test]
fn the_strategies_share_refresh_0_and_then_differ() {
    let scenario: Scenario = scenario();
    let warm = run(&scenario, Strategy::Warm);
    let cold = run(&scenario, Strategy::Cold);

    assert_eq!(warm.partitions[0], cold.partitions[0]);
    // Equal partitions throughout would mean the warm start was never passed.
    assert_ne!(warm.partitions, cold.partitions);
}

#[test]
fn every_refresh_is_reported_and_keeps_the_size_cap() {
    let scenario = scenario();
    for strategy in [Strategy::Warm, Strategy::Cold] {
        let run = run(&scenario, strategy);

        assert_eq!(run.refreshes.len(), BATCHES);
        for row in &run.refreshes {
            assert!(row.largest <= SIZE_CAP, "{strategy:?}: {row:?}");
            assert!(row.regenerated <= row.communities, "{strategy:?}: {row:?}");
            assert!(row.new_ids <= row.regenerated, "{strategy:?}: {row:?}");
        }
    }
}
