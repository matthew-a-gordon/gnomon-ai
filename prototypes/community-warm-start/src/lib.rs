//! PROTOTYPE, throwaway. See `../../PROTOTYPE.md` and `../../FINDINGS.md`.

pub mod graph;

use std::collections::{BTreeMap, BTreeSet};

use graph::{Graph, NODES, Scenario};
use leiden_rs::{GraphDataBuilder, Leiden, LeidenConfig, LeidenOutput, Partition};

// The detector's settings, the same for both strategies and on every refresh.
const RESOLUTION: f64 = 1.0;
pub const SIZE_CAP: usize = 100;
const SEED: u64 = 42;

/// The member set of a Community: indexes of Nodes.
pub type Members = BTreeSet<usize>;

/// The stored Communities of one Scope, by ID.
#[derive(Debug, Clone, Default)]
pub struct Stored {
    pub communities: BTreeMap<u64, Members>,
    pub next_id: u64,
}

/// A Community of fewer Nodes than this is dropped before matching.
const SMALLEST_COMMUNITY: usize = 3;

/// Whether the Jaccard similarity of two member sets is strictly above 0.5.
fn overlaps(a: &Members, b: &Members) -> bool {
    let shared = a.intersection(b).count();
    let all = a.len() + b.len() - shared;
    2 * shared > all
}

/// What one refresh did to the stored Communities.
pub struct Refresh {
    pub stored: Stored,
    /// Communities whose member set changed or that got a new ID.
    pub regenerated: usize,
    /// Those of the regenerated that got a new ID.
    pub new_ids: usize,
}

/// Matches a detector's partition (one label per Node) to the stored Communities.
pub fn refresh(before: &Stored, partition: &[usize]) -> Refresh {
    let mut communities: BTreeMap<usize, Members> = BTreeMap::new();
    for (node, label) in partition.iter().enumerate() {
        communities.entry(*label).or_default().insert(node);
    }
    let mut stored = Stored {
        communities: BTreeMap::new(),
        next_id: before.next_id,
    };
    let mut regenerated = 0;
    let mut new_ids = 0;
    for members in communities.into_values() {
        if members.len() < SMALLEST_COMMUNITY {
            continue;
        }
        // Both sides are partitions, so at most one stored Community can
        // share more than half of the union with this one.
        let matched = before
            .communities
            .iter()
            .find(|(_, old)| overlaps(old, &members));
        let (id, changed) = match matched {
            Some((id, old)) => (*id, *old != members),
            None => {
                new_ids += 1;
                stored.next_id += 1;
                (stored.next_id - 1, true)
            }
        };
        regenerated += usize::from(changed);
        stored.communities.insert(id, members);
    }
    Refresh {
        stored,
        regenerated,
        new_ids,
    }
}

/// The decision rule of #91: keep the warm start if, in total, it regenerates
/// at least a third fewer Communities than the cold run.
pub fn keeps_warm_start(warm_total: usize, cold_total: usize) -> bool {
    3 * warm_total <= 2 * cold_total
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strategy {
    /// Start Leiden from the previous refresh's partition.
    Warm,
    /// Start Leiden from every Node on its own.
    Cold,
}

/// One refresh, as the report prints it.
#[derive(Debug, PartialEq)]
pub struct Row {
    pub regenerated: usize,
    pub new_ids: usize,
    /// Stored Communities after the refresh.
    pub communities: usize,
    // Diagnostics, no part of the decision rule.
    pub modularity: f64,
    pub largest: usize,
    /// Detected Communities of fewer than three Nodes.
    pub dropped: usize,
}

pub struct Run {
    /// The detector's output at refresh 0 and after each batch.
    pub partitions: Vec<Vec<usize>>,
    /// Refresh 1 onwards. Refresh 0 only stores the first Communities.
    pub refreshes: Vec<Row>,
}

/// Runs one strategy over every graph of the scenario.
pub fn run(scenario: &Scenario, strategy: Strategy) -> Run {
    let first = detect(&scenario.graphs[0], None).partition;
    let mut stored = refresh(&Stored::default(), first.as_slice()).stored;
    let mut partitions = vec![first.as_slice().to_vec()];
    let mut refreshes = Vec::new();
    let mut previous = first;
    for graph in &scenario.graphs[1..] {
        let start = match strategy {
            Strategy::Warm => Some(previous),
            Strategy::Cold => None,
        };
        let output = detect(graph, start);
        let matched = refresh(&stored, output.partition.as_slice());
        stored = matched.stored;
        let sizes = output.partition.community_sizes();
        refreshes.push(Row {
            regenerated: matched.regenerated,
            new_ids: matched.new_ids,
            communities: stored.communities.len(),
            modularity: output.quality,
            largest: sizes.iter().copied().max().unwrap_or(0),
            dropped: sizes
                .iter()
                .filter(|size| (1..SMALLEST_COMMUNITY).contains(*size))
                .count(),
        });
        partitions.push(output.partition.as_slice().to_vec());
        previous = output.partition;
    }
    Run {
        partitions,
        refreshes,
    }
}

fn detect(graph: &Graph, previous: Option<Partition>) -> LeidenOutput {
    let mut builder = GraphDataBuilder::new(NODES);
    for (a, b) in &graph.edges {
        builder.add_edge(*a, *b, 1.0).expect("Nodes are in range");
    }
    let data = builder.build().expect("a valid graph");
    let leiden = Leiden::new(LeidenConfig {
        resolution: RESOLUTION,
        max_comm_size: SIZE_CAP,
        seed: Some(SEED),
        ..LeidenConfig::default()
    });
    match previous {
        Some(partition) => leiden.run_with_initial_partition(&data, partition),
        None => leiden.run(&data),
    }
    .expect("Leiden runs on a valid graph")
}
