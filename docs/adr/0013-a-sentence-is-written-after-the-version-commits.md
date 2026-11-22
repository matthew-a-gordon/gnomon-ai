---
status: accepted
---

# A Sentence is written after its Version commits, and Erasure deletes it on recompute

The Pipeline writes an Edge Version's Sentence in its own stage, from the Version as the Store committed it, using the small model. The extraction call does not write it. We chose this because a Version's values are chosen by the Store's merge (ADR-0007), so a value the model extracted can lose, and an Erasure recompute can change them again. A Sentence written from the extraction output could then state a value its Version does not hold, or repeat an erased value.

## Considered options

- The extraction call writes the Sentence with the Edge's properties. One fewer model call, but the Sentence can disagree with the Version.

## Consequences

- Erasure deletes the Sentence of every Edge Version it recomputes, with its token rows and embeddings, as it already does for embeddings (ADR-0003). The Pipeline then writes a new Sentence. Ticket #13 needs this added.
- Until the Sentence exists, an Edge Version is found through the properties its Kind marks as searchable (ADR-0002, ADR-0010).
- A Sentence leaves out Valid time, so a Refinement changes no Sentence and reuses its embedding (ADR-0010).
