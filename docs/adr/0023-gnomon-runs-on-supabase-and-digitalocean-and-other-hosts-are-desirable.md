---
status: accepted
---

# gnomon runs on Supabase and DigitalOcean, and other hosts are desirable

gnomon must run on two required hosts: Supabase, and DigitalOcean managed PostgreSQL on both Standard and Advanced Edition. Six more hosts are desirable: Amazon RDS, Aurora, Cloud SQL, AlloyDB, Azure Flexible Server and Neon.

The floor is PostgreSQL 17 with pgvector 0.7.0 or later and btree_gist. All eight hosts offer both extensions.

- A required host blocks a feature. If Supabase or either DigitalOcean edition lacks an extension, a setting or a PostgreSQL version, gnomon does not depend on it.
- A desirable host does not block a feature. The ADR that adopts the feature names each desirable host it loses.

We chose this because the consuming applications run on Supabase and DigitalOcean, and the other hosts are where a consumer might move or start later. The rule is about hosts, not about extensions. An extension costs a consumer one `create extension` line, and on Azure one more name in `azure.extensions`. A host that cannot run gnomon costs a consumer the move.

## Considered options

- No extension beyond pgvector, the rule in spec #1 story 8. It kept btree_gist out, and the exclusion constraint on Versions wrote equality of the Tenant and of the Node or Edge as overlap of one-point ranges over a custom `gnomon.uuid_range` type. That form is correct but unusual. It needed its own type, five revokes on the constructor functions Postgres creates with it, and a grant to the Erasure definer. No host lacks btree_gist, so the rule bought nothing.
- Every named host blocks a feature, unless an ADR overrides it. Hosts that no consuming application uses would decide gnomon's design.

## Consequences

- #80 adopts btree_gist. The first migration file creates it when it is missing, beside pgvector. The exclusion constraint uses `with =`, and `gnomon.uuid_range` is removed.
- btree_gist's schema does not matter to gnomon. The constraint finds its operator class as the default GiST class for `uuid`, and it uses the `=` operator in `pg_catalog`. pgvector is different, because gnomon's functions name its types and operators (#80).
- On Azure Flexible Server, an operator adds `vector` and `btree_gist` to `azure.extensions` before applying the migrations.
- The CI lanes test the `pgvector/pgvector:pg17` and `supabase/postgres` images (ADR-0020). No lane tests DigitalOcean, so its extension lists come from DigitalOcean's documentation (`docs/research/search.md`).
