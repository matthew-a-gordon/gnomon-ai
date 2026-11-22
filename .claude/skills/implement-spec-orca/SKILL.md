---
name: implement-spec-orca
description: Implement a whole /to-spec spec and its /to-tickets tickets on one integration branch through supervised Orca workers, ending in a single PR. Use only when the human asks to implement a spec or tranche with Orca. For an ad-hoc set of tickets that each get their own PR, use orchestrate-tickets.
---

Implement a spec, a parent issue with sub-issue tickets, as one unit. Workers build tickets in
parallel where the blocked-by edges allow, their work lands on one integration branch, and one PR
carries the whole spec. This is `mattpocock-skills:implement-spec` with Orca workers in place of
subagents. Read `docs/agents/orchestration.md` first: it holds the tier table, the Postgres lane,
launch, question and cleanup rules, the worker briefs and the integration branch rules this skill
applies.

## 1. Load Orca

Resolve the Orca executable as the `orchestration` skill's stub directs. Run `<orca> skills get
orchestration` for the run, wait loop, `worker_done` handling and release. Run `<orca> skills get
orchestration --reference references/coordinator-loop.md` before building a DAG, and
`--reference references/placement-and-remote.md` before creating a worktree. This step is done when
the guides have printed and the executable is fixed for the run.

## 2. Build the task graph

Read the spec and its sub-issues with the commands in `docs/agents/issue-tracker.md`. Each ticket
names its spec under `## Parent`, and blocked-by edges are GitHub's native issue dependencies. Read
`GLOSSARY.md` and the ADRs the spec cites, so the briefs use the glossary's terms.

For every ticket, record its blocked-by edges, its database flag and its model tier and effort.
Chain database-touching tickets as *Postgres lanes* says. Resolve each premise a ticket cites
against the tree, not against the spec's own references: the repo changes between the spec and the
run. This step is done when every ticket has edges, a database flag and a tier, and the frontier
(tickets with no open blockers) is known.

## 3. Show the plan

Print one table: ticket, edges, database flag, tier, effort, and the integration branch name
(`feat/<spec-slug>`). Wait for the human's approval before creating anything.

## 4. Create the integration branch

Run `git fetch origin`, create `feat/<spec-slug>` from `origin/main` and push it. A branch with no
open PR runs no CI. Start a merger worker on a new top-level worktree based on
`origin/feat/<spec-slug>`, with `--agent claude --model sonnet`. Keep that worktree for the run and
reuse it by its exact `id:` selector for every later merge and for the final fixes. This step is
done when the merger worktree has the integration branch checked out and its `id:` selector is
recorded.

## 5. Dispatch the frontier

Create the Run, then one Task per ready ticket. Each spec follows Orca's task-spec contract (target,
change, constraints, ownership, observable acceptance) and adds:

- the worker branch name `feat/<spec-slug>-t<N>`
- the integration branch name, and "your worktree starts at its tip"
- the launch instruction from *Launching `/mattpocock-skills:implement`*
- the spec-run worker brief from *Worker briefs*, copied in whole

Start each worker with `--worktree new-top-level --base-branch origin/feat/<spec-slug> --agent
claude --model <tier>`, plus `--effort` where the tier table sets one. Type
`/mattpocock-skills:implement #N` into each worker as *Launching `/mattpocock-skills:implement`*
says. This step is done when every frontier worker has a start receipt whose `launch.effective`
matches its requested tier and whose `terminal send` receipt shows the turn started.

## 6. Coordinate and merge

Follow Orca's wait loop. Answer an `ask` as *Questions from workers* allows and send everything else
to the human. When a worker sends `worker_done`:

1. Verify it as *Verifying a worker* says. Missing evidence goes back to the worker as a follow-up
   message.
2. Have the merger worker fast-forward the integration branch to the worker's branch and push it. A
   branch that no longer fast-forwards goes back to its worker: merge the integration tip, rerun the
   ticket's tests, report again.
3. Release the worker as *Cleanup and follow-ups* says. Any ticket whose blockers have now all
   merged joins the frontier: dispatch it as Step 5 does, from the new tip.

This step is done when every ticket is merged into the integration branch or has a named blocker
recorded on its issue.

## 7. Review the whole

On the integration branch, in the merger worktree, run every lane the aggregate diff touches, with
one agent on the database at a time. Run `/mattpocock-skills:code-review` against `origin/main`. If
the aggregate diff touches a sensitive row in the tier table, run the review on an `opus` worker.
Send every finding to one fix worker on the integration worktree and rerun the lanes it touched.
This step is done when the lanes are green and the review has no open in-scope findings.

## 8. Open the PR

Push the integration branch, then open one PR to `main`. Write the body with the
`mattpocock-skills:pr` skill, from the aggregate diff and the tickets' closing comments. End it with
a Closing keyword section holding `Closes #<spec>` and one `Closes #N` line per merged ticket. The
repo is public, so apply *Public text* to the body. The human merges.

## 9. Report

Report: the PR link, per ticket its outcome, tier used, database time against total time and a link
to its closing comment, the review result, and the follow-up issues filed as *Cleanup and follow-ups*
says. Release each terminal and mark the worktrees `in-review`. The worktrees stay until the human
merges the PR. `worker-list --run <run_id> --terminal-state reclaimable` returns none.
