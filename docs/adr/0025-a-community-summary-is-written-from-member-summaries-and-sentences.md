---
status: accepted
---

# A Community's Summary is written from member Summaries and Sentences, and follows every change to them

The Pipeline writes a Community's Summary with the main model from two inputs: the current Summary of each member Node in the Community's Scope, and the Sentences of the Edges between members whose Version is current in Recorded time and valid at the Community's `as_of_valid` instant. Its Citations are the union of the inputs' Citations. A member with no Summary adds none, and a Community with no input that carries a Citation is not written.

A refresh regenerates a Community when its member set changed or an input did. One change is a member joining or leaving, or a member's Summary or a Sentence between members being added, replaced or dropped. The minimum number of changes (ADR-0014) applies to each Community on its own and is 1 unless a deployment raises it. A new Community is always written, and an ending one always ends (ADR-0022). `maintain` ignores the minimum, so it regenerates every stale Community, and a second run with no new writes changes nothing.

A linked group is the new and stored Communities connected by shared members. A refresh finishes a group's model calls first, then commits its writes and ends in one transaction. When any Community of a group must be written, the whole group is written. The Summaries, the staleness rule, the minimum and the commits are planned, not built (#38, #39).

We chose these inputs because they bound the model call and keep the Citations exact, as for a Node's Summary (ADR-0015). An Erasure removes a Community that cites an erased Episode (ADR-0003), so a Summary that used a text without citing its Episodes could repeat erased content. We chose to follow every input change because a Retraction removes no Citation. A Community regenerated only for its members could keep stating a retracted statement, and Search would return it.

## Considered options

- The member Nodes' current Versions and the Sentences between members. A Node can have many current Versions, so the size cap would not bound the input for them.
- The member Summaries alone. They also describe Edges to Nodes outside the Community, and nothing in the input would mark which Edges join members.
- Citations of the member Versions only. Erasing an Episode that only an Edge cites would leave its content in the Summary.
- Regenerating only when the member set changed. It costs the fewest model calls and leaves retracted, corrected and ended statements in the Summary until a Node joins or leaves.
- Regenerating on a member change or when an input was retracted, corrected or ended. The Store would have to record why each input changed.
- A minimum counted over the whole Scope. A busy Community would trigger calls for quiet ones, and one retraction could wait for unrelated changes.
- One transaction for the whole Scope. One call that always fails would block every Community in it.
- One transaction for each Community. A split or a merge could leave a Node in two current Communities.

## Consequences

- This settles for Communities what ADR-0014 means by a change to member Nodes. It replaces story 72 of #23, which regenerated a Summary only when the member set changed.
- It costs more main-model calls than regenerating for members alone. The measurement in ADR-0022 counted member changes only, so it does not predict these calls.
- A member Summary also describes Edges to Nodes outside the Community, so the Community's Summary may mention them.
- A Community follows a member Summary, not the member's Versions. A statement stays in the Community's Summary until the member Summaries that repeat it are regenerated.
- A deployment that raises the minimum accepts that a retracted statement can stay until the Community reaches it or `maintain` runs.
- A failed model call holds back its linked group only. A retry calls the model again, and nothing of the failed attempt was committed (ADR-0011).
- The size cap bounds the member Summaries in one call. It does not bound the Sentences between members.
