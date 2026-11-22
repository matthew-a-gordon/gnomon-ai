# Research: facts for designing the Pipeline

Checked on 2026-11-22. This note collects primary-source facts only. It does not design the Pipeline and it recommends nothing. It uses the glossary's terms for gnomon concepts. Where a source names a concept differently (Graphiti's entity, edge, fact and invalidation; GraphRAG's entity and relationship), the note uses the source's word inside that source's section, and only there.

Source read at these pins: graphiti `9b8a463` (core package v0.31.2, 2026-11-20), graphrag `34b2b67` (v3.3.0 plus 2 commits), graspologic-native `7731814`, vllm `118d12f` (latest release tag v0.34.0), text-embeddings-inference `ea547d3` (OpenAPI version 1.9.4). Crate versions are those on crates.io on 2026-11-22. Vendor documentation pages have no version, so each is cited with the date it was read.

## 1. Graphiti: the model calls inside add_episode

Pinned at [graphiti 9b8a463](https://github.com/getzep/graphiti/tree/9b8a4630092084734b080f2e6f409c2c23bdcc3a). Every path below is under `graphiti_core/` at that commit.

### Order of the calls

`Graphiti.add_episode` ([graphiti.py#L1586](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/graphiti.py#L1586)) runs these steps in order. Steps 2 to 5 are awaited one after another. Within one step, calls for several items run in parallel under `semaphore_gather`.

1. Load the previous Episodes. With no `previous_episode_uuids`, it fetches the latest 10 Episodes of the same source (`RELEVANT_SCHEMA_LIMIT = 10`, [search_utils.py#L64](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/search/search_utils.py#L64)). No model call.
2. Node extraction. One model call per Episode.
3. Optional identity-attribute extraction, only when the caller passes `identity_properties`. One model call per Node of a typed class that has declared properties.
4. Node resolution. Candidate search, then deterministic rules, then at most one model call for all Nodes the rules could not settle.
5. Edge extraction. One model call per Episode. It also returns the valid and invalid times.
6. Edge resolution. For each extracted edge: an embedding call, two searches, then one model call that decides duplicates and contradictions. A timestamp call follows only when extraction left both times empty. An attribute call follows only when the edge has a custom edge type with declared fields.
7. Node attribute extraction, one call per Node whose class declares fields, then Node summary updates, batched.
8. Embeddings for the new or changed Nodes and edges, then the write.
9. Optional community update when `update_communities=True`, one pair of calls per resolved Node.

Graphiti also has a concurrent-ingestion path and `add_episode_bulk`. The bulk path defaults to one combined call that extracts Nodes and edges together ([bulk_utils.py#L362](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/utils/bulk_utils.py#L362), [prompts/extract_nodes_and_edges.py](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/prompts/extract_nodes_and_edges.py)). The sections below describe the single-Episode `add_episode` path only.

### Which calls use the small model

Every call goes through `generate_prompt_response`, whose default is `model_size=ModelSize.medium` ([graphiti_types.py#L125](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/graphiti_types.py#L125)). The OpenAI-style client maps `small` to `config.small_model` and `medium` to `config.model` ([openai_base_client.py#L114](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/llm_client/openai_base_client.py#L114)). The default temperature is 1 ([llm_client/config.py](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/llm_client/config.py)).

| Call | Prompt name | Model size | Source |
|---|---|---|---|
| Node extraction | `extract_nodes.extract_message`, `.extract_text`, `.extract_json` | main (default) | [node_operations.py#L302](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/utils/maintenance/node_operations.py#L302) |
| Node deduplication | `dedupe_nodes.nodes` | main (default), `max_tokens` capped at 16384 | [node_operations.py#L677](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/utils/maintenance/node_operations.py#L677) |
| Edge extraction | `extract_edges.edge` | main (default), `max_tokens` 16384 | [edge_operations.py#L139](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/utils/maintenance/edge_operations.py#L139) |
| Edge duplicate and contradiction decision | `dedupe_edges.resolve_edge` | small | [edge_operations.py#L928](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/utils/maintenance/edge_operations.py#L928) |
| Edge timestamp fallback | `extract_edges.extract_timestamps` | small | [edge_operations.py#L833](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/utils/maintenance/edge_operations.py#L833) |
| Edge attributes | `extract_edges.extract_attributes` | small | [edge_operations.py#L1035](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/utils/maintenance/edge_operations.py#L1035) |
| Node attributes | `extract_nodes.extract_attributes` | small | [node_operations.py#L1107](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/utils/maintenance/node_operations.py#L1107) |
| Node summary | `extract_nodes.extract_summaries_batch` | small | [node_operations.py#L1166](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/utils/maintenance/node_operations.py#L1166) |
| Community pair summary and community name | `summarize_nodes.summarize_pair`, `.summary_description` | main (default) | [community_operations.py#L124](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/utils/maintenance/community_operations.py) |

The Anthropic client in the same commit has `DEFAULT_MODEL = 'claude-haiku-5-5'`, and the OpenAI client has `DEFAULT_MODEL` and `DEFAULT_SMALL_MODEL` both set to one model name, so by default the two sizes are the same model unless a caller sets `small_model`.

### Node extraction

Inputs ([node_operations.py#L76](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/utils/maintenance/node_operations.py#L76)):

- `episode_content`, the Episode body.
- `episode_timestamp` and `source_description`.
- `previous_episodes`, a list of `{content, timestamp}`.
- `entity_types`, a numbered list of `{entity_type_id, entity_type_name, entity_type_description}`. The default class "Entity" is always entry 0.
- `custom_extraction_instructions`.

The system prompt is "You are an entity extraction specialist for conversational messages", with a separate text and JSON variant chosen by Episode source ([prompts/extract_nodes.py#L84](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/prompts/extract_nodes.py#L84)). The user prompt is mostly a long NEVER-extract list and rules on specificity, with worked examples. The structured output is ([prompts/extract_nodes.py#L29](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/prompts/extract_nodes.py#L29)):

```text
ExtractedEntities { extracted_entities: [ExtractedEntity] }
ExtractedEntity   { name: str, entity_type_id: int, episode_indices: [int] (default [0]) }
```

After the call, Graphiti drops empty names and collapses exact duplicates by normalized name.

### Node deduplication

Order of the work in `resolve_extracted_nodes` ([node_operations.py#L844](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/utils/maintenance/node_operations.py#L844)):

1. Candidate collection per extracted Node: identity-property matches, exact normalized-name matches in the same `group_id`, and a cosine search over name embeddings. The search takes at most 15 candidates (`NODE_DEDUP_CANDIDATE_LIMIT`) and drops scores below 0.6 (`NODE_DEDUP_COSINE_MIN_SCORE`).
2. Deterministic rules ([dedup_helpers.py](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/utils/maintenance/dedup_helpers.py)). One exact normalized-name match resolves the Node. Several matches send it to the model. Otherwise a fuzzy match runs over MinHash and LSH shingles, but only for names that pass an entropy gate (entropy at least 1.5, and either length at least 6 or at least 2 tokens). A fuzzy match needs Jaccard similarity of at least 0.9.
3. One model call for all Nodes still unresolved, with the merged candidate list for all of them.

Prompt inputs: `extracted_nodes` as `{id, name, entity_type, entity_type_description}`, `existing_nodes` as the candidate's attributes plus `{candidate_id, name, entity_types, summary[:120]}`, `episode_content`, `previous_episodes` ([prompts/dedupe_nodes.py#L118](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/prompts/dedupe_nodes.py#L118)). Output ([L26](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/prompts/dedupe_nodes.py#L26)):

```text
NodeResolutions { entity_resolutions: [NodeDuplicate] }
NodeDuplicate   { id: int, name: str, duplicate_candidate_id: int }   // -1 means no match
```

The code ignores out-of-range or repeated ids and treats an unknown `duplicate_candidate_id` as no match.

### Edge extraction and the valid and invalid times

Inputs ([edge_operations.py#L139](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/utils/maintenance/edge_operations.py#L139)): `episode_content`, `nodes` as `{name, entity_types}` (the resolved-name list the model must pick endpoints from), `previous_episodes`, `reference_time` (the Episode's `valid_at`, which is the Reference time in gnomon terms), optional `edge_types` as `{fact_type_name, fact_type_signatures, fact_type_description}`, and custom instructions. Output ([prompts/extract_edges.py#L26](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/prompts/extract_edges.py#L26)):

```text
ExtractedEdges { edges: [Edge] }
Edge { source_entity_name: str, target_entity_name: str,
       relation_type: str (SCREAMING_SNAKE_CASE),
       fact: str (natural-language sentence),
       valid_at: str | null (ISO 8601), invalid_at: str | null (ISO 8601),
       episode_indices: [int] }
```

How relative dates are resolved: the model does it, in the same call. The prompt gives `REFERENCE_TIME` and says it "used to resolve relative time mentions". The datetime rules in the prompt ([L95](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/prompts/extract_edges.py#L95)) say:

- Use ISO 8601 with a `Z` suffix. A bare date means 00:00:00. A bare year means January 1st.
- A present-tense fact gets `valid_at` equal to the reference time.
- A future-tense plan, promise or deadline also gets `valid_at` equal to the reference time, not the future date. The future date stays in the `fact` text.
- A soft deadline ("by March 2026") is never an `invalid_at`. A future `invalid_at` is allowed only when the state ends then ("until December 25, 2024").
- Leave both null when no time is stated or resolvable. Do not infer dates from unrelated events.

Code then adjusts the model's output. `normalize_future_temporal_bounds` ([temporal_edge_utils.py#L39](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/utils/maintenance/temporal_edge_utils.py#L39)) uses regular expressions on the fact text. If the fact has future-tense wording and `valid_at` is empty or later than the reference time, it sets `valid_at` to the reference time. If `invalid_at` is after the reference time, the fact has a deadline word ("by", "before", "no later than"), and it has no end word ("until", "no longer", "expires"), it clears `invalid_at`. A string that fails ISO parsing is skipped with a warning.

Fallback timestamp call: when extraction left both `valid_at` and `invalid_at` empty, `_extract_edge_timestamps` makes a small-model call with only `fact` and `reference_time` ([prompts/extract_edges.py#L260](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/prompts/extract_edges.py#L260)). Output:

```text
EdgeTimestamps { valid_at: str | null, invalid_at: str | null }
```

It runs only for an edge that is new after deduplication. A duplicate keeps the times of the stored edge. A failure is logged and ignored. A batch variant (`BatchEdgeTimestamps`, 20 facts per call, 2 attempts) exists for the combined bulk path.

### Edge deduplication and contradiction detection

One call decides both. Before the call, `resolve_extracted_edges` ([edge_operations.py#L429](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/utils/maintenance/edge_operations.py#L429)):

1. Drops same-endpoint, same-type exact repeats within the batch.
2. Embeds each edge's `fact`.
3. Runs a hybrid search (BM25 plus cosine, fused by reciprocal rank fusion, config `EDGE_HYBRID_SEARCH_RRF`) twice. The first search is restricted to edges with the same two endpoints in either direction, and yields the duplicate candidates. The second is unrestricted within the `group_id`, and yields the contradiction candidates, minus any already in the first list.
4. Skips the model call when a stored edge has the same endpoints and the same normalized fact text.

Prompt inputs ([prompts/dedupe_edges.py#L44](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/prompts/dedupe_edges.py#L44)): `existing_edges` as `{idx, fact}`, `edge_invalidation_candidates` as `{idx, fact}` with the index continuing where the first list ends, and `new_edge` (the fact sentence). The prompt sees only fact text, with no dates. Output ([L25](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/prompts/dedupe_edges.py#L25)):

```text
EdgeDuplicate { duplicate_facts: [int],      // only from the first list
                contradicted_facts: [int] }  // from either list
```

How the model decides a contradiction, per the prompt: mark an old fact contradicted only when the new fact "explicitly makes the old fact false". The prompt says not to infer a contradiction from recency, episode dates, meeting dates, weekly snapshots, or a new event involving the same entities. It also says activity or progress facts do not contradict role, ownership or membership facts. Its examples: "software engineer" then "senior engineer" at the same company is a contradiction and not a duplicate. Two runs on different days are neither.

After the model call, code applies the dates ([edge_operations.py#L762](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/utils/maintenance/edge_operations.py#L762), [L795](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/utils/maintenance/edge_operations.py#L795)). The model only nominates candidates. The code:

- Sets `expired_at = now` on the new edge when it arrives with an `invalid_at`.
- If a nominated candidate has a `valid_at` later than the new edge's `valid_at`, sets the new edge's `invalid_at` to that candidate's `valid_at` and expires the new edge.
- For each nominated candidate, skips it when the candidate's `invalid_at` is at or before the new edge's `valid_at`, or when the new edge's `invalid_at` is at or before the candidate's `valid_at` (the intervals do not overlap). Otherwise, if the candidate's `valid_at` is earlier than the new edge's `valid_at`, it sets the candidate's `invalid_at` to the new edge's `valid_at` and sets its `expired_at` to now.
- A candidate with no `valid_at`, or a new edge with no `valid_at`, is never invalidated by this rule.

Graphiti's stored fields are `valid_at`, `invalid_at`, `expired_at` and `created_at` on the edge ([edges.py#L228](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/edges.py#L228)). It overwrites those fields on the stored edge. There are no Versions.

### Attribute extraction and summary update

Attribute extraction is per Node and only for Nodes whose entity class declares fields ([node_operations.py#L1107](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/utils/maintenance/node_operations.py#L1107)). Inputs: `node` as `{name, entity_types, attributes}` (not the summary), `episode_content`, `previous_episodes`. The response model is the caller's own Pydantic class for that entity type, so the output schema is user-defined. The prompt ([prompts/extract_nodes.py#L384](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/prompts/extract_nodes.py#L384)) allows only values copied or directly normalized from the messages, or the existing value, or null. It forbids reasoning text in fields and inference from the Node name. The merge is an overlay: fields the model omits keep prior values. Edge attributes work the same way, with `merge_mode='replace'` and the fact text in place of messages.

Summary update is batched, up to 30 Nodes per call (`MAX_NODES = 30`) ([node_operations.py#L1166](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/utils/maintenance/node_operations.py#L1166)). Before any call, a Node whose new text is short skips the model: the sentences of its newly created edges are appended to the existing summary, and if the result is at most 2000 characters (`MAX_SUMMARY_CHARS * 2`), that result is the summary. Only longer ones go to the model. Prompt inputs ([prompts/extract_nodes.py#L513](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/prompts/extract_nodes.py#L513)): `entities` as `{name, summary, entity_types, attributes}`, `episode_content`, `previous_episodes`, and optional type descriptions. The output:

```text
SummarizedEntities { summaries: [SummarizedEntity] }
SummarizedEntity   { name: str, summary: str }
```

The code matches results to Nodes by lowercased name, then truncates at a sentence boundary to 1000 characters (`MAX_SUMMARY_CHARS`). The prompt limit is the same 1000 characters. A second prompt, `extract_entity_summaries_from_episodes`, is used when `skip_fact_appending` is set. It asks for 2 to 6 dense third-person sentences and says to return the existing summary unchanged when the new Episodes add nothing durable. `add_episode` does not set that flag.

### Community nodes

Algorithm ([community_operations.py#L92](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/utils/maintenance/community_operations.py#L92)): a hand-written label propagation in Python, not a library.

- Build, per `group_id`, an adjacency projection from `RELATES_TO` edges: for each Node, its neighbors with the count of edges between them.
- Start with every Node in its own community (an integer per Node).
- Update all Nodes synchronously in each round, reading the previous round's labels. Each Node adopts the community with the largest summed edge count among its neighbors. Ties go to the larger community id, because the candidates are sorted by `(count, community)` descending.
- If the winning count is 1 or less, the Node takes the larger of its candidate and its current id.
- Stop when a round changes nothing. There is no round cap and no random seed. Output order follows dict insertion order, so the result depends on the order of the Node list.

Summary: each community is summarized by pairwise merging ([L197](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/utils/maintenance/community_operations.py#L197)). It halves the list of Node summaries each round by calling `summarize_pair` on (left, right) with the first half against the second half, carrying an odd one over, until one summary remains. Then one more call writes a one-sentence `description`, which becomes the community name. Output schemas ([prompts/summarize_nodes.py#L24](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/prompts/summarize_nodes.py#L24)):

```text
Summary            { summary: str }       // under 1000 characters
SummaryDescription { description: str }   // one sentence
```

Both calls use the main model. The community node gets a name embedding.

When communities are rebuilt or updated:

- `Graphiti.build_communities` ([graphiti.py#L2196](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/graphiti.py#L2196)) first deletes all community nodes for the requested groups, then clusters and summarizes from scratch. It is a full rebuild, run only when the caller invokes it.
- `add_episode(update_communities=True)` runs an incremental step for each resolved Node ([community_operations.py#L327](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/utils/maintenance/community_operations.py#L327), [L389](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/utils/maintenance/community_operations.py#L389)). It finds the Node's existing community. If the Node has none, it takes the community that most of its neighbors belong to (a count of neighbor-community rows, highest wins). If no neighbor has one, nothing happens. Then it summarizes (Node summary, community summary) as a pair, regenerates the name, re-embeds, and saves. It adds a membership edge for a newly joined Node. It never splits or merges communities.
- The concurrent-ingestion path rejects `update_communities=True` and tells the caller to run `build_communities` afterwards ([graphiti.py#L1690](https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/graphiti.py#L1690)).

## 2. Microsoft GraphRAG

Pinned at [graphrag 34b2b67](https://github.com/microsoft/graphrag/tree/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857) (v3.3.0 plus 2 commits). Paths are under `packages/graphrag/graphrag/`.

### Community detection

- Algorithm: hierarchical Leiden, from the Rust package `graspologic-native` through Python bindings. [graphs/hierarchical_leiden.py](https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag/graphrag/graphs/hierarchical_leiden.py) calls `gn.hierarchical_leiden(edges, max_cluster_size, seed, starting_communities=None, resolution=1.0, randomness=0.001, use_modularity=True, iterations=1)`.
- Version pin: `graspologic-native>=1.2,<1.3`, with a comment that 1.3.x "changes Leiden clustering output (community counts/levels)" ([pyproject.toml](https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag/pyproject.toml)). The graspologic-native repo is MIT licensed, with latest release v1.3.1 on 2026-06-14, and its Rust crate is named `network_partitions` 0.3.0 in [that repo](https://github.com/microsoft/graspologic-native/tree/773181488b4e5b81925e302b972b294fd51f61a4/packages/network_partitions). That crate is not published on crates.io.
- Input preparation ([index/operations/cluster_graph.py](https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag/graphrag/index/operations/cluster_graph.py)): treat edges as undirected, put each edge's endpoints in sorted order, drop duplicate pairs keeping the last, optionally keep only the largest connected component (`use_lcc`, default true), and sort the edge list before the call so the input order is stable.
- Defaults ([config/defaults.py#L71](https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag/graphrag/config/defaults.py#L71)): `max_cluster_size = 10`, `use_lcc = True`, `seed = 0xDEADBEEF`.
- Hierarchy: the algorithm re-splits any community larger than `max_cluster_size`, producing levels from 0 (coarsest) upward. Each row records `level`, `community`, `parent` (-1 at the root) and the member titles. The `communities` table has `children`, `size` and `period` columns ([create_communities.py](https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag/graphrag/index/workflows/create_communities.py)). Members and relationship ids are recorded for each community at each level, so a Node appears once per level.

### Community report prompt

[prompts/index/community_report.py](https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag/graphrag/prompts/index/community_report.py). The input is a text block with CSV-like tables of the community's entities (`human_readable_id,title,description`), relationships (`human_readable_id,source,target,description`) and optional claims, plus `{max_report_length}` (default 2000 words, `max_input_length` default 8000 tokens, [defaults.py#L82](https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag/graphrag/config/defaults.py#L82)). The prompt text specifies this JSON:

```text
{ "title": str, "summary": str, "rating": float (0 to 10, "impact severity"),
  "rating_explanation": str,
  "findings": [ { "summary": str, "explanation": str } ] }   // 5 to 10 findings
```

The code enforces it as a Pydantic model passed as `response_format` ([community_reports_extractor.py#L25](https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag/graphrag/index/operations/summarize_communities/community_reports_extractor.py#L25)): `CommunityReportResponse { title, summary, findings: [FindingModel{summary, explanation}], rating: float, rating_explanation }`. The prompt also requires grounding markers in the text, as `[Data: Entities (5, 7); Relationships (23)]`, with at most 5 ids per reference and "+more" after that. Levels are processed from the deepest level up to level 0, so the context for a parent can use the reports of its sub-communities when the raw text does not fit ([summarize_communities.py](https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag/graphrag/index/operations/summarize_communities/summarize_communities.py), `get_levels` sorts descending).

### Updates

`graphrag index --method ... --update` uses a separate workflow list, `_update_workflows`, run after the standard workflows ([index/workflows/factory.py#L52](https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag/graphrag/index/workflows/factory.py#L52)). It is incremental over input documents but not over the graph:

- It loads only documents whose `title` is not in the previous output (`get_delta_docs`). It also computes deleted documents, but `load_update_documents` returns only the new ones, so a deletion is not applied there ([load_update_documents.py](https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag/graphrag/index/workflows/load_update_documents.py)).
- It runs the whole standard pipeline, including Leiden and community reports, on the new documents only, into a delta output.
- It merges the delta into the previous output ([update/communities.py](https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag/graphrag/index/update/communities.py)). Delta community ids are shifted up by the old maximum id, so old communities are kept as they are. The delta's communities are appended beside them. Delta communities are never merged with old ones, and old communities are not re-clustered.
- Entities are merged by title, and entity descriptions are re-summarized through a model call ([update_entities_relationships.py](https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag/graphrag/index/workflows/update_entities_relationships.py)). Community reports are merged with the id mapping, with no regeneration of old reports ([update_community_reports.py](https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag/graphrag/index/workflows/update_community_reports.py)).
- A full rebuild is a normal `index` run without `--update`.

## 3. Community detection crates for a Rust process

All figures from crates.io and the crate tarballs on 2026-11-22. "Initial partition" means the caller can pass a starting assignment. None of the crates below offers per-node dirty tracking, so none updates only the part of a graph that changed.

| Crate | Version | Updated | Licence | Algorithms | Seed | Initial partition |
|---|---|---|---|---|---|---|
| [leiden-rs](https://crates.io/crates/leiden-rs) | 0.8.1 | 2026-05-15 | MIT OR Apache-2.0 | Leiden, label propagation, Infomap, fluid communities | `Option<u64>` | Leiden only |
| [single-clustering](https://crates.io/crates/single-clustering) | 0.7.0 | 2026-08-04 | BSD-3-Clause (LICENSE.md; crates.io metadata says "non-standard") | Leiden, Louvain (`refine: false`) | `Option<u64>` | none found |
| [fa-leiden-cd](https://crates.io/crates/fa-leiden-cd) | 0.1.0 | 2025-08-27 | MIT (LICENSE file; crates.io metadata says "non-standard") | Leiden | none | none found |
| [clust-libreville-rs](https://crates.io/crates/clust-libreville-rs) | 0.1.1 | 2026-10-26 | MIT (GitHub reports NOASSERTION) | Leiden, parallel | yes | none found |
| [rune-leiden](https://crates.io/crates/rune-leiden) | 0.3.0 | 2026-11-13 | MIT | Leiden, Louvain | `random_seed` | none found |
| [scirs2-graph](https://crates.io/crates/scirs2-graph) | 0.6.5 | 2026-07-31 | Apache-2.0 | label propagation among many | mixed (see below) | none found |

Details by crate:

- leiden-rs: 92,579 downloads, created 2026-04-12, 5 versions. The `repository` field points to [gitcode.com/lileeei/leiden-rs](https://gitcode.com/lileeei/leiden-rs), not GitHub. The packaged commit is `fad950e`. It offers `Leiden::run_with_initial_partition(&GraphData, Partition)`, documented as warm start for "incremental refinement after minor graph changes". It renumbers the partition and rejects a size mismatch. `LeidenConfig` has `seed`, `resolution`, `quality` (modularity, CPM, RBConfiguration, RBER), `max_comm_size`, `max_iterations`, and `run_hierarchical` returns per-level membership. README: "When a seed is provided, all random decisions are reproducible across runs." Parallel local moving uses rayon and graph coloring above thresholds (2000 edge slots and 100 nodes by default). The README says the choice of code path depends only on graph structure, "same graph → same code path → same result", and a test named `test_large_random_graph_determinism` exists in `src/leiden.rs`. Its label propagation ([src/label_propagation.rs](https://crates.io/crates/leiden-rs/0.8.1)) is synchronous, takes `seed` and `max_iterations`, uses a seeded `StdRng`, has a `deterministic_with_seed` test, and has no initial-labels input.
- single-clustering: 14,753 downloads. Source [SingleRust/single-clustering at 7bf25cc](https://github.com/SingleRust/single-clustering/tree/7bf25cc8045b1111621bca9dd10c0e1290552176), last push 2026-08-24, 8 stars. 0.7.0 is described as a breaking rewrite of the Leiden core. README: "a fixed seed gives bit-for-bit identical results". `LeidenConfig` has `objective` (`Rb { resolution }` or other), `seed`, `n_iterations`, `max_levels`, `tolerance`, `refine`. Passes continue from the previous result within one call, but no caller-supplied starting partition appeared in the source.
- fa-leiden-cd: 12,578 downloads, one version, last push 2025-08-27, 1 star. Its source contains a step that "randomly choose[s] a node to move" when modularity stalls, and the crate exposes no seed. Dependencies are `boxcar`, `hashbrown`, `rayon`. This note found no documentation of determinism.
- clust-libreville-rs: created 2026-10-20, 41 downloads. README: seeded, parallel, and "the same seed gives the same labels at any thread count", including across x86_64 Linux and Apple M1. It is also described as within 1.5 times of GVE-Leiden's time, per the README's own footnote. It has an optional GPU module in the same package.
- rune-leiden: created 2026-05-31, 112 downloads. README: "deterministic for a given seed". Both Leiden and Louvain return the partition at every aggregation level. Repository [alexile/runes](https://github.com/alexile/runes). Its sibling `rune-labelprop` 0.1.2 is semi-supervised label propagation (seed labels are clamped), not community detection.
- scirs2-graph: 17,334 downloads, 39 versions, part of the SciRS2 project. The label propagation in `algorithms/community/label_propagation.rs` uses `scirs2_core::random::rng()` with no seed parameter, and the public function there is marked deprecated in favor of `label_propagation_result`. A different function in `community/mod.rs` has the signature `label_propagation(adj: &Array2<f64>, max_iter, seed: u64)` and takes a dense adjacency matrix, with labels initialized to node indices.
- label-propagation 0.1.1 (2021) is graph-based semi-supervised learning (LGC and CAMLP), not community detection.
- Other crates that appear in a crates.io search for "leiden" or "community detection" and were not examined in source: sdivi-detection 0.2.52 (Apache-2.0), infomap-rs 0.1.1, xgraph 3.0.0, rust-igraph 0.7.0 (GPL-2.0-or-later), mnem-graphrag 0.1.7.

No crate found offers a mode that takes a previous partition and a list of changed edges and touches only the affected Nodes. The closest feature is leiden-rs's `run_with_initial_partition`, which still runs a full pass over the whole graph.

## 4. Reranker endpoints

Read on 2026-11-22. None of these is OpenAI-compatible. OpenAI has no rerank endpoint. The vLLM source says so itself: "To indicate that the rerank API is not part of the standard OpenAI API, we have located it at `/rerank`... (Note: Conforms to JinaAI rerank API)" ([api_router.py](https://github.com/vllm-project/vllm/blob/118d12f728c4f0bff4f088eb197a1404a7302038/vllm/entrypoints/pooling/scoring/api_router.py)). The request and response shapes of Jina, vLLM and Cohere v2 are close to each other but not identical, and TEI is different.

| Service | Endpoint | Query and documents fields | Result item |
|---|---|---|---|
| Cohere Rerank v2 | `POST https://api.cohere.com/v2/rerank` | `model`, `query`, `documents` (strings), `top_n`, `max_tokens_per_doc` (default 4096), `priority`, `relevant_spans` (v5 and newer) | `{index, relevance_score}` in `results`, score in [0, 1], plus `id`, `meta` |
| Voyage | `POST https://api.voyageai.com/v1/rerank` | `model`, `query`, `documents`, `top_k`, `truncation` (default true) | `{index, document, relevance_score}` in `results`, plus `total_tokens` |
| Jina | `POST https://api.jina.ai/v1/rerank` | `model`, `query`, `documents` (strings or `{text}` objects), `top_n`, `return_documents` (default true), `max_doc_length` (v3 and v3.5 only, 1 to 8192), `return_embeddings` | `{index, relevance_score, document?, embedding?}` in `results`, plus `model`, `object: "list"`, `usage.total_tokens` |
| Hugging Face TEI | `POST /rerank` | `query`, `texts`, `raw_scores` (default false), `return_text` (default false), `truncate` (nullable), `truncation_direction` (Left or Right, default Right) | a bare JSON array of `{index, score, text?}` |
| vLLM | `POST /rerank`, `/v1/rerank`, `/v2/rerank` | `model`, `query`, `documents`, `top_n` (0 means all), `max_tokens_per_query`, `max_tokens_per_doc` (0 means off), `instruction`, plus pooling fields | `{id, model, usage{prompt_tokens, total_tokens}, results:[{index, document{text}, relevance_score}]}` |

Sources: [Cohere rerank reference](https://docs.cohere.com/reference/rerank), [Voyage reranker](https://docs.voyageai.com/docs/reranker), [Jina OpenAPI](https://api.jina.ai/openapi.json), [TEI OpenAPI at ea547d3](https://github.com/huggingface/text-embeddings-inference/blob/ea547d36dc71a03a60f9cb456c3e3eff653655c5/docs/openapi.json), [vLLM protocol.py at 118d12f](https://github.com/vllm-project/vllm/blob/118d12f728c4f0bff4f088eb197a1404a7302038/vllm/entrypoints/pooling/scoring/protocol.py).

Limits:

| Service | Max documents per call | Per-document limit | Total input limit |
|---|---|---|---|
| Cohere | no hard cap stated. The reference advises against "more than 1,000 documents in a single request" | the query plus one document must fit the model context: 32,768 tokens for rerank-v5.0 and v4.0 models, 4k for v3.5 and v3 models. A longer document is "automatically chunked and processed in multiple inferences" ([rerank docs](https://docs.cohere.com/docs/rerank)). `max_tokens_per_doc` default 4096 truncates | not stated |
| Voyage | 1,000 | query plus any one document: 32,000 tokens for rerank-3, rerank-3-lite, rerank-2.5, rerank-2.5-lite. 16,000 for rerank-2. 8,000 for rerank-2-lite and rerank-1. 4,000 for rerank-lite-1. Query alone: 8,000, 4,000, 2,000, 2,000 or 1,000 tokens respectively | query tokens times the document count plus all document tokens must not exceed 600K (rerank-3 and later 2.x models), or 300K (rerank-1, rerank-lite-1) |
| Jina | "no hard limit on the number of documents per request" (pricing page); the OpenAPI schema states only a minimum of 1 | `max_doc_length` 1 to 8192 for v3 and v3.5 (default 2048 and 8192). Model context: 131,072 tokens for jina-reranker-v3.5 (query plus all documents, with truncation), 10,000 for m0, 1,024 for v2 base (with automatic chunking). The page warns the hosted API "may enforce a lower limit" and gives no number | for v3.5, the 131,072 context covers the query and all documents together. An over-limit input returns HTTP 400 `INPUT_TOKEN_LIMIT_EXCEEDED`. Rate limits are per key: free 500 RPM and 1M TPM, paid 500 RPM and 10M TPM, premium 5,000 RPM and 100M TPM |
| TEI | `--max-client-batch-size`, default 32 ("the maximum number of inputs that a client can send in a single request") ([main.rs](https://github.com/huggingface/text-embeddings-inference/blob/ea547d36dc71a03a60f9cb456c3e3eff653655c5/router/src/main.rs)). A 413 "Batch size error" is documented | the model's maximum sequence length. `--auto-truncate` defaults to true, and `truncate` per request overrides it | `--max-batch-tokens` default 16384 (server-wide batch, a tuning value). `--payload-limit` default 2,000,000 bytes |
| vLLM | not set in the protocol file | `max_tokens_per_doc` and `max_tokens_per_query` default to 0, meaning no per-part truncation. Otherwise the model's maximum length applies to the combined pair | not set in the protocol file. The server's `--max-model-len` applies |

## 5. Embedder endpoints

Read on 2026-11-22.

| Service | Task parameter | Allowed values | Max inputs per request | Max tokens |
|---|---|---|---|---|
| OpenAI `POST /v1/embeddings` | none. The fields are `input`, `model`, `dimensions`, `encoding_format` (float or base64), `user` | not applicable | 2048 items in the `input` array | 8192 tokens per input, 300,000 tokens total per request |
| Cohere `POST /v2/embed` | `input_type`, required for v3 and newer models | `search_document`, `search_query`, `classification`, `clustering`, `image` | 96 `texts` (or 96 `inputs`) per call | no per-input number on the page. `truncate` is `NONE`, `START` or `END` (default `END`), and `max_tokens` caps tokens embedded per input |
| Voyage `POST /v1/embeddings` | `input_type`, default null | `null`, `query`, `document` | 1,000 items in `input` | total per request by model: 1M (voyage-4-lite, voyage-3.5-lite), 320K (voyage-4, voyage-3.5, voyage-2), 120K (voyage-4-large, voyage-3-large, voyage-code-3, voyage-finance-2, voyage-law-2). `truncation` default true |
| Gemini | `task_type` (`taskType` in REST), only for `gemini-embedding-001` | `TASK_TYPE_UNSPECIFIED`, `RETRIEVAL_QUERY`, `RETRIEVAL_DOCUMENT`, `SEMANTIC_SIMILARITY`, `CLASSIFICATION`, `CLUSTERING`, `QUESTION_ANSWERING`, `FACT_VERIFICATION`, `CODE_RETRIEVAL_QUERY` | not stated on the pages read | 2,048 tokens per input for `gemini-embedding-001`, 8,192 for `gemini-embedding-2` |
| Jina `POST /v1/embeddings` | `task`, for v5, v4 and v3 models | `retrieval.query`, `retrieval.passage`, `text-matching`, `classification`, `separation` | "no hard limit on the number of items per request" (a PDF must be one per request) | per input: 32,768 for jina-embeddings-v5-text-small, v5-omni-small, v4 and the code-embeddings models. 8,192 for v5-text-nano, v5-omni-nano, v3, clip-v2, colbert-v2. No per-request total is stated. Rate limits: free 100 RPM and 100K TPM, paid 500 RPM and 2M TPM |

Notes:

- Gemini changed shape between models. For `gemini-embedding-2`, the page says "You cannot use the `task_type` field" and gives text prefixes instead: `task: search result | query: {content}` for queries, and `title: {title or none} | text: {content}` for documents. Symmetric tasks use the form `task: classification | query: {content}`. The Gemini page names the Batch API at half price for throughput and gives no batch input count.
- The Gemini API reference lists the `taskType` enum under `EmbedContentConfig`. It does not say outright whether `batchEmbedContents` accepts it, though each inlined request carries an `embedContentConfig` field.
- Cohere's `embed` also takes `embedding_types` (float, int8, uint8, binary, ubinary, base64) and `output_dimension` (embed-v4 and newer).
- Voyage's `output_dimension` options for the 4 and 3.5 families are 2048, 1024 (default), 512 and 256.

Sources: [OpenAI create embeddings](https://developers.openai.com/api/reference/resources/embeddings/methods/create), [Cohere embed reference](https://docs.cohere.com/reference/embed), [Voyage embeddings API](https://docs.voyageai.com/reference/embeddings-api), [Gemini embeddings guide](https://aistudio.google.com/docs/embeddings), [Gemini API reference](https://aistudio.google.com/api-reference/embeddings), [Jina embeddings](https://jina.ai/embeddings/).

## 6. Structured output

### Anthropic

From the [structured outputs page](https://platform.claude.com/docs/en/build-with-claude/structured-outputs.md) read on 2026-11-22, and the claude-api skill reference (bundled skill 2.1.330).

Request shape: `output_config: {format: {type: "json_schema", schema: ...}}` on the Messages API. The old `output_format` parameter is deprecated, and it needs the beta header `structured-outputs-2025-11-13` or the API returns 400. Strict tool use is a separate switch, `strict: true` on a tool.

Supported: the types object, array, string, integer, number, boolean and null. Also `enum` and `const` (strings, numbers, booleans or nulls only), `anyOf` and `allOf` with restrictions, `$ref`, `$defs`, `definitions`, `default`, `required`, `additionalProperties` set to `false`, and array `minItems` of 0 or 1 only. String formats: `date-time`, `time`, `date`, `duration`, `email`, `hostname`, `uri`, `ipv4`, `ipv6`, `uuid`. Regular expression `pattern` is supported with limits: no backreferences, no lookahead or lookbehind, no `\b`, and no complex `{n,m}` quantifiers.

Not supported: recursive schemas, `oneOf`, `not`, numerical constraints (`minimum`, `maximum`, `multipleOf`), string constraints (`minLength`, `maxLength`), array constraints beyond `minItems` 0 or 1, `additionalProperties` other than `false`, external `$ref`, complex types inside `enum`, `anyOf` in the same schema object as `type` or `properties`, and `allOf` with conflicting schemas.

Whether bounds are rejected: yes for a raw schema. The page says "If you use an unsupported feature, you'll receive a 400 error with details." The Python, TypeScript and some other SDK helpers instead remove the unsupported constraints from the schema they send, add them to the descriptions, and validate the returned object locally.

Complexity limits that apply per request across all strict schemas: 20 strict tools, 24 optional parameters in total (each property not in `required`), and 16 parameters that use `anyOf` or a type array. There are also internal grammar-size limits that return 400 "Schema is too complex for compilation", and a 180-second compile timeout. The first use of a schema has extra latency, and the compiled grammar is cached for 24 hours from last use. Changing the schema structure, or the tool set when strict tools are in use, invalidates that cache. Changing only `name` or `description` does not.

How a violation or refusal appears:

- Schema violations are not expected on a normal completion. The page says output is produced by constrained decoding.
- A refusal returns HTTP 200 with `stop_reason: "refusal"` and a `stop_details` object. The output "might not match your schema because the refusal message takes precedence". The tokens are billed. The page notes that a property that asks for reasoning can trigger a `reasoning_extraction` refusal.
- `stop_reason: "max_tokens"` means the JSON may be cut off and invalid.
- Capitalization of `enum` and `const` string values is not guaranteed. The page's example is a value whose first letter after a space comes back capitalized, with no error and no special `stop_reason`.
- Property order in the output puts required properties first in schema order, then optional ones.
- Grammar applies only to the model's direct output, not to thinking, tool calls or tool results.

Compatibility: Message Batches API works with structured output ("Process structured outputs at scale with a 50 percent discount"), as do streaming and token counting. Citations are incompatible (400), and so is message prefilling. Managed Agents is listed as "Not available". In an organization with customer-managed encryption keys, structured outputs are unavailable for the Fable and Mythos model families, per the skill reference. Zero data retention is eligible, except for Covered Models, and only the schema is cached.

### OpenAI-compatible structured output

From the [OpenAI structured outputs guide](https://developers.openai.com/api/docs/guides/structured-outputs) read on 2026-11-22. Request shape on Responses is `text.format = {type: "json_schema", strict: true, schema}`. On Chat Completions it is `response_format: {type: "json_schema", json_schema: {strict: true, ...}}`.

Strict-mode rules, applied to the keyword profile in question (`type`, `properties`, `required`, `enum`, `const`, `items`, `anyOf`, and `format` limited to `date`, `date-time`, `uuid`):

- The root must be an object and must not be `anyOf`.
- All fields must be listed in `required`, and `additionalProperties` must be `false` on each object. Optional values are written as a union with null (the guide's examples use `"type": ["string", "null"]`). So `required` in a source schema that has optional properties cannot be passed through unchanged.
- Supported types: String, Number, Boolean, Integer, Object, Array, Enum, `anyOf`.
- `format` supported values: `date-time`, `time`, `date`, `duration`, `email`, `hostname`, `ipv4`, `ipv6`, `uuid`. So `date`, `date-time` and `uuid` are all inside the supported set.
- `const` is not listed among supported types or keywords in the supported-schemas section. It appears only in the string-length limit sentence. This note found no explicit statement that it is accepted or rejected, so its status is unconfirmed.
- `items` is the array element schema. `minItems` and `maxItems` are listed as supported array properties. Numeric `minimum`, `maximum`, `multipleOf`, `exclusiveMinimum` and `exclusiveMaximum` are listed as supported number properties, and `pattern` as a supported string property. This differs from Anthropic, which rejects numeric and length bounds.
- Limits: at most 5,000 object properties in total, at most 10 levels of nesting, at most 120,000 characters in total across property names, definition names, enum values and const values, and at most 1,000 enum values in total. A single enum with more than 250 string values has a 15,000-character total cap.
- The keys come back "in the same order as the ordering of keys in the schema".
- A refusal appears as a `refusal` item (Responses) or field (Chat Completions), and "a refusal does not necessarily follow the schema you have supplied". An incomplete response is also possible. The guide notes extra latency on the first request with a new schema.
- The guide's examples use `$ref: "#"` for recursion, so recursive schemas are supported there but not in Anthropic's.

## 7. OpenRouter embeddings and rerank

Read on 2026-11-22 from the OpenRouter API reference pages for [embeddings](https://openrouter.ai/docs/api/api-reference/embeddings/submit-an-embedding-request) and [rerank](https://openrouter.ai/docs/api/api-reference/rerank/submit-a-rerank-request), and the [Cohere rerank model page](https://openrouter.ai/cohere/rerank-v3.5). OpenRouter is a router, so the limits below come from whichever upstream provider serves the model.

### Embeddings

- Endpoint: `POST https://openrouter.ai/api/v1/embeddings`. The shape is OpenAI's: `model`, `input`, `dimensions` (integer, at least 1), `encoding_format` (`float` or `base64`), plus OpenRouter's `provider` routing preferences.
- `input` is a string, a list of strings, token arrays, `string[][]` (document chunks, valid only with a document input type), or objects with `content` parts for multimodal models.
- `input_type` is a free string. The page's example values are `search_query` and `search_document`. The Perplexity model pages say to send `query` and `document` instead. So the allowed values depend on the model, and OpenRouter does not define one list.
- The response is OpenAI's: `data[]` with `embedding`, `index` and `object`, then `model`, `object: "list"` and `usage` (`prompt_tokens`, `total_tokens`, `cost`).
- Documented error codes include 413 (payload too large) and 429. The page gives no per-request input count or token limit.
- OpenRouter lists embedding models from OpenAI, Cohere (for example `cohere/embed-5-fast`), Google (`google/gemini-embedding-2-preview`) and Perplexity.

### Rerank

- Endpoint: `POST https://openrouter.ai/api/v1/rerank`. The request has `model`, `query`, `documents` (strings, or objects with `text` and/or `image` for multimodal models) and `top_n`, which appears in every example but whose description this note did not find. It also has `provider`, `session_id`, `trace` and `user`.
- The response has `id`, `model`, `provider`, `results` and `usage`. Each result is `{index, relevance_score, document: {text}}`, sorted by relevance. `usage` has `search_units`, `total_tokens` and `cost`.
- Models seen: `cohere/rerank-v3.5` and `mongodb/rerank-2.5`.
- Documented error codes include 413 and 429. The page states no limit on the number of documents or their length. Those limits come from the upstream model (section 4).
- The request and response shape matches Cohere v2 rerank (section 4). It differs from Voyage (`top_k`) and TEI (`texts`, bare array).

### Not checked

- Whether OpenRouter's rerank accepts Cohere's `max_tokens_per_doc` or any truncation field.
- OpenRouter's structured-output behaviour for chat models, including whether it strips numeric and length bounds for Anthropic models. Prototype 3 used OpenRouter chat completions, but no run sent a schema with bounds.

## Where the sources are thin or conflict

- Graphiti: the claim that "Edge extraction uses the main model" follows from the default `ModelSize.medium`. Which actual model each size maps to depends on the caller's client config, and in the OpenAI-style client both defaults are the same model name.
- Graphiti's label propagation has quirks (no round cap, a second branch that repeats the same line of code, and a tie rule that follows integer id order). The note reports what the code does, and does not call any of it a bug.
- GraphRAG's update behavior for deleted documents: the code computes them but the loader does not use them. No document in the repo says whether that is intended.
- Rust crates: only leiden-rs documents an initial partition. Determinism claims for leiden-rs, single-clustering, clust-libreville-rs and rune-leiden come from each crate's own README and tests. This note did not run them. The leiden-rs repository is on gitcode.com, not GitHub, so its activity could not be checked with the GitHub API. crates.io marks the licence of single-clustering and fa-leiden-cd as "non-standard" while their LICENSE files read BSD 3-Clause and MIT. clust-libreville-rs is three weeks old and has 41 downloads.
- Reranker limits: Cohere states no hard document cap. Jina's documentation names no document cap and no per-document cap for v3.5, and warns that the hosted API may enforce a lower limit without saying what. vLLM's protocol file sets no document cap. TEI's document cap is a server option, not an API constant. The Cohere page names `rerank-v5.0-pro`, and the Voyage page lists `rerank-3` as the newest model in its table, while its model list elsewhere includes `rerank-2.5`.
- Embedder limits: Gemini's batch size and Cohere's per-input token limit are not stated on the pages read. Jina's per-request total is not stated.
- Vendor pages were read through a page-to-text tool that sometimes cut a page at 100,000 characters. The OpenAI and Anthropic structured output pages were read in two parts. OpenAI's supported-schemas section is the second part. Quoted limits were copied from that read, not from a published version number.
- OpenAI `const`: not confirmed either way (see section 6).

## Sources

### github.com/getzep/graphiti

- https://github.com/getzep/graphiti/tree/9b8a4630092084734b080f2e6f409c2c23bdcc3a
- https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/graphiti.py#L1586
- https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/graphiti.py#L2196
- https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/graphiti_types.py#L125
- https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/edges.py#L228
- https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/llm_client/config.py
- https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/llm_client/openai_base_client.py#L114
- https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/prompts/dedupe_edges.py#L25
- https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/prompts/dedupe_nodes.py#L26
- https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/prompts/extract_edges.py#L26
- https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/prompts/extract_nodes.py#L29
- https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/prompts/extract_nodes_and_edges.py
- https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/prompts/summarize_nodes.py#L24
- https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/search/search_utils.py#L64
- https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/utils/bulk_utils.py#L362
- https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/utils/maintenance/community_operations.py#L92
- https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/utils/maintenance/dedup_helpers.py
- https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/utils/maintenance/edge_operations.py#L139
- https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/utils/maintenance/node_operations.py#L76
- https://github.com/getzep/graphiti/blob/9b8a4630092084734b080f2e6f409c2c23bdcc3a/graphiti_core/utils/maintenance/temporal_edge_utils.py#L39

### github.com/microsoft/graphrag

- https://github.com/microsoft/graphrag/tree/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857
- https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag/pyproject.toml
- https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag/graphrag/config/defaults.py#L71
- https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag/graphrag/graphs/hierarchical_leiden.py
- https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag/graphrag/index/operations/cluster_graph.py
- https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag/graphrag/index/operations/summarize_communities/community_reports_extractor.py#L25
- https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag/graphrag/index/operations/summarize_communities/summarize_communities.py
- https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag/graphrag/index/update/communities.py
- https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag/graphrag/index/workflows/create_communities.py
- https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag/graphrag/index/workflows/factory.py#L52
- https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag/graphrag/index/workflows/load_update_documents.py
- https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag/graphrag/index/workflows/update_community_reports.py
- https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag/graphrag/index/workflows/update_entities_relationships.py
- https://github.com/microsoft/graphrag/blob/34b2b67ae4601ad0b47bcd179fbe3f2a23aa0857/packages/graphrag/graphrag/prompts/index/community_report.py

### github.com/microsoft/graspologic-native

- https://github.com/microsoft/graspologic-native/tree/773181488b4e5b81925e302b972b294fd51f61a4/packages/network_partitions

### github.com/vllm-project/vllm

- https://github.com/vllm-project/vllm/blob/118d12f728c4f0bff4f088eb197a1404a7302038/vllm/entrypoints/pooling/scoring/api_router.py
- https://github.com/vllm-project/vllm/blob/118d12f728c4f0bff4f088eb197a1404a7302038/vllm/entrypoints/pooling/scoring/protocol.py

### github.com/huggingface/text-embeddings-inference

- https://github.com/huggingface/text-embeddings-inference/blob/ea547d36dc71a03a60f9cb456c3e3eff653655c5/docs/openapi.json
- https://github.com/huggingface/text-embeddings-inference/blob/ea547d36dc71a03a60f9cb456c3e3eff653655c5/router/src/main.rs

### crates.io and crate repositories

- https://crates.io/crates/leiden-rs
- https://crates.io/crates/single-clustering
- https://github.com/SingleRust/single-clustering/tree/7bf25cc8045b1111621bca9dd10c0e1290552176
- https://crates.io/crates/fa-leiden-cd
- https://github.com/fixed-ai/fa-leiden-cd
- https://crates.io/crates/clust-libreville-rs
- https://github.com/btraven00/clust-libreville-rs/tree/4f19ef06867318d5cd6eae7dda87890f58dcf812
- https://crates.io/crates/rune-leiden
- https://crates.io/crates/rune-labelprop
- https://github.com/alexile/runes
- https://crates.io/crates/scirs2-graph
- https://crates.io/crates/label-propagation

### Vendor documentation

- https://docs.cohere.com/reference/rerank
- https://docs.cohere.com/docs/rerank
- https://docs.cohere.com/reference/embed
- https://docs.voyageai.com/docs/reranker
- https://docs.voyageai.com/reference/embeddings-api
- https://api.jina.ai/openapi.json
- https://jina.ai/reranker/
- https://jina.ai/embeddings/
- https://developers.openai.com/api/reference/resources/embeddings/methods/create
- https://developers.openai.com/api/docs/guides/structured-outputs
- https://aistudio.google.com/docs/embeddings
- https://aistudio.google.com/api-reference/embeddings
- https://platform.claude.com/docs/en/build-with-claude/structured-outputs.md
