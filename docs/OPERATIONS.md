# Operations

## Starting and stopping

```bash
./bin/tick.sh                 # one firing, right now
crontab -e                    # */2 * * * * /path/to/harness/bin/tick.sh
crontab -r                    # stop spawning; in-flight ticks finish on their own
```

To stop **after current work completes** — the usual way to shut down — remove the cron first,
then wait for the running ticks to exit. Never kill a tick mid-session: it may hold a lease or
a live resource that its own teardown is responsible for releasing.

```bash
crontab -r
until [ "$(pgrep -cf '^claude -p')" -eq 0 ]; do sleep 20; done
```

A tick running a long verification can take up to its budget (set in the tick prompt) to
finish. That is normal, not a hang. Runners chain ticks while work is queued; a commented-out
cron line or a `state/paused` file stops each chain at its next tick boundary.

## Reading the state

```bash
pgrep -cf '^claude -p'                     # live ticks
tail -5 state/logs/allocator.log           # sizing decisions
ls -lt state/logs | head                   # recent ticks (0 bytes = still running)
cat state/locks/*.lease 2>/dev/null        # who holds the exclusive resource
gh issue list --label ai:wip               # what is claimed right now
```

`allocator.log` is the fastest health check. A line reads:

```
20260819T081558Z five=22% week=53% allowed=8 busy=1 claimable=6 -> spawning 6
```

Usage in both windows, the cap that implies, how many slots are held, how many issues are
unclaimed, and what it did. No line means the firing was a no-op — that is the normal case.

## Is it stuck?

Usually not. Check in this order:

1. **`pgrep -cf '^claude -p'` returns 0 and the queue is non-empty.** Look at the newest log:
   an `exit=1` trailer with a short duration means API or network failure — cron retries by
   itself. No trailer at all means the tick died without finishing; see LESSONS.
2. **A tick has run for ages.** Check whether it holds the lease and whether its session is
   alive. Long verification runs are expected; compare against your budget.
3. **Nothing spawns although slots are free.** Check `allocator.log`: usage may have hit a
   band that caps at 0, or `claimable` may be 0 because every issue carries the WIP label.
4. **Everything looks idle mid-turn.** You are probably between firings. Wait two minutes.

## Recovery

**Orphaned lease.** Read the lease file, test the PID with `kill -0`. If dead, clean up
whatever it left running, then delete the lease. Ticks do this automatically; do it by hand
only when no tick is running.

**Stuck claims.** `ai:wip` on an issue whose claiming PID is gone is resumable by design —
a later tick continues from the recorded state. Remove the label by hand only if you want the
work abandoned rather than resumed.

**Container or service died.** The harness does not restart infrastructure. Bring it back
before the next firing, or add a liveness check to your runbook's setup step.

**After an outage.** Nothing to repair: slots released with their processes, and claims are
liveness-based. Restore infrastructure, confirm `gh auth status`, fire once manually.

## Tuning

| Symptom | Change |
|---|---|
| Slots idle while issues wait | Lower the cron interval; check `STAGGER_SECONDS` |
| Burning budget too fast | Lower `MAX_SLOTS`, or raise the band thresholds |
| Ticks racing on the same issue | Raise `STAGGER_SECONDS` |
| Queue growing faster than it drains | Enforce fix-first in the runbook (see LESSONS) |
| Too many audit ticks | Raise `HEARTBEAT_INTERVAL` |

## Operating discipline

- **Read the tick logs.** They are written for a human and contain the honest scope of each
  claim ("compile-verified only", "gate skipped because…"). Skimming PR titles is not the same.
- **Keep the runbook current.** When you rule on something — a convention's meaning, a merge
  policy, an excluded approach — write it into the runbook. A correction that lives only in
  chat will be re-litigated by the next tick.
- **Re-scope deliberately.** Discovery, fixing, and porting want different priorities. Say
  which phase you are in at the top of the runbook.
- **Watch the weekly window, not just the 5-hour one.** The 5-hour recovers on its own; the
  weekly is what actually ends a long run.
