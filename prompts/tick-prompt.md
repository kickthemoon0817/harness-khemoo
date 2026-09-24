You are one tick of an autonomous issue harness for MaumAI-Company/isaac_sim, executing the
owner-approved manure plan (mission 5). Read
/home/khemoo/tmp_workspace/claude-issue-harness/prompts/runbook-plan.md first and follow it exactly;
it tells you which parts of /home/khemoo/tmp_workspace/claude-issue-harness/prompts/runbook.md still
apply (mechanics only). Then read the plan's PRINCIPLE.md, DECISIONS.md §1, TODO.md and the item
document your issue names, all under /home/khemoo/tmp_workspace/artifacts/manure-plan/, and
/home/khemoo/tmp_workspace/isaac_sim/CLAUDE.md. Do not read design.md: its plan of record is void.

You are a FINITE BATCH RUN under `claude -p`: the process terminates the moment your turn
ends, and nothing scheduled survives it. Therefore:
- NEVER use a scheduling/wakeup tool, and never end your turn to "wait" for a background
  task, a timer, or a log marker — wait synchronously inside bounded polling loops in a
  single command.
- Before your turn ends, everything you started must be finished or torn down: kit sessions
  stopped (`tools/dev/iter.sh down`), the resource lease deleted, background tasks reaped.
- TICK BUDGET: 120 minutes of wall time from claim to summary, or 120 minutes when the claimed
  issue carries the `ai:long` label. Read the issue's labels in the same command that posts the
  claim, and state the budget in the claim comment ("budget 120 min" / "budget 120 min, ai:long").
  `ai:long` is the operator's to set: a tick never adds or removes it on any issue. Record the
  claim time with `date -u` in that same command, and read the clock again with `date -u` before
  you ever say the budget is short — never estimate elapsed time from how much you have done. A
  claim of 'the budget ran out' that does not quote claim time, current time and the difference
  is a failed tick: two ticks have stopped with 25 minutes unspent this way. A nine-minute suite
  fits whenever the elapsed time is under the budget minus 9 minutes. At the budget minus 5
  minutes start nothing new (no build, no suite, no kit), tear down, post the pause with the
  resume recipe, print the summary and END. A later tick resumes from the recipe. Any single
  wait that would cross the budget is a pause, not a wait.
  The budget is a ceiling, not a target: when the issue lands or pauses early, end the tick at
  once. The runner starts the next tick immediately on a fresh context, so never pad a tick and
  never claim a second issue in the same tick.
- LONG TICKS (`ai:long`): every 30 minutes of elapsed time post one checkpoint comment on the
  issue whose first line is `harness tick pid <pid> checkpoint <n> at <date -u>`, then what is
  done, what is running, and the resume recipe so far, so a tick that dies leaves a recipe.
  The first line must not use the words withdrawing, pausing or releasing, because the claim
  check reads those as the end of the claim.

Before you commit, run ONE review pass over your own change, scoped to this tick's purpose and
what it touches (the runbook's ONE REVIEW PER TICK rail). One pass, not two.

This tick: IMPLEMENT ONE PLAN STEP. Claim the oldest claimable `ai:plan` issue (runbook-plan.md
"Which issue") and implement it end to end: worktree, code, build, doctests, the evidence packet,
PR into `ai/manure-mpm`. Class A: merge on its evidence and close the issue. Class B: label PR and
issue `ai:signoff`, post the packet, pause, END — never merge it. Do not file new issues unless one
blocks you; findings that change the plan go in a `Plan finding:` comment.

Never touch protected branches, never modify the user's working checkout
(/home/khemoo/tmp_workspace/isaac_sim itself), and never work an unlabeled issue. End the
tick by printing a one-paragraph summary: what you claimed, what changed, the verification
evidence, and what landed. Your stdout IS the tick log — a tick that ends without a summary
is a failed tick.
