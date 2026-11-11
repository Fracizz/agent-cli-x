---
name: agent-cli-x
description: Run commands, search, read/edit and transfer files on another registered machine (peer) through agent-cli-x. Use when a task needs to build, test, inspect logs, or change code on a remote Windows/Linux server instead of the local one.
---

# agent-cli-x

Run commands and edit files on a **peer** machine over a pinned-TLS connection.
Install the binary on `PATH` (see README) so it can be called as `agent-cli-x` from any directory.

## Start here

```bash
agent-cli-x whoami             # which machine am I (node name + id)
agent-cli-x peer list          # registered peers: node name + id, OS/arch, default shell (cached, no network)
agent-cli-x info <peer>        # live: workspaces, deny rules, jobs (also refreshes the cached OS/shell)
agent-cli-x info @all          # every peer: online?, version, latency
agent-cli-x doctor             # local service, start at boot, firewall, peers
```

Each machine has a `node_name` (human label) and a `node_id` (UUID). Use them to tell machines apart in your reasoning and reports; the peer alias in `peer list` is just a local nickname.

## Paths

`workspace:relative/path`, e.g. `proj:src/main.rs`; workspace root is `proj:`.
The prefix may be omitted when the peer has a single or default workspace.
Absolute paths, `..`, anything outside a workspace and deny-listed paths are rejected (exit 102). Paths also carry CRUD permissions set by the peer owner (`info` shows them as `path=rcud`: r read, c create, u update, d delete); e.g. a read-only system drive refuses write/edit/rm with exit 102. Do not try to work around them (for example by deleting or overwriting through `exec`).

## Commands

```bash
agent-cli-x exec B cargo test                          # exit code is passed through
agent-cli-x exec B --cwd proj:web --timeout 1200 "pnpm build"   # options go BEFORE the command
agent-cli-x exec B --shell cmd "dir"                   # pwsh | powershell | cmd | sh | bash
agent-cli-x exec B --detach "pnpm build"              # background job, returns a job id
agent-cli-x job logs B <id> --follow                   # stream until done; exits with the job's code
agent-cli-x job list B | job kill B <id>
agent-cli-x ls   B proj:src --depth 2                  # respects .gitignore
agent-cli-x grep B "fn main" proj:src --glob "*.rs"    # regex; -i ignore case, -F literal; exit 1 = no match
agent-cli-x read B proj:src/main.rs --lines 100:200 -n
agent-cli-x edit B proj:src/main.rs --old "old text" --new "new text"
agent-cli-x edit B proj:src/main.rs --old-file old.txt --new-file new.txt   # multi-line / quotes
agent-cli-x write B proj:notes.md --content "..."      # or --from <local file>, or stdin
agent-cli-x put B ./dist proj:web/dist --exclude node_modules   # upload file/dir (like cp -r)
agent-cli-x get B proj:logs ./logs                     # download file/dir
agent-cli-x rm B proj:build/tmp -r                     # delete file / dir (-r) into the recycle bin; undo with rollback
agent-cli-x rollback B <op_id>                         # undo a write/edit/put/rm
agent-cli-x exec B,C "git pull && cargo build"         # several peers in parallel, output grouped per peer
agent-cli-x exec @all --json "uname -a"                # every registered peer; one result per peer
agent-cli-x upgrade B                                  # push this binary to B and restart its serve
```

Add `--json` to any command for machine-readable output.

## Rules of thumb

- Commands run non-interactively (empty stdin, `CI=1`). Anything that prompts will hang until timeout: pass `-y` / `--yes` / `--no-edit`.
- Default shell: `pwsh` (fallback `powershell`) on Windows, `sh` on Linux. `peer list` already shows each peer's OS and shell, so you do not need `info` just for that. If it shows `-` (peer registered by an older version), run `info <peer>` once to fill it in.
- Every `exec` is a fresh shell: `cd`, variables and env changes do not carry over to the next call. Use `--cwd` / `--env`, or chain steps in one command.
- Long output shows the first and last ~200 lines; the full log path is printed to stderr and can be read locally.
- `edit` needs `--old` to match **exactly once** (including indentation). Widen the context or use `--all`. CRLF/LF is handled.
- `read` before `edit`. To guard against concurrent changes, take `sha256` from `read --json` and pass `--expect-sha`.
- Every write/edit/put/rm returns an `op_id`; use `rollback` if something goes wrong. Prefer `rm` over `exec "rm ..."`: it checks permissions and can be undone. Use `rm --permanent` (no recycle bin, no undo) only when the user explicitly asks for permanent deletion.
- Raise `--timeout` for long builds (capped by the peer). On timeout the whole process tree is killed (exit 104).
- Anything longer than a few minutes (builds, test suites, servers): use `exec --detach`, then `job logs --follow`.
- Prefer `grep` over `exec "grep ..."`: it respects .gitignore and deny rules and works the same on Windows.
- Multi-peer `exec` (`A,B` / `@all`) exits 0 only if every peer succeeded; otherwise with the first failing peer's code. Read the per-peer headers and the summary line. `--detach` and file commands take a single peer.
- A `版本不一致` hint means a peer runs a different version. Run `agent-cli-x upgrade <peer>`, or `--binary <file>` if its OS/arch differs from yours. An upgrade is refused while background jobs are running (exit 105). Only use `--force` if interrupting those jobs is acceptable.
- If a peer is unreachable (exit 100), run `agent-cli-x doctor` here. On the peer itself, check `doctor` and `~/.agent-cli-x/serve-logs/`.
- Setting up start at boot for `serve` on a machine you run on: follow README "Start at boot". On Windows, the scheduled task needs `ExecutionTimeLimit 0`, both battery options turned off, and restart on failure. On Linux, use a systemd user unit with `Restart=on-failure` plus `loginctl enable-linger`. Confirm with `agent-cli-x doctor` afterwards.
- `put`/`get`: if the destination is an existing directory the source goes inside it; otherwise the destination is the new path. Overwritten files on the peer are backed up (`rollback <op_id>`); local files are overwritten by `get`.

## Exit codes

| Code | Meaning |
|---|---|
| other | the remote command's own exit code |
| 100 | cannot connect (peer not running, network, firewall) |
| 101 | invalid token |
| 102 | denied by policy (deny-listed path/command, path escape) |
| 103 | file or workspace not found |
| 104 | timeout |
| 105 | conflict (edit no/multiple match, sha mismatch) |
| 106 | certificate fingerprint mismatch: ask a human, never re-register on your own |
| 107 | I/O error (disk, permissions) |
| 108 | other internal error |
| 109 | config problem (unknown peer, missing/invalid config): run `agent-cli-x peer list` |
| 110 | bad arguments |
| 111 | busy (too many connections or jobs) |
| 137 | job was killed |
