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
agent-cli-x peer list          # registered peers with their node name + id
agent-cli-x info <peer>        # node name/id, OS, default shell, workspaces, deny rules
```

Each machine has a `node_name` (human label) and a `node_id` (UUID). Use them to tell machines apart in your reasoning and reports; the peer alias in `peer list` is just a local nickname.

## Paths

`workspace:relative/path`, e.g. `proj:src/main.rs`; workspace root is `proj:`.
The prefix may be omitted when the peer has a single or default workspace.
Absolute paths, `..`, anything outside a workspace and deny-listed paths are rejected (exit 102). Do not try to work around them.

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
agent-cli-x rollback B <op_id>                         # undo a write/edit/put
```

Add `--json` to any command for machine-readable output.

## Rules of thumb

- Commands run non-interactively (empty stdin, `CI=1`). Anything that prompts will hang until timeout: pass `-y` / `--yes` / `--no-edit`.
- Default shell: `pwsh` (fallback `powershell`) on Windows, `sh` on Linux. Check `info` and use matching syntax.
- Long output shows the first and last ~200 lines; the full log path is printed to stderr and can be read locally.
- `edit` needs `--old` to match **exactly once** (including indentation). Widen the context or use `--all`. CRLF/LF is handled.
- `read` before `edit`. To guard against concurrent changes, take `sha256` from `read --json` and pass `--expect-sha`.
- Every write/edit returns an `op_id`; use `rollback` if something goes wrong.
- Raise `--timeout` for long builds (capped by the peer). On timeout the whole process tree is killed (exit 104).
- Anything longer than a few minutes (builds, test suites, servers): use `exec --detach`, then `job logs --follow`.
- Prefer `grep` over `exec "grep ..."`: it respects .gitignore and deny rules and works the same on Windows.
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
| 110 | bad arguments |
| 111 | busy (too many connections or jobs) |
| 137 | job was killed |
