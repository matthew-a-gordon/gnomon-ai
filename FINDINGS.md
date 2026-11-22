# Findings: prototype 3, format of retrieval results for models

## Question

Which format should gnomon's formatters use by default when they hand retrieval results to a model: JSON, compact JSON, Markdown or TOON? LEAN is out.

## Answer

Accuracy did not separate the formats (Haiku 5.5 scored 100% in every format with reasoning on, and 70.6% to 76.1% in every format with reasoning off, a spread inside the noise), but input tokens did: a flat TOON table used 65% fewer tokens than pretty JSON (4,693 against 13,314), and compact JSON used 29% fewer. So the default should be chosen on token cost, and the cheapest accurate shape is a flat, per-Kind tabular layout.

## Method

- The code is throwaway and lives in `prototypes/llm-formatters/`. Run `node gen.mjs`, then `node run.mjs`, then `node report.mjs`. The generator is seeded, so the datasets reproduce.
- A synthetic bi-temporal graph with Nodes and Edges, each holding Versions with Valid time (bound precision from `unknown` to `instant`, plus the `uncertain` marker), Recorded time and Citations. Valid-time intervals are half-open and open bounds are `infinity`, as in ADR-0004 and ADR-0008.
- 90 questions with answers computed from an as-of engine, in 8 types: multihop (15), as-of by valid time (15), as-of by known time (15), uncertain (15), and what changed across Corrections (8), Ends of validity (8), Retractions (7) and Refinements (7).
- Two datasets. `small` has 3 to 6 noise entities per retrieval (about 20 Versions). `large` has 14 to 24 (56.9 Versions per retrieval on average). `small` was too easy, so the results below use `large` unless noted.
- Five formats rendered from the same data:
  - `json`: pretty-printed.
  - `json-compact`: no whitespace.
  - `markdown`: one table per entity, custom renderer.
  - `toon`: the nested structure encoded with `@toon-format/toon` 4.4.0.
  - `toon-flat`: an extra arm. Scalar columns grouped by Kind, so TOON's tabular layout applies. It has a different shape from the other four.
- Model: Haiku 5.5 (`anthropic/claude-haiku-5.5` through OpenRouter). Two regimes:
  - Regime A: reasoning effort `low` and a brief written justification, the realistic agent setting. One trial.
  - Regime B: reasoning effort `none` and an answer-only reply, the hardest setting. Two trials, so n = 180 per format.
- Grading is automatic: the last `ANSWER:` line is normalized and compared with the known answer.
- Input token counts come from the `usage` field of OpenRouter responses (provider-native counts). They are not counts from a local tokenizer.

## Results

Mean input tokens per call, Haiku 5.5 (output tokens are in the regime tables below):

| format | small dataset | large dataset | vs json (large) |
|---|---|---|---|
| json | 5,184 | 13,314 | baseline |
| json-compact | 3,796 | 9,404 | -29% |
| toon | 3,554 | 8,698 | -35% |
| markdown | 2,659 | 6,191 | -54% |
| toon-flat | 2,160 | 4,693 | -65% |

Regime A (reasoning `low`, justification), large dataset, n = 90 per format:

| format | accuracy % | mean output tokens | cost US$ |
|---|---|---|---|
| json | 100.0 | 517 | 0.143 |
| json-compact | 100.0 | 465 | 0.106 |
| markdown | 100.0 | 462 | 0.077 |
| toon | 100.0 | 475 | 0.100 |
| toon-flat | 100.0 | 498 | 0.065 |

Every question type scored 100% in every format in this regime, and also on the small dataset. This is a ceiling effect: the test cannot tell the formats apart here.

Regime B (reasoning `none`, answer only), large dataset, 2 trials, n = 180 per format:

| format | accuracy % | cost US$ |
|---|---|---|
| json-compact | 76.1 | 0.172 |
| toon | 75.6 | 0.158 |
| toon-flat | 73.3 | 0.087 |
| json | 72.8 | 0.242 |
| markdown | 70.6 | 0.113 |

The largest gap is 5.5 points. The standard error of one accuracy figure at n = 180 is about 3.3 points, so no pair differs reliably. Accuracy by type in regime B (percent, 30 or 14 to 30 calls per cell, so noisy):

| type (questions) | json | json-compact | markdown | toon | toon-flat |
|---|---|---|---|---|---|
| multihop (15) | 60.0 | 50.0 | 46.7 | 43.3 | 66.7 |
| asof_valid (15) | 50.0 | 60.0 | 46.7 | 56.7 | 50.0 |
| asof_known (15) | 40.0 | 50.0 | 50.0 | 66.7 | 40.0 |
| uncertain (15) | 96.7 | 96.7 | 93.3 | 86.7 | 93.3 |
| changed_correction (8) | 87.5 | 100.0 | 100.0 | 100.0 | 100.0 |
| changed_end (8) | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 |
| changed_retraction (7) | 92.9 | 100.0 | 71.4 | 100.0 | 78.6 |
| changed_refinement (7) | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 |

Multihop and as-of questions are the hard ones without reasoning, in every format. The format ordering flips from type to type, which is what noise looks like.

## What it means for the ADRs

- ADR-0008 (bound precision, `uncertain`): confirmed as usable. With the definitions in the system prompt, Haiku read the precision field and the `uncertain` marker correctly in every format (100% in regime A, 87% to 97% on the uncertain type in regime B).
- ADR-0004 (`infinity` for open bounds): confirmed. The model handled `infinity` and `-infinity` in every format.
- No ADR is amended or contradicted. None of the ADRs fixes a model-facing format, so this adds a decision rather than reopening one.

## Recommendation

1. Choose the default on tokens, since accuracy does not separate the formats in this test.
2. Do not default to pretty JSON. It costs 1.4 to 2.8 times the input tokens of the alternatives for no accuracy gain.
3. Make the default a flat tabular layout, one table per Kind, with scalar columns. Both `toon-flat` and `markdown` fit this, and `toon-flat` is smallest (4,693 tokens against 6,191). Its accuracy in regime B (73.3%) sits inside the noise of the rest. If a format-neutral default is wanted, `json-compact` is the safe fallback: it had the highest regime B accuracy (76.1%, not significant) and costs 29% less than pretty JSON.
4. Nested TOON saves only 6% over compact JSON (8,698 against 9,404). TOON pays off through its tabular layout, which needs a flat shape, so the formatter's shape matters more than the encoding.
5. Keep the formatter interface open so a consuming application can pick the format. The numbers justify a default, not a ban on the others.

## Caveats

- Only Haiku 5.5 was measured. Sonnet 5.5 and Opus 5.5 were not measured because OpenRouter does not let their reasoning be turned off: with `effort none` it returned HTTP 400 ("Reasoning is mandatory for this endpoint and cannot be disabled"). Haiku at `effort low` already sat at 100%, so the larger models with reasoning on would very likely sit at the ceiling too, and the Haiku regime B run is the only setting that discriminated. The two models' results files hold only 2-call smoke tests at `effort low` (one question, two formats each) and should be ignored.
- Spend: about US$1.69 in total on OpenRouter, including about US$0.16 for the four smoke-test calls above. The US$25 cap was not approached.
- Other prototypes were running on the same machine during these runs. They compete only for network and rate limits, which affects speed, not the token counts or accuracy.
- One synthetic domain (people, employers, cities). Real retrievals may be messier, and long free-text properties would change the format ranking.
- The `toon-flat` arm has a different shape from the other four, so its token saving mixes the encoding with the reshaping. The Markdown renderer is custom and was not tuned.
- Regime B trials had small samples per question type, and accuracy there is noisy. A larger trial count would be needed to rank formats within a few points.
- Token counts are OpenRouter provider-native usage figures, so they are consistent across formats but may differ from other tokenizers.
