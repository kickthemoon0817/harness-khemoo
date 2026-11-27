# claude-issue-harness

An autonomous loop that works a GitHub issue queue with Claude Code: it claims a labeled
issue, implements a fix in an isolated worktree, verifies it, opens a PR with real evidence,
and either lands it on a scratch branch or leaves it for human review. Concurrency is sized
from live API rate-limit headroom and queue depth, so it widens when you have budget and
throttles itself to zero when you do not.

It is a few hundred lines of bash around `claude -p` plus a runbook the ticks read. There is no
daemon, no database, and no server: cron fires a launcher, the launcher spawns runners, and all
durable state lives in GitHub issues and PRs where you can see it.

## How it works

```
cron (*/2)  ->  bin/tick.sh (launcher)  ->  bin/tick.sh runner  ->  claude -p
                      |                            |
                      |                            +- claims one issue, fixes it, opens a PR
                      +- decides HOW MANY runners to spawn:
                         min(usage-based cap - busy slots, unclaimed issues)
```

**Launcher.** Runs every couple of minutes and is ordered cheapest-first: it exits immediately
if every slot is held, refreshes usage telemetry only when the cache is stale, then reads the
queue. Most firings cost nothing and log nothing.

**Slots.** Concurrency is bounded by `flock`ed slot files. A runner that cannot take a slot
exits quietly, so spawning is racy-safe by construction.

**Allocator.** Neither the 5-hour nor the weekly utilization may reach `USAGE_CEILING_PCT`
(90 %). Ticks already running keep spending after spawning stops, so each window cuts the
pool well under the ceiling: the 5-hour window to half at 60 %, one slot at 70 % and none at
80 %; the weekly one to half at 80 %, one at 85 % and none at 88 % (`FIVE_*_AT`, `WEEK_*_AT`).
The tighter window decides. With no fresh usage data it starts nothing. A running tick checks
`bin/usage_ok.sh` before each build, suite, kit or agent and pauses two points under the
ceiling (`USAGE_TICK_STOP_PCT`). Decisions are appended to `state/logs/allocator.log`.

**Ticks.** Each tick is one `claude -p` invocation that reads `prompts/tick-prompt.md`, which
points at `prompts/runbook.md`. The runbook is where your project's rules live — the harness
itself knows nothing about your build, your tests, or your targets. A tick handles one issue and
ends as soon as it lands or pauses it; its budget is a ceiling set in the tick prompt.

**The public record.** Issues, PRs and comments are the harness's output and its only durable
state, and they are read by people who have the repository and nothing else. `prompts/public-record.md`
is the contract every body is written to (fixed sections, readings with their commit and condition,
nothing that is only true on the dev host), `prompts/glossary.md` holds the project's terms a body
defines on first use, and `bin/public_record_lint.py` blocks a post that cites a host path, a host,
a PID, a GPU slot or a harness file. The harness's own bookkeeping — the claim protocol line, host,
worktree, lease, resume recipe — goes under a comment's collapsed `Harness trace` block, where the
lint and the launcher both know to look; what a reader sees of a comment is the state in plain words.

**Chaining.** A runner keeps its slot after a tick ends and starts the next tick at once, on a
fresh context, while work is queued. It stops, leaving the retry to cron, when the queue is
empty, usage no longer allows its slot, a tick ended in under `CHAIN_MIN_TICK_S` (180 s, i.e.
nothing claimable or an error), after `MAX_CHAIN` (24) ticks, or when the harness is paused:
the `bin/tick.sh` line commented out of the crontab, or a `state/paused` file present.

**Landing tick.** While a merge slot is open, an issue labelled `ai:ready` waits for a tick.
When every slot the usage windows allow is held and no young tick is about to claim, the
launcher starts one tick past that width, told to claim only an `ai:ready` issue. It never
chains, and it never starts once a window allows no slot at all.

**Models.** `state/tick-model` (whitespace-separated, best first) or `TICK_MODELS` names the
model priority. Each tick runs on the first model that is not cooling down. A tick that dies on
a model's usage limit marks it in `state/model-cooldown/` for `MODEL_COOLDOWN_S` (1800 s) and
re-runs at once on the next model; the launcher spawns nothing while every model is cooling.

**Remote hosts.** `bin/host_health.sh` (cron, every 3 minutes) probes each host in
`state/locks/remote-card`: ssh connect time, upload rate (1 MB to `cat > /dev/null`), load,
cores, and the card's utilisation and memory, one row a host in `state/host_health.tsv`. A
host is degraded when it does not answer or uploads under `HOST_MIN_UPLOAD_MB_S` (5); it is
demoted after 2 bad probes in a row and promoted after 3 good ones. With `lease.sh
harness-only` on, the probe keeps the harness hosts to the operator's preference,
`state/locks/harness-host-preference` (one host a line, seeded once from the harness-only
list), with demoted hosts moved last, never dropped. It applies a changed order through `lease.sh
harness-only --on` and logs it to `state/host_health.log`; a running tick keeps its host.
To change the preferred order, edit the preference file: an order set with `lease.sh
harness-only --on` alone is replaced on the next probe.

**Stall alarm.** `bin/stall_watch.sh` (cron, every 5 minutes) appends `<UTC> STALL <kind>
<key> <detail>` lines to `state/stalls.log`: `lock_wait`, a process waiting `STALL_LOCK_MIN`
(15) minutes on a remote build or suite lock in `/tmp`, with what the holder is running;
`gpu_idle`, a card at 0 % on every probe of `STALL_GPU_IDLE_MIN` (30) minutes while a lock
has a waiter or the allocator's last line counts claimable work; `queue_wait`, a run queue whose lease log
has said BUSY for `STALL_QUEUE_MIN` (10) minutes on end while a card no live lease holds read 0 %, with
the queue's own BUSY reason; `host_degraded`, once per
demotion. A kind and key repeats at most every 30 minutes. It reads local state only.

**Leftover pull requests.** `bin/pr_sweep.sh` (cron, every 30 minutes) closes each open pull request on an
`ai/<n>-…` branch whose issue #<n> is closed, with a comment saying so. An issue that lands on another pull
request, or that the operator closes, otherwise leaves its own one open. The branch stays, so a closed pull
request can be reopened. `--dry-run` names them without closing; each close is logged to `state/logs/pr_sweep.log`.

```
*/3 * * * * /home/you/harness/bin/host_health.sh
*/5 * * * * /home/you/harness/bin/stall_watch.sh
*/30 * * * * /home/you/harness/bin/pr_sweep.sh
* * * * * /home/you/harness/bin/card_dispatch.sh
```

## Quickstart

```bash
git clone <this repo> ~/harness && cd ~/harness
cp config/harness.env.example config/harness.env   # set TARGET_REPO at minimum
cp prompts/runbook.template.md   prompts/runbook.md
cp prompts/tick-prompt.template.md prompts/tick-prompt.md
cp prompts/glossary.template.md    prompts/glossary.md
# fill in the <PLACEHOLDERS> in the prompt files — the runbook is the real work

gh label create ai     --description "Queued for the autonomous harness"
gh label create ai:wip --description "Claimed by a running tick"

./bin/tick.sh            # one manual firing; check state/logs/
crontab -e               # */2 * * * * /home/you/harness/bin/tick.sh
```

Label an issue `ai` and the next firing picks it up. Stop everything with `crontab -r`;
in-flight ticks finish on their own.

## Requirements

- `claude` (Claude Code) on PATH, authenticated
- `gh` authenticated with write access to the target repo
- `flock`, `setsid` (util-linux), bash 4+
- Optional: a usage-telemetry script writing `{timestamp, fiveHourPercent, weeklyPercent}`
  JSON. Without it the harness runs, capped at 3 slots.

## Layout

| Path | Purpose |
|---|---|
| `bin/tick.sh` | launcher + runner; the whole engine |
| `config/harness.env` | your paths, labels, and pool sizing (gitignored) |
| `prompts/runbook.md` | the rules ticks follow — mission, procedure, rails |
| `prompts/tick-prompt.md` | what a single tick is told to do |
| `prompts/public-record.md` | how every issue body, PR body and comment is written, for a reader with the repository alone |
| `prompts/glossary.md` | the project's terms, in the words a body uses to define them on first use |
| `bin/public_record_lint.py` | blocks a body that cites a host path, a host, a PID, a slot or a harness file; `--audit` tables the last N bodies |
| `tests/` | the harness's own tests, one script per tool (`bash tests/test_<tool>.sh`, `python3 tests/test_<tool>.py`) |
| `state/logs/` | one log per tick, plus `allocator.log` |
| `state/locks/` | slot locks, GPU slot leases (`resource.lease`, `resource.lease.2`, …), heartbeat stamp |
| `state/tick-model` | model priority list, best first |
| `state/model-cooldown/` | one stamp per model that hit its usage limit |
| `bin/slow_set.sh` | cron every 3 h: the whole doctest set, `[slow]` included, once per branch head; a red head files one issue naming the merge range |
| `state/paused` | present: no new ticks chain; pair with commenting out the cron line |
| `bin/host_health.sh` | cron every 3 min: probes the remote hosts into `state/host_health.tsv` and orders the harness hosts, degraded ones last |
| `bin/stall_watch.sh` | cron every 5 min: long lock waits, idle cards with work waiting and demoted hosts, to `state/stalls.log` |
| `bin/pr_sweep.sh` | cron every 30 min: closes open `ai/<n>-…` pull requests whose issue is closed |
| `bin/card_dispatch.sh` | cron every minute: unpins a queue waiting 5+ min on a busy card when another card is free, idle and quiet (`PULL` lines in `state/stalls.log`) |
| `bin/build_host.sh` | prints the `DOCKER_HOST` and `WORV_DEV_BUILD_LOCK` exports for the host whose compile lock is free |
| `bin/arm_queue.sh` | runs a packet's queue of scenario arms under one GPU lease it holds, then stops the kit and releases |
| `bin/dashboard.sh` | starts the status dashboard (`bin/dashboard.py`, `bin/dashboard_lib/`) on 127.0.0.1 and prints its URL |

## Scenario arm queues

A tick runs simulator arms through `bin/arm_queue.sh`, launched detached and polled, never through a
copy of a runner. The packet holds the queue file, the inputs and the outputs:

```bash
setsid nohup bin/arm_queue.sh --issue <N> --packet "$PKT" --queue "$PKT/arms.queue" \
    --deadline <epoch> --acquire-args "--all-remote" --collect /tmp/census.csv >/dev/null 2>&1 &
```

Each queue line is `tree|arm|scenario|extra_exts|kit_settings|seconds|outdir|flags`, with commas
inside a field (`--no-moments,--through-sim-s,13`). The script holds the lease as its own PID
(`LEASE_HOLDER_PID=$$`), polls `lease.sh acquire` until granted or the deadline, brings each tree up
with `iter.sh up` and `iter.sh sidecar`, renews the lease between arms and polls again after a
`YIELDED`, and on every exit, a `TERM` included, stops its container and releases. It writes
`<packet>/runs/status` in the arm queues' format (`<arm> start`, `<arm> rc=<n> end`, `skipped`,
`void<n>`, `STOP before`, `DEADLINE before`, `ALLDONE`), whose last line is always
`RELEASED <queue> <time>`, beside `lease.log`, `queue.log` and each arm's `.out` and card-memory log.
A STOP file in the packet ends the queue before its next arm. A kit crash or GPU out-of-memory
voids an arm and runs it once more. A parent arm names `evidence/parent-<base9>` as its outdir:
an empty folder there is the tick's mkdir claim, a missing one is claimed, a full one is skipped.
`bin/arm_queue.sh --help` lists the options: the whole-card host (`--want-host`), the margins,
the baseline runner, files to collect, host load sampling and the head check.
`tests/test_arm_queue.sh` runs it against fake lease, iter, docker, card and host commands.

## Status dashboard

`bin/dashboard.sh` starts `bin/dashboard.py` detached and prints its URL,
`http://127.0.0.1:8787/?t=<token>`. The token is new at each start, and every route refuses a
request without it. `dashboard.sh status` prints the URL again from `state/dashboard/dashboard.url`,
which only its owner can read. The server listens on 127.0.0.1 only. It answers only requests whose
Host is `127.0.0.1:<port>` or `localhost:<port>`, which stops a web page that rebinds its own name to
127.0.0.1. Every response forbids framing, MIME sniffing, cross-origin reads and any outside source.
A visible tab polls `/api/sections` every 30 s (`--refresh`). The index maps section names to
SHA-256 hashes. The tab fetches only new hashes from `/api/sections/<hash>` and patches the live
page. Hidden tabs resume on return. CSS travels with the initial HTML and `/dashboard.js` loads
once per navigation. Without JavaScript the page reloads at the same interval. `/api/state` serves
the full JSON snapshot. Responses support gzip and conditional GET with private revalidation.

```bash
bin/dashboard.sh                           # prints the URL
bin/dashboard.sh status                    # pid and URL
bin/dashboard.sh stop
HARNESS_HOME=/path/to/live/harness DASHBOARD_PORT=8790 bin/dashboard.sh   # read another checkout
```

`bin/dashboard.py --help` lists the options: the cadences, the sparkline window, the evidence root
(`EVIDENCE_ROOT`), the build locks' names and folder (`BUILD_LOCK_NAMES`, `BUILD_LOCK_DIR`, as
`bin/build_host.sh` reads them), how long a closed issue stays as done, and `--once`, which
prints one JSON snapshot and exits. The dashboard writes only its own history, GitHub cache, flow state,
URL, pid file and log, under `state/dashboard/` (`DASHBOARD_DATA_DIR`). From `config/harness.env` it reads `GH_TOKEN`,
`GH_REPO`, `BUILD_LOCK_NAMES` and the label keys (`ISSUE_LABEL`, `WIP_LABEL`, `SIGNOFF_LABEL`, `OPERATOR_LABEL`,
`READY_LABEL`, `FABLE_LABEL`, `FIRST_LABEL`, `LOW_LABEL`) and no other key; `--queue-label`
overrides `ISSUE_LABEL`. The token reaches `gh` only through its environment and is removed from
every response. Every GitHub call is a read.

| Panel | Reads | Cadence |
|---|---|---|
| Machines: load against cores, memory, card use and memory, card processes, containers (folded) | this host: `/proc`, `nvidia-smi`, `docker ps`, kept in `state/dashboard/local_history.tsv`; remote hosts: `state/host_health.tsv` and `.streaks` (status, upload, history), plus a read-only ssh probe for memory, card size, processes and containers (`--remote-interval 0` turns it off) | 30 s local, 2 min ssh |
| GPU slots and leases | `bin/lease.sh status` | snapshot cadence |
| Running ticks | `claude -p` processes whose stdout is a `state/logs/tick-*.log`: slot from the slot lock they hold, model from `--model`, issue from the newest `state/locks/claim.<issue>` naming the pid, else a lease the pid holds, else a cached GitHub search for its claim comment. "Now" summarises the tick's youngest tool call: the working command's executable and a few arguments, with secret-looking values masked. No raw command line is served | snapshot cadence |
| Issue queue and landings | GraphQL reads of the open `ai` issues, any `FIRST_LABEL` issue outside them, their `Depends on` states and the last day's merged PRs. Claimable follows `bin/claimable.sh`'s rules, its first-before-low gate included, and the panel lists what the gate holds back. The count is the dashboard's own; `bin/claimable.sh`'s (run every 5 min) and the allocator's stand beside it, marked unverified when taken while the dashboard's own read was failing, since `claimable.sh` prints 1 when `gh` fails | 1 min |
| Stalls, allocator | the tails of `state/stalls.log` and `state/logs/allocator.log` | snapshot cadence |
| Flow: each device with its card use, load, card memory, lease holders and compile lane; each issue as a card in its state column (waiting, claimed, compiling, waiting for a card, running on a card, landing, held, done), a line to each device lane it holds (solid) or waits for (dashed), and the latest transitions | the panels' data, plus the run folders `<evidence root>/*/runs*/` written in the last 6 h (`lease.log`: holder and BUSY streak; `status`: the arm running and its host), the build locks `<lock dir>/worv-dev-build-remote-<name>.lock` and `worv-dev-build.lock` (holder from the lock file, waiters from `lslocks`) and the issues closed in the last 6 h. Cards that changed state slide there on the next update; the last positions and transitions are kept in `state/dashboard/flow_state.json` | snapshot cadence (activity reads at least 10 s apart) |
| Usage | `~/.claude/usage-cache.json`, placed against the allocator's cut points (`bin/usage-lib.sh` defaults) | snapshot cadence |

A slow ssh or `gh` call never holds a page: each source keeps its last result, and the footer
shows its age and last error. `tests/test_dashboard.py` runs it against fixture state files and a fake
`lease.sh`, `claimable.sh` and `gh`.

## Tests

Each `tests/test_*.sh` builds a scratch harness home with fake commands and prints one `PASS`
or `FAIL` line a case; `tests/test_evidence_trim.py` runs under `python3`. The suites need Linux
with bash 4+, `flock`, `lslocks`, `bc` and `zstd`, and GNU `stat -c` and `date -d`.

## Documentation

- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — why the design is shaped this way
- [`docs/OPERATIONS.md`](docs/OPERATIONS.md) — running it, reading the logs, recovery
- [`docs/LESSONS.md`](docs/LESSONS.md) — failure modes found in production, and their fixes

Read `LESSONS.md` before your first long run. Every entry in it cost real time to discover.

## Safety posture

Ticks run unattended with permission prompts disabled, so the guardrails are in the runbook,
not in the tool layer. The defaults that matter:

- only labeled issues are ever worked;
- work happens in worktrees, never in your checkout;
- merging is scoped by base branch — the scratch branch may be landed by a tick, protected and
  shared branches are human-approval-only;
- no force-push, no reviewer assignment, no touching branches a tick did not create;
- evidence is archived honestly and gates are never weakened to obtain a pass;
- the public record is written for a reader with the repository alone, and linted before each
  post, so the issues and PRs never depend on one machine's paths, hosts or state.

Keep those in your runbook. They are the difference between an assistant and an incident.
