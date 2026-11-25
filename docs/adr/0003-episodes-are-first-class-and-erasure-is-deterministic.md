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
- Erasure deletes Search's derived rows in the same transaction (ADR-0010):
  - the full and quantized embeddings of every Version it deletes or recomputes;
  - the token rows and the BM25 counts they feed;
  - the name and Alias rows.

  The Pipeline re-embeds recomputed text afterwards. There is no adjacency table to delete from: the neighbourhood branch of Search reads `gnomon.edge` and `gnomon.version` under RLS, through the `edge_source` and `edge_target` indexes, and meets the Search budgets that way (ADR-0010).
- An Erasure is complete at commit, when nothing derived from the Episode is readable. Copies of the erased bytes stay in dead tuples and index pages until the affected tables are vacuumed.
  - `VACUUM` cannot run in a transaction, so a separate call, which the deployment runs, vacuums them. That call uses the `MAINTAIN` privilege granted to the gnomon role.
  - Only `VacuumMode::Full` purges the pages. It rewrites each table and its indexes and drops the old files, so the bytes are gone from the tables when that call has finished.
  - `VacuumMode::Plain` removes the dead tuples and their index entries but does not overwrite the space it frees. The erased bytes stay in the heap's free space and in index pages until Postgres reuses that space.
  - WAL archives, physical replicas and backups keep their own copies of the bytes. They are outside the call's reach, and a deployment that must purge them does so by its own means.
- Erasure is not exposed over MCP unless a deployment turns it on.
