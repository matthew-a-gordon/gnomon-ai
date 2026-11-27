# Prototype 6: warm start against cold Community detection

## Question

Should gnomon's default Community detector warm-start Leiden from the previous partition, or run it cold on each refresh? A Community keeps its identity while most of its members stay (`GLOSSARY.md`). Both strategies match the new partition to the stored Communities the same way, so the only difference left is how many Communities are regenerated. Issue #91.

## Decision rule

Fixed before the prototype runs:

- A Community is regenerated when its member set changed or it got a new ID.
- Keep the warm start if it regenerates at least a third fewer Communities in total over the 50 refreshes than the cold run. Otherwise detection runs cold, and the previous partition is only an optional input to the `CommunityDetector` trait.

## Matching, the same for both strategies

- Communities of fewer than three Nodes are dropped before matching.
- A new Community matches a stored one when their Jaccard similarity (shared members over all members of the two) is strictly above 0.5. The match keeps the stored ID.
- An unmatched new Community gets a new ID. An unmatched stored Community ends. An ended Community is not counted as regenerated.

## Fixed inputs

The issue fixes the strategies, about 2,000 Nodes, 50 batches of 10 Edge changes and the matching. Every other value below was chosen before the first run and is not changed after it.

Synthetic Scope graph:

- 2,000 Nodes in planted communities. Sizes are drawn uniformly from 20 to 80. A last remainder under 20 joins the community before it.
- Each planted community of `s` Nodes gets `4 * s` Edges between distinct pairs of its own Nodes, a mean of 8 per Node.
- 2,000 Edges join distinct pairs of Nodes in different planted communities, a mean of 2 per Node.
- Detection ignores Edge direction. No two current Edges join the same pair, and every Edge has weight 1.
- The graph generator is rand's `StdRng`, seeded with 91.

Batches:

- Each of the 50 batches is 5 Ends of validity and then 5 new Edges.
- An End of validity picks a current Edge uniformly at random.
- A new Edge joins two Nodes of one planted community with probability 0.8, and Nodes of two planted communities otherwise. It joins a pair that no Edge joined at the start of the batch, so a batch never ends an Edge and adds it back.
- The batches use the same generator as the graph, so both strategies see the same graphs in the same order.

Detector:

- `leiden-rs` 0.8.1 with default features off. This takes the sequential path, so no result depends on the number of threads.
- Modularity at resolution 1.0, size cap 100, seed 42. The same seed on every refresh, for both strategies. Every other setting is the crate default.
- Nodes keep the same index on every refresh, and Edges are passed in sorted order.

Refreshes:

- Refresh 0 runs cold on the starting graph, for both strategies, and stores the first Communities. It is not counted.
- A refresh then runs after each batch, 50 in all.
- The warm start passes the previous refresh's whole detector output to `run_with_initial_partition`, Communities under three Nodes included. The report counts the Communities dropped, so a reader can tell whether starting from the stored Communities alone could have differed.

## Report

- For each strategy: the Communities regenerated per refresh and in total, and the number of Communities after each refresh.
- Two runs on the same input with the same seed give the same partition, for each strategy.

Quality (modularity), the largest Community and the Communities dropped are printed as diagnostics. They play no part in the decision rule.

## Environment

- No database. Rust 1.93.1, the repository's pinned toolchain.

## Rules for every prototype

- Read `GLOSSARY.md` and every ADR in `docs/adr/` first, and use the glossary's terms. The decisions there are settled; this prototype tests them, it does not reopen them unless the numbers force it.
- Use the `/prototype` skill. Answer exactly the question above with throwaway code. Do not build gnomon itself.
- Commit only to the `prototype/community-warm-start` branch. Open no pull request for it and never merge it.
- The repo is public. Committed files must not name the consuming applications or quote their code, ADRs or issues; call them "the consuming applications".
- When you finish, write `FINDINGS.md` at the worktree root: the question; the answer in one sentence with its numbers; the method; a results table; what it means for which ADR (confirms, amends or contradicts); a recommendation; and caveats. Commit it.
