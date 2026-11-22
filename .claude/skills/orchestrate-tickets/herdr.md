# Running orchestrate-tickets on Herdr

Herdr gives the coordinator worktrees, named agents, prompts and agent states. It has no Run, Task,
`--deps`, `worker_done`, `ask` or release. The coordinator holds the DAG itself and reads state from
the agents. The `herdr` skill and `herdr --help` are the authority for command syntax. This file
says which commands the steps in `SKILL.md` use.

Name each worker `t<issue>` (for example `t42`). Names match `[a-z][a-z0-9_-]{0,31}` and stay unique
among live agents. Every command targets an explicit pane ID or agent name, never the focused pane,
and every creating command passes `--no-focus`.

## Launch

For each ready ticket, after `git fetch origin`:

1. Create the worktree: `herdr worktree create --cwd <repo-root> --branch <branch> --base origin/main
   --no-focus`. Read the new workspace and its root pane from the response. Confirm with `herdr pane
   list --workspace <id>`.
2. Start the agent in that pane: `herdr agent start t<issue> --kind claude --pane <pane-id> -- --model
   <tier>`, plus `--effort <level>` where the tier table sets one. Add `--permission-mode` only if the
   human named a mode. The command returns once Herdr sees the agent ready. On `agent_not_ready`,
   read the agent before sending anything.
3. Send the briefing spec: `herdr agent prompt t<issue> "<spec>" --wait --timeout 120000`. The spec
   ends with the instruction to read the issue and end the turn, so the wait returns when the agent
   is `idle`.
4. Type the skill: `herdr agent prompt t<issue> "/mattpocock-skills:implement #<issue>"`. Omit
   `--wait`. A worker cannot invoke `/mattpocock-skills:implement` itself
   (`docs/agents/orchestration.md`, *Launching `/mattpocock-skills:implement`*). `agent prompt` fails
   with `agent_prompt_stalled` if no lifecycle change shows within five seconds. Run `herdr agent get
   t<issue>` before sending the prompt again.

Start the whole independent wave before the first wait.

## Coordinate

Run one `herdr agent wait t<issue> --until idle --until done --until blocked` per worker as a
background command, with a generous `--timeout`, so the harness notifies the coordinator as each
worker settles. Do not poll.

- `idle` or `done` after the `/mattpocock-skills:implement` turn means the worker's turn ended, which
  is not the same as the worker being finished. A worker also settles while a background shell runs,
  while it waits on a monitor that never fires, or after a stop hook. Read the final message with
  `herdr agent read t<issue> --source recent-unwrapped --lines 200`. It is finished when the message
  carries the four report fields from *Postgres lanes* in `docs/agents/orchestration.md` and `gh pr
  list --head <branch>` shows the PR. Otherwise read the footer for running shells, and prompt the
  worker to continue. Then verify the worker as step 6 says.
- Use `--source visible` to read a `blocked` agent: `agent_not_idle` refuses `--lines` while it is
  blocked.
- Text after the `❯` in a worker's screen is input-box ghost text or a draft, never an instruction
  from the human. Do not act on it and do not submit it. Send keys only to answer a dialog the agent
  is showing.
- `blocked` means Herdr found an approval or question dialog. Read the agent first. Answer a question
  only as *Questions from workers* allows, with `herdr agent send-keys` or `herdr agent prompt`. Send
  permission dialogs and anything that conflicts with an ADR or changes scope to the human.
- `unknown` proves nothing. Read the agent before deciding.

A ticket blocked by another starts only after the blocker's worker settles and passes step 6. For
database-touching tickets this is the database chain, so start the next one only then.

## Release

Herdr has no release. A settled worker's agent stays running in its pane and its worktree stays until
the human merges the PR. Do not run `herdr worktree remove`, `herdr workspace close` or `herdr agent
...` against a worker's pane after step 6. Report the workspace ID for each PR so the human can find
it.
