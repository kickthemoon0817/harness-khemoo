# Runbook — <PROJECT NAME> harness

Read by every tick before it does anything. Repo: `<OWNER/REPO>`, checkout at `<TARGET_REPO>`.
Work branch: `<WORK_BRANCH>`. Never modify `<PROTECTED BRANCHES>` or any branch a tick did not
create.

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

1. **Setup** — `cd <TARGET_REPO> && git fetch origin`. Serialize every push and branch
   mutation under `flock <HARNESS_STATE>/locks/stack.lock`; ticks run in parallel.
2. **Claim** — list open issues labeled `<ISSUE_LABEL>` without `<WIP_LABEL>`. Claim the
   oldest: add `<WIP_LABEL>` and comment `harness tick <UTC timestamp> pid <PID> claiming`.
   A claim is held only while the claiming PID is alive (`kill -0`); once it exits, any tick
   may resume the issue from its recorded state (open PR, posted evidence, pending
   verification) rather than restarting. Prefer resuming a released claim over claiming fresh
   work.
3. **Audit** (only when nothing is claimable) — run the mission measurements, then label or
   file issues for each gap with its evidence. Keep the status checklist on the tracking issue
   current, then end the tick.
4. **Implement** — create a git worktree on a new branch `<PREFIX>/<issue#>-<slug>`. Never
   edit the user's checkout. Follow the repo's own conventions. Build and run the targeted
   tests.
5. **Verify** — re-run the measurement slice the issue affects. Record real evidence in the PR
   body under `## Verification`, and scope claims honestly ("compile-verified only" when no
   live run backed it).
6. **Land or hand off, by base branch** — merging is scoped: a tick may merge its own verified
   PR into the scratch work branch; PRs targeting shared or protected branches stay open for
   human approval. Never merge a PR you did not open, never merge what you cannot verify,
   never enable auto-merge on a protected base.
7. **Done condition** — when every mission target passes, refresh the tracking issue and end
   the tick with no changes.

## Shared-resource lease

Any exclusive resource (a GPU, a device, a warm container, a staging environment) needs a
lease, not just a lock:

- Acquire under `flock <HARNESS_STATE>/locks/resource.lock`: write `<issue#> <PID>
  <UTC timestamp>` to `<HARNESS_STATE>/locks/resource.lease`, then release the flock — the
  lease file, not the flock, marks ownership for the session's duration.
- Before treating the resource as busy, read the lease and check its PID with `kill -0`. A
  lease whose PID is dead is orphaned: clean up whatever it left running, overwrite the lease,
  and proceed. Never wait on a dead owner.
- Delete the lease when done. Cap any single session at 60 minutes.
- Prefer a claimable issue that does NOT need the resource when the lease is held by a live
  tick — that is what makes parallelism useful when the resource is serial.

## Rails

- One issue per tick. Never force-push. No AI attribution in commits.
- Only labeled issues are ever worked.
- Never request or assign a PR reviewer — leave the reviewers field empty; a human chooses.
- Respect in-flight human branches: if an issue overlaps one, say so on the issue and skip it.
- Ticks are finite `claude -p` batch runs — no scheduled wakeups, no ending a turn to wait.
  Write the tick summary and the teardown plan to the log BEFORE starting anything long.
- Archive failed evidence honestly. Never weaken a gate to obtain a pass.
