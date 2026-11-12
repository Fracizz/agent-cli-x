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
| `token add\|list\|set\|remove\|rotate` | per-caller tokens for this machine (`rotate default` replaces the config token); `add <name> --no-relay` / `set <name> --no-relay\|--allow-relay` stops that caller from hopping to other machines through this one |
| `peer add\|list\|remove` | manage peers (`~/.agent-cli-x/peers.toml`); `list` shows the cached OS/arch and shell, refreshed by `peer add`, `info`, `upgrade` and `cp`; peers with no cached OS (registered by an older version) are looked up automatically (an unreachable one is retried at most once a day) |
| `info <peer>` | OS, shell, the user `exec` runs as, workspaces, deny rules; `info @all` is a one-line-per-peer overview |
| `exec <peer> [--cwd] [--shell] [--timeout] [--env K=V] <cmd>` | run a command, streamed; `A,B` or `@all` runs on several peers in parallel |
| `exec <peer> --script <file>` | run the contents of a local script file (`-` = stdin) with the peer's shell: no nested quoting; CRLF and BOM are normalized |
| `upgrade <peer> [--binary <file>] [--force]` | replace the peer's binary and restart its `serve` (`A,B` / `@all` allowed) |
| `exec <peer> --detach <cmd>`, `job list\|logs\|kill` | background jobs (`job logs --follow` exits with the job's code) |
| `ls` / `grep` / `read` / `write` / `edit` | file operations inside workspaces |
| `rm <peer> <path> [-r] [--permanent]` | delete a file or directory: moved to the peer's recycle bin by default (undo with `rollback`); `--permanent` deletes for good |
| `purge <peer> <op_id>` | permanently remove what an operation left behind (its recycle-bin entries and backups) to free space; it can no longer be rolled back |
| `put <peer> <local> <dest>` / `get <peer> <src> <local>` | transfer files or directories (`--exclude`) |
| `cp <src-peer> <src> <dst-peer> <dest>` | copy between two peers: streamed through this machine without touching its disk (`--exclude`; undo on the destination with `rollback`). The destination needs 0.0.4+; with an older one it falls back to a temporary local copy that is deleted afterwards |
| `rollback <peer> <op_id>` | undo a write/edit/put/rm |
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

### One-way access

Access is per direction: A can call B only with a token B issued. B never calls back on its own, so if A issues no token to B, B cannot call A directly.

B can still go **through a third machine**. If B may `exec` on C and C holds a token for A, B can run `agent-cli-x exec A …` on C, because commands run as C's user with C's credentials. To stop this, issue B's token on C with `--no-relay`:

```bash
# On C
agent-cli-x token add B --no-relay      # new token
agent-cli-x token set B --no-relay      # or restrict an existing one (no new token needed)
```

B's `exec` and background jobs on C then carry `AGENT_CLI_X_NO_RELAY=B`. Inside them, `agent-cli-x` refuses everything that talks to peers or touches credentials (exit 102): `exec`/`info`/file commands to any peer, `peer`, `token`, `whoami --pair`, `upgrade`, `init`, `serve`. Local read-only commands (`whoami`, `fingerprint`, `doctor` without peer checks, `skill`) still work. B cannot clear the variable with `--env`, and `info C` tells B about the restriction. Only C needs this version; A and B need no change. The default token from `config.toml` cannot be restricted, so give restricted callers their own token.

**This prevents mistakes, not attacks.** A command can unset the variable or read C's `~/.agent-cli-x/peers.toml` directly. For a hard boundary, run a second `serve` on C as a separate low-privilege OS user with its own `AGENT_CLI_X_HOME`, port and binary copy. Give B only that instance, and make sure that user cannot read your home directory and that you never run code B can write.

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
deny_paths = [             # hard deny: no access at all, cannot be re-opened by [[policy.paths]]
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
rm_use_trash = true        # rm moves to the recycle bin; false = back up to ~/.agent-cli-x/backup then delete
# prewarm_shells = 2       # idle PowerShell kept ready for exec; unset = 1 per 4 GB RAM (1–4), set = capped at RAM in GB, 0 = off

# CRUD permissions per path (keep these at the end of [policy]).
# allow: r = read (ls/read/grep/get, exec cwd), c = create, u = update (overwrite/edit), d = delete (rm); "" = none
[[policy.paths]]
path = "C:/"               # Windows preset: system drive read-only
allow = "r"

[[policy.paths]]
path = "C:/Users/me/code"  # more specific rule wins: full access here
allow = "rcud"

[[policy.paths]]
path = "proj:releases"     # may add new files, never overwrite or delete
allow = "rc"
```

Path rules (`deny_paths` and `[[policy.paths]]`): relative to each workspace root (`secrets`, `**/.env`), scoped (`proj:config/prod`; `proj:` alone is the workspace root), or absolute (`C:/`, `/etc`); `* ? ** [abc]` globs, case-insensitive on Windows.

How permissions are resolved for a path:

1. The agent-cli-x data folder, or a `deny_paths` match on the path or any parent → no access.
2. Otherwise walk from the path up to the root; the first level with a matching `[[policy.paths]]` rule decides. At that level, rules without wildcards win over glob rules; several rules of the same kind are intersected. (A glob like `**/*.lock` matches the file itself, so it is more specific than a rule on its directory.)
3. No match → full access (`rcud`).

`readonly_paths = [...]` from older configs still works and equals `allow = "r"`. **`exec` commands are not covered by path permissions or `deny_paths`**: they run with the full rights of the user `serve` runs as (shown by `info` as "exec 身份"), limited only by `deny_commands`.

### Performance

`serve` is one process: one thread per connection (up to 64 concurrent), each `exec` a fresh child process. On Windows the dominant cost is PowerShell startup (pwsh ≈ 0.3 s, Windows PowerShell 5.1 ≈ 2 s), so `serve` keeps `prewarm_shells` idle PowerShell processes started and waiting. A request takes one, hands it cwd/env/command over stdin, and a replacement is started in the background. Every command still runs in its own new process, so nothing leaks between calls; when the pool is empty `exec` falls back to a cold start. Measured locally for `exec "echo hi"` round trips: pwsh 520 ms → 170 ms, 5.1 2.6 s → 0.2–0.9 s. If only 5.1 is installed, installing PowerShell 7 (`pwsh`) helps further. `sh`/`bash`/`cmd` start fast and are not prewarmed.

## Security model

Intended for machines you own on a trusted network.

- Traffic is TLS. Clients pin the server certificate fingerprint; a changed fingerprint is a hard error (exit 106).
- Access requires a token: the `default` one from `config.toml`, or a per-caller token from `token add`. Tokens are compared in constant time, and failures are delayed and audited.
- File operations are confined to workspaces: canonicalized paths, no `..`, no absolute paths, no symlink/junction escapes, deny-list checked on every ancestor. The agent-cli-x data folder is always denied.
- Path permissions are enforced per operation: `ls`/`read`/`grep`/`get` and the `exec` working directory need `r`; `write`/`put` need `c` for new files and `u` for existing ones; `edit` needs `u`; `rm` needs `d` on **every** entry it would remove (otherwise nothing is deleted); `rollback` checks the action it performs (restore = `u`, re-create = `c`, remove = `d`). **A shell command itself cannot be sandboxed**: `exec "cat secrets/x"` or `exec "rm x"` still works, and if `serve` runs as root/Administrator so can `exec "npm install -g …"` into a read-only `/usr`. `info` prints the exec user and whether it is elevated so callers can see this. For hard isolation, run `serve` as a dedicated low-privilege OS user and protect directories with file-system permissions.
- `init` writes OS-specific presets, which you are free to edit:
  - Windows: `C:/` read-only, `C:/Windows/System32/config` denied; commands such as `rd /s /q c:\`, `Remove-Item -Recurse -Force C:\Windows`, `format c:`, `diskpart`, `bcdedit` blocked. **If your workspaces live on `C:`, add a more specific `[[policy.paths]]` rule for them (e.g. `allow = "rcud"`) to allow writes.**
  - Linux: `/bin`, `/sbin`, `/lib*`, `/usr`, `/etc`, `/boot`, `/var/lib`, `/proc`, `/sys`, `/dev` read-only; `/etc/shadow`, `/etc/gshadow`, `/etc/sudoers*`, `/root/.ssh` denied; `rm -rf /etc` (and other system dirs), `--no-preserve-root`, `mkfs`, `reboot` etc. blocked.
  - macOS: `/System`, `/Library`, `/usr`, `/bin`, `/sbin`, `/private/etc`, `/private/var/db`, `/Applications` read-only.
- `deny_commands` is a guard rail against accidents, not a security boundary. So is `token add --no-relay` (see [One-way access](#one-way-access)).
- `grep`, `get` and `ls` skip unreadable paths; `put` refuses to write where it lacks `c`/`u`.
- Background jobs run as the same user; their output lives in `~/.agent-cli-x/jobs/` (7 days). Jobs still running when `serve` restarts are marked `lost`.
- Every write/edit/put is backed up to `~/.agent-cli-x/backup/` (7 days). All requests are logged to `~/.agent-cli-x/audit/YYYY-MM-DD.jsonl` with the client IP and token name.
- `rm` moves files to the system recycle bin (Windows Recycle Bin, freedesktop Trash on Linux, Trash on macOS), so it is fast and has no size cap; `rollback` restores them from there (Windows/Linux; on macOS restore from the Trash by hand). It refuses to restore over a path that exists again. If the recycle bin is unavailable (e.g. a Linux mount without a writable `.Trash` dir) or `rm_use_trash = false`, it falls back to backing up to `~/.agent-cli-x/backup/` and deleting, limited to `max_transfer_mb`. `rm --permanent` skips both and cannot be undone. `purge <op_id>` permanently deletes an earlier `rm`'s recycle-bin entries (needs `d` on the original paths) and removes the operation's backups, freeing the space without waiting for the 7-day cleanup.
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
