# Architecture

## Design constraints

The harness was built under four constraints, and every design choice follows from them:

1. **A tick can die at any moment** — network drop, rate limit, a crash. Nothing may be
   corrupted by a tick that never returns.
2. **Ticks run in parallel and cannot talk to each other.** Coordination has to happen through
   the filesystem and GitHub, not through shared memory.
3. **API budget is finite and shared with the human.** The harness must yield rather than
   starve its owner.
4. **All durable state must be human-inspectable.** If the only record of what happened is in
   a model's context, the run is unauditable.

## Components

### Launcher (`tick.sh`, no arguments)

Decides how many ticks should exist right now, then spawns that many runners. It is stateless:
every firing recomputes the answer from scratch, so a missed firing costs nothing and a
duplicate firing is harmless.

Ordered cheapest-first because it runs every couple of minutes:

1. Count held slot locks. If none are free, exit — no API calls at all.
2. Refresh usage telemetry only if the cache is older than `USAGE_REFRESH_AFTER`.
3. Map utilization to a slot cap.
4. Ask GitHub how many labeled issues are unclaimed.
5. `spawn = clamp(allowed − busy, 0, unclaimed)`, plus a rate-limited heartbeat when the queue
   is empty.

### Runner (`tick.sh runner <max> <delay>`)

Sleeps for its stagger, takes the first free slot via `flock`, and runs a `claude -p`
invocation with the tick prompt. If no slot is free it exits silently — losing the race is a
normal outcome, not an error.

A runner keeps its slot and chains: when a tick ends, it re-reads usage and the queue and starts
the next tick at once on a fresh context, so work finished early is followed immediately rather
than at the next cron firing. It stops when the queue is empty, usage no longer allows its slot, a
tick ended in under `CHAIN_MIN_TICK_S` (nothing claimable, or an error), after `MAX_CHAIN` ticks,
or when the harness is paused (cron line commented out, or `state/paused`).

Each tick runs on the best model in the priority list (`state/tick-model`, else `TICK_MODELS`)
that is not cooling down. A tick that dies on a model's usage limit marks that model in
`state/model-cooldown/` for `MODEL_COOLDOWN_S` and re-runs at once on the next model.

The stagger exists because two ticks starting in the same second will read the same unclaimed
issue list and claim the same issue. Staggering is cheaper and more robust than a distributed
lock for this.

### Slots

`state/locks/slotN.lock` held with a non-blocking `flock` for the lifetime of the tick
process. Slots give three properties for free: a hard concurrency bound, an accurate live-tick
count (count the held locks), and automatic release on death — the kernel drops the lock when
the process exits, so a crashed tick never leaks a slot.

### Allocator

Maps `max(fiveHourPercent, weeklyPercent)` to a cap. Using the *worse* window means a healthy
5-hour window cannot mask an exhausted weekly one. The bands step down rather than cut off, so
the harness degrades to one careful tick before stopping entirely at 95%, leaving the last
slice of budget for the human.

Reading percentages rather than counting tokens keeps the harness honest about usage it did
not cause — the human's own sessions consume the same budget.

### Leases (for exclusive resources)

A lock held for a long session is fragile: if the holder dies, other ticks cannot tell whether
the resource is in use. So exclusive resources use a two-part scheme — a short `flock` to
serialize the handoff, and a **lease file** recording `issue PID timestamp` for the session.
Ownership is then testable by any tick with `kill -0`, and a dead owner's resource is reclaimed
immediately.

This is the single most important pattern in the harness: **ownership is proven by process
liveness, never by elapsed time.** The same rule governs issue claims.

### Issue queue as the work protocol

Issues are the queue, labels are the state machine (`ai` = queued, `ai:wip` = claimed), and PRs
are the output. Nothing about a run lives only in a tick's context: a claim is a label plus a
comment with a PID, progress is comments and evidence, results are PRs. A human can read the
entire history in the GitHub UI, and any tick can resume any other tick's work from it.

## Failure model

| Failure | Handling |
|---|---|
| Tick crashes | Kernel releases the slot; PID-liveness frees the claim and lease |
| Network outage | Ticks exit with `exit=1`; cron retries; nothing corrupted |
| Rate limit exhausted | Allocator reaches 0 and stops spawning; in-flight ticks finish; chains stop at the next boundary |
| One model's limit reached | The tick re-runs on the next model; that model cools down; the launcher spawns nothing while every model cools |
| Two ticks race one issue | Stagger plus label claim; the loser picks the next issue |
| Resource orphaned | Next tick detects a dead lease PID, cleans up, reclaims |
| Launcher missed | Stateless recompute; the next firing catches up |

## What is deliberately absent

**No daemon** — cron plus flock is more robust than a supervisor process that can itself die.
**No queue database** — GitHub already is one, and it is auditable.
**No inter-tick messaging** — ticks coordinate through issue labels; anything richer would
create states a human cannot inspect.
**No retry logic in the harness** — a failed tick just ends; the next firing re-derives what to
do. Retries hide problems that logs should surface. The one exception is a model's usage limit,
which says nothing about the work: that tick re-runs once per remaining model, and the trailer
in its log names the model each attempt used.
