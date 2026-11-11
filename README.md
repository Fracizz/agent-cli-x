# agent-cli-x

A small, single-binary CLI that lets AI agents on different machines (Windows or Linux) run commands and edit files on each other.

```
Machine A: Claude Code ──> agent-cli-x ──TLS──> Machine B: agent-cli-x serve ──> exec / read / edit
Machine B: Codex       ──> agent-cli-x ──TLS──> Machine A: agent-cli-x serve ──> exec / read / edit
```

- **One binary, both roles.** Every machine runs `agent-cli-x serve`, and every machine can call its peers.
- **Lightweight.** Synchronous std + rustls, no async runtime, no OpenSSL. Short-lived HTTPS request per call.
- **Built for agents.** Exit codes pass through, long output is trimmed to head/tail with the full log saved locally, `--json` everywhere, exact-match `edit`, `rollback`.
- **Guard rails.** Token auth, self-signed TLS with fingerprint pinning, workspace allow-list, path deny-list, command deny-list, automatic backups, audit log.

## Install

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
agent-cli-x init --workspace proj=/home/me/proj     # prints token + fingerprint
agent-cli-x serve                                   # keep running (service setup below)
```

On the machine that wants to call B:

```bash
agent-cli-x peer add B 10.0.0.5 --token <B's token> --fingerprint <B's fingerprint>
agent-cli-x info B
agent-cli-x exec B cargo test
```

Without `--fingerprint`, `peer add` only prints the fingerprint it sees. Compare it with `agent-cli-x fingerprint` on B before registering.

Then teach your agent the tool: copy [`skills/agent-cli-x/SKILL.md`](skills/agent-cli-x/SKILL.md) into its skills folder (e.g. `~/.claude/skills/agent-cli-x/`), or run `agent-cli-x skill` to print it.

## Commands

| Command | Purpose |
|---|---|
| `init` / `serve` / `fingerprint` | set up and run this machine |
| `peer add\|list\|remove` | manage peers (`~/.agent-cli-x/peers.toml`) |
| `info <peer>` | OS, shell, workspaces, deny rules |
| `exec <peer> [--cwd] [--shell] [--timeout] [--env K=V] <cmd>` | run a command, streamed |
| `ls` / `read` / `write` / `edit` | file operations inside workspaces |
| `rollback <peer> <op_id>` | undo a write/edit |
| `skill` | print the agent guide |

Paths use `workspace:relative/path`. See `agent-cli-x --help`.

## Configuration

`~/.agent-cli-x/config.toml` (override the folder with `AGENT_CLI_X_HOME`). The generated file is fully commented. Key parts:

```toml
listen = "0.0.0.0:7700"
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
```

## Security model

Intended for machines you own on a trusted network.

- Traffic is TLS. Clients pin the server certificate fingerprint; a changed fingerprint is a hard error (exit 106).
- Access requires the peer's token (compared in constant time; failures are delayed and audited).
- File operations are confined to workspaces: canonicalized paths, no `..`, no absolute paths, no symlink/junction escapes, deny-list checked on every ancestor. The agent-cli-x data folder is always denied.
- `deny_paths` applies to `ls`/`read`/`write`/`edit`/`rollback` and to the `exec` working directory. **A shell command itself cannot be sandboxed**: `exec "cat secrets/x"` can still read the file. For hard isolation, run `serve` as a dedicated low-privilege OS user and protect directories with file-system permissions.
- `deny_commands` is a guard rail against accidents, not a security boundary.
- Every write/edit is backed up to `~/.agent-cli-x/backup/` (7 days). All requests are logged to `~/.agent-cli-x/audit/YYYY-MM-DD.jsonl`.

## Running as a background service

Run `serve` **as your own user**, not as SYSTEM/root, so commands see your toolchain and logins.

- **Linux (systemd user unit):** `~/.config/systemd/user/agent-cli-x.service` with `ExecStart=%h/.local/bin/agent-cli-x serve`, then `systemctl --user enable --now agent-cli-x` (and `loginctl enable-linger $USER` to start at boot).
- **Windows:** Task Scheduler → "At log on" → `agent-cli-x.exe serve`. Allow the port once from an admin shell:
  `netsh advfirewall firewall add rule name="agent-cli-x" dir=in action=allow protocol=TCP localport=7700`

## Exit codes

Remote exit codes pass through. agent-cli-x's own errors: 100 connect, 101 auth, 102 policy denied, 103 not found, 104 timeout, 105 conflict, 106 fingerprint, 110 bad arguments.

## Roadmap

- `put` / `get` file transfer, `grep`
- detached jobs (`exec --detach`, `job logs|kill`)
- `ask`: hand a task to the peer's own AI CLI (`claude -p`, `codex exec`) with depth limits

## License

MIT
