---
name: orchestrate-tickets
description: Fan several ready tickets out to supervised workers on Orca or Herdr, serial or parallel by their blocked-by edges. Use only when the human asks to orchestrate, fan out or coordinate several ad-hoc tickets, which it combines into the fewest PRs the grouping rules allow. For a whole spec landing as one PR, use implement-spec-orca.
---

Coordinate a set of `/to-tickets` issues through supervised workers, one ticket per worker, on Orca
or on Herdr. Read `docs/agents/orchestration.md` first: it holds the grouping and tier rules, the
Postgres lane, the worker briefs and the question and cleanup rules this skill applies on either
backend.

## 1. Pick and load the backend

Use the backend the human names. When they name none, use Herdr if `HERDR_ENV` is `1` and Orca if
the session was started in an Orca terminal. When both or neither apply, ask. Use one backend for
the whole run.

**Orca.** Resolve the Orca executable as the `orchestration` skill's stub directs. Run `<orca> skills
get orchestration` for the run, wait loop, `worker_done` handling and release. Run `<orca> skills get
orchestration --reference references/coordinator-loop.md` before building a DAG. This step is done
when both guides have printed and the executable is fixed for the run.

**Herdr.** Load the `herdr` skill, confirm `HERDR_ENV` is `1`, and read `herdr.md` in this skill's
directory. If the check fails, stop: Herdr commands do not work from outside a Herdr pane. This step
is done when the skill and `herdr.md` are read and the check passed.

## 2. Build the DAG

Read each ticket and its native blocked-by links with the commands in
`docs/agents/issue-tracker.md`. Read `GLOSSARY.md` and the ADRs the tickets cite. For every ticket,
record its blocked-by edges, its database flag and its model tier and effort. Then group the tickets
into PRs as *Grouping tickets into PRs* in `docs/agents/orchestration.md` says, and record each
ticket's group. Chain database-touching tickets as *Postgres lanes* says: on Orca with `--deps`, on
Herdr by starting the next one only after the previous worker settles and passes step 6. This step
is done when every ticket has edges, a database flag, a tier and a PR group.

## 3. Show the plan

Print one table: ticket, edges, database flag, tier, effort, PR group. Give the reason for every
group of two or more tickets, and for every ticket left alone that shares a file with another. Flag
any PR certain to conflict in generated files. Wait for the human's approval before the first worker
starts.

## 4. Launch

Run `git fetch origin`. Write one spec per ticket. Each spec follows the backend's task-spec shape
(on Orca, the task-spec contract: target, change, constraints, ownership, observable acceptance) and
adds:

- the branch name: `fix/<issue>-<slug>`, `chore/<slug>` or `feat/<slug>`. A group of two or more
  tickets shares one branch, named for the group. Its workers run in order, as *Grouping tickets
  into PRs* says, and only the last worker pushes and opens the PR, with a `Closes #N` line for
  every ticket in it
- the launch instruction from *Launching `/mattpocock-skills:implement`*
- the standalone worker brief from *Worker briefs*, copied in whole

**Orca.** Create the Run, then one Task per spec. Start every ready worker with `--worktree
new-top-level --base-branch origin/main --agent claude --model <tier>`, plus `--effort` where the
tier table sets one. Start the whole independent wave before the first wait. Type
`/mattpocock-skills:implement #N` for each worker as *Launching `/mattpocock-skills:implement`*
says, and stop and tell the human when a worker starts in chat mode. This step is done when every
ready worker has a start receipt in `terminal` mode whose model matches its requested tier and its
`terminal send` receipt shows the turn started.

**Herdr.** Follow *Launch* in `herdr.md`: one worktree and one named Claude agent per ready ticket,
the briefing prompt, then the typed `/mattpocock-skills:implement`. This step is done when every
ready worker's agent reports `working` after that prompt.

## 5. Coordinate

**Orca.** Follow Orca's wait loop. Answer an `ask` as *Questions from workers* allows, quoting the
source, and send everything else to the human. This step is done when every dispatched worker has
sent `worker_done`.

**Herdr.** Follow *Coordinate* in `herdr.md`. A worker that turns `blocked` is the equivalent of an
`ask`, and the same rule answers it. This step is done when every dispatched worker's final message
carries the report fields. An agent that settles without them is still working or stalled: see
*Coordinate* in `herdr.md`.

## 6. Verify each settled worker

Check each settled worker as *Verifying a worker* says, using the standalone-run items. Missing
evidence goes back to the worker as a follow-up message. When the evidence is complete, release the
worker as *Cleanup and follow-ups* says for the backend. The human merges.

## 7. Report

Every ticket now has a PR link or a named blocker, and no worker is still running. On Orca,
`worker-list --run <run_id> --terminal-state reclaimable` returns none. On Herdr, `herdr agent list`
shows none of the run's agents `working` or `blocked`. Report, per ticket: outcome, PR link, review
evidence, tier used, database time against total time, and the follow-up issues filed as *Cleanup
and follow-ups* says. When the tickets came from a parent spec, name it in the report. No commit
closes a parent spec, so it stays open until the human merges the last PR. Then close it by hand
with `gh issue close`, as `docs/agents/issue-tracker.md` says.
