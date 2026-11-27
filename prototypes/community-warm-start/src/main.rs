//! PROTOTYPE, throwaway. Runs both strategies, prints the report and writes
//! every partition to the directory named by the first argument.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::{env, fs, io};

use community_warm_start_proto::graph::{BATCHES, NODES, Scenario, scenario};
use community_warm_start_proto::{Run, Strategy, keeps_warm_start, run};

fn main() -> io::Result<()> {
    let out = PathBuf::from(env::args().nth(1).expect("an output directory"));
    fs::create_dir_all(&out)?;

    let scenario = scenario();
    let warm = run(&scenario, Strategy::Warm);
    let cold = run(&scenario, Strategy::Cold);

    let report = report(&scenario, &warm, &cold);
    print!("{report}");
    fs::write(out.join("report.txt"), report)?;
    for (name, run) in [("warm", &warm), ("cold", &cold)] {
        let lines: String = run
            .partitions
            .iter()
            .map(|labels| {
                let labels: Vec<String> = labels.iter().map(usize::to_string).collect();
                labels.join(",") + "\n"
            })
            .collect();
        fs::write(out.join(format!("partitions-{name}.txt")), lines)?;
    }
    Ok(())
}

fn report(scenario: &Scenario, warm: &Run, cold: &Run) -> String {
    let mut text = String::new();
    let planted = scenario.planted.iter().max().map_or(0, |c| c + 1);
    let edges = |at: usize| scenario.graphs[at].edges.len();
    writeln!(
        text,
        "Synthetic Scope graph: {NODES} Nodes, {planted} planted communities, {} Edges at the start, {} after {BATCHES} batches.\n",
        edges(0),
        edges(BATCHES)
    )
    .unwrap();

    writeln!(
        text,
        "         |           warm start                    |            cold run\n \
         refresh | regen  new ID  comms  mod.  largest drop | regen  new ID  comms  mod.  largest drop"
    )
    .unwrap();
    let cells = |run: &Run, at: usize| {
        let row = &run.refreshes[at];
        format!(
            "{:>5}  {:>6}  {:>5}  {:.3} {:>7} {:>4}",
            row.regenerated, row.new_ids, row.communities, row.modularity, row.largest, row.dropped
        )
    };
    for at in 0..BATCHES {
        writeln!(
            text,
            " {:>7} | {} | {}",
            at + 1,
            cells(warm, at),
            cells(cold, at)
        )
        .unwrap();
    }

    let total = |run: &Run| -> usize { run.refreshes.iter().map(|r| r.regenerated).sum() };
    let new_ids = |run: &Run| -> usize { run.refreshes.iter().map(|r| r.new_ids).sum() };
    let (warm_total, cold_total) = (total(warm), total(cold));
    writeln!(
        text,
        "\nTotal Communities regenerated over {BATCHES} refreshes: warm start {warm_total} ({} with a new ID), cold run {cold_total} ({} with a new ID).",
        new_ids(warm),
        new_ids(cold)
    )
    .unwrap();
    writeln!(
        text,
        "The warm start regenerates {:.1}% fewer. The rule needs at least a third fewer, so at most {} here.",
        100.0 * (1.0 - warm_total as f64 / cold_total as f64),
        2 * cold_total / 3
    )
    .unwrap();
    let decision = if keeps_warm_start(warm_total, cold_total) {
        "keep the warm start"
    } else {
        "detection runs cold, and the previous partition is an optional input to CommunityDetector"
    };
    writeln!(text, "Decision: {decision}.").unwrap();
    text
}
