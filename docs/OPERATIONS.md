# Operations

## Starting and stopping

```bash
./bin/tick.sh check           # what the instance resolves to; exit 1 names what is missing
./bin/tick.sh                 # one firing, right now
crontab -e                    # */2 * * * * /path/to/harness/bin/tick.sh
crontab -r                    # stop spawning; in-flight ticks finish on their own
```

To stop **after current work completes** — the usual way to shut down — remove the cron first,
then wait for the running ticks to exit. Never kill a tick mid-session: it may hold a claim, the merge lock
or a resource the instance gave it, which its own teardown is responsible for releasing.

```bash
crontab -r
until [ -z "$(target/release/harness ticks)" ]; do sleep 20; done
```

A tick running a long verification can take up to its budget (set in the tick prompt) to
finish. That is normal, not a hang. Runners chain ticks while work is queued; a commented-out
cron line or a `state/paused` file stops each chain at its next tick boundary.

## Reading the state

```bash
target/release/harness ticks               # live ticks: pid, age, slot, agent, model, kind
bin/tick.sh check | grep '^usage:'         # each agent's usage windows and the ticks they allow
tail -5 state/logs/allocator.log           # sizing decisions
ls -lt state/logs | head                   # recent ticks (0 bytes = still running)
cat state/locks/claim.* 2>/dev/null        # local claims: issue, PID, since
libexec/merge-lock.sh status                   # who holds the merge slot
gh issue list --label ai:wip               # what is claimed right now
```

`allocator.log` is the fastest health check. A line reads:

```
20260819T081558Z claude=4[five_hour:62%,weekly:53%] codex=8[primary:20%] allowed=8 busy=1 claimable=6 -> spawning 6
```

Each agent's cap and the usage of its windows (`unknown` without a fresh reading), the pool's cap
they sum to, how many slots are held, how many issues are unclaimed, and what it did. No line means the firing was a no-op — that is the normal case.
Other lines name a state the launcher noticed, logged once rather than at every firing: `held: <reason>`
(the instance's `hooks/hold`), `instance incomplete: <item>`, `hooks/env.sh failed`, a model that
hit its usage limit, or no fresh usage reading. A line with `ERROR queue count failed` means the
queue count could not be read (a GitHub outage, or a broken `QUEUE_DEPTH_CMD`); the launcher then
counts one claimable issue so a tick still runs.

## Is it stuck?

Usually not. Check in this order:

1. **`harness ticks` lists none and the queue is non-empty.** Look at the newest log:
   an `exit=1` trailer with a short duration means API or network failure — cron retries by
   itself. No trailer at all means the tick died without finishing; see LESSONS.
2. **A tick has run for ages.** Check what it holds and whether its process is alive. Long verification runs are expected; compare against your budget.
3. **Nothing spawns although slots are free.** Check `allocator.log`: the instance may be held
   or incomplete, usage may have hit a band that caps at 0, or `claimable` may be 0 because every
   issue carries the WIP label.
4. **Everything looks idle mid-turn.** You are probably between firings. Wait two minutes.

## Recovery

**Orphaned claim or merge lock.** `libexec/claim.sh status <issue>` and `libexec/merge-lock.sh status`
print `ORPHAN` when the holder PID is dead. The next acquire takes it over, so there is nothing to
delete. Clean up whatever the dead tick left running first.

**Stuck claims.** `ai:wip` on an issue whose claiming PID is gone is resumable by design —
a later tick continues from the recorded state. Remove the label by hand only if you want the
work abandoned rather than resumed.

**Container or service died.** The harness does not restart infrastructure. Bring it back,
and give the instance a `hooks/hold` that holds the harness while it is down, so no tick spends
its claim finding that out.

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
- **Audit the public record.** `"$HARNESS_BIN" lint --audit prs --last 20 --base <branch>`
  (and `--audit issues --label <label>`) tables the last bodies with their blocking findings. A
  body with a blocking finding was posted around the lint, or by a hand that skipped it; the fix
  is a rewrite of that body, and a runbook line if the cause is a rule that asks for host detail.
  `--pr N` and `--issue N` lint one body; the issue bodies you write yourself are held to the
  same contract, because ticks copy their shape.
- **Keep the runbook current.** When you rule on something — a convention's meaning, a merge
  policy, an excluded approach — write it into the runbook. A correction that lives only in
  chat will be re-litigated by the next tick.
- **Re-scope deliberately.** Discovery, fixing, and porting want different priorities. Say
  which phase you are in at the top of the runbook.
- **Watch the long windows, not just the short ones.** A 5-hour window recovers on its own; a
  weekly one is what actually ends a long run. The harness stops each window short of 90 %
  (`USAGE_CEILING_PCT`); `bin/tick.sh check` prints every agent's reading.
- **Pausing.** `touch state/paused` (or comment out the `tick.sh` cron line): the launcher
  starts nothing and runners chain nothing. A tick already running finishes its claim, so
  stop one that must not go on by its PID.
