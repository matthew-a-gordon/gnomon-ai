---
status: accepted
---

# A Summary or Community belongs to one Scope and cites only Episodes from it

The Pipeline writes each Summary and each Community into a single Scope, and builds it only from Episodes in that Scope. A Node cited from two Scopes has one Summary per Scope. A refresh call regenerates the Summaries and Communities whose cited Episodes or member Nodes changed since they were generated, found by comparing Citations, with an optional minimum number of changes before it acts. A Summary left with no Citation to a surviving Episode is never regenerated from nothing, so it stays missing (ADR-0003).

We chose this because every row has a Scope and RLS shows a caller only the Scopes they may read (ADR-0005). A Summary built from Episodes in several Scopes would repeat one Scope's content to a caller who can read only another, and no read-time filter could take it back out of the prose.

## Considered options

- Summaries and Communities built across a Tenant's Scopes, with the caller's allowed Scopes filtering them at read. A filter can hide a row but cannot remove one Scope's words from a sentence that also uses another's.
- Refreshing a Summary on every Episode that touches its Node. A bulk load would regenerate the same Summary many times.

## Consequences

- A Community is detected over the Nodes and Edges of one Scope. Two Scopes never share a Community.
- Search over Summaries and Communities needs no extra filter. RLS already limits them to the caller's Scopes.
