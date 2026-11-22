---
status: accepted
---

# add_episode is one resumable call, and its stage records hold no model output

The Pipeline's `add_episode` stores the Episode, then runs three stages: extract and write the graph, write Sentences, and store embeddings. Each stage commits its output to the Store and its own stage record in one transaction, so a retry resumes at the first unfinished stage. The call returns after the embeddings, with a per-stage result. Summaries and Communities are not part of it. They are refreshed by a separate call, because one Episode can touch many Nodes and a bulk load would otherwise regenerate the same Summary many times. Consumers wrap `add_episode` in their own job system.

A stage record is a Store table under RLS. It holds the Episode, the stage name, a status, an attempt count and the last error. It holds no model output. We chose this because anything derived from an Episode must be reachable by Erasure (ADR-0003), and a stored model output would be one more derived table to delete and to cover in the no-residue test.

## Considered options

- Stage records that keep each stage's model output, so a retry skips the model call. A retry would cost less, but the stored output repeats the Episode's content and Erasure would have to delete it.
- Returning after the Store write only, with every derived step run later. A caller could not tell when its Episode was searchable.
- Running Summaries and Communities inside the call. Unbounded latency, and repeated work during a bulk load.

## Consequences

- The Store needs a stage table and functions to write and read it. It is a new ticket.
- A retry of an unfinished stage calls the model again, and the output may differ from the lost attempt. Nothing of the lost attempt was committed, so this is safe.
- Erasure deletes the Episode's stage records.
- A stage record also stores usage counts: model name, call count, input and output tokens, and the provider's reported cost when given. It holds no text.
- The server's optional queue is the stage table itself. A worker claims a pending or retry-due record with `FOR UPDATE SKIP LOCKED`, sets a lease time and commits, runs the stage, and writes the next record. A failure sets a retry time with exponential backoff, and after a maximum number of attempts the record stays visible as failed. No separate jobs table exists, and no `pg_cron`.
- The claim-and-run step is one bounded call, `run_queue`, which runs due stage records until a deadline or a stage limit is reached or the queue is empty, and then returns. The server's polling loop is a loop around it, and the bindings expose it for a consumer who runs its own scheduler (ADR-0016).
