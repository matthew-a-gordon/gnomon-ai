//! The synthetic Scope graph and its batches of Edge changes. Every value here
//! is fixed in `../../PROTOTYPE.md`, which was committed before the first run.

use std::collections::BTreeSet;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

pub const NODES: usize = 2000;
pub const BATCHES: usize = 50;
const ENDS_PER_BATCH: usize = 5;
const NEW_PER_BATCH: usize = 5;
const SMALLEST_PLANTED: usize = 20;
const LARGEST_PLANTED: usize = 80;
const INSIDE_EDGES_PER_NODE: usize = 4;
const BETWEEN_EDGES: usize = NODES;
const NEW_EDGE_INSIDE: f64 = 0.8;
const GRAPH_SEED: u64 = 91;

/// The current Edges, each as the pair of Nodes it joins, lower index first.
/// Detection ignores direction.
#[derive(Debug, Clone)]
pub struct Graph {
    pub edges: BTreeSet<(usize, usize)>,
}

pub struct Scenario {
    /// The planted community of each Node.
    pub planted: Vec<usize>,
    /// The starting graph, then the graph after each batch.
    pub graphs: Vec<Graph>,
}

pub fn scenario() -> Scenario {
    let mut rng = StdRng::seed_from_u64(GRAPH_SEED);

    let mut sizes: Vec<usize> = Vec::new();
    let mut left = NODES;
    while left > 0 {
        let drawn = rng.random_range(SMALLEST_PLANTED..=LARGEST_PLANTED);
        match sizes.last_mut() {
            Some(last) if left < SMALLEST_PLANTED => *last += left,
            _ => sizes.push(drawn.min(left)),
        }
        left = NODES - sizes.iter().sum::<usize>();
    }
    let planted: Vec<usize> = sizes
        .iter()
        .enumerate()
        .flat_map(|(community, size)| std::iter::repeat_n(community, *size))
        .collect();

    let mut edges = BTreeSet::new();
    let mut first = 0;
    for size in &sizes {
        let nodes = first..first + size;
        let target = edges.len() + INSIDE_EDGES_PER_NODE * size;
        while edges.len() < target {
            let a = rng.random_range(nodes.clone());
            let b = rng.random_range(nodes.clone());
            if a != b {
                edges.insert(pair(a, b));
            }
        }
        first += size;
    }
    let target = edges.len() + BETWEEN_EDGES;
    while edges.len() < target {
        let a = rng.random_range(0..NODES);
        let b = rng.random_range(0..NODES);
        if planted[a] != planted[b] {
            edges.insert(pair(a, b));
        }
    }

    let mut graphs = vec![Graph { edges }];
    for _ in 0..BATCHES {
        let at_start = &graphs[graphs.len() - 1].edges;
        let mut edges = at_start.clone();
        for _ in 0..ENDS_PER_BATCH {
            let picked = rng.random_range(0..edges.len());
            let ended = *edges.iter().nth(picked).expect("picked within length");
            edges.remove(&ended);
        }
        for _ in 0..NEW_PER_BATCH {
            let inside = rng.random_bool(NEW_EDGE_INSIDE);
            loop {
                let a = rng.random_range(0..NODES);
                let b = rng.random_range(0..NODES);
                let fits = a != b && (planted[a] == planted[b]) == inside;
                if fits && !at_start.contains(&pair(a, b)) && edges.insert(pair(a, b)) {
                    break;
                }
            }
        }
        graphs.push(Graph { edges });
    }

    Scenario { planted, graphs }
}

fn pair(a: usize, b: usize) -> (usize, usize) {
    (a.min(b), a.max(b))
}
