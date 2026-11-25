---
status: accepted
---

# Episodes are first-class records, and Erasure is deterministic

An Episode is an immutable record of its own, not a Node. A Version, Summary, Community or embedding links to the Episodes it was derived from by Citations, which may name a property and a place in the Episode. The Store offers Retract Episode, which records a Retraction for every Version cited only by that Episode and keeps history readable, and Erase Episode, which physically deletes the Episode, its Citations, every Version cited only by it, and every derived item that cites it, in one transaction. We chose this because erasure on request is a legal obligation for some consumers, and it is simpler and safer to guarantee when a source cannot be edited, resolved or merged like an ordinary Node.

## Considered options

- A vacuum call after Erasure, run as a NOLOGIN role holding `MAINTAIN` on the tables an Erasure touches. Dropped, as the Consequences say.
- Sources as Nodes, with citing Edges to them. Provenance would be traversable by the same functions, but Erasure would have to count Edges, and nothing would stop a source Node being versioned or resolved like any other.

## Consequences

- A Version cited by another Episode survives Erasure, but loses the Citation and every value the erased Episode alone supplied. Its values are recomputed from the Contributions that remain, in closed Versions as well as current ones (ADR-0007).
- After an Erasure, a Summary or Community that drew on the erased Episode is missing until the Pipeline regenerates it from the remaining Episodes.
- Erasure deletes Search's derived rows in the same transaction (ADR-0010):
  - the full and quantized embeddings of every Version it deletes or recomputes;
  - the token rows and the BM25 counts they feed;
  - the name and Alias rows.

  The Pipeline re-embeds recomputed text afterwards. There is no adjacency table to delete from: the neighbourhood branch of Search reads `gnomon.edge` and `gnomon.version` under RLS, through the `edge_source` and `edge_target` indexes, and meets the Search budgets that way (ADR-0010).
- An Erasure is complete at commit, when no role can read anything derived from the erased Episode.
  - Dead tuples and index pages keep copies of the erased bytes until Postgres vacuums the tables, which autovacuum does, or until the operator runs `VACUUM`.
  - gnomon offers no call for it and promises nothing about the bytes on disk, in WAL, in backups or on replicas.
  - Spec #1 asked for a vacuum call after Erasure. It was built and then dropped by the project owner's decision, because it kept a privileged role and code for a physical guarantee the library cannot give. A plain `VACUUM` leaves the bytes in free space and index pages, and skips pruning a page another process has pinned. A transaction that began before the Erasure committed keeps the tuples through `VACUUM FULL` too, and no call reaches WAL, backups or replicas.
- Erasure is not exposed over MCP unless a deployment turns it on.
