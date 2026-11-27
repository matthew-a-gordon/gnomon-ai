use std::collections::BTreeSet;

use community_warm_start_proto::graph::{BATCHES, NODES, Scenario, scenario};

fn current(scenario: &Scenario) -> Vec<BTreeSet<(usize, usize)>> {
    scenario.graphs.iter().map(|g| g.edges.clone()).collect()
}

#[test]
fn each_batch_ends_five_edges_and_adds_five() {
    let graphs = current(&scenario());

    assert_eq!(graphs.len(), BATCHES + 1);
    for pair in graphs.windows(2) {
        assert_eq!(pair[0].difference(&pair[1]).count(), 5);
        assert_eq!(pair[1].difference(&pair[0]).count(), 5);
    }
}

#[test]
fn the_starting_graph_has_the_planted_structure_of_the_brief() {
    let scenario = scenario();
    let planted = &scenario.planted;
    let edges = &scenario.graphs[0].edges;

    assert_eq!(planted.len(), NODES);
    let mut sizes = vec![0; planted.iter().max().unwrap() + 1];
    planted.iter().for_each(|c| sizes[*c] += 1);
    // The last remainder under 20 joins the community before it.
    assert!(sizes.iter().all(|s| (20..100).contains(s)), "{sizes:?}");

    let inside = edges
        .iter()
        .filter(|(a, b)| planted[*a] == planted[*b])
        .count();
    assert_eq!(inside, 4 * NODES);
    assert_eq!(edges.len() - inside, NODES);
    // One direction per pair, and no Edge from a Node to itself.
    assert!(edges.iter().all(|(a, b)| a < b));
}

#[test]
fn the_same_input_is_generated_every_time() {
    assert_eq!(current(&scenario()), current(&scenario()));
}
