//! Added after the result, to explain it. Neither test is part of the decision
//! rule, and neither changes an input.

use std::collections::BTreeSet;

use community_warm_start_proto::graph::{NODES, scenario};
use community_warm_start_proto::{Strategy, run};
use leiden_rs::{GraphDataBuilder, Leiden, LeidenConfig, Partition};

/// `leiden-rs` stops a warm start as soon as no single Node wants to move, and
/// a Node only moves to a neighbour's community. So two halves that no Edge
/// joins stay one community, which a cold run splits.
#[test]
fn a_warm_start_keeps_two_unjoined_triangles_in_one_community() {
    let mut builder = GraphDataBuilder::new(6);
    for (a, b) in [(0, 1), (1, 2), (0, 2), (3, 4), (4, 5), (3, 5)] {
        builder.add_edge(a, b, 1.0).unwrap();
    }
    let graph = builder.build().unwrap();
    let leiden = Leiden::new(LeidenConfig {
        seed: Some(42),
        ..LeidenConfig::default()
    });

    let warm = leiden
        .run_with_initial_partition(&graph, Partition::from_membership(vec![0; 6]))
        .unwrap();
    let cold = leiden.run(&graph).unwrap();

    assert_eq!(warm.partition.num_communities(), 1);
    assert_eq!(cold.partition.num_communities(), 2);
}

/// How many detected Communities of one partition are not connected by their
/// own Edges.
fn disconnected(edges: &BTreeSet<(usize, usize)>, labels: &[usize]) -> usize {
    let mut parent: Vec<usize> = (0..NODES).collect();
    fn root(parent: &mut [usize], mut node: usize) -> usize {
        while parent[node] != node {
            parent[node] = parent[parent[node]];
            node = parent[node];
        }
        node
    }
    for (a, b) in edges {
        if labels[*a] == labels[*b] {
            let (a, b) = (root(&mut parent, *a), root(&mut parent, *b));
            parent[a] = b;
        }
    }
    let mut pieces = vec![BTreeSet::new(); NODES];
    for (node, label) in labels.iter().enumerate() {
        pieces[*label].insert(root(&mut parent, node));
    }
    pieces.iter().filter(|p| p.len() > 1).count()
}

#[test]
fn no_community_of_this_run_is_disconnected() {
    let scenario = scenario();
    for strategy in [Strategy::Warm, Strategy::Cold] {
        let run = run(&scenario, strategy);
        for (at, (graph, labels)) in scenario.graphs.iter().zip(&run.partitions).enumerate() {
            assert_eq!(
                disconnected(&graph.edges, labels),
                0,
                "{strategy:?}, refresh {at}"
            );
        }
    }
}
