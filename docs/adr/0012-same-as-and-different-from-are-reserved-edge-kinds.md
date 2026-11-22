---
status: accepted
---

# Resolution records its findings as same_as and different_from Edges

Nodes never merge. When the Pipeline created two Nodes for one thing, or a person says two Nodes differ, the finding is recorded as an Edge of one of two reserved Edge Kinds, `same_as` and `different_from`. Every Ontology ships both, joining a Node to a Node. They are ordinary Edges, so they carry Valid time, Citations, Retraction and Erasure with no new mechanism, which follows ADR-0002: anything that points at a Node is an Edge.

A `different_from` Edge stops the Pipeline proposing the same pair again. A `same_as` Edge lets the Pipeline's context assembly treat two Nodes as one. Reads and Search do not collapse them. A wrong `same_as` is undone with a Retraction.

## Considered options

- A table of links in the Store. It would need its own rules for Valid time, Citations and Erasure.
- No stored links, with the Pipeline deciding again each time. The same wrong or right answer would cost a model call on every Episode, and a person's "these differ" would be lost.
- Merging Nodes. A Node is a stable identity, and a merge would rewrite history that As-of reads must reproduce.

## Consequences

- The Store ships the two Kinds in the root Ontology and declares both symmetric. It is a new ticket.
- Reads that need to follow `same_as` do it by traversal. Only the Pipeline's context assembly does so by default.
