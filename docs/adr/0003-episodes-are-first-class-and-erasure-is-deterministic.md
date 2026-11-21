---
status: accepted
---

# Episodes are first-class records, and Erasure is deterministic

An Episode is an immutable record of its own, not a Node. A Version, Summary, Community or embedding links to the Episodes it was derived from by Citations, which may name a property and a place in the Episode. The Store offers Retract Episode, which records a Retraction for every Version cited only by that Episode and keeps history readable, and Erase Episode, which physically deletes the Episode, its Citations, every Version cited only by it, and every derived item that cites it, in one transaction. We chose this because erasure on request is a legal obligation for some consumers, and it is simpler and safer to guarantee when a source cannot be edited, resolved or merged like an ordinary Node.

## Considered options

- Sources as Nodes, with citing Edges to them. Provenance would be traversable by the same functions, but Erasure would have to count Edges, and nothing would stop a source Node being versioned or resolved like any other.

## Consequences

- A Version cited by another Episode survives Erasure, but loses the Citation and every value the erased Episode alone supplied. Its values are recomputed from the Contributions that remain, in closed Versions as well as current ones (ADR-0007).
- After an Erasure, a Summary or Community that drew on the erased Episode is missing until the Pipeline regenerates it from the remaining Episodes.
- Erasure is not exposed over MCP unless a deployment turns it on.
