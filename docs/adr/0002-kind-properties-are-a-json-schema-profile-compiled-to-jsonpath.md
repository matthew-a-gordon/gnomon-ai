---
status: accepted
---

# Kind property schemas are a JSON Schema profile compiled to jsonpath

A Kind declares its properties in a profile of JSON Schema 2020-12: `type`, `properties`, `required`, `enum`, `const`, `items`, minimum and maximum, length bounds, `anyOf`, and `format` limited to `date`, `date-time` and `uuid`. Registering a Kind rejects any other keyword, flattens the Kind with its ancestors, and compiles one jsonpath rule per property, stored as data with that Kind definition. The Store's write functions evaluate the rules with `jsonb_path_match`, so every host accepts or rejects the same write and one conformance suite covers it. We chose JSON Schema because the Pipeline sends the same document to structured-output model APIs and clients generate types from it with standard tools.

## Considered options

- pg_jsonschema. Hosted Supabase ships it, but DigitalOcean managed Postgres lists it for neither edition. The same write would pass on one host and fail on another. It stays an optional extra a deployment may enable, never part of the guarantee.
- A notation of gnomon's own. Every client language would have to translate it to JSON Schema for structured output.
- A vendored PL/pgSQL JSON Schema validator. Draft 4 only and unmaintained, inside the tenant-isolation boundary.
- Validation in clients only. Any caller that bypasses a client could write invalid Versions.

## Consequences

- No property type refers to a Node. Anything that points at a Node is an Edge, so it gets its own Valid time, traversal, endpoint rules, Citations and Erasure.
- `like_regex` is POSIX and JSON Schema `pattern` is ECMA-262, so `pattern` is left out or restricted to a subset both read the same way. `oneOf` has no clean jsonpath form and is left out.
- Anthropic's structured output rejects numeric and length bounds, so the Pipeline strips them from the request and checks them on the reply. The Store enforces them on every write.
