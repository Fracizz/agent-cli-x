# Changelog

## 0.0.7

Fixes for safe file recovery, process supervision and bounded agent output.

- Recheck real paths and CRUD permissions before rollback to reject parent symlink escapes.
- Identify recycle-bin entries by their system identity so repeated deletion of one path cannot restore or purge another version.
- Keep enforcing command timeouts and background-job termination after stdout and stderr close.
- Enforce the 64-connection limit before spawning workers, and authenticate before reading request bodies. Preserve authentication errors during large uploads.
- Retain complete logs for every output cut, including single-line cuts; apply one stdout/stderr budget to ordinary and JSON output.
- Preserve the actual tail of multi-peer output beyond 8MB, and finish rejected frame uploads promptly.
- Bound long-line reads without blocking later lines; provide download instructions for oversized explicit ranges.
- Add byte budgets and stable `--offset` pagination to `ls` and `grep`, with matching JSON continuation fields and `--full` overrides.
- Align the minimum Rust version with locked dependencies and check Rust 1.88 in CI.

Compatibility notes:

- Building from source now requires Rust 1.88 or newer.
- Legacy or ambiguous recycle-bin records return exit 105 and require manual handling in the system recycle bin. Backup metadata remains available.
- Default command output shares its budget across stdout and stderr. `ls`/`grep` pagination requires an upgraded peer; old peers retain basic command support.

Validation: 64 unit tests, 12 end-to-end tests and 6 runtime regressions pass on Linux, together with formatting, strict Clippy, the release build and a Rust 1.88 check of all targets.
