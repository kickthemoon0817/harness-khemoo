# Fleet: devices and agent sessions

Fleet monitors devices and the coding-agent sessions on them (Claude Code, Codex, any agent
CLI), and dispatches new agent sessions to them. It is a Cargo workspace under `fleet/`:

| Crate | Kind | Role |
|---|---|---|
| `fleet-proto` | library | every wire type, the framing and the handshake; the single source of truth for the protocol |
| `fleetd` | binary and library | the device daemon, one per device |
| `fleet` | binary and library | the hub: the client that the harness, the dashboard and people use |

The harness can run its ticks through fleet instead of a local `claude -p`, and the dashboard can
show fleet's devices and sessions. Both are optional; without fleet the harness behaves as it does
on one host.

## Architecture

```text
 harness tick / dashboard / person
              │  fleet status | sessions | dispatch | logs | cancel
              ▼
          fleet (hub) ── fleet.toml: the device registry
     ┌────────┼──────────────────────┐
   local     ssh                    tls
 unix socket  ssh -T … fleetd --stdio  TCP + mutual TLS 1.3
     │        │ (relays to the socket) │
     ▼        ▼                        ▼
   fleetd on each device ── fleetd.toml: the operator's policy
     ├─ inventory: CPU, memory, load, disks, GPUs
     ├─ discovery: agent processes matching the adapters
     └─ dispatch: agent CLIs run under the adapter's fixed rules
```

`fleetd` runs on each device as the user whose agents it watches and runs. It:

1. reports inventory and metrics: identity, CPU, memory, swap, load and disks through the
   `sysinfo` crate, and GPUs through an optional probe command run without a shell;
2. discovers agent sessions: processes matching an adapter's `match_exe`, plus the sessions it
   started itself. A discovered session exposes its pid, adapter, start time, working directory,
   executable and a redacted summary, never its argv or environment;
3. dispatches: it starts an adapter's agent CLI under the rules in
   [Dispatch sandbox](#dispatch-sandbox), then reports its state, streams its log, and cancels it.

`fleet` reads the device registry, talks to each device over its transport, and prints human
tables or, with `--json`, the documents in [JSON output](#json-output).

## Protocol

`fleet-proto` implements everything in this section; the daemon and the hub use its types and
never build frames by hand.

### Framing

A connection is a sequence of frames each way. A frame is a 4-byte big-endian unsigned length
followed by that many bytes holding exactly one JSON document (UTF-8). The length is at most
1 MiB (`MAX_FRAME_LEN`, 1048576). A peer that receives a longer length cannot resynchronise; it
closes the connection, after sending an error frame if it is the server.

Every inbound type rejects unknown fields, except `Hello`. Every string with a format is
validated while it is decoded, so a frame that decodes holds only valid values. Any change to a
wire type, an added field included, raises `PROTOCOL_VERSION`.

### Handshake

The first frame each way is a `Hello`:

```json
{"protocol_version": 1, "role": "monitor"}
```

1. The client sends its `Hello`, naming the role it asks for, and then waits. It sends nothing
   else until it has read the server's `Hello`.
2. The server reads it and answers with its own `Hello`, whose `role` is the role it grants: the
   lesser of the role asked for and the role the transport authenticated (see [Roles](#roles)).
3. Both sides compare `protocol_version` with their own (`PROTOCOL_VERSION`, now 1) before
   decoding anything else in the `Hello`. On a mismatch the server still answers with its
   `Hello` (granting `monitor`), then sends a `version_mismatch` error with id 0 and closes; the
   client closes on reading a `Hello` of another version.

`Hello` ignores unknown fields and is read version first, so a peer of a later version, even one
asking for a role this version does not know, still gets the mismatch reported. The server
applies `handshake_timeout_s` to the handshake (with the TLS handshake, on the tls transport).

Once a frame has been read from a connection, the server always completes the `Hello` exchange
before it sends any error, so a client can always decode what it is told. A server refusing a
connection after reading the client's `Hello` (a unix connection over `max_connections`, a TLS
certificate without a valid role or revoked, a relay with no daemon) answers with a `Hello`
granting `monitor`, then the error with id 0, then closes. A connection refused before any frame
is read (a wrong peer uid, a peer outside `allow_cidrs`, a tls connection over a limit) is closed
without a frame.

`Connection::client_hello` and `Connection::server_hello` implement both sides. A relay reads the
client's `Hello` with `Connection::recv_hello` (version first, as above), since it can answer only
once the daemon behind it has granted a role. It takes each stream back with
`Connection::into_parts`, which also returns any bytes already read past the `Hello`; it forwards
those first.

### Requests and responses

After the handshake the client sends requests, one at a time: it sends the next request only after
the previous one's last response frame. A request is

```json
{"id": 7, "body": {"op": "logs", "session": "d-1700000000-1a2b3c4d", "lines": 50, "follow": true}}
```

`id` is a positive integer the client chooses (`RequestId`, non-zero `u64`); every response frame
to that request carries it. A response is

```json
{"id": 7, "body": {"kind": "event", "data": "running tests…\n"}}
```

Response `id` 0 marks a connection-level error, one that belongs to no request (a frame that did
not decode, an oversize frame, a refused peer). The server closes the connection after sending it.
The server also treats a frame that arrives while a stream is in progress as such an error.

| `op` | Fields | Role | Answer |
|---|---|---|---|
| `status` | none | monitor | one `status` frame |
| `sessions` | none | monitor | one `sessions` frame |
| `logs` | `session`; `lines` (1 to 10000, default 200); `follow` (default false) | monitor | `event` frames with the last `lines` lines of the log; with `follow`, then the output as it arrives until the session ends; then `end` |
| `wait` | `session` | monitor | `event` frames with the whole log from its start, then the output as it arrives, then `end` when the session has ended |
| `dispatch` | `adapter`, `workdir`, `prompt`; `model`, `timeout_s`, `labels` optional | dispatch | one `dispatched` frame once the process has started |
| `cancel` | `session` | dispatch | one `cancelled` frame once SIGTERM has been sent; for a session that has already ended, `cancelled` at once and no signal |

`status` and `sessions` take no fields: `{"op": "status"}`.

| `kind` | Fields |
|---|---|
| `status` | `device` (`DeviceInfo`), `metrics` (`Metrics`), `adapters` (`AdapterInfo` list) |
| `sessions` | `sessions` (`Session` list): running sessions, then dispatched sessions that ended within `session_retention_s`, newest first, at most 256 (`MAX_SESSIONS`); `truncated` (boolean): whether any were left out |
| `dispatched` | `session`: the new session id |
| `cancelled` | `session` |
| `event` | `data`: a chunk of the session's combined stdout and stderr |
| `end` | `state`: the session's `SessionState` when the stream closed |
| `error` | `code`, `message` (see [Errors](#errors)) |

Any request may be answered by one `error` frame instead. A stream (`logs`, `wait`) ends with
either `end` or `error`.

#### Streams

- An `event`'s `data` is at most 64 KiB (`MAX_EVENT_DATA`). fleetd splits the log at UTF-8
  character boundaries and replaces bytes that are not UTF-8 with U+FFFD.
- While a stream is open and the session is silent, fleetd sends an `event` with empty `data`
  every 30 seconds. The hub treats a stream with no frame for `stream_idle_timeout_s` as dead.
  Readers skip empty events.
- `logs` without `follow`, and any stream for a session that has already ended, sends what the
  log holds and then `end` at once. `end` on a stream that did not follow can carry `running`.
- A client that disconnects mid-stream ends the stream only. The session keeps running.

#### Dispatch request

```json
{"id": 1, "body": {
  "op": "dispatch",
  "adapter": "claude",
  "workdir": "/srv/work/repo-42",
  "model": "sonnet",
  "prompt": "Fix the failing test in…",
  "timeout_s": 3600,
  "labels": {"issue": "42"}
}}
```

| Field | Rule |
|---|---|
| `adapter` | `[a-z0-9][a-z0-9_-]{0,31}` |
| `workdir` | absolute, at most 4096 bytes, no NUL |
| `model` | optional; `[A-Za-z0-9][A-Za-z0-9._:/@+-]{0,127}` |
| `prompt` | 1 byte to 256 KiB of UTF-8, with no control characters (C0, DEL or C1) other than tab, LF and CR. It is a JSON string, not base64. Excluding control characters bounds JSON escaping to twice the size, so a dispatch always fits in one frame. |
| `timeout_s` | optional; 1 to 604800 |
| `labels` | optional; at most 16; keys `[a-z0-9][a-z0-9_.-]{0,63}`, values at most 256 bytes with no control characters. `issue` holds an issue number by convention. A key repeated in the JSON object keeps its last value. |

A request never names an executable, arguments or environment: the decoder rejects such fields
as unknown.

### Types

```text
DeviceInfo  { name, hostname, os, kernel, arch, cpus: u32, fleetd_version }
Metrics     { sampled_at: unix s, uptime_s, load: [f64; 3], cpu_percent: f32 (0-100),
              memory: Memory, swap: Memory, disks: [Disk], gpus: [Gpu] }
Memory      { total_bytes, used_bytes }
Disk        { mount, total_bytes, available_bytes }
Gpu         { index: u32, name, util_percent: f32?, memory_total_bytes?, memory_used_bytes?,
              temperature_c: f32? }
AdapterInfo { name, models: [model], max_concurrent: u32, running: u32 }
Session     { id, adapter, origin: "dispatched" | "discovered", state: SessionState,
              pid: u32?, started_at: unix s, ended_at: unix s?, cwd?, exe?, model?, summary,
              labels: {key: value} }
SessionState: {"kind": "running"}
            | {"kind": "exited", "code": i32}
            | {"kind": "signalled", "signal": i32}
            | {"kind": "cancelled"}
            | {"kind": "timed_out"}
            | {"kind": "failed", "message": string}
```

`?` marks a field that is `null` when unknown. All byte counts are `u64` bytes; all times are
Unix seconds.

Every string in a report other than `event` data comes from the device. fleetd replaces control
characters in them with U+FFFD and cuts each at 1024 bytes (`MAX_REPORTED_STR`) on a character
boundary, which with `MAX_SESSIONS` keeps every reply within one frame. The hub still treats
them as untrusted: its human tables replace control characters again, and the dashboard passes
them through its redaction. `fleet logs` and `fleet dispatch --wait` write `event` data raw,
since it is the agent's own output.

- An `AdapterInfo` with `max_concurrent` 0 is discovery-only: it cannot be dispatched to.
  `models[0]` is the model a dispatch without `model` gets.
- `DeviceInfo.name`, fleetd's `device` key and the hub's registry keys are device names:
  `[a-z0-9][a-z0-9.-]{0,62}` (`DeviceName`).
- A session id matches `[A-Za-z0-9][A-Za-z0-9_-]{0,63}`. fleetd mints `d-<unix start>-<8 hex>`
  for a dispatched session and `p-<pid>-<unix start>` for a discovered one.
- `pid` is `null` once a dispatched session's process has gone.
- `signalled` means a signal fleet did not send. A cancel gives `cancelled`, a timeout gives
  `timed_out`, whatever signal ended the process.
- `failed` means fleetd could not start the process, or lost track of it across a restart.

### Errors

An error is `{"code": "...", "message": "..."}`. Codes are stable strings; scripts match on them.
A message is for people and never holds a prompt, environment value or secret.

| Code | Sent by | Meaning |
|---|---|---|
| `unauthorized` | fleetd | the role does not permit the request, or the peer's certificate carries no valid role or is revoked |
| `forbidden` | fleetd | the device's policy refuses it: workdir outside the roots, model not allowed, a discovery-only adapter, or `logs`, `wait` or `cancel` on a discovered session |
| `bad_request` | fleetd | the frame did not decode or a value failed validation |
| `not_found` | fleetd | no such adapter or session, or the workdir does not exist |
| `busy` | fleetd | the adapter is at `max_concurrent`, or the unix socket is at `max_connections` |
| `too_large` | fleetd | a frame exceeded 1 MiB |
| `version_mismatch` | fleetd, hub | the peers' protocol versions differ |
| `unavailable` | fleetd | `fleetd --stdio` found no running daemon to relay to |
| `internal` | fleetd | a fault inside fleetd, such as a failed spawn |
| `unreachable` | hub | the hub could not connect, or the transport failed during the handshake |
| `timeout` | hub | the device did not answer within the hub's timeouts |
| `protocol` | hub | the device broke the protocol: an undecodable frame, a wrong id, a missing `end` |

## Transports

All three carry the same frames, and the server side is the same code.

### local

The hub connects to fleetd's unix socket on the same host. The socket is mode 0600 inside a
directory of mode 0700 owned by fleetd's user. fleetd reads the peer's uid with
`UnixStream::peer_cred` and closes any connection whose uid is not its own, before reading a
frame. The role granted is at most `[unix] max_role`.

### ssh

The hub runs, as an argv with no shell:

```text
ssh -T -o BatchMode=yes [-o OPTION]... -- TARGET fleetd --stdio
```

and speaks the protocol over the child's stdin and stdout. ssh does the authentication.
`fleetd --stdio` is a relay: it reads the client's `Hello`, connects to the running daemon's unix
socket, completes the handshake there asking for the lesser of the client's role, `--role` and
`[stdio] max_role`, returns the granted `Hello` to the client, and then copies bytes both ways
until either side closes. Sessions therefore belong to the daemon and outlive the ssh connection.
If no daemon is running, the relay sends a `Hello` granting `monitor`, then an `unavailable`
error with id 0, and exits 1. If its config does not load, it answers the same way with an
`internal` error, and exits 2.

Pin the key's command in the device's `~/.ssh/authorized_keys`, so the key can do nothing else:

```text
command="/usr/local/bin/fleetd --stdio --role monitor",restrict ssh-ed25519 AAAA… hub-monitor
command="/usr/local/bin/fleetd --stdio --role dispatch",restrict ssh-ed25519 AAAA… hub-dispatch
```

This works the same over a LAN and over Tailscale (MagicDNS names as `TARGET`).

### tls

fleetd listens on TCP with rustls, TLS 1.3 only, and requires a client certificate.

- `fleet pki` makes a private CA (rcgen, ECDSA P-256). No webpki roots and no OpenSSL are used
  anywhere.
- fleetd accepts a client whose certificate chains to `[tls] ca`. The hub accepts a server whose
  certificate chains to its CA and carries a DNS SAN equal to the device's `server_name`.
- A client certificate carries its role as a SAN URI, `fleet:role:monitor` or
  `fleet:role:dispatch`. A certificate without exactly one such URI, or whose serial is in
  `revoked_serials`, is refused after the `Hello` exchange with an `unauthorized` error (id 0),
  and the connection closes.
  The role granted is the lesser of the certificate's role and the `Hello`'s.
- [Network policy](#network-policy) is applied before the TLS handshake.

## Security model

### What is trusted

| Trusted | Why |
|---|---|
| the device operator and `fleetd.toml` | the operator decides what may run on the device |
| fleetd's OS user | fleetd runs as it; anything with that uid already controls fleetd's agents |
| the hub operator, `fleet.toml` and the client keys | they hold the credentials |
| the CA key | it mints every identity; it stays on the machine that ran `fleet pki init` |

| Untrusted | How it is handled |
|---|---|
| the network and any peer before authentication | CIDR filter, mutual TLS, ssh, uid check, frame cap, timeouts, connection limits |
| request contents, even from an authenticated dispatcher | decoded strictly, validated, and limited to choices the adapter allows |
| device responses, at the hub | decoded strictly; displayed only through the dashboard's redaction |
| agent output | treated as data; never interpreted |

### Authentication and roles

| Transport | Authenticated by | Role granted at most |
|---|---|---|
| local | peer uid equals fleetd's uid | `[unix] max_role` (default `dispatch`) |
| ssh | ssh keys; `authorized_keys` pins the command | `--role` on the pinned command, and `[stdio] max_role` |
| tls | client certificate chaining to `[tls] ca` | the certificate's `fleet:role:` URI |

#### Roles

| Request | monitor | dispatch |
|---|---|---|
| `status`, `sessions`, `logs`, `wait` | yes | yes |
| `dispatch`, `cancel` | no | yes |

`Role::permits(op)` and `Op::required_role()` in fleet-proto are this table. fleetd checks every
request against the granted role, and refuses one it does not permit with `unauthorized`. The hub
asks for the least role a command needs: `dispatch` and `cancel` ask for `dispatch`, everything
else for `monitor`.

### Network policy

These apply to the tls transport:

- `listen` is an explicit `IP:port`. A host name is a config error. fleetd refuses to bind
  `0.0.0.0` or `::` unless `allow_cidrs` is non-empty.
- When `allow_cidrs` is non-empty, a peer whose address is in none of them is closed right after
  `accept`, before the TLS handshake. IPv4-mapped IPv6 addresses are matched as IPv4.
- The tls listener has its own connection budget, `[tls] max_connections`, apart from the unix
  socket's (top-level `max_connections`, which ssh relays also count against). A tls connection
  holds a place from `accept` to close, so peers that have not authenticated, or never will, can
  fill only the tls budget and never lock out local or ssh clients. Within it, at most
  `[tls] max_connections_per_ip` come from one address, and at most `[tls] max_handshakes` are
  still in their TLS handshake or `Hello`, so slow handshakes cannot fill it either; a finished
  handshake gives its slot back.
- A tls connection over any of these limits is closed right after `accept`, before the handshake;
  a unix one over `max_connections` gets a `Hello` granting `monitor`, then a `busy` error with
  id 0.
- `handshake_timeout_s` bounds the TLS handshake plus the `Hello`. `idle_timeout_s` closes a
  connection that sends no frame while it has no request in progress.

### Dispatch sandbox

A dispatch only selects among what the operator configured. For an adapter:

- `exe` is an absolute path, fixed in the config. There is no shell anywhere, and no PATH lookup.
- `args` is a fixed list. The only substitutions are `{model}`, the requested model if it is in
  the adapter's `models` allowlist (else `models[0]`), and `{workdir}`, the canonical workdir.
  Model names cannot begin with `-`, so they cannot read as options.
- The prompt goes to the child's stdin, never into argv. fleetd writes it, closes stdin, and does
  not store it.
- The workdir is canonicalized (symlinks resolved) and must be one of the adapter's
  `workdir_roots` or below one, compared by path components. The child runs there, and
  `{workdir}` is the canonical path.
- The environment is cleared. The child gets `HOME` and `PATH` from fleetd's environment, plus
  each name in `env_allow` that fleetd's environment sets, then the adapter's static `env` table.
  Secrets therefore come from the device, never from the request.
- At most `max_concurrent` dispatched sessions of the adapter run at once.
- The effective timeout is the lesser of the request's `timeout_s` and the adapter's.
- The child runs in its own process group. Its stdout and stderr are pipes that fleetd drains
  into `<state_dir>/sessions/<id>/output.log` (mode 0600), up to `log_max_bytes`; beyond that
  fleetd appends one line `[fleetd: log truncated]` and discards the rest while still draining, so
  the agent never blocks on output.
- On Linux, fleetd sets `RLIMIT_CORE` to 0 and `RLIMIT_NOFILE` to `rlimit_nofile` on the child
  with `prlimit` right after spawning it. Other systems get no rlimits.
- Cancel and timeout send SIGTERM to the process group, then SIGKILL after `kill_grace_s`, through
  rustix's safe wrappers. When the agent exits on its own, anything left in its group gets the
  same SIGTERM then SIGKILL, so no helper outlives its session.
- fleetd logs every dispatch and cancel at info level with the peer's identity: the certificate's
  subject and serial, or `unix uid N`. A `--stdio` relay reaches the daemon over the socket, so
  ssh peers log as `unix uid N`; the relay logs its own connection and granted role.

Discovery is read-only. It reports only processes of fleetd's own uid unless
`[discovery] all_users` is true. A process in the process group of a session fleetd dispatched is
reported once, as that dispatched session. A session's `summary`, dispatched or discovered, is the
executable's base name followed by the flag names in its argv (elements that start with `-`, cut
at any `=`), at most 8 flags and 120 bytes. Values and positional arguments, which can hold
prompts or secrets, are never included. A discovered session's `model` is the value after a
`--model` flag, when that value is a valid model name.

### Files

| Path | Mode | Owner |
|---|---|---|
| `state_dir` | 0700 | fleetd's user; fleetd creates it and refuses to start if it exists with another owner or group/other permission bits |
| the unix socket | 0600 | fleetd's user, inside a 0700 directory |
| `<state_dir>/sessions/<id>/session.json`, `output.log` | 0600 | fleetd's user |
| a TLS private key (`[tls] key`, the hub's `key`, `ca.key`) | 0600 or 0400 | its reader; fleetd and fleet refuse a key file with group or other permission bits |

### What fleet does not protect against

- A compromised account of fleetd's user, or of the hub operator.
- A dispatcher doing harm through an allowed agent in an allowed workdir. A `dispatch` role is
  the power to run that agent; grant it as such, keep certificates short-lived, and keep
  `workdir_roots` to directories only trusted users can write.
- A device that lies in its reports.
- Certificate revocation beyond `revoked_serials`; to revoke at scale, reissue the CA.

## Recipes

### LAN only (a company network)

The device is reachable only inside the office network and never from the internet.

`fleetd.toml` on the device:

```toml
device = "dev-a"

[tls]
listen = "192.0.2.10:7443"          # the device's LAN address, not 0.0.0.0
allow_cidrs = ["192.0.2.0/24"]      # the office subnet
ca = "/etc/fleetd/ca.pem"
cert = "/etc/fleetd/dev-a.pem"
key = "/etc/fleetd/dev-a.key"
```

`fleet.toml` on the hub:

```toml
[tls]
ca = "pki/ca.pem"
cert = "pki/hub.pem"
key = "pki/hub.key"

[devices.dev-a]
transport = "tls"
address = "192.0.2.10:7443"
```

Do not forward the port at the network edge. A host firewall that admits 7443 from the subnet
only is a second layer.

### Tailscale

Bind the device's tailnet address and admit only the tailnet range; tailnet ACLs decide which
nodes may reach the port.

```toml
device = "dev-a"

[tls]
listen = "100.64.0.10:7443"         # `tailscale ip -4` on the device
allow_cidrs = ["100.64.0.0/10"]
ca = "/etc/fleetd/ca.pem"
cert = "/etc/fleetd/dev-a.pem"
key = "/etc/fleetd/dev-a.key"
```

```toml
[devices.dev-a]
transport = "tls"
address = "dev-a:7443"              # MagicDNS
```

Start fleetd after tailscaled has the address (for systemd, `After=tailscaled.service` and
`Restart=on-failure`), since binding an address the host does not yet have fails. In the tailnet
policy, admit port 7443 on the devices only from the hub's tag.

### ssh only

Leave `[tls]` out of `fleetd.toml`, pin the hub's key with `command="…fleetd --stdio --role …",
restrict` (see [ssh](#ssh)), and register the device with `transport = "ssh"`. Nothing listens on
the network beyond sshd.

### Certificates

```bash
fleet pki init                                   # pki/ca.pem, pki/ca.key
fleet pki issue-server dev-a --ip 192.0.2.10     # pki/dev-a.pem, pki/dev-a.key
fleet pki issue-client hub --role dispatch       # pki/hub.pem, pki/hub.key
```

Copy `ca.pem`, `dev-a.pem` and `dev-a.key` to the device; `ca.key` never leaves the hub.

## fleetd.toml

fleetd reads, in order, `--config PATH`, `$FLEETD_CONFIG`, `$XDG_CONFIG_HOME/fleetd/fleetd.toml`,
`$HOME/.config/fleetd/fleetd.toml`. A missing file, a TOML error, an unknown key, a wrong type or
a value outside its range is a startup error that names the key, and fleetd exits 2. Paths must
be absolute unless the table says otherwise.

### Top level

| Key | Type | Default | Meaning |
|---|---|---|---|
| `device` | string | required | the device's name, a `DeviceName`: 1 to 63 of `[a-z0-9.-]`, starting with a letter or digit. It is `DeviceInfo.name` and, with `[tls]`, must be a DNS SAN of `[tls] cert`. |
| `state_dir` | path | `$XDG_STATE_HOME/fleetd`, else `$HOME/.local/state/fleetd` | sessions, logs and the default socket; see [Files](#files) |
| `session_retention_s` | integer ≥ 0 | 604800 | how long an ended dispatched session (metadata and log) is kept |
| `log_max_bytes` | integer ≥ 4096 | 67108864 | per-session log cap |
| `kill_grace_s` | integer 1 to 300 | 10 | the wait between SIGTERM and SIGKILL |
| `max_connections` | integer 1 to 4096 | 32 | open unix socket connections, the ssh relays' included; tls has its own budget |
| `handshake_timeout_s` | integer 1 to 300 | 10 | TLS handshake plus `Hello` |
| `idle_timeout_s` | integer ≥ 1 | 300 | a connection with no request in progress that sends nothing this long is closed |

### `[unix]`

The unix listener always runs; the ssh relay depends on it.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `socket` | path | `<state_dir>/fleetd.sock` | fleetd creates the parent directory 0700 if missing and refuses one with another owner or group/other bits. At startup a socket file nothing answers on is removed; one something answers on means another fleetd is running, and fleetd exits 1. |
| `max_role` | `"monitor"` or `"dispatch"` | `"dispatch"` | the most a same-uid local peer gets |

### `[stdio]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `max_role` | `"monitor"` or `"dispatch"` | `"dispatch"` | the most `fleetd --stdio` asks for; `--role` can only lower it |

### `[metrics]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `sample_interval_s` | integer 1 to 3600 | 5 | how often CPU and the GPU probe are sampled; `status` returns the latest sample |
| `disks` | array of paths | `["/"]` | mount points to report; one not mounted is left out |
| `gpu_probe` | array of strings | absent (no GPUs) | the probe's argv; `gpu_probe[0]` absolute. It runs with a cleared environment plus `PATH`. |
| `gpu_probe_timeout_s` | integer 1 to 60 | 5 | a probe still running is killed, and that sample has no GPUs |

The probe prints one line per GPU: `index, name, utilization %, memory total MiB, memory used
MiB, temperature °C`, comma-separated. That is the output of

```toml
gpu_probe = ["/usr/bin/nvidia-smi",
  "--query-gpu=index,name,utilization.gpu,memory.total,memory.used,temperature.gpu",
  "--format=csv,noheader,nounits"]
```

A field that is empty or `[N/A]` becomes `null`. A line that does not parse is skipped, and
logged at warn level at most once per minute. Other vendors' tools need a small wrapper that
prints the same lines.

### `[discovery]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `all_users` | boolean | false | also report other users' agent processes; their `cwd` and `exe` are `null` where unreadable |

Discovery scans the process table on each `sessions` request, and when a `logs`, `wait` or
`cancel` names a session fleetd did not dispatch.

### `[tls]`

Absent means fleetd listens on no TCP port.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `listen` | string | required | `IP:port`, such as `192.0.2.10:7443` or `[2001:db8::10]:7443`; no host names |
| `allow_cidrs` | array of CIDR strings | `[]` | peers admitted before the handshake; empty admits any peer that reaches `listen`, and is refused with a wildcard `listen` |
| `ca` | path | required | PEM file with one or more CA certificates that client certificates must chain to |
| `cert` | path | required | PEM chain, leaf first; the leaf must have a DNS SAN equal to `device`, checked at startup |
| `key` | path | required | PEM private key (PKCS#8, SEC1 or PKCS#1), mode 0600 or 0400, owned by fleetd's user |
| `max_connections` | integer 1 to 4096 | 64 | open tls connections, from `accept` to close |
| `max_connections_per_ip` | integer 1 to `max_connections` | 8, or `max_connections` if lower | open tls connections from one address |
| `max_handshakes` | integer 1 to `max_connections` | 32, or `max_connections` if lower | tls connections still in the TLS handshake or `Hello` |
| `revoked_serials` | array of strings | `[]` | client certificate serials to refuse, as hex (case, `:` separators and leading zeros ignored) |

### `[adapters.<name>]`

One table per adapter; `<name>` follows the adapter rule `[a-z0-9][a-z0-9_-]{0,31}`. An adapter
without `exe` is discovery-only.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `preset` | `"claude"` or `"codex"` | none | fills every key below that the table leaves out, except `exe` and `workdir_roots` |
| `exe` | path | none | the agent CLI. Must be an executable regular file at startup. Absent: discovery-only. |
| `args` | array of strings | preset, else `[]` | the fixed argv after `exe`. `{model}` and `{workdir}` are replaced wherever they appear; any other `{` or `}` is a config error. |
| `models` | array of model names | preset, else `[]` | the allowlist; `models[0]` is the default. Required non-empty when `args` uses `{model}`. A request naming a model not listed is `forbidden`. |
| `workdir_roots` | array of paths | required with `exe` | each must exist; canonicalized at startup |
| `env_allow` | array of names | `[]` | variables copied from fleetd's environment; each `[A-Za-z_][A-Za-z0-9_]*`. `HOME` and `PATH` are always copied. |
| `env` | table of strings | `{}` | variables the operator sets for the child, applied after the `env_allow` copy, so they win over it; names `[A-Z_][A-Z0-9_]*` (`HOME` and `PATH` included), values without NUL. `--check` prints them, so secrets belong in `env_allow` instead |
| `max_concurrent` | integer 1 to 64 | preset, else 1 | dispatched sessions running at once |
| `timeout_s` | integer 1 to 604800 | preset, else 3600 | the longest a session may run |
| `match_exe` | array of strings | preset, else the base name of `exe` | process names (or executable base names) that discovery attributes to this adapter |
| `rlimit_nofile` | integer ≥ 64 | 4096 | the child's open-file limit, on Linux |

A discovery-only adapter needs `match_exe`; `max_concurrent`, `timeout_s`, `workdir_roots`,
`env_allow` and `env` do not apply to it, and it reports `max_concurrent` 0.

Presets:

| Preset | `args` | `models` | `match_exe` | `max_concurrent` | `timeout_s` |
|---|---|---|---|---|---|
| `claude` | `["-p", "--model", "{model}"]` | `["opus", "sonnet", "haiku"]` | `["claude"]` | 1 | 3600 |
| `codex` | `["exec", "--model", "{model}", "-"]` | none: set `models` | `["codex"]` | 1 | 3600 |

A preset grants the agent no permissions of its own. An unattended agent usually needs a
permission flag, and the operator adds it to `args` knowingly.

Example:

```toml
device = "dev-a"

[metrics]
disks = ["/", "/srv"]

[adapters.claude]
preset = "claude"
exe = "/usr/local/bin/claude"
args = ["-p", "--model", "{model}", "--permission-mode", "acceptEdits"]
workdir_roots = ["/srv/work"]
env_allow = ["ANTHROPIC_API_KEY"]
max_concurrent = 2

[adapters.aider]
match_exe = ["aider"]               # discovery only
```

### Running harness ticks on a device

A device that runs the harness's dispatched ticks needs its own harness checkout with its own
instance. Point the adapter at them with `env`, and give the claude preset a `timeout_s` that
covers the tick budget:

```toml
[adapters.claude]
preset = "claude"
exe = "/usr/local/bin/claude"
workdir_roots = ["/srv/work"]
env_allow = ["ANTHROPIC_API_KEY"]
timeout_s = 7200                    # at least the tick budget

[adapters.claude.env]
HARNESS_HOME = "/srv/harness"
HARNESS_INSTANCE_DIR = "/srv/harness-instance"
TARGET_REPO = "/srv/work/repo"
```

### Lifecycle

- fleetd persists each dispatched session's `session.json` under `state_dir`. After a restart,
  a session recorded as running becomes `failed` with the message `fleetd restarted while the
  session ran`. Its processes are not signalled; any still alive show up through discovery.
- On SIGTERM or SIGINT, fleetd stops accepting, closes its connections, removes its socket and
  exits 0. Dispatched sessions are not signalled, but their output pipes close with fleetd, so
  an agent that writes again gets SIGPIPE or EPIPE.
- Ended sessions older than `session_retention_s` are deleted, metadata and log together, as are
  the oldest ended sessions beyond the newest 1024.

## fleet.toml

fleet reads `--config PATH`, else `$FLEET_CONFIG`, else `$HARNESS_INSTANCE_DIR/fleet.toml`. No
path, a missing file, a TOML error, an unknown key, a wrong type or a value outside its range is
an error that names the key, and fleet exits 2. Relative paths resolve against the directory of
fleet.toml; a leading `~/` expands to `$HOME`.

### Top level

| Key | Type | Default | Meaning |
|---|---|---|---|
| `connect_timeout_s` | integer 1 to 300 | 10 | connect, transport handshake and `Hello` |
| `request_timeout_s` | integer 1 to 3600 | 30 | a single-frame answer |
| `stream_idle_timeout_s` | integer ≥ 60 | 120 | a stream with no frame this long is dead |
| `ssh` | string | `"ssh"` | the ssh program; a bare name is looked up in PATH |

### `[tls]`

The client identity for tls devices. Required if any device uses `transport = "tls"` without its
own keys.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `ca` | path | required | the CA that server certificates must chain to |
| `cert` | path | required | the client certificate (PEM chain, leaf first) |
| `key` | path | required | its private key, mode 0600 or 0400 |

### `[devices.<name>]`

`<name>` is a `DeviceName` (1 to 63 of `[a-z0-9.-]` starting with a letter or digit), and not `auto`. Devices keep
their file order, which is the registry order everywhere.

| Key | Type | Default | Applies to | Meaning |
|---|---|---|---|---|
| `transport` | `"local"`, `"ssh"` or `"tls"` | required | all | |
| `auto` | boolean | true | all | whether `--device auto` may pick it |
| `socket` | path | `$XDG_STATE_HOME/fleetd/fleetd.sock`, else `~/.local/state/fleetd/fleetd.sock` | local | fleetd's socket |
| `target` | string | required | ssh | the ssh destination, `host` or `user@host`; must not begin with `-` |
| `ssh_options` | array of strings | `[]` | ssh | each passed as `-o OPTION`; each must be `Key=value` |
| `remote_command` | array of strings | `["fleetd", "--stdio"]` | ssh | appended after the target; ignored by a pinned `command=` |
| `address` | string | required | tls | `host:port`; the host may be an IP or a DNS name |
| `server_name` | string | the device's `<name>` | tls | the DNS SAN the server certificate must carry |
| `ca`, `cert`, `key` | paths | `[tls]` | tls | this device's own client identity |

Example:

```toml
[tls]
ca = "pki/ca.pem"
cert = "pki/hub.pem"
key = "pki/hub.key"

[devices.here]
transport = "local"

[devices.dev-a]
transport = "tls"
address = "192.0.2.10:7443"

[devices.dev-b]
transport = "ssh"
target = "dev@dev-b.example.com"
ssh_options = ["Port=2222"]
auto = false
```

## CLI

### fleetd

```text
fleetd [--config PATH] [--log-level LEVEL]
fleetd --stdio [--role monitor|dispatch] [--config PATH]
fleetd --check [--config PATH]
fleetd --version | --help
```

| Mode | Behaviour |
|---|---|
| none | runs the daemon in the foreground, logging to stderr; for a service manager |
| `--stdio` | the ssh relay (see [ssh](#ssh)); `--role` defaults to `dispatch` and can only lower `[stdio] max_role` |
| `--check` | loads and validates the config, including file modes, `exe` and `workdir_roots`, prints the resolved settings, exits 0 or 2 |

`--log-level` is `error`, `warn`, `info` (default) or `debug`. Exit codes: 0 after a clean
shutdown, 1 on a runtime failure, 2 on a usage or config error.

### fleet

```text
fleet [--config PATH] <command>

fleet status   [--json] [--device NAME]...
fleet sessions [--json] [--device NAME]... [--running]
fleet dispatch --device NAME|auto --adapter NAME --workdir PATH [--model M] [--timeout S]
               [--label KEY=VALUE]... [--wait] [--json]          (prompt on stdin)
fleet logs     <device> <session> [--lines N] [--follow] [--json]
fleet cancel   <device> <session> [--json]
fleet pki init         [--dir DIR] [--days N]
fleet pki issue-server <device> [--dir DIR] [--ip IP]... [--dns NAME]... [--days N]
fleet pki issue-client <name> --role monitor|dispatch [--dir DIR] [--days N]
fleet --version | --help
```

- `status` asks every device (or each `--device`) concurrently. It prints a table, or with
  `--json` the [status document](#fleet-status---json). It exits 0 when every device answered and
  1 when any failed; the output is complete either way.
- `sessions` is the same for sessions. `--running` keeps only sessions in state `running`.
- `dispatch` reads the prompt from stdin, up to the prompt cap; an empty or oversize prompt is an
  error before any connection. Without `--wait` it prints the device and session id
  (`{"device": "dev-a", "session": "d-…"}` with `--json`) and exits 0. With `--wait` it then sends
  `wait`, writes the session's output to stdout as it arrives, and exits with the agent's status:

  | Session end | Exit code |
  |---|---|
  | `exited` | its code |
  | `signalled` | 128 + the signal |
  | `timed_out` | 124 |
  | `cancelled` | 130 |
  | `failed`, or fleet itself failed (refused, unreachable, lost stream) | 125, with the reason on stderr |

  If fleet receives SIGINT or SIGTERM while waiting, it cancels the session over a new connection,
  best effort, and exits 128 + the signal.
- `--device auto` asks every device with `auto = true` for its status, concurrently, and keeps
  those that answered, have the adapter with `max_concurrent` above 0 and `running` below it,
  and allow `--model` if one is given. It orders them by `load[0] / cpus`, lowest first, ties in
  registry order, and dispatches to each in turn until one accepts. With none left it fails with
  `not_found` when no device offers the adapter and model, `busy` when all that do are full, and
  `unreachable` when none answered.
- `logs` prints the last `--lines` lines (default 200) and, with `--follow`, follows until the
  session ends.
- With `--json`, `logs` and `dispatch --wait` print one JSON object per line instead of raw
  output: `{"data": "…"}` per non-empty event, then `{"state": {…}}` last.
- `cancel` prints the cancelled session and exits 0. Cancelling an ended session succeeds.
- `pki` writes into `--dir`, by default `pki/` beside the resolved fleet.toml. The directory is
  created 0700, key files are written 0600, and no file is overwritten: an existing file is an
  error. Keys are ECDSA P-256 and serials random. `init` makes `ca.pem` and `ca.key` (default
  3650 days). `issue-server` makes `<device>.pem` and `<device>.key` with DNS SAN `<device>`,
  each `--dns` and each `--ip`, and serverAuth usage. `issue-client` makes `<name>.pem` and
  `<name>.key` with the SAN URI `fleet:role:<role>` and clientAuth usage. Leaves default to 397
  days. Each prints the files it wrote and the serial in hex.

Exit codes, except `dispatch --wait`: 0 success, 1 a device or operation failed, 2 a usage or
config error. Errors go to stderr as `fleet: <device>: <code>: <message>`. Human tables are not
a stable interface; scripts use `--json`.

## JSON output

Both documents carry `version` (`OUTPUT_VERSION`, now 1), raised only when a field is removed or
changes meaning. Scripts reading them ignore fields they do not know. The types are in
`fleet_proto::output`; the wire types nested in them decode strictly in Rust and change only with
`PROTOCOL_VERSION`.

### `fleet status --json`

```json
{
  "version": 1,
  "generated_at": 1700000000,
  "devices": [
    {
      "name": "dev-a",
      "transport": "tls",
      "ok": true,
      "error": null,
      "status": {
        "device": {"name": "dev-a", "hostname": "dev-a.example.com", "os": "Ubuntu 24.04",
                   "kernel": "6.8.0", "arch": "x86_64", "cpus": 16, "fleetd_version": "0.1.0"},
        "metrics": {"sampled_at": 1700000000, "uptime_s": 86400, "load": [0.5, 0.25, 0.12],
                    "cpu_percent": 12.5,
                    "memory": {"total_bytes": 68719476736, "used_bytes": 8589934592},
                    "swap": {"total_bytes": 0, "used_bytes": 0},
                    "disks": [{"mount": "/", "total_bytes": 1099511627776,
                               "available_bytes": 549755813888}],
                    "gpus": [{"index": 0, "name": "Example GPU", "util_percent": 50.0,
                              "memory_total_bytes": 25769803776, "memory_used_bytes": null,
                              "temperature_c": 61.0}]},
        "adapters": [{"name": "claude", "models": ["sonnet", "opus"], "max_concurrent": 2,
                      "running": 1}]
      },
      "sessions": [
        {"id": "d-1700000000-1a2b3c4d", "adapter": "claude", "origin": "dispatched",
         "state": {"kind": "running"}, "pid": 4242, "started_at": 1700000000, "ended_at": null,
         "cwd": "/srv/work/repo-42", "exe": "/usr/local/bin/claude", "model": "sonnet",
         "summary": "claude -p --model", "labels": {"issue": "42"}}
      ],
      "sessions_truncated": false
    },
    {
      "name": "dev-b",
      "transport": "ssh",
      "ok": false,
      "error": {"code": "unreachable", "message": "ssh exited 255"},
      "status": null,
      "sessions": [],
      "sessions_truncated": false
    }
  ]
}
```

| Field | Meaning |
|---|---|
| `devices` | one entry per device asked, in registry order |
| `name`, `transport` | the registry name and transport |
| `ok` | true when the device answered; then `status` is set and `error` is null, otherwise the reverse |
| `status` | the device's `status` answer: `device`, `metrics`, `adapters` as in [Types](#types) |
| `sessions` | the device's `sessions` answer; empty when `ok` is false |
| `sessions_truncated` | the answer's `truncated` |

### `fleet sessions --json`

```json
{
  "version": 1,
  "generated_at": 1700000000,
  "sessions": [
    {"device": "dev-a", "session": {"id": "p-5151-1699990000", "adapter": "codex",
     "origin": "discovered", "state": {"kind": "running"}, "pid": 5151,
     "started_at": 1699990000, "ended_at": null, "cwd": "/srv/work/repo-7",
     "exe": "/usr/local/bin/codex", "model": null, "summary": "codex", "labels": {}}}
  ],
  "errors": [
    {"device": "dev-b", "error": {"code": "timeout", "message": "no answer in 30s"}}
  ],
  "truncated": []
}
```

`sessions` is ordered by device in registry order, then by `started_at`. `errors` lists the
devices that could not be asked, and `truncated` those whose answer was truncated. The dashboard
shows `device`, `adapter`, `pid` or `id`, the age from `started_at`, `cwd`, and the `issue` label
when present.

## Building and testing

```bash
cd fleet
cargo build --release          # target/release/fleetd, target/release/fleet
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo deny check advisories bans licenses sources
```

Every crate forbids `unsafe` code. TLS is rustls with the ring provider; there is no OpenSSL. CI
(`.github/workflows/ci.yml`) runs the commands above and the core's `tests/run.sh`.
