# gnomon

A bi-temporal knowledge graph for agentic AI systems, stored in plain Postgres. It records what was true in the world and what the system believed, each at any past instant.

## Language

### The graph

**Node**:
A stable identity for a noun the graph knows about. A Node never changes; what is known about it is held in its Versions.
_Avoid_: Entity, vertex, thing

**Edge**:
A stable identity for a verb: a directed relationship between two Nodes. It joins the Nodes themselves, not their Versions.
_Avoid_: Fact, relationship, relation, link

**Version**:
One immutable state of a Node or an Edge, carrying its properties, its Valid time and its Recorded time.
_Avoid_: revision, snapshot, record

### The ontology

**Kind**:
A named class of Node or of Edge, declaring its properties and, for an Edge Kind, which Kinds it may join. A Kind may specialize one parent Kind and inherit what it declares. Every Ontology has a root Kind for Nodes and one for Edges.
_Avoid_: type, label, class, entity type

**Ontology**:
The Kinds a deployment registers, together with the hierarchy among them.
_Avoid_: schema, taxonomy

### Time

**Valid time**:
The interval during which a Version was true in the world, as its source states it, not when the system learned it.
_Avoid_: event time, effective time

**Recorded time**:
The interval during which the system held a Version as its current belief. Only gnomon assigns it.
_Avoid_: transaction time, system time

**As-of read**:
A read of the graph as it stood at a chosen valid-at instant and a chosen known-at instant.
_Avoid_: time-travel query, point-in-time read

### Ending a belief

**Correction**:
A new Version that replaces an earlier one because the earlier one was wrong about the same Valid time.
_Avoid_: Invalidation, update, fix

**End of validity**:
A new Version recording that what an earlier one stated was true and stopped being true.
_Avoid_: Invalidation, expiry, close

**Retraction**:
The recorded finding that what a Version stated was never true. The retracted Version stays readable in As-of reads.
_Avoid_: Invalidation, deletion, revocation

**Refinement**:
A new Version that states a bound of Valid time the earlier one left unknown and changes nothing else. The earlier Version was incomplete, not wrong.
_Avoid_: Correction, update, enrichment

### Sources and removal

**Episode**:
An immutable source from which Versions are derived, such as a message, a document, an event or a person's edit.
_Avoid_: document, source, input

**Reference time**:
When an Episode's source was produced, such as when a message was sent or a document was dated. It is not when gnomon recorded the Episode.
_Avoid_: timestamp, created at, event time

**Contribution**:
One Episode's statement of a value for one property of a Node or an Edge, over a Valid time. A Version's values are chosen from the Contributions that remain.
_Avoid_: claim, assertion, observation

**Authority**:
The rank a consumer gives an Episode, used to choose between values that different Episodes state for the same property.
_Avoid_: rank, weight, priority, trust

**Citation**:
The link from a Version, Summary or Community to an Episode it was derived from, optionally naming the property and the place in the Episode.
_Avoid_: support, cites edge, provenance link, reference

**Erasure**:
Physical deletion of an Episode, Node, Edge or Scope, of every Version derived from it alone, of every value it alone supplied to a Version that survives, and of every Summary or Community that drew on it, leaving no tombstone. The same request always removes the same rows.
_Avoid_: delete, purge, hard delete

### Derived content

**Summary**:
Generated prose describing one Node, derived from the Episodes it cites.
_Avoid_: description, profile, digest

**Community**:
A group of closely connected Nodes, with a generated Summary of the group.
_Avoid_: cluster, group, neighborhood

### Isolation

**Tenant**:
The unit of isolation: one customer's graph, isolated from every other Tenant's. A single-tenant deployment has exactly one, the default Tenant, and callers never name it.
_Avoid_: organization, account, workspace

**Scope**:
An opaque key below the Tenant that partitions a Tenant's graph. gnomon gives it no meaning of its own.
_Avoid_: namespace, tier, partition

### Parts of gnomon

**Store**:
The part of gnomon that holds the graph and enforces its guarantees in Postgres. It makes no model calls.
_Avoid_: core, engine, database

**Pipeline**:
The part of gnomon that turns Episodes into Versions with model calls, writing through the Store as any other caller would.
_Avoid_: ingestion, extractor, engine
