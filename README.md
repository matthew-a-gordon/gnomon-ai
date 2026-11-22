# gnomon

> **gno·mon** /ˈnoʊ.mɒn/ (*NOH-mon*) &nbsp; *noun*
>
> 1. The part of a sundial that casts the shadow by which the time is read.
> 2. *Geometry.* The figure left when a parallelogram is removed from one corner of a larger, similar parallelogram.
>
> *Origin:* Greek *gnōmōn*, "indicator, one who knows," from *gignōskein*, "to know."

Gnomon is a bi-temporal knowledge graph for agentic AI systems. It is built on plain Postgres and needs no dedicated graph database.

Every fact in gnomon carries two times: when it was true in the world, and when the system came to know it. An agent can therefore ask what was true at a past instant and what it believed at a past instant as two separate questions. A correction writes a new version instead of overwriting the old one. Every belief traces back to the source it came from. Tenants are isolated by Postgres row-level security.

The name fits on both counts. A gnomon tells the time by the shadow it casts, and the word comes from the Greek for "one who knows."

## Status

Early design. The data model, packaging and client languages are still being decided, and there is nothing to install yet.

## Development

`docs/testing.md` explains how to run the test suites. They need Docker and start their own Postgres containers.
