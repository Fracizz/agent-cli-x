# agent-cli-x

**English** · [简体中文](README.zh-CN.md)

Run a build or test on another machine, inspect its logs, or edit files in a registered project from your current terminal. `agent-cli-x` is a single binary for the caller and the remote host, with output limits designed for AI coding agents.

[Download for Windows or Linux](https://github.com/Fracizz/agent-cli-x/releases/latest) · [Quick start](#quick-start) · [Agent Skill](skills/agent-cli-x/SKILL.md) · [Security model](#security-model)

## Install

Prebuilt binaries for Linux (x86_64, statically linked) and Windows (x86_64) are on the [releases page](https://github.com/Fracizz/agent-cli-x/releases).

```bash
cargo install --git https://github.com/Fracizz/agent-cli-x
```

Building from source requires Rust 1.88 or newer.

Or build with `cargo build --locked --release` and put `target/release/agent-cli-x(.exe)` on your `PATH`:

- **Windows:** copy to e.g. `%USERPROFILE%\bin` and add that folder to the user `Path` (System Properties → Environment Variables).
- **Linux:** `install -m 755 target/release/agent-cli-x ~/.local/bin/`

Having it on `PATH` lets agents call `agent-cli-x` from any working directory.

## Quick start

Install the binary on both machines. On the host where the project lives, run these commands as the user who should run builds. Replace the workspace path with your project directory and make the listener reachable on your trusted network.

```bash
agent-cli-x init --name build-01 --workspace proj=/home/me/proj
agent-cli-x serve
agent-cli-x token add dev-workstation
```

Without `--workspace`, the host's home directory is served as workspace `home` (read/write), so an authorized caller can work right away. To register more directories, run `agent-cli-x workspace add proj /home/me/proj --default` on the host, or `agent-cli-x workspace add build-01 proj /home/me/proj --default` from an authorized caller. Either way, `config.toml` is edited with its comments kept, validated the way `serve` does it, and `serve` is restarted. No hand-editing is needed. Workspace names need at least 2 characters. Once any workspace is configured, the automatic `home` workspace goes away.

Verify the fingerprint on the build host with `agent-cli-x fingerprint`. On the calling machine, run the pairing command printed by `token add`, using the verified fingerprint and a reachable address. Then check the connection and run a test in the registered workspace.

For one-off access you can skip `init`: `agent-cli-x token add guest --code` initializes the machine with defaults if needed, and warns when `serve` is not running yet.

```bash
agent-cli-x info build-01
agent-cli-x exec build-01 --cwd proj: cargo test
```

`build-01` is the peer name in the pairing command. Use the name you registered if it differs. A managed host can print its original pairing command again with `agent-cli-x whoami --pair`.

`serve` starts in the background by default and survives closing the terminal. It prints the PID after the listener is ready (`--json`: `{"pid":1234}`). Use `serve --foreground` for troubleshooting or service managers. Logs live in `~/.agent-cli-x/serve-logs/`. To keep the host reachable after a reboot, [set up start at boot](#start-at-boot-recommended) and run `agent-cli-x doctor`.

To give an agent the command reference, install [the Skill](skills/agent-cli-x/SKILL.md) in its skills folder or print it with `agent-cli-x skill`.

## How it works

```text
Caller (Codex, Claude Code, or shell) → agent-cli-x → pinned TLS → agent-cli-x serve → project
```

Each host can run the same binary as a client and server. Pairing authorises a caller in one direction. See [Multiple machines](#multiple-machines) to set up access in both directions.

- **One binary, both roles.** Every machine runs `agent-cli-x serve`, and every machine can call its peers.
- **Lightweight.** Synchronous std + rustls, no async runtime, no OpenSSL. Short-lived HTTPS request per call.
- **Machine identity.** Every node has a configurable `node_name` and a random short `node_id` (6 hex chars), so agents know exactly which machine they are talking to.
- **Built for agents.** Exit codes pass through, long output is trimmed to head/tail (also with `--json`) with the full log saved locally, `read` and `ls` return bounded windows by default (several files per `read`), `edit` with exact match first and a whitespace/quote-tolerant fallback, `rollback`.
- **Many machines.** Run one command on several peers at once (`exec A,B,C` / `@all`), issue a separate revocable token per caller, push upgrades remotely, and check everything with `doctor`.
- **Guard rails.** Token auth, self-signed TLS with fingerprint pinning, workspace allow-list, path deny-list, command deny-list, automatic backups, audit log.

## Commands

| Command | Purpose |
|---|---|
| `init` / `serve [--foreground]` / `whoami [--pair]` / `fingerprint` | set up, run in background by default and identify this machine; `serve --check` only validates `config.toml` and the certificate (run it after editing the config by hand) |
| `stop [--force]` | stop this machine's `serve` (local only: a peer can't be stopped remotely because nothing could start it again; use `restart`). Waits until the port is closed; exits 0 if it wasn't running. Background jobs keep running; `--force` is only needed under systemd without `KillMode=process`, where stopping would kill them |
| `workspace add [peer] <name> <dir> [--default] [--allow rcud]` / `workspace remove [peer] <name>` | register or remove a directory callers may access, on this machine or (with a peer name) on that peer. Any authorized caller may do this remotely. Edits `config.toml` keeping comments, validates it, then restarts `serve` (`--force` as for `restart`). With no workspace configured, the user's home directory is served as `home`. Adding a second workspace without `--default` writes the first one as `default_workspace`, so unprefixed paths keep working. `--allow` writes a `[[policy.paths]]` rule `<name>:`, needed for workspaces under a read-only preset such as `C:/`. `remove` also drops the default setting and the `<name>:` rules |
| `doctor [--no-peers]` | check config, local service, start at boot, firewall, peers and their versions |
| `firewall status\|open\|close` | inbound firewall rule for this machine. `serve` checks it once at startup: when the rule is missing it adds it if running as admin/root, otherwise prints one line with the command to run. `open` adds it by hand (Windows: a per-program rule `agent-cli-x`, valid for any port and across restarts and upgrades; asks for UAC in an interactive terminal. Linux: allows the listen port in ufw/firewalld, needs root: `sudo agent-cli-x firewall open --port N`). `close` removes what agent-cli-x added. Set `AGENT_CLI_X_FIREWALL=off` to keep `serve` from touching the firewall |
| `token add\|list\|set\|remove\|rotate` | per-caller tokens for this machine (`rotate default` replaces the config token); `add <name> --no-relay` / `set <name> --no-relay\|--allow-relay` stops that caller from hopping to other machines through this one; `add <name> --code [--ttl 8h]` issues an authorization code: a full-access temporary token valid 4h by default (or 8h), rejected with 101 once expired; `rotate <name> [--ttl 8h]` renews it |
| `peer add\|set\|list\|remove` | manage peers (`~/.agent-cli-x/peers.toml`); `list` shows the cached OS/arch and shell, refreshed by `peer add`, `info`, `upgrade` and `cp`; `set <name> --addr <ip:port>` / `--token <t>` changes a peer's address or token after checking it against the registered fingerprint; peers with no cached OS (registered by an older version) are looked up automatically (an unreachable one is retried at most once a day) |
| `info <peer>` | OS, shell, the user `exec` runs as, workspaces, deny rules; `info @all` is a one-line-per-peer overview |
| `desktop windows <peer> [--title text] [--max N] [--offset N]` | visible Windows top-level windows, IDs, physical rectangles and per-window DPI |
| `desktop screenshot <peer> [--window foreground\|ID\|--screen\|--region X,Y,W,H] [--out local.png]` | native PNG capture, automatically downloaded to this machine; foreground window by default |
| `exec <peer> [--cwd] [--shell] [--timeout] [--env K=V] <cmd>` | run a command, streamed; `A,B` or `@all` runs on several peers in parallel |
| `exec <peer> --script <file>` | run the contents of a local script file (`-` = stdin) with the peer's shell: no nested quoting; CRLF and BOM are normalized |
| `upgrade <peer> [--binary <file>] [--force]` | replace the peer's binary and restart its `serve` (`A,B` / `@all` allowed) |
| `restart <peer> [--force]` | restart the peer's `serve` without replacing the binary (`A,B` / `@all` allowed) |
| `set-port <peer> <port>` | move the peer to another listen port without restarting it; also updates this machine's `peers.toml` |
| `exec <peer> --detach <cmd>`, `job list\|logs\|kill` | background jobs (`job logs --follow` exits with the job's code); they keep running across `restart` / `upgrade` of the peer |
| `ls` / `find` / `grep` / `read` / `write` / `edit` | file operations inside workspaces. `find <peer> '<glob>' [dir]` lists files by name, newest first (a pattern without `/` matches at any depth). `grep` takes `-C/-A/-B N` context, `-l` (files only) and `-c` (count per file). `edit --edits <file\|->` applies a JSON array `[{"old","new","all"?}]` in order, all or nothing. When `old` has no exact match, `edit` retries ignoring trailing whitespace and smart-quote / dash / special-space differences, replaces only the matched span and reports `fuzzy`. `read <peer> a b#100:200` reads several files in one call (per-file range after `#`); an image path (png/jpg/gif/webp/bmp) is downloaded to the local temp dir and its path printed. `write` creates missing parent dirs. Writes to the same file (`write`/`edit`/`rm`) are serialized on the peer |
| Output limits | Bounded head/tail logs, file windows and list/search pages; see [Output limits](#output-limits) for budgets and continuation flags |
| `rm <peer> <path> [-r] [--permanent]` | delete a file or directory: moved to the peer's recycle bin by default (undo with `rollback`); `--permanent` deletes for good |
| `purge <peer> <op_id>` | permanently remove what an operation left behind (its recycle-bin entries and backups) to free space; it can no longer be rolled back |
| `put <peer> <local> <dest>` / `get <peer> <src> <local>` | transfer files or directories (`--exclude`) |
| `cp <src-peer> <src> <dst-peer> <dest>` | copy between two peers: streamed through this machine without touching its disk (`--exclude`; undo on the destination with `rollback`). The destination needs 0.0.4+; with an older one it falls back to a temporary local copy that is deleted afterwards |
| `rollback <peer> <op_id>` | undo a write/edit/put/rm |
| `skill` | print the agent guide |

Paths use `workspace:relative/path`. See `agent-cli-x --help`.

`peer list [query...]` narrows the list by case-insensitive substring of name, node name, IP:port, OS or remark; several terms must all match (`peer list linux 10.0.0`, `peer list prod`). No match exits 1.

Peer remarks are local to the caller and appear in `peer list` and `info` (including JSON). Use `peer add <name> <addr> --token <token> --fingerprint <fingerprint> --remark "production"` when registering, or `peer set <name> --remark "production"` to edit offline; `--remark ""` clears it. Remarks accept one line, at most 300 characters (1200 UTF-8 bytes), without control characters. Re-registering without `--remark` preserves the existing remark.

### Output limits

The same data budgets apply to ordinary output and JSON. JSON escaping, metadata and `read -n` line numbers add presentation overhead.

| Command | Default budget | Retrieve omitted data |
|---|---|---|
| `exec`, `job logs` | stdout/stderr share first + last 100 lines / 16KB each; 2000 characters per line | Full local `log` retained after any cut; `--full` removes normal trimming |
| Multi-peer `exec` | Each peer shares first + last 50 lines / 8KB each | Each peer has its own full local log |
| `read` without an end line | 2000 lines / 64KB; 2000 characters per line. Several paths share this one window; files that no longer fit are listed as `skipped` | `--lines N:` (or `path#N:` with several paths) / JSON `next_start` continues; clipped lines give a `get` command for full text |
| `ls` | 500 entries; up to 60KB of serialized entries, reserving space for metadata within 64KB | `--offset N` / JSON `next_offset`; `--max` changes the entry count |
| `find` | 200 paths, newest first; scan stops after 100000 matches (`total_capped`) | Narrow the pattern/dir or raise `--max` |
| `grep` | 200 matches (files with `-l`/`-c`) / 300 characters per line; up to 60KB of serialized entries including `-C/-A/-B` context lines | Same pagination as `ls`; narrow with `--glob` |
| `desktop windows` | 50 windows / 32KB of entries; titles clipped to 200 characters | `--offset` / `next_offset`, `--max`; `--full` preserves titles and raises the page byte cap to 8MB, while `--max` still applies |
| `desktop screenshot` | PNG preview: longest edge <=1600 pixels and <=1MB; shrinks further if needed | `--max-edge N`, a smaller `--region`, or `--full` for original pixels (hard cap 64M pixels / 32MB PNG) |

- JSON reports `truncated`, omitted line/byte/character counts, and a log path or continuation cursor. List/search pages stop after detecting another result, so the remaining count is a lower bound. Ordinary output gives the equivalent retrieval command on stderr. Page order is stable while the tree and file contents remain unchanged.
- `ls` / `grep --full` removes byte and row clipping; `--max` still applies. Old peers without pagination reject a requested nonzero offset with an upgrade hint rather than silently repeating the first page.
- An explicit `read --lines START:END` permits long lines but remains capped by `max_read_kb`; a single line exceeding that cap returns an error with a download command.
- Explicit large-output representations remain memory-bounded: single-peer `--full --json` keeps at most 1MB total, and multi-peer `--full` keeps 8MB per peer. Any remainder is available in the full local log. Single-peer ordinary `--full` streams all output.

### Native Windows desktop observation

Available from 0.0.11. The caller can run on any supported OS; the peer must run Windows 10 (1703+) or Windows 11 in an unlocked, connected user session. The feature is off by default. The peer owner enables all authenticated callers' read-only desktop observation with `[policy] desktop_capture = true`, then restarts `serve`. This gates the desktop API separately from workspace paths and command rules; arbitrary `exec` commands still run with the serve user's rights. A Windows service in session 0 cannot capture the interactive user's desktop.

```bash
agent-cli-x desktop windows win --title Codex --max 10
agent-cli-x desktop screenshot win --out ./foreground.png
agent-cli-x desktop screenshot win --window 0x123ABC --out ./window.png --json
agent-cli-x desktop screenshot win --region -1920,0,800,600 --out ./region.png
```

PNG bytes travel directly over the pinned TLS connection; no capture scripts, remote image files, or Base64 stdout are used. `--out` is a **local file**, is returned as an absolute `path`, and never overwrites an existing file (105). Without `--out`, a unique PNG is saved under the caller's `~/.agent-cli-x/screenshots/`. Failed downloads remove their partial local file. Successful screenshots remain until the caller removes them.

Coordinates are physical virtual-desktop pixels, including negative monitor origins. JSON includes `rect`, image `width`/`height`, `scale_x`/`scale_y` (source pixels per image pixel), and UTC `captured_at`. For example, desktop X = `rect.x + image_x * scale_x`. Window captures also report `dpi`; screen/region captures report `null` because they may span monitors with different DPI settings.

Capture reads **displayed pixels**, including layered/overlapping windows. Selecting a window crops its visible desktop bounds; it does not activate it or recover covered content. Background targets and partly off-screen windows receive a warning. Hidden, minimized or wholly off-screen targets are rejected. Window enumeration order can change with the desktop's Z-order; rerun the title filter when a window disappears. No keyboard, mouse, activation, or accessibility controls are included in this phase.

`info` exposes native support and the permission state. Errors use 102 for disabled desktop permission, 103 for a vanished window, 105 for an unusable window/local file conflict, 112 for a locked/disconnected/non-interactive desktop, and 113 for an unsupported peer OS. Images are not stored on the peer; the audit log records metadata, not image bytes or window titles.

Interactive validation on Windows: `cargo test --test e2e native_desktop_roundtrip -- --ignored --test-threads=1`. This uses isolated client/server profiles and only reads pixels; it does not activate windows or upgrade an existing service.

## Multiple machines

Every machine runs `serve` and registers the others as peers, so A, B and C can all call each other. A full mesh of N machines needs N×(N−1) `peer add` commands. `token add <caller>` prints each one for you.

- **One command, many peers.** `exec A,B,C <cmd>` or `exec @all <cmd>` runs in parallel and prints the output grouped by peer, followed by a summary line. The exit code is 0 when every peer succeeded, otherwise the first failing peer's code. `--json` returns one result per peer. `info @all` shows which peers are online, their versions and latency.
- **Per-caller tokens.** `token add <name>` issues a token for one caller. `token remove <name>` revokes it without affecting the others, and `token rotate <name>` replaces it. Only the sha256 of these tokens is stored, in `~/.agent-cli-x/tokens.toml`. Changes take effect immediately, with no `serve` restart. The audit log records which token made each request (`"caller"`). The original `token` in `config.toml` remains valid under the name `default`.
- **Version check.** Every response carries the server version. When a peer runs a different version, the client prints a one-time hint to run `agent-cli-x upgrade <peer>`.
- **Remote upgrade.** `upgrade <peer>` uploads the local binary, or `--binary <file>` for a peer with a different OS or architecture. The peer checks the sha256, runs the new file's `--version`, swaps it into place, and restarts. The client waits until the new process answers. Background jobs keep running (see below). Peers on 0.0.2 or earlier have no upgrade endpoint and must be upgraded by hand once.
  - Under **systemd**, `serve` exits with code 75 and the unit's `Restart=on-failure` brings it back.
  - Everywhere else (Task Scheduler, a terminal, `nohup`), it starts the new `serve` detached and exits.
  - Set `AGENT_CLI_X_RESTART=exit` to force the exit-and-restart behaviour for another supervisor.
  - The client waits up to 60 seconds for the new process (150 seconds for a Windows peer, because Task Scheduler restarts a task no sooner than one minute after it exits), then checks once more. If the peer still isn't back it exits with 104: the restart was issued but not confirmed yet, so check later with `info`.
- **Restart.** `restart <peer>` restarts `serve` the same way without uploading anything.
- **Background jobs survive restarts.** Each `exec --detach` job runs under its own small supervisor process (`agent-cli-x job-run`, started detached from `serve`). The supervisor runs the command, writes its output and result into `~/.agent-cli-x/jobs/<id>/` and enforces the timeout. `serve` only reads those files, so a restarted or upgraded `serve` keeps showing, following and killing jobs started before. A job shows as `lost` only if its supervisor is gone (the machine rebooted, or someone killed it). The one exception is systemd with the default `KillMode=control-group`: when `serve` exits for a restart, systemd kills every process in the unit, jobs included. Add `KillMode=process` to the unit as shown in [Start at boot](#start-at-boot-recommended). Until you do, `upgrade` / `restart` refuse while jobs are running unless you pass `--force`, and `doctor` warns about it.
- **Config check before restarting.** Before `upgrade` or `restart`, the peer validates its own `config.toml`, workspaces and certificate exactly as a fresh `serve` would. If anything is wrong it refuses with exit 109 and says why, instead of restarting into a process that can't start.
- **Broken config at startup.** Every time `serve` starts successfully it saves a copy as `~/.agent-cli-x/config.toml.last-good`. If `config.toml` later fails to parse or validate, `serve` logs the error and runs on that copy instead of exiting, so the machine stays reachable. `info` then shows a `配置异常` line (`config_error` in `--json`), `info @all` marks the peer, and `doctor` on the machine reports the config as failed. Fix the file and run `restart <peer>`. Note that the old copy may carry looser rules than the ones you were trying to set. A missing `config.toml` does not fall back.
- **Change the listen port.** `set-port <peer> <port>` asks the peer to listen on the new port as well, confirms through the new port that this machine can reach it, and only then writes `listen` into the peer's `config.toml` and closes the old port. No restart is needed. If the new port can't be reached (a firewall or cloud security group), the change is rolled back and the old port keeps working. Other machines that registered the peer must run `peer set <name> --addr <ip:new-port>`, and the command prints that line for you. Firewall: when the peer's `serve` runs as admin/root it opens the new port itself (Windows: the per-program rule covers every port; Linux: allows the new port in ufw/firewalld) and, once switched, removes the rule it had added for the old port (on Windows, an old per-port `agent-cli-x` rule once the per-program rule exists; on Linux, only ports agent-cli-x opened itself). A rolled-back or expired switch removes the rule for the new port again. Without those rights the error tells you to run `agent-cli-x firewall open` on the peer.
- **Handshake failures.** When TCP connects but TLS fails, the error says why. A dropped connection usually means something other than `serve` is on that port, for example a port forwarder whose backend `serve` has exited. Check the peer's `~/.agent-cli-x/serve-logs/`.
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
node_id = "4f0c…"          # random 6-hex short ID generated by init
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
# prewarm_shells = 2       # idle PowerShell kept ready for exec; unset = 1 (<16 GB RAM) or 2, set = capped at RAM in GB, 0 = off
prewarm_idle_secs = 600    # release idle warm shells after this long without exec (next exec cold-starts and refills); 0 = keep forever

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

`serve` is one process: one thread per admitted connection (up to 64 concurrent, including pending TLS handshakes; excess TCP connections are closed before creating a thread), each `exec` a fresh child process. On Windows the dominant cost is PowerShell startup (pwsh ≈ 0.3 s, Windows PowerShell 5.1 ≈ 2 s), so `serve` keeps `prewarm_shells` idle PowerShell processes started and waiting. A request takes one, hands it cwd/env/command over stdin, and a replacement is started in the background. Every command still runs in its own new process, so nothing leaks between calls; when the pool is empty `exec` falls back to a cold start. Measured locally for `exec "echo hi"` round trips: pwsh 520 ms → 170 ms, 5.1 2.6 s → 0.2–0.9 s. If only 5.1 is installed, installing PowerShell 7 (`pwsh`) helps further. `sh`/`bash`/`cmd` start fast and are not prewarmed. After `prewarm_idle_secs` (default 600) without any `exec` the idle processes are released, so an unused machine holds no extra memory; the next `exec` cold-starts once and refills the pool.

## Security model

Intended for machines you own on a trusted network.

- Traffic is TLS. Clients pin the server certificate fingerprint; a changed fingerprint is a hard error (exit 106).
- Access requires a token: the `default` one from `config.toml`, or a per-caller token from `token add`. Tokens are compared in constant time, and failures are delayed and audited.
- Rollback re-resolves recorded paths before checking permissions and again before applying each entry, rejecting directory symlinks that would leave the workspace.
- File operations are confined to workspaces: canonicalized paths, no `..`, no absolute paths, no symlink/junction escapes, deny-list checked on every ancestor. The agent-cli-x data folder is always denied.
- Path permissions are enforced per operation: `ls`/`find`/`read`/`grep`/`get` and the `exec` working directory need `r`; `write`/`put` need `c` for new files and `u` for existing ones; `edit` needs `u`; `rm` needs `d` on **every** entry it would remove (otherwise nothing is deleted); `rollback` checks the action it performs (restore = `u`, re-create = `c`, remove = `d`). **A shell command itself cannot be sandboxed**: `exec "cat secrets/x"` or `exec "rm x"` still works, and if `serve` runs as root/Administrator so can `exec "npm install -g …"` into a read-only `/usr`. `info` prints the exec user and whether it is elevated so callers can see this. For hard isolation, run `serve` as a dedicated low-privilege OS user and protect directories with file-system permissions.
- `init` writes OS-specific presets, which you are free to edit:
  - Windows: `C:/` read-only, `C:/Windows/System32/config` denied; commands such as `rd /s /q c:\`, `Remove-Item -Recurse -Force C:\Windows`, `format c:`, `diskpart`, `bcdedit` blocked. **If your workspaces live on `C:`, add a more specific `[[policy.paths]]` rule for them (e.g. `allow = "rcud"`) to allow writes.**
  - Linux: `/bin`, `/sbin`, `/lib*`, `/usr`, `/etc`, `/boot`, `/var/lib`, `/proc`, `/sys`, `/dev` read-only; `/etc/shadow`, `/etc/gshadow`, `/etc/sudoers*`, `/root/.ssh` denied; `rm -rf /etc` (and other system dirs), `--no-preserve-root`, `mkfs`, `reboot` etc. blocked.
  - macOS: `/System`, `/Library`, `/usr`, `/bin`, `/sbin`, `/private/etc`, `/private/var/db`, `/Applications` read-only.
- `deny_commands` is a guard rail against accidents, not a security boundary. So is `token add --no-relay` (see [One-way access](#one-way-access)).
- Workspaces and path rules are guard rails too: any authorized caller can already change files through `exec`, so `workspace add <peer> …` is open to every token, and each change is recorded in the audit log (`"op": "workspace"`). Without configured workspaces the user's home directory is served as `home`; the agent-cli-x data dir inside it is always denied.
- `grep`, `find`, `get` and `ls` skip unreadable paths; `put` refuses to write where it lacks `c`/`u`.
- Background jobs run as the same user; their output lives in `~/.agent-cli-x/jobs/` (7 days). They survive a normal `serve` restart; a job is marked `lost` when its supervisor is gone.
- Every write/edit/put is backed up to `~/.agent-cli-x/backup/` (7 days). All requests are logged to `~/.agent-cli-x/audit/YYYY-MM-DD.jsonl` with the client IP and token name.
- `rm` moves files to the system recycle bin (Windows Recycle Bin, freedesktop Trash on Linux, Trash on macOS), so it is fast and has no size cap; `rollback` restores them from there (Windows/Linux; on macOS restore from the Trash by hand). It refuses to restore over a path that exists again. Trash-backed operations record the exact system entry ID (on Linux also its filesystem identity), so repeated deletion of the same path cannot select an earlier version. Legacy or ambiguous trash records are refused with exit 105 and require manual recovery or deletion in the system recycle bin; their backup metadata is retained. If the recycle bin is unavailable (e.g. a Linux mount without a writable `.Trash` dir) or `rm_use_trash = false`, it falls back to backing up to `~/.agent-cli-x/backup/` and deleting, limited to `max_transfer_mb`. `rm --permanent` skips both and cannot be undone. `purge <op_id>` permanently deletes an earlier `rm`'s recycle-bin entries (needs `d` on the original paths) and removes the operation's backups, freeing the space without waiting for the 7-day cleanup.
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
ExecStart=%h/.local/bin/agent-cli-x serve --foreground
Restart=on-failure
RestartSec=5
# keep background jobs running when serve restarts or is upgraded
KillMode=process

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
$action    = New-ScheduledTaskAction -Execute $exe -Argument 'serve --foreground'
$trigger   = New-ScheduledTaskTrigger -AtStartup
$principal = New-ScheduledTaskPrincipal -UserId "$env:USERDOMAIN\$env:USERNAME" -LogonType S4U -RunLevel Limited
$settings  = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) `
               -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
               -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) -MultipleInstances IgnoreNew
Register-ScheduledTask -TaskName agent-cli-x -Action $action -Trigger $trigger -Principal $principal -Settings $settings
Start-ScheduledTask -TaskName agent-cli-x

# allow the program once (unlike a port rule, it keeps working after `set-port`; upgrades keep the same path)
& $exe firewall open
```

Why these settings: Task Scheduler's defaults stop a task after 3 days and skip or stop it on battery power. `S4U` runs it at boot without anyone logged in, with no console window and no stored password. The trade-off is that the task has no network credentials, so mapped drives and Credential Manager logins are unavailable to `exec`. If you need them, use `-AtLogOn` with `-LogonType Interactive` instead. That variant only starts after you log in and shows a console window, and closing the window stops `serve`.

For a manual windowless start, run `agent-cli-x serve`; `agent-cli-x stop` ends it. `stop` exits with code 0, so `Restart=on-failure` and the Task Scheduler restart setting leave it stopped, but autostart starts it again at the next boot: disable it with `systemctl --user disable agent-cli-x` or `Disable-ScheduledTask -TaskName agent-cli-x`. For windowless autostart, use the `S4U` task above. Use `serve --foreground` in Task Scheduler/systemd actions so the manager tracks the actual server and can restart it on failure; default background mode exits the launching process after startup. Update existing task actions/unit files to add `--foreground` when upgrading.

After a reboot, run `agent-cli-x doctor` on the machine. On Windows it flags a task that will stop after 72 hours, a task that stops or won't start on battery, a task that launches a different binary path, and a missing firewall rule. On Linux it flags a unit that isn't enabled and linger that is off. Also run `agent-cli-x info <peer>` from another machine to confirm the peer can be reached.

## Exit codes

Remote exit codes pass through. agent-cli-x's own errors: 100 connect, 101 auth, 102 policy denied, 103 not found, 104 timeout, 105 conflict, 106 fingerprint, 107 local/remote I/O error, 108 other, 109 config (e.g. unknown peer, missing config), 110 bad arguments, 111 busy, 112 desktop unavailable, 113 unsupported OS. `grep` and `find` exit 1 when nothing matches; `job logs` exits 137 for a killed job. `upgrade` / `restart` exit 104 when the restart was issued but the peer hasn't come back yet.

## License

[MIT](LICENSE)
