# agent-cli-x

A small, single-binary CLI that lets AI agents on different machines (Windows or Linux) run commands and edit files on each other.

```
Machine A: Claude Code ──> agent-cli-x ──TLS──> Machine B: agent-cli-x serve ──> exec / read / edit
Machine B: Codex       ──> agent-cli-x ──TLS──> Machine A: agent-cli-x serve ──> exec / read / edit
```

- **One binary, both roles.** Every machine runs `agent-cli-x serve`, and every machine can call its peers.
- **Lightweight.** Synchronous std + rustls, no async runtime, no OpenSSL. Short-lived HTTPS request per call.
- **Machine identity.** Every node has a configurable `node_name` and a random `node_id` (UUID), so agents know exactly which machine they are talking to.
- **Built for agents.** Exit codes pass through, long output is trimmed to head/tail with the full log saved locally, `--json` everywhere, exact-match `edit`, `rollback`.
- **Many machines.** Run one command on several peers at once (`exec A,B,C` / `@all`), issue a separate revocable token per caller, push upgrades remotely, and check everything with `doctor`.
- **Guard rails.** Token auth, self-signed TLS with fingerprint pinning, workspace allow-list, path deny-list, command deny-list, automatic backups, audit log.

## Install

Prebuilt binaries for Linux (x86_64, statically linked) and Windows (x86_64) are on the [releases page](https://github.com/Fracizz/agent-cli-x/releases).

```bash
cargo install --git https://github.com/Fracizz/agent-cli-x
```

Or build with `cargo build --release` and put `target/release/agent-cli-x(.exe)` on your `PATH`:

- **Windows:** copy to e.g. `%USERPROFILE%\bin` and add that folder to the user `Path` (System Properties → Environment Variables).
- **Linux:** `install -m 755 target/release/agent-cli-x ~/.local/bin/`

Having it on `PATH` lets agents call `agent-cli-x` from any working directory.

## Quick start

On **each** machine:

```bash
agent-cli-x init --name build-01 --workspace proj=/home/me/proj   # prints node id, token, fingerprint
agent-cli-x serve                                   # keep running; set up start at boot (see below)
```

`init` prints a ready-to-paste `peer add` command. To give each caller its own token instead, run on B:

```bash
agent-cli-x token add A          # prints the exact `peer add` command for machine A
```

Then on the machine that wants to call B (A here):

```bash
agent-cli-x peer add B 10.0.0.5:47821 --token <token> --fingerprint <B's fingerprint>
agent-cli-x info B
agent-cli-x exec B cargo test
```

Without `--fingerprint`, `peer add` only prints the fingerprint it sees. Compare it with `agent-cli-x fingerprint` on B before registering. `agent-cli-x whoami --pair` prints the command again later.

Set up [start at boot](#start-at-boot-recommended) on every machine, then run `agent-cli-x doctor` to verify.

Then teach your agent the tool: copy [`skills/agent-cli-x/SKILL.md`](skills/agent-cli-x/SKILL.md) into its skills folder (e.g. `~/.claude/skills/agent-cli-x/`), or run `agent-cli-x skill` to print it.

## Commands

| Command | Purpose |
|---|---|
| `init` / `serve` / `whoami [--pair]` / `fingerprint` | set up, run and identify this machine |
| `doctor [--no-peers]` | check config, local service, start at boot, firewall, peers and their versions |
| `token add\|list\|remove\|rotate` | per-caller tokens for this machine (`rotate default` replaces the config token) |
| `peer add\|list\|remove` | manage peers (`~/.agent-cli-x/peers.toml`); `list` shows the cached OS/arch and shell, refreshed by `peer add`, `info` and `upgrade` |
| `info <peer>` | OS, shell, workspaces, deny rules; `info @all` is a one-line-per-peer overview |
| `exec <peer> [--cwd] [--shell] [--timeout] [--env K=V] <cmd>` | run a command, streamed; `A,B` or `@all` runs on several peers in parallel |
| `upgrade <peer> [--binary <file>] [--force]` | replace the peer's binary and restart its `serve` (`A,B` / `@all` allowed) |
| `exec <peer> --detach <cmd>`, `job list\|logs\|kill` | background jobs (`job logs --follow` exits with the job's code) |
| `ls` / `grep` / `read` / `write` / `edit` | file operations inside workspaces |
| `put <peer> <local> <dest>` / `get <peer> <src> <local>` | transfer files or directories (`--exclude`) |
| `rollback <peer> <op_id>` | undo a write/edit/put |
| `skill` | print the agent guide |

Paths use `workspace:relative/path`. See `agent-cli-x --help`.

## Multiple machines

Every machine runs `serve` and registers the others as peers, so A, B and C can all call each other. A full mesh of N machines needs N×(N−1) `peer add` commands. `token add <caller>` prints each one for you.

- **One command, many peers.** `exec A,B,C <cmd>` or `exec @all <cmd>` runs in parallel and prints the output grouped by peer, followed by a summary line. The exit code is 0 when every peer succeeded, otherwise the first failing peer's code. `--json` returns one result per peer. `info @all` shows which peers are online, their versions and latency.
- **Per-caller tokens.** `token add <name>` issues a token for one caller. `token remove <name>` revokes it without affecting the others, and `token rotate <name>` replaces it. Only the sha256 of these tokens is stored, in `~/.agent-cli-x/tokens.toml`. Changes take effect immediately, with no `serve` restart. The audit log records which token made each request (`"caller"`). The original `token` in `config.toml` remains valid under the name `default`.
- **Version check.** Every response carries the server version. When a peer runs a different version, the client prints a one-time hint to run `agent-cli-x upgrade <peer>`.
- **Remote upgrade.** `upgrade <peer>` uploads the local binary, or `--binary <file>` for a peer with a different OS or architecture. The peer checks the sha256, runs the new file's `--version`, swaps it into place, and restarts. The client waits until the new process answers. If background jobs are running, the upgrade is refused unless you pass `--force`. Peers on 0.0.2 or earlier have no upgrade endpoint and must be upgraded by hand once.
  - Under **systemd**, `serve` exits with code 75 and the unit's `Restart=on-failure` brings it back.
  - Everywhere else (Task Scheduler, a terminal, `nohup`), it starts the new `serve` detached and exits.
  - Set `AGENT_CLI_X_RESTART=exit` to force the exit-and-restart behaviour for another supervisor.
- **Not supported:** relaying through another machine (A → B → C) or automatic discovery. Every pair of machines must reach each other directly on the listen port.

## Configuration

`~/.agent-cli-x/config.toml` (override the folder with `AGENT_CLI_X_HOME`). The generated file is fully commented. Key parts:

```toml
node_name = "build-01"     # shown to peers; defaults to the hostname
node_id = "4f0c…"          # random UUID generated by init
listen = "0.0.0.0:47821"
token = "..."

[workspaces]               # only these directories are reachable
proj = "D:/code/proj"

[policy]
deny_paths = [             # forbidden paths (dirs block everything below them)
  "secrets",               # relative to each workspace root
  "**/.env",               # glob, any depth
  "proj:config/prod",      # only in workspace `proj`
  "C:/Users/me/.ssh",      # absolute
]
deny_commands = ["rm -rf /", "git push --force"]   # word-boundary match
default_timeout_secs = 600
max_timeout_secs = 3600
max_transfer_mb = 2048     # per put/get
max_jobs = 16              # concurrent background jobs
job_timeout_secs = 86400
# prewarm_shells = 2       # idle PowerShell kept ready for exec; unset = 1 per 4 GB RAM (1–4), set = capped at RAM in GB, 0 = off
```

### Performance

`serve` is one process: one thread per connection (up to 64 concurrent), each `exec` a fresh child process. On Windows the dominant cost is PowerShell startup (pwsh ≈ 0.3 s, Windows PowerShell 5.1 ≈ 2 s), so `serve` keeps `prewarm_shells` idle PowerShell processes started and waiting. A request takes one, hands it cwd/env/command over stdin, and a replacement is started in the background. Every command still runs in its own new process, so nothing leaks between calls; when the pool is empty `exec` falls back to a cold start. Measured locally for `exec "echo hi"` round trips: pwsh 520 ms → 170 ms, 5.1 2.6 s → 0.2–0.9 s. If only 5.1 is installed, installing PowerShell 7 (`pwsh`) helps further. `sh`/`bash`/`cmd` start fast and are not prewarmed.

## Security model

Intended for machines you own on a trusted network.

- Traffic is TLS. Clients pin the server certificate fingerprint; a changed fingerprint is a hard error (exit 106).
- Access requires a token: the `default` one from `config.toml`, or a per-caller token from `token add`. Tokens are compared in constant time, and failures are delayed and audited.
- File operations are confined to workspaces: canonicalized paths, no `..`, no absolute paths, no symlink/junction escapes, deny-list checked on every ancestor. The agent-cli-x data folder is always denied.
- `deny_paths` applies to `ls`/`read`/`write`/`edit`/`rollback` and to the `exec` working directory. **A shell command itself cannot be sandboxed**: `exec "cat secrets/x"` can still read the file. For hard isolation, run `serve` as a dedicated low-privilege OS user and protect directories with file-system permissions.
- `deny_commands` is a guard rail against accidents, not a security boundary.
- `grep`, `get` and `ls` skip deny-listed paths; `put` refuses to write into them.
- Background jobs run as the same user; their output lives in `~/.agent-cli-x/jobs/` (7 days). Jobs still running when `serve` restarts are marked `lost`.
- Every write/edit/put is backed up to `~/.agent-cli-x/backup/` (7 days). All requests are logged to `~/.agent-cli-x/audit/YYYY-MM-DD.jsonl` with the client IP and token name.
- `serve` writes its own log (startup, token reloads, upgrades, internal errors, panics) to `~/.agent-cli-x/serve-logs/` (7 days) as well as to stderr. When it runs at boot with no console, check this log first.

## Start at boot (recommended)

A peer is only reachable while its `serve` is running, and once it is down nobody can restart it remotely. **Set up autostart on every machine.** Run `serve` **as your own user**, not as SYSTEM/root, so commands see your toolchain and logins. You (or your agent) can apply the snippets below as-is.

**Linux (systemd user unit)**

```ini
# ~/.config/systemd/user/agent-cli-x.service
[Unit]
Description=agent-cli-x serve
After=network-online.target

[Service]
ExecStart=%h/.local/bin/agent-cli-x serve
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
```

```bash
systemctl --user daemon-reload
systemctl --user enable --now agent-cli-x
sudo loginctl enable-linger $USER   # start at boot and keep running after logout
```

**Windows (Task Scheduler)**, from an elevated PowerShell, logged in as the user that should run `serve`:

```powershell
$exe       = (Get-Command agent-cli-x).Source
$action    = New-ScheduledTaskAction -Execute $exe -Argument 'serve'
$trigger   = New-ScheduledTaskTrigger -AtStartup
$principal = New-ScheduledTaskPrincipal -UserId "$env:USERDOMAIN\$env:USERNAME" -LogonType S4U -RunLevel Limited
$settings  = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) `
               -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
               -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) -MultipleInstances IgnoreNew
Register-ScheduledTask -TaskName agent-cli-x -Action $action -Trigger $trigger -Principal $principal -Settings $settings
Start-ScheduledTask -TaskName agent-cli-x

# allow the port once
netsh advfirewall firewall add rule name="agent-cli-x" dir=in action=allow protocol=TCP localport=47821
```

Why these settings: Task Scheduler's defaults stop a task after 3 days and skip or stop it on battery power. `S4U` runs it at boot without anyone logged in, with no console window and no stored password. The trade-off is that the task has no network credentials, so mapped drives and Credential Manager logins are unavailable to `exec`. If you need them, use `-AtLogOn` with `-LogonType Interactive` instead. That variant only starts after you log in and shows a console window, and closing the window stops `serve`.

After a reboot, run `agent-cli-x doctor` on the machine. On Windows it flags a task that will stop after 72 hours, a task that stops or won't start on battery, a task that launches a different binary path, and a missing firewall rule. On Linux it flags a unit that isn't enabled and linger that is off. Also run `agent-cli-x info <peer>` from another machine to confirm the peer can be reached.

## Exit codes

Remote exit codes pass through. agent-cli-x's own errors: 100 connect, 101 auth, 102 policy denied, 103 not found, 104 timeout, 105 conflict, 106 fingerprint, 107 local/remote I/O error, 108 other, 109 config (e.g. unknown peer, missing config), 110 bad arguments, 111 busy. `grep` exits 1 when nothing matches; `job logs` exits 137 for a killed job.

## License

MIT
