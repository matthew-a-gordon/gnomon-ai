# Orchestration policy

Tier, database, question and cleanup rules for the coordinator that fans tickets out to workers. Two
skills hold the procedure: `implement-spec-orca` for a whole spec landing as one PR, and
`orchestrate-tickets` for an ad-hoc set of tickets that land in the fewest PRs the grouping rules
below allow. The rules apply to both unless a section says otherwise.

`orchestrate-tickets` runs on Orca or on Herdr, whichever the human names. `implement-spec-orca`
runs on Orca only, because it needs Orca's task DAG and merger worker. Each section gives the Orca
form and the Herdr form where they differ.

## Public text

The repo is public, and so is every issue, comment, commit and PR body. The consuming applications
are private. Never put their code, their ADR or issue text, or links to them in anything a worker or
the coordinator writes. Describe a need in gnomon's own terms from `GLOSSARY.md`, and call them "the
consuming applications".

## Grouping tickets into PRs

A PR is the unit the human reviews and merges, so the coordinator opens the smallest number that
keeps each one reviewable. It groups two or more tickets into one PR when all of these hold:

- They edit the same file or the same component, or one is blocked by the other. Two PRs touching
  the same lines force a stack or a rebase, which costs the human more than one larger diff.
- None of them touches a sensitive row in the tier table below, and none adds or changes an exported
  migration. Those stay in their own PR, so a reviewer can accept or revert one alone.
- The combined diff stays small enough for one review. As a rule of thumb, stay under about 400
  changed lines and 10 files, and split a group that passes either.
- No ticket in the group is a decision ticket waiting on the human.

Two tickets that change the same generated output (the OpenAPI document, the stubs and types) make
their PRs conflict in those files even when no hand-edited file overlaps. The plan table flags the
second PR as certain to conflict. Its worker resolves it by merging `origin/main` and rerunning the
generator, never by hand-merging the generated files.

Everything else gets its own PR. Unrelated tickets that merely share an area stay separate. When in
doubt, ask the human in the plan table rather than guess.

A group is one branch and one PR, built the way a spec run builds its integration branch:

- The group branch comes off `origin/main`, named `feat/<slug>` or `fix/<slug>` for the group.
- Its tickets run one after another, each in its own worker session, and each worker starts from the
  branch tip left by the one before. Database-touching tickets were serial already. On Orca, start
  each later worker with `--base-branch <previous worker's local branch>`. Each worker names its
  branch `<group-branch>-t<N>` and does not push. The last worker renames its branch to the group
  name before it pushes, so one branch carries every commit in order. Orca names a new worktree's
  branch itself, so every worker renames it first.
- Each worker runs the standalone brief below, commits with `Closes #N` for its own ticket, and does
  not push or open a PR. Only the last worker pushes, opens the PR with a `Closes #N` line for every
  ticket, and runs `/mattpocock-skills:code-review` against `origin/main` over the whole diff.
- Each worker still comments on its own issue, and the coordinator verifies each ticket. The group's
  PR link goes in every ticket's report line.

## Model tier per worker

The coordinator passes `--agent claude --model <tier>` on every Orca `worker-start`, and `--model
<tier>` after `--` on every Herdr `agent start`. This table is the human naming the model, which
Orca's guide requires before the coordinator passes `--model`. The coordinator may raise a tier for
one ticket. The table is the floor.

| Work | Model | Effort |
|---|---|---|
| `/mattpocock-skills:implement` on a ticket that touches none of the sensitive rows | `sonnet` | default |
| A ticket touching Tenant isolation or RLS, JWT verification or token issuers (ADR-0018), Erasure or Vacuum (ADR-0003), invoker-mode Store functions (ADR-0009), or exported migrations (ADR-0006) | `opus` | `high` |
| `/mattpocock-skills:code-review` worker on a diff that touches a sensitive row but was implemented below `opus` | `opus` | `high` |
| Read-only checks: issue status, CI watching, `gh` queries | `haiku` | default |

## Launching `/mattpocock-skills:implement`

A worker cannot invoke `/mattpocock-skills:implement`: it is user-invocation only
(`disable-model-invocation`), so a spec line saying "invoke the skill" cannot run. The coordinator
types it instead.

1. The worker's first prompt is a briefing spec, not the procedure. It carries the branch name, the
   database flag, ownership, the report format, and this instruction: "Read the issue, then end your
   turn with no further tool calls. `/mattpocock-skills:implement #N` will be typed into this
   terminal. Do not substitute `/mattpocock-skills:tdd` plus `/mattpocock-skills:code-review`."
2. Read `mode` in the Orca start receipt first. `terminal` means `orca terminal send` reaches the
   worker. `structured` is a chat worker, and `terminal send` refuses it: stop the worker and ask the
   human to set new agent tabs to CLI in Orca. Do not type into a chat worker through
   `orchestration send`.
3. The receipt reads `ready` before the briefing turn ends. Wait for that turn to end, then run
   `orca terminal send --terminal <handle> --text "/mattpocock-skills:implement #N" --enter
   --wait-submit 30`. The receipt separates input accepted from turn started. A timeout means the
   command is queued. Check the worker before resending, and reissue only with the reported
   `--retry-request` ID.
4. The briefing turn's end raises a "worker stopped without reporting" escalation on Orca. Ignore
   that one.

On Orca, start workers without `--terminal`: `--model` and `--effort` cannot combine with it, and
every `worker-start` passes a model tier. On Herdr, `orchestrate-tickets/herdr.md` has the sequence.

## Postgres lanes

A ticket is database-touching when a lane needs a running Postgres: Store and Pipeline integration
tests, RLS tests, migration export checks, and the PgBouncer and Supavisor conformance runs. Run
database-touching tickets one at a time, chained with `--deps` on Orca and by starting the next one
only after the previous worker settles on Herdr. Two workers on one instance see each other's schema
and rows, and a migration changes what every other branch reads. Tickets with no database need
(docs, pure unit tests, type and stub generation) run in parallel with anything.

A worker owns the lifecycle of any database it starts. It uses the repo's documented command to
start one, stops it when the lane ends, and confirms it is down before it reports done. If the repo
has no documented command yet, the worker asks and does not invent one.

Each worker's report has four fields: its branch (and the PR link, where it opens one), its
wall-clock time inside database lanes, its total wall-clock time, and "database stopped: yes". A
report missing a field goes back to the worker.

## Worker briefs

Both briefs go into every worker's spec, copied in whole, since the worker has no other copy. Steps
1 to 3 and 6 to 8 are common. A spec run (`implement-spec-orca`) and a standalone run
(`orchestrate-tickets`) differ in steps 4 and 5.

> 1. Read `GLOSSARY.md` and the ADRs the ticket cites, and use the glossary's terms in code, tests
>    and comments. If your work contradicts an ADR, stop and say so in your report, citing the ADR
>    number.
> 2. Run `/mattpocock-skills:implement #N` when it is typed into your terminal. It drives
>    `/mattpocock-skills:tdd` at the seams the ticket names. When the tests are green, apply the
>    ticket's named mutation to the finished code, keep the failing run, and revert the mutation.
> 3. Run every lane the diff touches, and quote the passing output. Start the database only for a
>    lane that needs it, and stop it when the lane ends.
> 4. Run `/mattpocock-skills:code-review` against `<review-base>`.
> 5. `<finish>`
> 6. Comment on your issue: the design chosen, the test that proves it, the named mutation's failing
>    run, the `/mattpocock-skills:code-review` result, and your branch (and PR, where you open one).
>    List every out-of-scope finding there too.
> 7. If you are blocked, comment the blocker on the issue before you report.
> 8. Report done with the four fields in *Postgres lanes*.
>
> Everything you write to the issue, a commit or a code comment is public. Never name the consuming
> applications or quote their text.

**Spec run.** `<review-base>` is the integration branch. `<finish>` is: "Commit with no `Closes #N`,
since the one PR closes every ticket. Merge the integration tip into your branch and rerun the tests
that touch the merge. Do not push. Do not open a PR."

**Standalone run.** `<review-base>` is `origin/main`. `<finish>` is: "Commit with `Closes #N` in the
message. Push the branch and open a PR whose body carries `Closes #N`, written with
`/mattpocock-skills:pr`. Merging is the human's call." A worker that is not last on a group branch
instead commits and stops, as *Grouping tickets into PRs* says.

## Verifying a worker

Before merging or releasing a worker, confirm with `gh` and `git`:

- The issue has a closing comment showing the `/mattpocock-skills:code-review` result and the named
  mutation's failing run, shown after the test went green. Transcripts never show
  `/mattpocock-skills:tdd` or `/mattpocock-skills:code-review` as skill calls, so this comment is the
  evidence that `/mattpocock-skills:implement` ran.
- Out-of-scope findings appear in that comment.
- The report has all four fields.
- No database is left running.
- `git diff --stat <base>...<branch>` touches no sensitive row in the tier table, or the worker ran
  on `opus`. Otherwise start the tier table's review worker on the branch first. `<base>` is the
  integration branch in a spec run and `origin/main` in a standalone run.
- Spec run: the worker's branch contains the integration tip, and no commit message or comment
  closes the issue.
- Standalone run: the branch is pushed and its PR body carries `Closes #N`. For a group, only the
  last worker's check applies to the branch and PR, and the body must carry a `Closes #N` line for
  every ticket in the group. Earlier workers need only a commit on the group branch.

Missing evidence goes back to the worker as a follow-up message, so files change only through
workers.

## Questions from workers

The coordinator answers a worker's `ask` (on Herdr, a worker in the `blocked` state) only from the
ticket, `GLOSSARY.md` or an ADR, and quotes the source. A question that conflicts with an ADR,
changes scope, or blocks the worker goes to the human. The worker records the blocker in the issue.

## Integration branch (spec runs)

`implement-spec-orca` lands every ticket on one branch, `feat/<spec-slug>`, and opens one PR from it
once the whole is green. Three rules keep that branch sound.

- Workers branch off the integration tip and never push or open a PR. A single merger worker, kept
  in one worktree for the run, fast-forwards the integration branch to each verified worker branch
  and pushes it. A branch with no open PR runs no CI.
- A ticket starts only after its blockers have merged, not when they report done, so its worktree
  contains their code. A dependent ticket that waits for a merge still waits on the database chain
  too.
- The PR opens after the final lanes and review pass on the integration branch, never as a draft
  after the first merge. Opening it earlier starts CI runs on a branch that has not been verified
  locally.

## Cleanup and follow-ups

The closing comment is the record of out-of-scope findings, and the repo has no `has-followups`
label. After the PR opens, the coordinator files each finding as its own issue with the
`needs-triage` label and links it in the report. A finding that is a kill gets no issue, and the
report says so.

Before `worker-stop` or removing a worktree, run `git status` in it. A worker that looks idle may
hold uncommitted edits, and a stop leaves them on disk with no session to finish them.

On Orca, the coordinator releases a settled worker's terminal and marks its worktree `in-review`.
Herdr has no release, so the coordinator leaves the agent and workspace alone and reports the
workspace ID. On both, the worktree stays until the human merges the PR and removes it.
