# Prototype 3: output format for retrieval context

## Question

Which format should gnomon's formatters use by default when they hand retrieval results to a model: JSON, compact JSON, Markdown or TOON? LEAN is out.

## Measure

For the same retrievals rendered in each format: input tokens (count them with the Anthropic token-counting endpoint, not another vendor's tokenizer) and answer accuracy.

1. Build a small synthetic bi-temporal graph and 60 to 100 questions with known answers: multi-hop questions, As-of questions (a valid-at and a known-at instant), and "what changed" questions across Corrections, Ends of validity, Retractions and Refinements.
2. Each retrieval holds Versions with their Valid time (including a bound's precision and the `uncertain` marker), their Recorded time and their Citations, as the glossary defines them.
3. Render each retrieval in every format from the same structured data. Use the current, maintained TOON library.
4. Ask current Claude models (at least `claude-sonnet-5-5` and `claude-haiku-5-5`; `claude-opus-5-5` on a subset if the budget allows), grade the answers automatically, and report accuracy per format, per model and per question type.

Load the `claude-api` skill before writing SDK code.

## Credentials and budget

- Read `ANTHROPIC_API_KEY` from the environment, or else from `~/.config/gnomon-prototypes/env`. If neither has it, stop and ask the user.
- Spending cap: US$25 in model usage. Estimate the cost before the full run, and ask the user before going over the cap.

## Environment

- No database is needed. If you want one, use `postgres:17` on host port 55436.

## Rules for every prototype

- Read `GLOSSARY.md` and every ADR in `docs/adr/` first, and use the glossary's terms. The decisions there are settled; this prototype tests them, it does not reopen them unless the numbers force it.
- Use the `/prototype` skill. Answer exactly the question above with throwaway code. Do not build gnomon itself.
- Other prototypes run at the same time on this machine (48 cores, 251 GiB RAM). Run Docker containers only on the host ports listed in this brief, name them `gnomon-proto-<this prototype>-<role>`, and remove them when you finish. Never stop, change or remove any container you did not create. Other work is running on this machine.
- Commit only to this worktree's `prototype/...` branch. Never push and never touch `main`.
- The repo is public. Committed files must not name the consuming applications or quote their code, ADRs or issues; call them "the consuming applications".
- Never print, commit or log a secret. Read credentials from the environment or from files outside the repo.
- When you finish, write `FINDINGS.md` at the worktree root: the question; the answer in one sentence with its numbers; the method; a results table; what it means for which ADR (confirms, amends or contradicts); a recommendation; and caveats, including that other prototypes were running. Commit it, then tell the user you are done.
