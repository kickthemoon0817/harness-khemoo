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
`LANE_LABEL`), and the fleet keys (`TICK_AGENT`, `TICK_AGENT_EXES`, `FLEET_DISPATCH`, `FLEET_DEVICE`, `FLEET_BIN`,
`FLEET_CONFIG`). None of them is secret. No other config key reaches a tick's environment unless
`TICK_EXPORTS` lists it. `GH_TOKEN` never does: `tick.sh` unexports it, whether the environment,
`TICK_EXPORTS` or `hooks/env.sh` exported it, and passes it only to the queue count. `pr_sweep.sh` and
`worktree_gc.sh` export every key, because cron gives them no environment and `gh` reads `GH_TOKEN`.

The dashboard reads the same file without sourcing it. It matches plain `KEY=value` lines and keeps
only the keys it uses. It expands no `$` text except these names, as `$NAME` or `${NAME}`: in a
command key (`RESOURCE_STATUS_CMD`, `DEVICE_PROBE_CMD`, `QUEUE_DEPTH_CMD`, `FLEET_STATUS_CMD`), split shell-style first,
`$HARNESS_HOME` and `$HARNESS_INSTANCE_DIR`; in a path key (`HARNESS_STATE`, `USAGE_CACHE`,
`WORKTREE_ROOT`, `FLEET_CONFIG`), those two and `$HOME`. A path key from the environment wins over the file's. An
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
resolves on the PATH (`FLEET_BIN` instead with `FLEET_DISPATCH=1`, and `TICK_AGENT` other than
`claude` only then). Until then the launcher starts nothing. It logs
`instance incomplete: <first missing item>` to `$HARNESS_STATE/logs/allocator.log` at most once an
hour. `bin/tick.sh check` prints the resolved paths, repository, queue command, `CLAUDE_BIN`, models,
lane, the agent and where it runs, each hook's state (`present`, `none`, `present but not executable (ignored)`, or for `env.sh`
`present but not readable (ignored)`), the hold state,
and whether the harness is paused (`state/paused` exists, or no uncommented crontab line runs
`bin/tick.sh`). With `FLEET_DISPATCH=1` it also runs `fleet status --json` under
`FLEET_CHECK_TIMEOUT_S` (default 60 s) and prints each device, reachable with its `TICK_AGENT`
capacity or unreachable with fleet's error. It prints `missing: <item>` for each gap, including no
reachable device that can take a tick (`FLEET_DEVICE`, when it names one), and exits 1, or prints
`instance complete` and exits 0.

## Fleet

[`docs/FLEET.md`](FLEET.md) describes fleet. With `FLEET_DISPATCH=1` the runner runs each tick as

```
$FLEET_BIN dispatch --device ${FLEET_DEVICE:-auto} --adapter $TICK_AGENT --workdir $TARGET_REPO \
    --model <model> --wait --timeout <(LONG_TICK_BUDGET_MIN + 30) * 60> [--label tick=landing]
```

as an argv, with the composed prompt on stdin, instead of `$CLAUDE_BIN -p <prompt> $CLAUDE_FLAGS`.
fleet writes the agent's output to the tick log and exits with its status, so the model order, the
usage-limit cooldown, the exit trailer and chaining work as they do locally. The device's
`fleetd.toml` fixes the agent's command line, so `CLAUDE_BIN` and `CLAUDE_FLAGS` do not apply;
`templates/instance/fleetd.toml.example` lists what a device needs to take ticks: a harness checkout
and its own instance, with `HARNESS_HOME`, `HARNESS_INSTANCE_DIR` and `TARGET_REPO` set in the
adapter's `env` table, since fleetd gives a dispatched agent no environment of the hub's (see
[`docs/FLEET.md`](FLEET.md)). The adapter's `timeout_s` caps the tick: a device whose `timeout_s` is
under the `--timeout` above ends a tick before its budget does. The launcher counts
a live tick by its `fleet dispatch` process, and a landing tick by its label, since its prompt is not
in its argv.

An adapter's `exe` may be a wrapper script, but it must stay inside the adapter's `workdir_roots`:
on Linux, fleetd checks the child's working directory right after the spawn and kills its process
group when it is outside the roots, so a wrapper that changes directory elsewhere before it execs
the agent (to the harness checkout, say) has its tick killed. Pass such paths to the agent as
environment or arguments instead of `cd`-ing to them.

| Key | Default | Meaning |
|---|---|---|
| `FLEET_DISPATCH` | empty | `1` dispatches every tick through fleet |
| `TICK_AGENT` | `claude` | the adapter a tick runs; another value needs `FLEET_DISPATCH=1`, or `check` fails and the launcher logs the instance as incomplete |
| `TICK_AGENT_EXES` | `claude:-p claude-code:-p node:-p` | the agent processes a tick runs as, space-separated, each `exe` or `exe:arg`; `bin/tick-pid.sh` gives a claim to the nearest ancestor whose argv[0] or executable has base name `exe` and, with `:arg`, whose argv[1] is exactly `arg`. The argument keeps an operator's interactive `claude` from taking claims or the merge lock. Add `codex:exec` for codex ticks |
| `FLEET_DEVICE` | `auto` | a device name from `fleet.toml`, or `auto` |
| `FLEET_BIN` | `fleet` | the fleet CLI |
| `FLEET_CONFIG` | `$HARNESS_INSTANCE_DIR/fleet.toml` | the device registry (`templates/instance/fleet.toml.example`) |

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
`This tick is the merge slot's landing tick`; through fleet, the launcher recognises it by its label.

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
| `FLEET_STATUS_CMD` | Devices and Agent sessions, from `fleet status --json` (or `fleet sessions --json`, which gives the sessions only) every 30 s (`--fleet-interval`). Split shell-style and run without a shell, with `HARNESS_HOME`, `HARNESS_INSTANCE_DIR` and any `FLEET_CONFIG` in its environment as a tick's fleet has them, so a plain `fleet status --json` reads the instance's fleet.toml |

The host-health and stall panels appear when `$HARNESS_STATE/host_health.tsv` and
`$HARNESS_STATE/stalls.log` exist. The instance's own scripts write them. `host_health.tsv` has
tab-separated `time host connect_s upload_mb_s load1 nproc gpu_util_pct gpu_mem_used_mb status`
columns. `stalls.log` has `<UTC> STALL <kind> <key> <detail>` lines.

### `FLEET_STATUS_CMD` output

The document must carry `version` 1 and the fields [`docs/FLEET.md`](FLEET.md) gives, each of its
type; anything else is a failed read, and the panels keep the last good one. fleet exits 1 when a
device failed, with the document complete, so the document decides. The Devices panel shows each
device's transport, reachability or error, CPU, load, memory, GPUs and each adapter's running and
maximum dispatched sessions. The Agent sessions panel shows each session's device, adapter and
model, pid (else id), state, age, workdir, and its issue: the `issue` label, else the worktree its
workdir lies in. Every string from fleet goes through the dashboard's redaction. The command runs
without any variable whose name holds `TOKEN`, `SECRET`, `PASSWORD`, `PASSWD`, `API_KEY`,
`PRIVATE_KEY` or `CREDENTIAL`, in any case, and its stdout is capped at 8 MiB: past that it is
killed and the read fails.

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
