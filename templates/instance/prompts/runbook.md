# Runbook — <PROJECT NAME> harness

Read by every tick after the core rails, which it may narrow and never loosen. Repo:
`<OWNER/REPO>`, checkout at `$TARGET_REPO`. Work branch: `<WORK_BRANCH>`. Never modify
`<PROTECTED BRANCHES>` or any branch a tick did not create.

## Priority — fix, don't accumulate findings

Every tick's default is to **land a fix for an existing issue**, not to produce another
finding.

- Claim the oldest issue that has an identified fix or a clear implementation path. Prefer
  issues that unblock others.
- **Do not file a new issue unless it blocks the fix you are currently implementing.** If you
  notice something unrelated, write one sentence about it in your tick log and move on.
- An audit tick is only warranted when NOTHING in the queue is claimable. Prefer resuming a
  released claim, or advancing a stalled fix, over running a fresh audit.
- A tick that ends with a new issue and no code change is a failed tick unless the issue was a
  genuine blocker.

## Mission

<State the goal as measurable targets, each with the exact command or tool that decides
pass/fail, and the exact configuration the target is measured under. A target without a
measurement procedure is a wish, not a target.>

Example shape:
- **T1 — <name>**: <condition>. Measured with `<command>` under `<configuration>`.
- **T2 — <name>**: <threshold with margin>. Measured with `<tool>`; state whether the system
  must be idle or under load — they are different numbers.

## Tick procedure

1. **Setup** — `cd $TARGET_REPO && git fetch origin`. Serialize every push and branch
   mutation under `flock $HARNESS_STATE/locks/stack.lock`; ticks run in parallel.
2. **Claim** — as the core rails say: the local claim, the work label, and the claim comment
   with its protocol line inside the Harness trace block.
3. **Audit** (only when nothing is claimable) — run the mission measurements, then label or
   file issues for each gap with its evidence. Keep the status checklist on the tracking issue
   current, then end the tick.
4. **Implement** — create a git worktree `$WORKTREE_ROOT/<name>-<issue#>` on a new branch
   `<BRANCH_PREFIX><issue#>-<slug>`. Never edit the user's checkout. Follow the repo's own
   conventions. Build and run the targeted tests.
5. **Verify** — re-run the measurement slice the issue affects. Record real evidence in the PR
   body's `## Evidence` section, written to the public record (every reading with its commit and
   condition, nothing that is only true on this host), and scope claims honestly under
   `## Scope and limits` ("compile-verified only" when no live run backed it).
6. **Land or hand off, by base branch** — merging is scoped: a tick may merge its own verified
   PR into the work branch; PRs targeting shared or protected branches stay open for human
   approval. Never merge a PR you did not open, never merge what you cannot verify, never enable
   auto-merge on a protected base.
7. **Done condition** — when every mission target passes, refresh the tracking issue and end
   the tick with no changes.

## Shared devices

<If your instance has shared devices (a GPU, a test board, a staging environment), give their
rules here: how a tick takes one, how it tells a live holder from a dead one, how long it may
hold it, and what a tick does while every one is taken.>

## Rails

- Respect in-flight human branches: if an issue overlaps one, say so on the issue and skip it.
- Write the tick summary and the teardown plan to the log BEFORE starting anything long.
- Archive failed evidence honestly. Never weaken a gate to obtain a pass.
- <The project's own rails: what a tick must never touch, and what it must always run.>
