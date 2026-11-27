# The instance interface

The core (`bin/`, `prompts/`) holds no project, host or device. Everything specific to one
installation lives in its instance directory. The core reads that directory only through
`bin/instance-lib.sh` (`harness_load` and `harness_hook`), and the dashboard and the lint read it
the same way. `templates/instance/` is a working starting point, and `tests/fixtures/instance/`
is the smallest complete one.

## Resolution

| Name | Set by | Default |
|---|---|---|
| `HARNESS_HOME` | environment | the checkout that holds `bin/instance-lib.sh` |
| `HARNESS_INSTANCE_DIR` | environment only | `$HARNESS_HOME/instance` (gitignored) |
| `HARNESS_ENV` | environment only | `$HARNESS_INSTANCE_DIR/harness.env` |
| `HARNESS_STATE` | environment or `harness.env` | `$HARNESS_HOME/state` |

The config cannot name the directory it is read from, so `HARNESS_INSTANCE_DIR` and `HARNESS_ENV`
come from the environment only. `harness_load` sources the config, puts `$HARNESS_INSTANCE_DIR/bin`
first on PATH, and exports `HARNESS_HOME`, `HARNESS_INSTANCE_DIR`, `HARNESS_STATE`, `GH_REPO` and each
name in `TICK_EXPORTS`. An empty `GH_REPO` is derived from the `origin` remote of `TARGET_REPO`, as the
`[HOST/]OWNER/NAME` that `gh --repo` takes, so every `gh` call and queue count names the same
repository. An ssh remote's host goes through `ssh -G`, so a `~/.ssh/config` alias resolves to the
host it names; `github.com` gives `OWNER/NAME`, another host name with a dot gives
`HOST/OWNER/NAME`, and anything else (an unresolved alias, a `file://` remote) derives nothing, so
`check` asks for `GH_REPO`. A name in `TICK_EXPORTS` that is not a valid variable name is reported on stderr and
skipped.

`tick.sh` also exports `TARGET_REPO`, `WORKTREE_ROOT`, the lint's `PUBLIC_RECORD_RULES`,
`PUBLIC_RECORD_PR_SECTIONS` and `PUBLIC_RECORD_ISSUE_SECTIONS`, and the label keys (`ISSUE_LABEL`,
`WIP_LABEL`, `READY_LABEL`, `SIGNOFF_LABEL`, `OPERATOR_LABEL`, `LONG_LABEL`, `FIRST_LABEL`, `LOW_LABEL`,
`LANE_LABEL`). None of them is secret. No other config key reaches a tick's environment unless
`TICK_EXPORTS` lists it. `GH_TOKEN` never does: `tick.sh` unexports it, whether the environment,
`TICK_EXPORTS` or `hooks/env.sh` exported it, and passes it only to the queue count. `pr_sweep.sh` and
`worktree_gc.sh` export every key, because cron gives them no environment and `gh` reads `GH_TOKEN`.

The dashboard reads the same file without sourcing it. It matches plain `KEY=value` lines and keeps
only the keys it uses. It expands no `$` text except these names, as `$NAME` or `${NAME}`: in a
command key (`RESOURCE_STATUS_CMD`, `DEVICE_PROBE_CMD`, `QUEUE_DEPTH_CMD`), split shell-style first,
`$HARNESS_HOME` and `$HARNESS_INSTANCE_DIR`; in a path key (`HARNESS_STATE`, `USAGE_CACHE`,
`WORKTREE_ROOT`), those two and `$HOME`. A path key from the environment wins over the file's. An
empty `GH_REPO` is derived through `bin/instance-lib.sh`, as the scripts derive it. The dashboard runs `QUEUE_DEPTH_CMD`, else `bin/claimable.sh`, for its queue count,
and reads a worktree's issue from its directory under `WORKTREE_ROOT`.

## Files

| Path | | Purpose |
|---|---|---|
| `harness.env` | required, with `TARGET_REPO` and `WORKTREE_ROOT` set | the config; `templates/instance/harness.env.example` lists every key |
| `prompts/tick.md` | required | the installation's section of every tick prompt |
| `prompts/runbook.md`, `prompts/glossary.md`, `prompts/public-record.md` | optional | read by the tick when `tick.md` or the core prompt tells it to |
| `lint_rules.json` | optional | the project's lint rules (below) |
| `hooks/env.sh`, `hooks/hold`, `hooks/worktree-busy`, `hooks/worktree-remove` | optional | the hooks (below) |
| `bin/` | optional | the instance's scripts, first on PATH |
| `tests/test_*.sh`, `tests/test_*.py` | optional | run by `tests/run.sh` after the core suites |

The instance is usable when the required files are readable, `TARGET_REPO` is a git checkout,
`WORKTREE_ROOT` exists or its nearest existing parent is a writable directory, `GH_REPO` is set or
derivable, the first word of `QUEUE_DEPTH_CMD` is executable when that key is set, and `CLAUDE_BIN`
resolves on the PATH. Until then the launcher starts nothing. It logs
`instance incomplete: <first missing item>` to `$HARNESS_STATE/logs/allocator.log` at most once an
hour. `bin/tick.sh check` prints the resolved paths, repository, queue command, `CLAUDE_BIN`, models,
lane, each hook's state (`present`, `none`, `present but not executable (ignored)`, or for `env.sh`
`present but not readable (ignored)`), the hold state,
and whether the harness is paused (`state/paused` exists, or no uncommented crontab line runs
`bin/tick.sh`). It prints `missing: <item>` for each gap and exits 1, or prints `instance complete`
and exits 0.

## Queue count

`QUEUE_DEPTH_CMD` prints how many issues carrying `ISSUE_LABEL` a tick could claim, as one integer.
It runs with `GH_REPO`, `ISSUE_LABEL` (the label being counted), `WIP_LABEL`, `SIGNOFF_LABEL`,
`OPERATOR_LABEL`, `FIRST_LABEL`, `LOW_LABEL`, `LANE_LABEL` and, when the config sets it, `GH_TOKEN` in
its environment. The value is a command line that `tick.sh` splits on whitespace, without quoting, so
no path in it may contain a space. Unset, the launcher counts the open `ISSUE_LABEL` issues without
`WIP_LABEL` with `gh issue list`, run in `TARGET_REPO`, and the model lane and landing ticks stay off.

A count that fails or prints anything but an integer is an error, logged to `allocator.log` as
`ERROR queue count failed (label <l>: exit <n>, output '...', stderr '...')`. The launcher then counts
one claimable issue, so a GitHub outage does not stop every slot: the tick that starts ends early when
it finds nothing. A failed lane or landing count starts no lane or landing tick.

## Worktrees

A tick's worktree is the directory `$WORKTREE_ROOT/<name>-<issue>` (a name, a hyphen, the issue
number, as in `app-42`) on a `<BRANCH_PREFIX><issue>-<slug>` branch. `worktree_gc.sh` reads a
worktree's issue from its branch, else from the trailing number of its directory name, and never removes one that names no
issue. The dashboard reads the issue from the directory name the same way.

## Hooks

`harness_hook <name> [args]` returns 3 when the hook is absent. A hook whose name ends in `.sh` is
sourced into the caller's shell, with `set -u` lifted while it runs. Any other hook must be
executable. It runs under `timeout HOOK_TIMEOUT_S` when the caller sets that variable.

| Hook | Called | Contract |
|---|---|---|
| `hooks/env.sh` | sourced by `tick.sh` after the config, in launcher and runner mode (not in `check`) | what it exports reaches every tick; a non-zero status is logged once as `hooks/env.sh failed (exit N); ticks run without it` |
| `hooks/hold` | executed with no arguments, under `HOLD_TIMEOUT_S` (default 5 s), when the launcher starts, when a runner starts, and before each chained tick | exit 0 means held: the launcher starts nothing and runners chain nothing. The first line of stdout is the reason, logged as `held: <reason>` once until the state changes. Any other exit, or a timeout, means not held |
| `hooks/worktree-busy` | executed by `worktree_gc.sh` with no arguments, under `WORKTREE_BUSY_TIMEOUT_S` (default 30 s) | prints paths in use, one a line (for example what containers mount); a worktree that holds one of them is kept. A non-zero exit or a timeout removes nothing |
| `hooks/worktree-remove` | executed by `worktree_gc.sh --apply` with every removed worktree's path | removes what `git worktree remove --force` could not, such as root-owned files or copies elsewhere; its output and status are ignored |

The templates ship `hooks/env.sh.example` and `hooks/hold.example`. Copy them without the
`.example` suffix to turn them on, and keep `hold` executable.

## Prompt composition

Each tick's prompt is built in this order:

1. the lane line (below), then a blank line;
2. `$HARNESS_HOME/prompts/tick-core.md`, the rails every tick follows;
3. a `## This installation` heading;
4. `$HARNESS_INSTANCE_DIR/prompts/tick.md`.

The installation's section may narrow the core rails, never loosen them. The core prompt tells the
tick to read the instance's `prompts/glossary.md` and `prompts/public-record.md` when they exist, and
never to read `harness.env`.

In both files, `{{KEY}}` is replaced by that key's value from the config, with newlines turned into
spaces. The replacement escapes the value for `sed` and never `eval`s it. Only these keys are
replaced, and an unset key becomes empty:

`GH_REPO ISSUE_LABEL WIP_LABEL READY_LABEL SIGNOFF_LABEL OPERATOR_LABEL LONG_LABEL LANE_LABEL TICK_BUDGET_MIN LONG_TICK_BUDGET_MIN USAGE_CEILING_PCT`

## The lane line

The prompt's first line tells the tick which issues it may claim:

| Line | When |
|---|---|
| `This tick runs on <model>.` | the model lane is off |
| `This tick runs on <model>: never claim an issue labelled <LANE_LABEL>; those are worked on the model lane.` | the lane is on and this tick is not a lane tick |
| `This tick runs on <LANE_MODEL>, the model lane: claim only an issue labelled <LANE_LABEL>. ...` | claimable lane issues outnumber the young lane ticks |
| `This tick runs on <model>. <LANE_MODEL> is cooling down on its usage limit, so an issue labelled <LANE_LABEL> may be claimed too.` | `LANE_MODEL` is cooling down |
| `This tick is the merge slot's landing tick: claim only an issue labelled <READY_LABEL>. ...` | a landing tick started past the usage width while every allowed slot is held |

The lane is on only when both `LANE_MODEL` and `LANE_LABEL` are set, and it needs `QUEUE_DEPTH_CMD`
to count lane issues. The launcher and the dashboard recognise a landing tick by a prompt that starts with
`This tick is the merge slot's landing tick`.

## Lint rules

`bin/public_record_lint.py` always applies the core rules: `harness-path` (any path under
`WORKTREE_ROOT`, `HARNESS_HOME` or `HARNESS_STATE`), `host-path`, `host-address`, `process-id`,
`harness-file`, `tick-aside`, `container-tmp`, `operator-stamp`, and the `label-mention` warning for
the harness labels (`WIP_LABEL` through `LOW_LABEL`, as set in the environment). It reads these
settings from its environment, not from `harness.env`.

The installation's rules come from `PUBLIC_RECORD_RULES` when that is set, else from
`$HARNESS_INSTANCE_DIR/lint_rules.json` when that file exists:

```json
{
  "version_words": ["SDK"],
  "labels": ["ai:plan"],
  "rules": [
    {"id": "scratch-path", "pattern": "\\bscratch-runs/", "hint": "cite the repository path instead"},
    {"id": "device-slot", "pattern": "\\bdevice slot ?\\d\\b", "hint": "state the condition", "prose": true},
    {"id": "shorthand-target", "pattern": "\\bT[1-9]\\b", "hint": "name the target", "blocking": false}
  ]
}
```

- `version_words`: words after which a dotted quad reads as a version rather than an address. They
  are added to `v` and `version`.
- `labels`: more labels to warn on in prose.
- `rules`: each entry needs a non-empty `id`, `pattern` (a Python regex) and `hint`. `blocking`
  defaults to true, and false makes the rule a warning. `prose` defaults to false. A prose rule is
  skipped inside a closed fenced code block, and every other rule applies there too. A rule with a
  core rule's `id` replaces that core rule.

The lint fails closed. When `PUBLIC_RECORD_RULES` names a file that is missing, is not JSON, has an
unknown key, or has a malformed rule or regex, the lint prints the reason and exits 2, so nothing
gets posted. When the instance directory exists but holds no `lint_rules.json`, the lint applies
only the core rules and warns on stderr on every run. `--self-test` uses the core rules only. Exit
codes: 0 clean (warnings allowed), 1 blocking findings, 2 usage, rules file or fetch error.

## Dashboard sources

Every source below is optional. While its keys are empty, its panel stays off the page.

| Key | Panel |
|---|---|
| `DASHBOARD_TITLE` | the page title (default `Harness status`) |
| `RESOURCE_STATUS_CMD` | shared devices: slots, holders, waits. Split shell-style and run without a shell |
| `DEVICE_PROBE_CMD` | the device row of each machine, under a 5 s timeout. Split shell-style and run without a shell on this host. On each remote host ssh hands it to the remote user's shell, as one command whose words are each quoted, so that shell runs the same words and expands nothing in them. A probe that needs a shell names one: `sh -c '...'` |
| `RUN_QUEUE_GLOB`, `RUN_QUEUE_HOLDER`, `RUN_QUEUE_STOP_WORDS`, `RUN_QUEUE_LOG` | run queues on the flow board |
| `BUILD_LOCK_DIR`, `BUILD_LOCK_LOCAL`, `BUILD_LOCK_REMOTE`, `BUILD_LOCK_NAMES` | build lanes on the flow board |

The host-health and stall panels appear when `$HARNESS_STATE/host_health.tsv` and
`$HARNESS_STATE/stalls.log` exist. The instance's own scripts write them. `host_health.tsv` has
tab-separated `time host connect_s upload_mb_s load1 nproc gpu_util_pct gpu_mem_used_mb status`
columns. `stalls.log` has `<UTC> STALL <kind> <key> <detail>` lines.

### `RESOURCE_STATUS_CMD` output

One fact a line. A line that matches no form below is shown as unparsed.

```
remote device: <user@host> (slots <a>-<b>)          slots a..b live on that host
harness ticks: <host> <host> only, ...              the hosts harness ticks are kept to
local device: off ...                               this host's device takes no runs
slot <n> free
slot <n> HELD|ORPHAN <holder> <pid> <since> [tags...] (alive|pid dead)
device HELD|ORPHAN <holder> <pid> <since> [tags...] (alive|pid dead)
operator priority: <issue> <issue> ...
wait whole-device|whole-pool|single-slot issue <n> for <m> min [(<reason>)]
```

`card` and `host` are accepted for `device` in the `remote` and `local` lines, and `whole-card` for
`whole-device`. `<holder>` is an issue number or a name. A `domain=<n>` tag is shown as the slot's
domain. The tags `remote-all`, `held-for-all`, `exclusive` and `reserved-for-all` mark a hold on every
slot.

### `DEVICE_PROBE_CMD` output

```
<name>, <utilisation %>, <memory used MiB>, <memory total MiB>
<pid>, <memory MiB>, <process name>
...
```

The first line describes the device. Each further line is a process using it. Fields are
comma-separated, and a process name may contain commas. For an NVIDIA card, two queries joined by a
shell the probe names itself:

```
DEVICE_PROBE_CMD="sh -c 'nvidia-smi --query-gpu=name,utilization.gpu,memory.used,memory.total --format=csv,noheader,nounits | head -1; nvidia-smi --query-compute-apps=pid,used_memory,process_name --format=csv,noheader,nounits'"
```

### Run queues

A run queue is a process (its holder) that waits for a device and then runs a series of jobs,
writing what happens to a run directory. `RUN_QUEUE_GLOB` matches run directories (`~` is
expanded). Only directories written in the last 6 hours are read. A directory's parent is its
packet, and an issue number in the packet's name (`<YYYY-MM-DD>-<issue>-...`) ties it to the issue.
Each directory holds:

- the queue log, `RUN_QUEUE_LOG` (default `queue.log`): a `holder <pid> start ... want=<w>` line,
  then `<UTC> <answer>` lines, the device's answers to the holder. Trailing `BUSY <reason>` answers
  make the busy streak, and an `ACQUIRED` answer's `remote=<host>` names the host it got.
- `status`: `<job> start <UTC> ... host=<host>` while a job runs, until the job's end line or a line
  that starts with one of `RUN_QUEUE_STOP_WORDS`. A job ends with `<job> rc=<n> end`,
  `<job> void<n>: ...` (abandoned, with reason number n), `<job> skipped: ...`, or `<job> up failed`,
  `<job> sidecar failed` or `<job> no sidecar` (its environment did not come up). It also holds
  `granted: ...` lines, where `DOCKER_HOST=ssh://<host>` names the granted host (else this one), and
  `RELEASED <queue> <UTC>` as the last line once the queue ends.

The queue is live while its holder pid lives and, when `RUN_QUEUE_HOLDER` is set, while one of the
holder's first three arguments contains that word. When the packet name has no issue, an
`--issue <n>` argument of the holder supplies it.

### Build locks

`<BUILD_LOCK_DIR>/<BUILD_LOCK_LOCAL>` is this host's build lock. `<BUILD_LOCK_DIR>/<BUILD_LOCK_REMOTE>`
is a remote host's, with `{name}` replaced by the host's name from `BUILD_LOCK_NAMES`
(`<user@host>=<name> ...`). `BUILD_LOCK_DIR` defaults to `/tmp`. The holder is the pid written in
the lock file (`pid <n>, <tree>, ...`) while it lives. The waiters are the blocked entries that
`lslocks` reports for the file.
