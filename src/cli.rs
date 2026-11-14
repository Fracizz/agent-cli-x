//! 命令行入口。

use crate::client::{self, multi, output::ExecOutput, Client};
use crate::config::{self, Peer, TokenEntry, DEFAULT_TOKEN_NAME};
use crate::error::{AppError, Result};
use crate::{doctor, http, server, tls, util};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::time::Duration;

const SKILL: &str = include_str!("../skills/agent-cli-x/SKILL.md");
const CALL_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Parser)]
#[command(
    name = "agent-cli-x",
    version,
    about = "Cross-machine tool for AI agents: run commands and read/write files on other servers",
    after_help = "Path format: workspace:relative/path, e.g. proj:src/main.rs (prefix optional with one workspace).\n\
Exit codes: remote command's code is passed through; agent-cli-x errors: 100 connect / 101 auth / 102 policy denied / \
103 not found / 104 timeout / 105 conflict / 106 fingerprint / 110 args / 111 busy.\nFull guide for AI: agent-cli-x skill"
)]
pub struct Cli {
    /// Output as JSON
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Initialize this machine: machine ID, token, self-signed cert, commented config.toml
    Init {
        /// Node name (default: hostname), shown in peers' info / peer list
        #[arg(long)]
        name: Option<String>,
        /// Listen address
        #[arg(long, default_value = "0.0.0.0:47821")]
        listen: String,
        /// Workspace, repeatable: --workspace proj=D:/code/proj
        #[arg(long = "workspace", value_name = "NAME=PATH")]
        workspaces: Vec<String>,
        /// Overwrite existing config (replaces token, cert and machine ID; peers must re-run peer add)
        #[arg(long)]
        force: bool,
    },
    /// Run server in foreground (logs also go to ~/.agent-cli-x/serve-logs/)
    Serve {
        /// Max seconds to wait if the port is in use (new process waits for old one on upgrade)
        #[arg(long, hide = true, default_value_t = 0)]
        wait_port: u64,
        /// Only validate config.toml and cert, don't start (use after manual edits)
        #[arg(long)]
        check: bool,
    },
    /// Show this node's name, ID, fingerprint and workspaces (offline)
    Whoami {
        /// Also print the `peer add` command for other machines (includes default token)
        #[arg(long)]
        pair: bool,
    },
    /// Self-check: config, local service, autostart, firewall, peer connectivity and versions
    Doctor {
        /// Skip peer checks
        #[arg(long)]
        no_peers: bool,
    },
    /// Manage tokens for access to this machine (issued per caller, individually revocable)
    #[command(subcommand)]
    Token(TokenCmd),
    /// Upgrade peers remotely: upload new binary and restart (peer may be A,B or @all)
    Upgrade {
        peer: String,
        /// Binary to deploy, default: the running one (peer OS/arch must match)
        #[arg(long)]
        binary: Option<PathBuf>,
        /// Upgrade even if peer has running background jobs (jobs are interrupted)
        #[arg(long)]
        force: bool,
    },
    /// Restart peer serve (binary unchanged; peer validates config first, refuses if invalid); peer may be A,B or @all
    Restart {
        peer: String,
        /// Restart even if peer has running background jobs (jobs are interrupted)
        #[arg(long)]
        force: bool,
    },
    /// Change peer listen port: switches only after the new port is confirmed reachable, else reverted; also updates the local peer address
    SetPort { peer: String, port: u16 },
    /// Print this machine's cert fingerprint (for verification in peer add)
    Fingerprint,
    /// Background job runner (started by serve, do not run manually)
    #[command(hide = true)]
    JobRun { dir: PathBuf },
    /// Print usage guide for AI
    Skill,
    /// Manage peers
    #[command(subcommand)]
    Peer(PeerCmd),
    /// Show peer info (name/ID, OS, shell, workspaces, deny list); A,B or @all shows an overview
    Info { peer: String },
    /// Run a command on a peer (options before the command); A,B or @all runs in parallel with grouped output
    Exec {
        peer: String,
        /// Working dir (workspace:relative/path), default: peer's default workspace root
        #[arg(long)]
        cwd: Option<String>,
        /// pwsh / powershell / cmd / sh / bash; default pwsh on Windows, sh on Linux
        #[arg(long)]
        shell: Option<String>,
        /// Timeout in seconds (capped by max_timeout_secs foreground, job_timeout_secs background)
        #[arg(long)]
        timeout: Option<u64>,
        /// Extra env var, repeatable: --env KEY=VALUE
        #[arg(long = "env", value_name = "KEY=VALUE")]
        envs: Vec<String>,
        /// Run as background job, return job ID immediately (manage with job logs / job kill)
        #[arg(long)]
        detach: bool,
        /// Don't truncate output
        #[arg(long)]
        full: bool,
        /// Run contents of a local script file (`-` = stdin), avoids quote escaping; not allowed with a command
        #[arg(long, value_name = "FILE", conflicts_with = "command")]
        script: Option<PathBuf>,
        /// Command to run (multiple args are joined by spaces)
        #[arg(
            required_unless_present = "script",
            trailing_var_arg = true,
            allow_hyphen_values = true
        )]
        command: Vec<String>,
    },
    /// Manage background jobs
    #[command(subcommand)]
    Job(JobCmd),
    /// List a directory (honors .gitignore, hides denied paths)
    Ls {
        peer: String,
        path: Option<String>,
        #[arg(long, default_value_t = 1)]
        depth: usize,
        /// Max entries to list
        #[arg(long, default_value_t = 500)]
        max: usize,
    },
    /// Find files by name glob, newest first (honors .gitignore, hides denied paths)
    #[command(
        after_help = "Pattern is relative to the search dir; without `/` it matches at any depth.\nExamples: find B '*.rs' proj:src   find B 'src/**/test_*.py'   find B 'Cargo.toml'"
    )]
    Find {
        peer: String,
        /// Glob, e.g. "*.rs", "src/**/*.ts"
        pattern: String,
        /// Search dir (workspace:relative/path), default: default workspace root
        path: Option<String>,
        /// Max paths to return
        #[arg(long, default_value_t = 200)]
        max: usize,
    },
    /// Search file contents on a peer (regex, honors .gitignore, skips binaries and denied paths)
    Grep {
        peer: String,
        pattern: String,
        /// Search scope (workspace:relative/path), default: default workspace root
        path: Option<String>,
        /// Only search matching files, repeatable: --glob "*.rs"
        #[arg(long)]
        glob: Vec<String>,
        /// Case-insensitive
        #[arg(short = 'i', long)]
        ignore_case: bool,
        /// Match literally (not regex)
        #[arg(short = 'F', long)]
        fixed: bool,
        /// Lines of context around each match (max 50)
        #[arg(short = 'C', long, value_name = "N")]
        context: Option<usize>,
        /// Lines of context after each match
        #[arg(short = 'A', long, value_name = "N")]
        after: Option<usize>,
        /// Lines of context before each match
        #[arg(short = 'B', long, value_name = "N")]
        before: Option<usize>,
        /// Only list files that contain a match
        #[arg(short = 'l', long, conflicts_with_all = ["count", "context", "after", "before"])]
        files: bool,
        /// Count matching lines per file
        #[arg(short = 'c', long, conflicts_with_all = ["context", "after", "before"])]
        count: bool,
        /// Max matching lines to return (files with -l / -c)
        #[arg(long, default_value_t = 200)]
        max: usize,
        /// Skip the first N results (continue after a truncated search)
        #[arg(long, default_value_t = 0)]
        skip: usize,
    },
    /// Read a text file
    Read {
        peer: String,
        path: String,
        /// Line range, e.g. 100:200, 100:, :50. Without an end line, reads at most 2000 lines / 64KB
        #[arg(long)]
        lines: Option<String>,
        /// Prefix line numbers
        #[arg(short = 'n', long)]
        number: bool,
    },
    /// Write a whole file (content from --content / --from / stdin)
    Write {
        peer: String,
        path: String,
        #[arg(long, conflicts_with = "from")]
        content: Option<String>,
        /// Read content from a local file
        #[arg(long)]
        from: Option<PathBuf>,
        /// Create parent dirs if missing
        #[arg(long)]
        mkdir: bool,
        /// Write only if the remote file's current sha256 equals this (optimistic lock)
        #[arg(long)]
        expect_sha: Option<String>,
    },
    /// Exact text replace in a file (old must match uniquely)
    #[command(
        after_help = "Several edits in one call (applied in order, all or nothing):\n  agent-cli-x edit B proj:a.rs --edits - <<'EOF'\n  [{\"old\": \"fn a()\", \"new\": \"fn b()\"}, {\"old\": \"x\", \"new\": \"y\", \"all\": true}]\n  EOF"
    )]
    Edit {
        peer: String,
        path: String,
        /// JSON array [{"old","new","all"?}] from a file or `-` (stdin); replaces --old/--new
        #[arg(long, value_name = "FILE", conflicts_with_all = ["old", "old_file", "new", "new_file", "all"])]
        edits: Option<PathBuf>,
        #[arg(long, conflicts_with = "old_file")]
        old: Option<String>,
        /// Read old text from a local file (avoids shell escaping)
        #[arg(long)]
        old_file: Option<PathBuf>,
        #[arg(long, conflicts_with = "new_file")]
        new: Option<String>,
        /// Read new text from a local file
        #[arg(long)]
        new_file: Option<PathBuf>,
        /// Replace all matches
        #[arg(long)]
        all: bool,
        #[arg(long)]
        expect_sha: Option<String>,
    },
    /// Upload a local file or dir to a peer (into the target if it is an existing dir, like cp -r)
    Put {
        peer: String,
        local: PathBuf,
        /// Remote target (workspace:relative/path)
        dest: String,
        /// Files/dirs to exclude, repeatable: --exclude node_modules --exclude "*.log"
        #[arg(long)]
        exclude: Vec<String>,
    },
    /// Download a file or dir from a peer (into the local target if it is an existing dir)
    Get {
        peer: String,
        /// Remote source (workspace:relative/path)
        src: String,
        local: PathBuf,
        #[arg(long)]
        exclude: Vec<String>,
    },
    /// Copy a file or dir directly between two peers (streamed via this machine, not saved to local disk)
    #[command(
        after_help = "Example: agent-cli-x cp win127 dl:pkg ub40 home:\nSame as put: into the target if it is an existing dir. Target needs 0.0.4+; older versions fall back to a local temp dir relay (deleted afterwards)."
    )]
    Cp {
        /// Source peer
        src_peer: String,
        /// Source path (workspace:relative/path)
        src: String,
        /// Target peer
        dst_peer: String,
        /// Target path (workspace:relative/path)
        dest: String,
        #[arg(long)]
        exclude: Vec<String>,
    },
    /// Delete a file or dir: moved to the peer's trash by default, restorable via rollback (needs path permission d)
    Rm {
        peer: String,
        path: String,
        /// Delete a dir and all its contents
        #[arg(short = 'r', long)]
        recursive: bool,
        /// Delete permanently: no trash, no backup, unrecoverable
        #[arg(long)]
        permanent: bool,
    },
    /// Undo one write / edit / put / rm
    Rollback { peer: String, op_id: String },
    /// Permanently remove the trash entry and backups of an operation (frees space; no rollback afterwards)
    Purge { peer: String, op_id: String },
}

#[derive(Subcommand)]
enum PeerCmd {
    /// Register a peer. Without --fingerprint it only shows the fingerprint for verification
    Add {
        name: String,
        /// IP:port (default port 47821)
        addr: String,
        #[arg(long)]
        token: String,
        /// Peer fingerprint (get via `agent-cli-x fingerprint` on the peer)
        #[arg(long)]
        fingerprint: Option<String>,
    },
    /// Change a registered peer's address or token (verified by the stored fingerprint; saved only once reachable)
    Set {
        name: String,
        /// New address IP:port
        #[arg(long, required_unless_present = "token")]
        addr: Option<String>,
        #[arg(long)]
        token: Option<String>,
    },
    /// List registered peers
    List,
    /// Remove a peer
    Remove { name: String },
}

#[derive(Subcommand)]
enum TokenCmd {
    /// Issue a token for a caller and print the `peer add` command it needs
    Add {
        name: String,
        /// No relay: this caller's exec on this machine can't use agent-cli-x to reach other nodes or manage credentials
        #[arg(long)]
        no_relay: bool,
    },
    /// Change restrictions of an issued token (token unchanged)
    Set {
        name: String,
        /// No relay
        #[arg(long, required_unless_present = "allow_relay")]
        no_relay: bool,
        /// Allow relay again
        #[arg(long, conflicts_with = "no_relay")]
        allow_relay: bool,
    },
    /// List issued tokens (name, creation time, restrictions)
    List,
    /// Revoke a caller's token (takes effect within seconds on a running serve)
    Remove { name: String },
    /// Rotate a token (name `default` rotates the default token in config.toml)
    Rotate { name: String },
}

#[derive(Subcommand)]
enum JobCmd {
    /// List recent background jobs
    List {
        peer: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Show job output; --follow streams until the job ends and exits with its exit code
    Logs {
        peer: String,
        id: String,
        #[arg(short = 'f', long)]
        follow: bool,
        /// Don't truncate output
        #[arg(long)]
        full: bool,
    },
    /// Kill a job (whole process tree)
    Kill { peer: String, id: String },
}

/// 在“禁止借道”的 exec 环境里，只放行不联网、不涉及凭据的本地命令。
fn check_no_relay(cmd: &Cmd) -> Result<bool> {
    let Some(who) = std::env::var(config::NO_RELAY_ENV)
        .ok()
        .filter(|v| !v.is_empty())
    else {
        return Ok(false);
    };
    match cmd {
        Cmd::Skill | Cmd::Fingerprint | Cmd::Doctor { .. } | Cmd::Whoami { pair: false } => {
            Ok(true)
        }
        _ => Err(AppError::denied(format!(
            "This command runs inside an exec started by caller `{who}`, which is not allowed to relay: it can't use agent-cli-x \
             to reach other nodes or manage local credentials. Work on your own machine; if other nodes are needed, tell the user, \
             and do not try to bypass (reading credential files, clearing {} env vars, direct connections)",
            config::NO_RELAY_ENV
        ))),
    }
}

pub fn run() -> Result<i32> {
    let cli = Cli::parse();
    let json = cli.json;
    let no_relay = check_no_relay(&cli.cmd)?;
    match cli.cmd {
        Cmd::Init {
            name,
            listen,
            workspaces,
            force,
        } => init(name, &listen, &workspaces, force, json),
        Cmd::JobRun { dir } => server::job_run(&dir),
        Cmd::Serve { check: true, .. } => {
            let listen = server::check(&config::home_dir())?;
            if json {
                println!("{}", json!({"ok": true, "listen": listen}));
            } else {
                println!("Config valid (listen {listen})");
            }
            Ok(0)
        }
        Cmd::Serve { wait_port, .. } => server::serve(&config::home_dir(), wait_port).map(|_| 0),
        Cmd::Whoami { pair } => whoami(pair, json),
        // 禁止借道时不探测对端
        Cmd::Doctor { no_peers } => doctor::run(json, !no_peers && !no_relay),
        Cmd::Token(t) => token(t, json),
        Cmd::Upgrade {
            peer,
            binary,
            force,
        } => {
            client::disable_version_warning();
            each_peer(&peer, json, "upgrade", |c| {
                client::upgrade::upgrade(c, binary.as_deref(), force)
            })
        }
        Cmd::Restart { peer, force } => {
            each_peer(&peer, json, "restart", |c| client::admin::restart(c, force))
        }
        Cmd::SetPort { peer, port } => set_port(&peer, port, json),
        Cmd::Fingerprint => {
            let fp = tls::local_fingerprint(&config::home_dir())?;
            if json {
                println!("{}", json!({"fingerprint": fp}));
            } else {
                println!("{fp}");
            }
            Ok(0)
        }
        Cmd::Skill => {
            print!("{SKILL}");
            Ok(0)
        }
        Cmd::Peer(p) => peer(p, json),
        Cmd::Info { peer } if multi::is_multi(&peer) => {
            Ok(multi::info_many(&multi::resolve(&peer)?, json))
        }
        Cmd::Info { peer } => {
            let v = Client::load(&peer)?.call("/v1/info", &json!({}), CALL_TIMEOUT)?;
            let _ = config::remember_peer_infos(&config::home_dir(), &[(peer.as_str(), &v)]);
            if json {
                println!("{v}");
            } else {
                print_info(&peer, &v);
            }
            Ok(0)
        }
        Cmd::Exec {
            peer,
            cwd,
            shell,
            timeout,
            envs,
            detach,
            full,
            script,
            command,
        } => {
            let cmd = match &script {
                Some(f) => script_text(f, shell.as_deref())?,
                None => command.join(" "),
            };
            let body = exec_body(cwd, shell, timeout, envs, cmd)?;
            if multi::is_multi(&peer) {
                if detach {
                    return Err(AppError::bad_request("--detach works with one peer only"));
                }
                let peers = multi::resolve(&peer)?;
                return Ok(multi::exec_many(&peers, &body, timeout, full, json));
            }
            if detach {
                start_job(&peer, &body, json)
            } else {
                exec(&peer, &body, timeout, full, json)
            }
        }
        Cmd::Job(j) => job(j, json),
        Cmd::Ls {
            peer,
            path,
            depth,
            max,
        } => {
            let v = Client::load(&peer)?.call(
                "/v1/fs/ls",
                &json!({"path": path.unwrap_or_default(), "depth": depth, "max": max}),
                CALL_TIMEOUT,
            )?;
            if json {
                println!("{v}");
            } else {
                for e in v["entries"].as_array().into_iter().flatten() {
                    let p = e["path"].as_str().unwrap_or_default();
                    match e["kind"].as_str() {
                        Some("d") => println!("{p}/"),
                        Some("l") => println!("{p}@"),
                        _ => println!("{p}\t{}", human_size(e["size"].as_u64().unwrap_or(0))),
                    }
                }
                if v["truncated"].as_bool() == Some(true) {
                    eprintln!(
                        "[agent-cli-x: --max {max} limit reached; narrow the dir, lower --depth, or use grep]"
                    );
                }
            }
            Ok(0)
        }
        Cmd::Find {
            peer,
            pattern,
            path,
            max,
        } => {
            let v = Client::load(&peer)?.call(
                "/v1/fs/find",
                &json!({"pattern": pattern, "path": path.unwrap_or_default(), "max": max}),
                CALL_TIMEOUT,
            )?;
            let paths = v["paths"].as_array().cloned().unwrap_or_default();
            if json {
                println!("{v}");
            } else {
                let ws = v["ws"].as_str().unwrap_or_default();
                for p in &paths {
                    println!("{ws}:{}", p.as_str().unwrap_or_default());
                }
                if v["truncated"].as_bool() == Some(true) {
                    let more = if v["total_capped"].as_bool() == Some(true) {
                        "+"
                    } else {
                        ""
                    };
                    eprintln!(
                        "[agent-cli-x: showing {} of {}{more} files (newest first); narrow the pattern/dir or raise --max]",
                        paths.len(),
                        v["total"]
                    );
                }
            }
            Ok(if paths.is_empty() { 1 } else { 0 })
        }
        Cmd::Grep {
            peer,
            pattern,
            path,
            glob,
            ignore_case,
            fixed,
            context,
            after,
            before,
            files,
            count,
            max,
            skip,
        } => {
            let mode = if files {
                "files"
            } else if count {
                "count"
            } else {
                "lines"
            };
            let before = before.or(context).unwrap_or(0);
            let after = after.or(context).unwrap_or(0);
            let v = Client::load(&peer)?.call(
                "/v1/fs/grep",
                &json!({
                    "pattern": pattern,
                    "path": path.unwrap_or_default(),
                    "glob": glob,
                    "ignore_case": ignore_case,
                    "fixed": fixed,
                    "max": max,
                    "skip": skip,
                    "mode": mode,
                    "before": before,
                    "after": after,
                }),
                CALL_TIMEOUT,
            )?;
            // 旧版对端会忽略新参数，返回结果里没有 mode：不能把它当成正确结果
            if v["mode"].is_null() && (mode != "lines" || before + after + skip > 0) {
                return Err(AppError::bad_request(format!(
                    "peer does not support -l/-c/-C/-A/-B/--skip (older version); run `agent-cli-x upgrade {peer}`"
                )));
            }
            print_grep(&v, mode, before + after > 0, max, json);
            // 与 grep 一致：无匹配返回 1
            let n = v["count"]
                .as_u64()
                .unwrap_or_else(|| v["matches"].as_array().map_or(0, |a| a.len() as u64));
            Ok(if n == 0 { 1 } else { 0 })
        }
        Cmd::Read {
            peer,
            path,
            lines,
            number,
        } => read(&peer, &path, lines, number, json),
        Cmd::Write {
            peer,
            path,
            content,
            from,
            mkdir,
            expect_sha,
        } => {
            let content = match (content, from) {
                (Some(c), _) => c,
                (None, Some(f)) => read_local_text(&f)?,
                (None, None) => read_stdin()?,
            };
            let v = Client::load(&peer)?.call(
                "/v1/fs/write",
                &json!({"path": path, "content": content, "mkdir": mkdir, "expect_sha": expect_sha}),
                CALL_TIMEOUT,
            )?;
            print_write_result(&v, json, "wrote");
            Ok(0)
        }
        Cmd::Edit {
            peer,
            path,
            edits,
            old,
            old_file,
            new,
            new_file,
            all,
            expect_sha,
        } => {
            let body = match edits {
                Some(f) => {
                    let list = parse_edits(&f)?;
                    json!({"path": path, "edits": list, "expect_sha": expect_sha})
                }
                None => {
                    let old = text_arg(old, old_file, "--old / --old-file (or --edits)")?;
                    let new = text_arg(new, new_file, "--new / --new-file")?;
                    json!({"path": path, "old": old, "new": new, "replace_all": all, "expect_sha": expect_sha})
                }
            };
            let v = Client::load(&peer)?
                .call("/v1/fs/edit", &body, CALL_TIMEOUT)
                .map_err(|mut e| {
                    // 旧版对端不认识 edits，只会报缺少 old 字段
                    if body.get("edits").is_some() && e.message.contains("missing field `old`") {
                        e.message = format!(
                            "peer does not support --edits (older version); run `agent-cli-x upgrade {peer}` or use --old/--new"
                        );
                    }
                    e
                })?;
            print_write_result(&v, json, "edited");
            Ok(0)
        }
        Cmd::Put {
            peer,
            local,
            dest,
            exclude,
        } => {
            let v = client::transfer::put(&Client::load(&peer)?, &local, &dest, &exclude)?;
            if json {
                println!("{v}");
            } else {
                println!(
                    "Uploaded {} files ({}) to {}",
                    v["files"],
                    human_size(v["bytes"].as_u64().unwrap_or(0)),
                    v["dest"].as_str().unwrap_or_default()
                );
                if let Some(op) = v["op_id"].as_str() {
                    println!("  op_id={op} (undo: agent-cli-x rollback {peer} {op})");
                }
                if v["skipped_symlinks"].as_u64().unwrap_or(0) > 0 {
                    eprintln!("[agent-cli-x: skipped {} symlinks]", v["skipped_symlinks"]);
                }
            }
            Ok(0)
        }
        Cmd::Get {
            peer,
            src,
            local,
            exclude,
        } => {
            let v = client::transfer::get(&Client::load(&peer)?, &src, &local, &exclude)?;
            if json {
                println!("{v}");
            } else {
                println!(
                    "Downloaded {} files ({}) to {}",
                    v["files"],
                    human_size(v["bytes"].as_u64().unwrap_or(0)),
                    v["dest"].as_str().unwrap_or_default()
                );
            }
            Ok(0)
        }
        Cmd::Cp {
            src_peer,
            src,
            dst_peer,
            dest,
            exclude,
        } => cp(&src_peer, &src, &dst_peer, &dest, &exclude, json),
        Cmd::Purge { peer, op_id } => {
            let v =
                Client::load(&peer)?.call("/v1/purge", &json!({"op_id": op_id}), CALL_TIMEOUT)?;
            if json {
                println!("{v}");
            } else {
                for f in v["files"].as_array().into_iter().flatten() {
                    let action = match f["action"].as_str() {
                        Some("purged") => "Permanently deleted from trash",
                        _ => "No longer in trash (may have been emptied or restored)",
                    };
                    println!("{action}: {}", f["path"].as_str().unwrap_or_default());
                }
                let n = v["backups"].as_u64().unwrap_or(0);
                if n > 0 {
                    println!(
                        "Deleted {n} backup copies ({})",
                        human_size(v["backup_bytes"].as_u64().unwrap_or(0))
                    );
                }
                println!("Operation {op_id} purged; rollback no longer possible");
            }
            Ok(0)
        }
        Cmd::Rm {
            peer,
            path,
            recursive,
            permanent,
        } => {
            let v = Client::load(&peer)?.call(
                "/v1/fs/rm",
                &json!({"path": path, "recursive": recursive, "permanent": permanent}),
                CALL_TIMEOUT,
            )?;
            if json {
                println!("{v}");
            } else {
                let mut parts = vec![format!(
                    "{} files ({})",
                    v["files"],
                    human_size(v["bytes"].as_u64().unwrap_or(0))
                )];
                if v["dirs"].as_u64().unwrap_or(0) > 0 {
                    parts.push(format!("{} dirs", v["dirs"]));
                }
                if v["links"].as_u64().unwrap_or(0) > 0 {
                    parts.push(format!("{} symlinks", v["links"]));
                }
                let how = match v["method"].as_str() {
                    Some("trash") => "Moved to trash",
                    Some("permanent") => "Permanently deleted",
                    _ => "Deleted (backed up)",
                };
                println!(
                    "{how} {}: {}",
                    v["path"].as_str().unwrap_or_default(),
                    parts.join(", ")
                );
                if let Some(op) = v["op_id"].as_str() {
                    println!("  op_id={op} (undo: agent-cli-x rollback {peer} {op}; free space: agent-cli-x purge {peer} {op})");
                }
                if let Some(e) = v["trash_error"].as_str() {
                    eprintln!(
                        "[agent-cli-x: trash unavailable ({e}); deleted after backup instead]"
                    );
                }
            }
            Ok(0)
        }
        Cmd::Rollback { peer, op_id } => {
            let v = Client::load(&peer)?.call(
                "/v1/rollback",
                &json!({"op_id": op_id}),
                CALL_TIMEOUT,
            )?;
            if json {
                println!("{v}");
            } else {
                for f in v["files"].as_array().into_iter().flatten() {
                    let action = match f["action"].as_str() {
                        Some("deleted") => "Removed created file",
                        Some("rmdir") => "Removed created dir",
                        Some("kept_dir") => "Kept existing dir",
                        Some("recreated_dir") => "Recreated deleted dir",
                        Some("untrashed") => "Restored from trash",
                        Some("relinked") => "Recreated symlink",
                        Some("kept_link") => "Kept existing symlink",
                        Some("link_failed") => {
                            "Symlink recreation failed (possibly missing permission)"
                        }
                        _ => "Restored to pre-change content",
                    };
                    println!("{action}: {}", f["path"].as_str().unwrap_or_default());
                }
            }
            Ok(0)
        }
    }
}

fn init(
    name: Option<String>,
    listen: &str,
    workspaces: &[String],
    force: bool,
    json: bool,
) -> Result<i32> {
    let home = config::home_dir();
    let cfg_path = config::config_path(&home);
    if cfg_path.exists() && !force {
        return Err(AppError::config(format!(
            "{} already exists. To regenerate (replaces token, cert and machine ID) add --force",
            cfg_path.display()
        )));
    }
    let node_name = name.unwrap_or_else(config::default_node_name);
    config::validate_name(&node_name, "machine")?;
    let mut ws = Vec::new();
    for item in workspaces {
        let (name, path) = item.split_once('=').ok_or_else(|| {
            AppError::bad_request(format!("--workspace `{item}` must be NAME=PATH"))
        })?;
        config::validate_name(name, "workspace")?;
        let canon = dunce::canonicalize(path)
            .map_err(|e| AppError::bad_request(format!("Invalid workspace path `{path}`: {e}")))?;
        ws.push((name.to_string(), canon.to_string_lossy().replace('\\', "/")));
    }
    std::fs::create_dir_all(&home)?;
    let token = util::random_hex(24);
    let node_id = util::short_id();
    let fp = tls::generate(&home)?;
    util::atomic_write(
        &cfg_path,
        config::template(&node_name, &node_id, listen, &token, &ws).as_bytes(),
    )?;
    util::restrict_permissions(&cfg_path);
    let port = listen.rsplit(':').next().unwrap_or("47821");
    if json {
        let ws_map: serde_json::Map<String, Value> =
            ws.iter().map(|(n, p)| (n.clone(), json!(p))).collect();
        println!(
            "{}",
            json!({
                "home": home.display().to_string(),
                "config": cfg_path.display().to_string(),
                "node_name": node_name,
                "node_id": node_id,
                "listen": listen,
                "token": token,
                "fingerprint": fp,
                "workspaces": ws_map,
            })
        );
        return Ok(0);
    }
    println!("Initialized {}", home.display());
    println!("  Node:   {node_name} ({node_id})");
    println!("  Config: {}", cfg_path.display());
    println!("  Listen: {listen}");
    println!("  token:  {token}");
    println!("  Fingerprint: {fp}");
    println!();
    println!("Next steps:");
    println!("  1. Edit the config: register accessible dirs under [workspaces]; adjust deny_paths / [[policy.paths]] / deny_commands as needed");
    println!("  2. Run `agent-cli-x serve`");
    println!("  3. On the machine that will access this one (per-caller token: `agent-cli-x token add <name>`):");
    println!("     {}", pair_command(&node_name, listen, &token, &fp));
    println!("  4. Set up autostart (see README 'Start at boot'), then run `agent-cli-x doctor`");
    if cfg!(windows) {
        println!("  Firewall (run once in admin PowerShell; allows the program, so port changes need no update):");
        println!("     {}", crate::firewall::program_rule_cmd());
    } else {
        println!("  Firewall (if ufw is enabled): sudo ufw allow {port}/tcp");
    }
    Ok(0)
}

/// 其他机器登记本机所需的 `peer add` 命令。监听全部网卡时用本机主 IP。
fn pair_command(node_name: &str, listen: &str, token: &str, fp: &str) -> String {
    let port = listen.rsplit(':').next().unwrap_or_default();
    let host = listen[..listen.len() - port.len()].trim_end_matches(':');
    let ip = match host {
        "" | "0.0.0.0" | "[::]" => util::local_ip().unwrap_or_else(|| "<local-IP>".into()),
        h => h.to_string(),
    };
    format!("agent-cli-x peer add {node_name} {ip}:{port} --token {token} --fingerprint {fp}")
}

fn whoami(pair: bool, json: bool) -> Result<i32> {
    let home = config::home_dir();
    config::ensure_node_id(&home)?;
    let cfg = config::load_config(&home)?;
    let fp = tls::local_fingerprint(&home).unwrap_or_default();
    let v = json!({
        "node_name": cfg.node_name(),
        "node_id": cfg.node_id,
        "hostname": crate::platform::hostname(),
        "os": std::env::consts::OS,
        "listen": cfg.listen,
        "fingerprint": fp,
        "workspaces": cfg.workspaces,
        "version": http::VERSION,
        "pair_command": pair.then(|| pair_command(&cfg.node_name(), &cfg.listen, &cfg.token, &fp)),
    });
    if json {
        println!("{v}");
    } else {
        println!(
            "{} ({})  {}/{}",
            cfg.node_name(),
            cfg.node_id.as_deref().unwrap_or("-"),
            std::env::consts::OS,
            std::env::consts::ARCH
        );
        println!("  Listen: {}", cfg.listen);
        println!("  Fingerprint: {fp}");
        for (n, p) in &cfg.workspaces {
            println!("  Workspace {n} = {p}");
        }
        if pair {
            println!("  On the machine that will access this one:");
            println!(
                "    {}",
                pair_command(&cfg.node_name(), &cfg.listen, &cfg.token, &fp)
            );
        }
    }
    Ok(0)
}

fn token(cmd: TokenCmd, json: bool) -> Result<i32> {
    let home = config::home_dir();
    let cfg = config::load_config(&home)?;
    let fp = tls::local_fingerprint(&home)?;
    let mut tokens = config::load_tokens(&home)?;
    let issued = |name: &str, tok: &str, verb: &str| {
        let cmd = pair_command(&cfg.node_name(), &cfg.listen, tok, &fp);
        if json {
            println!(
                "{}",
                json!({"name": name, "token": tok, "pair_command": cmd})
            );
        } else {
            println!("{verb} token `{name}`: {tok}");
            println!("Run on the caller (token shown only once):");
            println!("  {cmd}");
        }
    };
    match cmd {
        TokenCmd::Add { name, no_relay } => {
            config::validate_name(&name, "token")?;
            if name == DEFAULT_TOKEN_NAME || tokens.tokens.contains_key(&name) {
                return Err(AppError::conflict(format!(
                    "token `{name}` already exists; to replace use `agent-cli-x token rotate {name}`"
                )));
            }
            let tok = util::random_hex(24);
            tokens.tokens.insert(
                name.clone(),
                TokenEntry {
                    sha256: util::sha256_hex(tok.as_bytes()),
                    created: util::now_iso(),
                    no_relay,
                },
            );
            config::save_tokens(&home, &tokens)?;
            issued(&name, &tok, "Issued");
            if no_relay && !json {
                println!("This caller is barred from relaying: its exec on this machine can't use agent-cli-x to reach other nodes");
            }
        }
        TokenCmd::Set {
            name,
            no_relay,
            allow_relay: _,
        } => {
            if name == DEFAULT_TOKEN_NAME {
                return Err(AppError::bad_request(
                    "The default token can't be restricted; use a separate `token add <name> --no-relay` for this caller",
                ));
            }
            let Some(e) = tokens.tokens.get_mut(&name) else {
                return Err(AppError::not_found(format!("No token named `{name}`")));
            };
            e.no_relay = no_relay;
            config::save_tokens(&home, &tokens)?;
            if json {
                println!("{}", json!({"name": name, "no_relay": no_relay}));
            } else if no_relay {
                println!("token `{name}` relay disabled (takes effect within seconds on a running serve)");
            } else {
                println!("token `{name}` relay re-enabled (takes effect within seconds on a running serve)");
            }
        }
        TokenCmd::Rotate { name } => {
            let tok = util::random_hex(24);
            if name == DEFAULT_TOKEN_NAME {
                config::replace_config_token(&home, &tok)?;
            } else {
                let Some(e) = tokens.tokens.get_mut(&name) else {
                    return Err(AppError::not_found(format!("No token named `{name}`")));
                };
                e.sha256 = util::sha256_hex(tok.as_bytes());
                e.created = util::now_iso();
                config::save_tokens(&home, &tokens)?;
            }
            issued(&name, &tok, "Rotated");
            if !json {
                println!("Old token invalidated (effective within seconds on a running serve); callers registered with it must re-run peer add");
            }
        }
        TokenCmd::Remove { name } => {
            if name == DEFAULT_TOKEN_NAME {
                return Err(AppError::bad_request(
                    "The default token can't be deleted; rotate it with `agent-cli-x token rotate default`",
                ));
            }
            if tokens.tokens.remove(&name).is_none() {
                return Err(AppError::not_found(format!("No token named `{name}`")));
            }
            config::save_tokens(&home, &tokens)?;
            if json {
                println!("{}", json!({"removed": name}));
            } else {
                println!("Revoked token `{name}` (effective within seconds on a running serve)");
            }
        }
        TokenCmd::List => {
            if json {
                let list: Vec<Value> =
                    std::iter::once(
                        json!({"name": DEFAULT_TOKEN_NAME, "created": null, "no_relay": false}),
                    )
                    .chain(tokens.tokens.iter().map(
                        |(n, e)| json!({"name": n, "created": e.created, "no_relay": e.no_relay}),
                    ))
                    .collect();
                println!("{}", Value::Array(list));
            } else {
                println!("{DEFAULT_TOKEN_NAME}\t(config.toml)");
                for (n, e) in &tokens.tokens {
                    let flag = if e.no_relay { "\tno-relay" } else { "" };
                    println!("{n}\t{}{flag}", e.created);
                }
            }
        }
    }
    Ok(0)
}

/// 逐台执行 upgrade / restart：出问题时影响面最小，也便于看清是哪一台失败。
fn each_peer(
    spec: &str,
    json: bool,
    verb: &str,
    f: impl Fn(&Client) -> Result<Value>,
) -> Result<i32> {
    let peers = multi::resolve(spec)?;
    let mut results = Vec::new();
    let mut code = 0;
    for p in &peers {
        if !json {
            eprintln!("[agent-cli-x: {verb} {p} ...]");
        }
        match Client::load(p).and_then(|c| f(&c)) {
            Ok(v) => {
                if !json {
                    let how = if v["restart"] == "exit" {
                        "restarted by service manager"
                    } else {
                        "restarted automatically"
                    };
                    let secs = v["ms"].as_u64().unwrap_or(0) as f64 / 1000.0;
                    let how = match v["kept_jobs"].as_u64() {
                        Some(n) if n > 0 => format!("{how}, {n} background jobs still running"),
                        _ => how.to_string(),
                    };
                    match v["new_version"].as_str() {
                        Some(new) => println!(
                            "{p}: {} → {new}({how}, took {secs:.1}s)",
                            v["old_version"].as_str().unwrap_or("?")
                        ),
                        None => println!("{p}: restarted({how}, took {secs:.1}s)"),
                    }
                }
                results.push(v);
            }
            Err(e) => {
                if code == 0 {
                    code = e.exit_code();
                }
                if !json {
                    println!("{p}: {verb} failed {e}");
                }
                results.push(json!({"peer": p, "error": http::error_json(&e)}));
            }
        }
    }
    if json {
        println!("{}", json!({"exit_code": code, "results": results}));
    }
    Ok(code)
}

fn set_port(peer: &str, port: u16, json: bool) -> Result<i32> {
    let v = client::admin::set_port(&Client::load(peer)?, port)?;
    if json {
        println!("{v}");
        return Ok(0);
    }
    let addr = v["addr"].as_str().unwrap_or_default();
    let names: Vec<&str> = v["updated_peers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|x| x.as_str())
        .collect();
    println!(
        "{peer}: now listening on port {port}; local entry for {} updated to {addr}",
        names.join(", ")
    );
    println!("Other nodes that registered this machine must run: agent-cli-x peer set <name> --addr {addr}");
    Ok(0)
}

fn peer(cmd: PeerCmd, json: bool) -> Result<i32> {
    let home = config::home_dir();
    match cmd {
        PeerCmd::Add {
            name,
            addr,
            token,
            fingerprint,
        } => {
            config::validate_name(&name, "peer")?;
            let addr = config::normalize_addr(&addr);
            let seen = client::discover_fingerprint(&addr)?;
            let Some(want) = fingerprint else {
                return Err(AppError::fingerprint(format!(
                    "{addr} cert fingerprint is {seen}\nVerify it matches `agent-cli-x fingerprint` on the peer, then register again with:\n  --fingerprint {seen}"
                )));
            };
            if !want.trim().eq_ignore_ascii_case(&seen) {
                return Err(AppError::fingerprint(format!(
                    "Fingerprint mismatch: expected {want}, got {seen}. Do not register; verify the peer's identity and retry"
                )));
            }
            let mut p = Peer {
                addr: addr.clone(),
                token,
                fingerprint: seen,
                ..Default::default()
            };
            let info =
                Client::from_peer(&name, p.clone()).call("/v1/info", &json!({}), CALL_TIMEOUT)?;
            p.absorb_info(&info);
            let mut peers = config::load_peers(&home)?;
            if let Some(id) = &p.node_id {
                if let Some((other, _)) = peers
                    .peers
                    .iter()
                    .find(|(n, q)| *n != &name && q.node_id.as_ref() == Some(id))
                {
                    eprintln!(
                        "[agent-cli-x: note: this machine is already registered as `{other}` (same node_id)]"
                    );
                }
            }
            peers.peers.insert(name.clone(), p);
            config::save_peers(&home, &peers)?;
            if json {
                println!("{}", json!({"added": name, "info": info}));
            } else {
                println!("Registered peer {name} ({addr})");
                print_info(&name, &info);
            }
            Ok(0)
        }
        PeerCmd::Set { name, addr, token } => {
            let mut peers = config::load_peers(&home)?;
            let Some(old) = peers.peers.get(&name) else {
                return Err(AppError::not_found(format!("Peer `{name}` not registered")));
            };
            let mut p = old.clone();
            if let Some(a) = addr {
                p.addr = config::normalize_addr(&a);
            }
            if let Some(t) = token {
                p.token = t;
            }
            // 指纹沿用登记时的：连得上且指纹一致才保存，防止误指到别的机器
            let info =
                Client::from_peer(&name, p.clone()).call("/v1/info", &json!({}), CALL_TIMEOUT)?;
            p.absorb_info(&info);
            let addr = p.addr.clone();
            peers.peers.insert(name.clone(), p);
            config::save_peers(&home, &peers)?;
            if json {
                println!("{}", json!({"updated": name, "addr": addr}));
            } else {
                println!("Updated peer {name} ({addr})");
            }
            Ok(0)
        }
        PeerCmd::List => {
            let mut peers = config::load_peers(&home)?;
            // 缓存缺系统信息的对端（旧版本登记的）：联网补全。补不全（离线或对端太旧）的
            // 记下尝试时间，24 小时内不再重试，避免每次 peer list 都卡在连接超时上
            const RETRY_SECS: i64 = 24 * 3600;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64);
            let missing: Vec<String> = peers
                .peers
                .iter()
                .filter(|(_, p)| p.os.is_none() || p.shell.is_none())
                .filter(|(_, p)| p.info_tried.map_or(true, |t| now - t >= RETRY_SECS))
                .map(|(n, _)| n.clone())
                .collect();
            if !missing.is_empty() {
                client::disable_version_warning();
                let got = multi::parallel(&missing, |p| (p.to_string(), multi::probe(p).ok()));
                let fresh: Vec<(&str, &Value)> = got
                    .iter()
                    .filter_map(|(p, r)| r.as_ref().map(|(v, _)| (p.as_str(), v)))
                    .collect();
                let _ = config::remember_peer_infos(&home, &fresh);
                peers = config::load_peers(&home)?;
                let mut tried = false;
                for n in &missing {
                    if let Some(p) = peers.peers.get_mut(n) {
                        if p.os.is_none() || p.shell.is_none() {
                            p.info_tried = Some(now);
                            tried = true;
                        }
                    }
                }
                if tried {
                    let _ = config::save_peers(&home, &peers);
                }
                if !fresh.is_empty() && !json {
                    let names: Vec<&str> = fresh.iter().map(|(n, _)| *n).collect();
                    eprintln!(
                        "[agent-cli-x: fetched system info for {}]",
                        names.join(", ")
                    );
                }
            }
            if json {
                let list: Vec<Value> = peers
                    .peers
                    .iter()
                    .map(|(n, p)| {
                        json!({
                            "name": n,
                            "addr": p.addr,
                            "node_name": p.node_name,
                            "node_id": p.node_id,
                            "os": p.os,
                            "arch": p.arch,
                            "shell": p.shell,
                            "fingerprint": p.fingerprint,
                        })
                    })
                    .collect();
                println!("{}", Value::Array(list));
            } else if peers.peers.is_empty() {
                println!(
                    "No peers registered. Use `agent-cli-x peer add <name> <IP:port> --token <token>`"
                );
            } else {
                for (n, p) in &peers.peers {
                    // 系统/架构与 shell 来自登记或最近一次 info 的缓存；缺失时显示 -
                    let platform = match (&p.os, &p.arch) {
                        (Some(o), Some(a)) => format!("{o}/{a}"),
                        (Some(o), None) => o.clone(),
                        _ => "-".into(),
                    };
                    println!(
                        "{n}\t{}\t{} ({})\t{platform}\t{}",
                        p.addr,
                        p.node_name.as_deref().unwrap_or("-"),
                        p.node_id.as_deref().unwrap_or("-"),
                        p.shell.as_deref().unwrap_or("-")
                    );
                }
            }
            Ok(0)
        }
        PeerCmd::Remove { name } => {
            let mut peers = config::load_peers(&home)?;
            if peers.peers.remove(&name).is_none() {
                return Err(AppError::not_found(format!("Peer `{name}` not registered")));
            }
            config::save_peers(&home, &peers)?;
            println!("Removed peer {name}");
            Ok(0)
        }
    }
}

fn exec_body(
    cwd: Option<String>,
    shell: Option<String>,
    timeout: Option<u64>,
    envs: Vec<String>,
    command: String,
) -> Result<Value> {
    let mut env = BTreeMap::new();
    for kv in envs {
        let (k, v) = kv
            .split_once('=')
            .ok_or_else(|| AppError::bad_request(format!("--env `{kv}` must be KEY=VALUE")))?;
        env.insert(k.to_string(), v.to_string());
    }
    Ok(json!({"cmd": command, "cwd": cwd, "shell": shell, "timeout_secs": timeout, "env": env}))
}

/// 读取 --script 的内容：去掉 BOM、统一为 LF（sh 不认 CRLF）；cmd 不支持多行脚本。
fn script_text(f: &std::path::Path, shell: Option<&str>) -> Result<String> {
    let raw = if f.as_os_str() == "-" {
        read_stdin()?
    } else {
        read_local_text(&f.to_path_buf())?
    };
    let text = raw.trim_start_matches('\u{feff}').replace("\r\n", "\n");
    if text.trim().is_empty() {
        return Err(AppError::bad_request("Script is empty"));
    }
    if shell.is_some_and(|s| s.eq_ignore_ascii_case("cmd")) && text.trim_end().contains('\n') {
        return Err(AppError::bad_request(
            "cmd doesn't support multi-line --script; use pwsh / powershell, or write a .bat first and run it",
        ));
    }
    Ok(text)
}

fn exec(peer: &str, body: &Value, timeout: Option<u64>, full: bool, json: bool) -> Result<i32> {
    let client = Client::load(peer)?;
    // 读超时要覆盖远端命令的最长运行时间
    let read_timeout = Duration::from_secs(timeout.unwrap_or(3600) + 60);
    let mut out = ExecOutput::new(json, full, peer);
    let mut exit: Option<Value> = None;
    client.stream("/v1/exec", body, read_timeout, |v| {
        match v["t"].as_str() {
            Some("out") => out.feed(false, v["d"].as_str().unwrap_or_default()),
            Some("err") => out.feed(true, v["d"].as_str().unwrap_or_default()),
            Some("exit") => exit = Some(v),
            _ => {}
        }
        Ok(())
    })?;
    let exit = exit
        .ok_or_else(|| AppError::connect("Connection lost before the exit code arrived (command may still be running on the peer)"))?;
    Ok(out.finish(&exit))
}

fn start_job(peer: &str, body: &Value, json: bool) -> Result<i32> {
    let v = Client::load(peer)?.call("/v1/jobs/start", body, CALL_TIMEOUT)?;
    if json {
        println!("{v}");
    } else {
        let id = v["job_id"].as_str().unwrap_or_default();
        println!(
            "Started job {id} in background (cwd {})",
            v["cwd"].as_str().unwrap_or_default()
        );
        println!("  Logs: agent-cli-x job logs {peer} {id} --follow");
        println!("  Kill: agent-cli-x job kill {peer} {id}");
    }
    Ok(0)
}

fn job(cmd: JobCmd, json: bool) -> Result<i32> {
    match cmd {
        JobCmd::List { peer, limit } => {
            let v = Client::load(&peer)?.call(
                "/v1/jobs/list",
                &json!({"limit": limit}),
                CALL_TIMEOUT,
            )?;
            if json {
                println!("{v}");
                return Ok(0);
            }
            let jobs = v["jobs"].as_array().cloned().unwrap_or_default();
            if jobs.is_empty() {
                println!("No background jobs");
            }
            for j in jobs {
                let cmd: String = j["cmd"]
                    .as_str()
                    .unwrap_or_default()
                    .chars()
                    .take(80)
                    .collect();
                let code = if j["code"].is_null() {
                    "-".to_string()
                } else {
                    j["code"].to_string()
                };
                println!(
                    "{}\t{}\t{code}\t{}\t{cmd}",
                    j["id"].as_str().unwrap_or_default(),
                    j["status"].as_str().unwrap_or_default(),
                    j["started"].as_str().unwrap_or_default()
                );
            }
            Ok(0)
        }
        JobCmd::Kill { peer, id } => {
            let v = Client::load(&peer)?.call("/v1/jobs/kill", &json!({"id": id}), CALL_TIMEOUT)?;
            if json {
                println!("{v}");
            } else {
                match v["status"].as_str() {
                    Some("killing") => {
                        println!(
                            "Kill requested: {id} (not yet confirmed; check later with job list)"
                        )
                    }
                    _ => println!("Killed job: {id}"),
                }
            }
            Ok(0)
        }
        JobCmd::Logs {
            peer,
            id,
            follow,
            full,
        } => {
            let client = Client::load(&peer)?;
            // follow 时服务端每 15 秒发心跳，读超时只需覆盖心跳间隔
            let read_timeout = Duration::from_secs(if follow { 120 } else { 60 });
            let mut out = ExecOutput::new(json, full, &peer);
            let mut end: Option<Value> = None;
            client.stream(
                "/v1/jobs/log",
                &json!({"id": id, "follow": follow}),
                read_timeout,
                |v| {
                    match v["t"].as_str() {
                        Some("out") => out.feed(false, v["d"].as_str().unwrap_or_default()),
                        Some("end") => end = Some(v),
                        _ => {}
                    }
                    Ok(())
                },
            )?;
            let end =
                end.ok_or_else(|| AppError::connect("Connection lost before job status arrived"))?;
            let status = end["status"].as_str().unwrap_or_default().to_string();
            match status.as_str() {
                "running" => {
                    out.finish(&json!({"code": 0}));
                    eprintln!("[agent-cli-x: job still running; use --follow to wait]");
                    Ok(0)
                }
                "lost" => {
                    out.finish(&json!({"code": 1}));
                    eprintln!(
                        "[agent-cli-x: job process is gone (machine restarted or process killed); result unknown]"
                    );
                    Ok(1)
                }
                "killed" => {
                    out.finish(&json!({"code": 137}));
                    eprintln!("[agent-cli-x: job was killed]");
                    Ok(137)
                }
                _ => Ok(out.finish(&end)),
            }
        }
    }
}

fn cp(
    src_peer: &str,
    src: &str,
    dst_peer: &str,
    dest: &str,
    exclude: &[String],
    json: bool,
) -> Result<i32> {
    let sc = Client::load(src_peer)?;
    let dc = Client::load(dst_peer)?;
    let dinfo = dc.call("/v1/info", &json!({}), CALL_TIMEOUT)?;
    let _ = config::remember_peer_infos(&config::home_dir(), &[(dst_peer, &dinfo)]);
    let direct = dinfo["features"]
        .as_array()
        .is_some_and(|f| f.iter().any(|x| x == "put_stream"));
    let started = std::time::Instant::now();
    let mut v = if direct {
        let show = !json && std::io::stderr().is_terminal();
        let mut last = std::time::Instant::now();
        let r = client::transfer::relay(&sc, src, &dc, dest, exclude, |files, bytes| {
            if show && last.elapsed() >= Duration::from_millis(500) {
                last = std::time::Instant::now();
                eprint!(
                    "\r[agent-cli-x: forwarded {files} files ({})]   ",
                    human_size(bytes)
                );
            }
        });
        if show {
            eprint!("\r\x1b[K");
        }
        let mut v = r?;
        v["via"] = json!("relay");
        v
    } else {
        // 目标端版本较旧：经本机临时目录中转，结束后删除
        let tmp = std::env::temp_dir().join(format!("agent-cli-x-cp-{}", util::random_hex(4)));
        std::fs::create_dir_all(&tmp)?;
        let r = (|| {
            let got = client::transfer::get(&sc, src, &tmp, exclude)?;
            let local = PathBuf::from(got["dest"].as_str().unwrap_or_default());
            client::transfer::put(&dc, &local, dest, &[])
        })();
        let _ = std::fs::remove_dir_all(&tmp);
        let mut v = r?;
        v["via"] = json!("local_temp");
        if !json {
            eprintln!(
                "[agent-cli-x: {dst_peer} version {} lacks direct transfer; relayed via local temp dir (deleted); upgrade the peer for direct transfer]",
                dinfo["version"].as_str().unwrap_or("?")
            );
        }
        v
    };
    let ms = started.elapsed().as_millis() as u64;
    v["ms"] = json!(ms);
    if json {
        println!("{v}");
    } else {
        let bytes = v["bytes"].as_u64().unwrap_or(0);
        println!(
            "Copied {} files ({}, {:.1}s, {}/s): {src_peer} {src} → {dst_peer} {}",
            v["files"],
            human_size(bytes),
            ms as f64 / 1000.0,
            human_size(bytes * 1000 / ms.max(1)),
            v["dest"].as_str().unwrap_or_default()
        );
        if let Some(op) = v["op_id"].as_str() {
            println!("  op_id={op} (undo: agent-cli-x rollback {dst_peer} {op})");
        }
    }
    Ok(0)
}

/// read 不给结束行时，单次最多读取的行数 / KB 数。
const READ_WINDOW_LINES: usize = 2000;
const READ_WINDOW_KB: u64 = 64;

fn read(peer: &str, path: &str, lines: Option<String>, number: bool, json: bool) -> Result<i32> {
    let (start, end) = match lines.as_deref() {
        None => (None, None),
        Some(spec) => {
            let (a, b) = spec.split_once(':').unwrap_or((spec, spec));
            let p = |s: &str| -> Result<Option<usize>> {
                if s.trim().is_empty() {
                    Ok(None)
                } else {
                    s.trim().parse::<usize>().map(Some).map_err(|_| {
                        AppError::bad_request(format!("--lines `{spec}` must be START:END"))
                    })
                }
            };
            (p(a)?, p(b)?)
        }
    };
    // 没给结束行时只读一个窗口（行数 + 字节双上限），避免一次把大文件整个读进上下文
    let windowed = end.is_none();
    let (end, max_kb) = if windowed {
        let s = start.unwrap_or(1).max(1);
        (Some(s + READ_WINDOW_LINES - 1), Some(READ_WINDOW_KB))
    } else {
        (end, None)
    };
    let v = Client::load(peer)?.call(
        "/v1/fs/read",
        &json!({"path": path, "start": start, "end": end, "max_kb": max_kb}),
        CALL_TIMEOUT,
    )?;
    if json {
        println!("{v}");
        return Ok(0);
    }
    let content = v["content"].as_str().unwrap_or_default();
    let first = v["start"].as_u64().unwrap_or(1) as usize;
    if number {
        for (i, line) in content.split_inclusive('\n').enumerate() {
            print!("{:>6}\t{line}", first + i);
        }
        if !content.is_empty() && !content.ends_with('\n') {
            println!();
        }
    } else {
        print!("{content}");
    }
    let total = v["total_lines"].as_u64();
    let last = v["end"].as_u64().unwrap_or(0);
    let partial =
        v["truncated"].as_bool() == Some(true) || first > 1 || total.is_some_and(|t| last < t);
    if partial {
        eprintln!(
            "\n[agent-cli-x: showing lines {first}-{last} of {}{}; continue with --lines {}:{}]",
            total.map(|t| t.to_string()).unwrap_or_else(|| "?".into()),
            if v["truncated"].as_bool() == Some(true) {
                ", read limit reached"
            } else {
                ""
            },
            last + 1,
            last + READ_WINDOW_LINES as u64
        );
    }
    Ok(0)
}

fn print_info(name: &str, v: &Value) {
    println!(
        "{name} → node {} ({})",
        v["node_name"].as_str().unwrap_or("?"),
        v["node_id"].as_str().unwrap_or("-")
    );
    let ver = v["version"].as_str().unwrap_or("?");
    println!(
        "  host {}  {}/{}  agent-cli-x {ver}{}  default shell: {}",
        v["hostname"].as_str().unwrap_or("?"),
        v["os"].as_str().unwrap_or("?"),
        v["arch"].as_str().unwrap_or("?"),
        if ver != http::VERSION {
            format!(" (local {}; use upgrade to sync)", http::VERSION)
        } else {
            String::new()
        },
        v["shell"].as_str().unwrap_or("?")
    );
    if let Some(user) = v["exec_user"].as_str() {
        let elevated = match (v["exec_elevated"] == true, v["os"] == "windows") {
            (true, true) => ", admin",
            (true, false) => ", root",
            _ => "",
        };
        println!(
            "  exec identity: {user}{elevated} (exec is not bound by the path rules/deny list below, only the command deny list)"
        );
    }
    if let Some(e) = v["config_error"].as_str() {
        println!(
            "  Config error: config.toml is invalid, running with the last good config ({e}). After fixing run `agent-cli-x restart {name}`"
        );
    }
    if v["caller_no_relay"] == true {
        println!(
            "  Relay barred for you: your ({}) exec on this machine can't use agent-cli-x to reach other nodes",
            v["caller"].as_str().unwrap_or("?")
        );
    }
    let def = v["default_workspace"].as_str();
    for w in v["workspaces"].as_array().into_iter().flatten() {
        let n = w["name"].as_str().unwrap_or_default();
        println!(
            "  workspace {n}{} = {}",
            if Some(n) == def { " (default)" } else { "" },
            w["path"].as_str().unwrap_or_default()
        );
    }
    let deny: Vec<&str> = v["deny_paths"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|x| x.as_str())
        .collect();
    if !deny.is_empty() {
        println!("  denied: {}", deny.join(", "));
    }
    let rules: Vec<String> = v["path_rules"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| {
            format!(
                "{}={}",
                r["path"].as_str().unwrap_or_default(),
                r["allow"].as_str().unwrap_or_default()
            )
        })
        .collect();
    if !rules.is_empty() {
        println!(
            "  path permissions (r read, c create, u update, d delete): {}",
            rules.join(", ")
        );
    }
}

/// grep 普通输出：匹配行 `ws:path:line:text`，上下文行 `ws:path-line-text`，
/// 不连续的块之间用 `--` 分隔（与 rg / grep 相同）；-l 只输出路径，-c 输出 `ws:path:count`。
fn print_grep(v: &Value, mode: &str, context: bool, max: usize, json: bool) {
    if json {
        println!("{v}");
    } else {
        let ws = v["ws"].as_str().unwrap_or_default();
        let mut prev: Option<(&str, u64)> = None;
        for m in v["matches"].as_array().into_iter().flatten() {
            let path = m["path"].as_str().unwrap_or_default();
            match mode {
                "files" => println!("{ws}:{path}"),
                "count" => println!("{ws}:{path}:{}", m["count"]),
                _ => {
                    let line = m["line"].as_u64().unwrap_or(0);
                    if context && prev.is_some_and(|(p, l)| p != path || l + 1 != line) {
                        println!("--");
                    }
                    prev = Some((path, line));
                    let sep = if m["ctx"].as_bool() == Some(true) {
                        '-'
                    } else {
                        ':'
                    };
                    println!(
                        "{ws}:{path}{sep}{line}{sep}{}",
                        m["text"].as_str().unwrap_or_default()
                    );
                }
            }
        }
    }
    if v["truncated"].as_bool() == Some(true) {
        let unit = if mode == "lines" { "matches" } else { "files" };
        let why = if v["byte_limit"].as_bool() == Some(true) {
            "64KB output limit".to_string()
        } else {
            format!("--max {max} {unit}")
        };
        match v["next_skip"].as_u64() {
            Some(n) => eprintln!(
                "[agent-cli-x: stopped at {why}; continue with --skip {n}, or narrow the path/--glob]"
            ),
            None => eprintln!(
                "[agent-cli-x: --max {max} limit reached; narrow the scope or filter with --glob]"
            ),
        }
    }
}

/// 读取 --edits 的 JSON 数组（文件或 `-` 表示 stdin），本地先校验形状，错误提示更直接。
fn parse_edits(f: &PathBuf) -> Result<Value> {
    let text = if f.as_os_str() == "-" {
        read_stdin()?
    } else {
        read_local_text(f)?
    };
    let v: Value = serde_json::from_str(&text)
        .map_err(|e| AppError::bad_request(format!("--edits is not valid JSON: {e}")))?;
    let ok = v.as_array().is_some_and(|a| {
        !a.is_empty()
            && a.iter().all(|e| {
                let has = |k: &str, alt: &str| e[k].is_string() || e[alt].is_string();
                has("old", "old_string") && has("new", "new_string")
            })
    });
    if !ok {
        return Err(AppError::bad_request(
            r#"--edits must be a non-empty JSON array like [{"old": "a", "new": "b", "all": false}]"#,
        ));
    }
    Ok(v)
}

fn print_write_result(v: &Value, json: bool, verb: &str) {
    if json {
        println!("{v}");
        return;
    }
    let mut extra = String::new();
    if let Some(n) = v["replacements"].as_u64() {
        extra = format!(
            ", {n} replacements, starting at line {}",
            v["line"].as_u64().unwrap_or(0)
        );
    } else if let Some(b) = v["bytes"].as_u64() {
        extra = format!(
            ", {b} bytes{}",
            if v["created"] == true {
                ", created"
            } else {
                ""
            }
        );
    }
    println!(
        "{verb} {}{extra}\n  op_id={} (undo: agent-cli-x rollback <peer> {})\n  sha256={}",
        v["path"].as_str().unwrap_or_default(),
        v["op_id"].as_str().unwrap_or_default(),
        v["op_id"].as_str().unwrap_or_default(),
        v["sha256"].as_str().unwrap_or_default()
    );
}

fn human_size(n: u64) -> String {
    match n {
        n if n >= 1 << 30 => format!("{:.1}G", n as f64 / (1u64 << 30) as f64),
        n if n >= 1 << 20 => format!("{:.1}M", n as f64 / (1u64 << 20) as f64),
        n if n >= 1 << 10 => format!("{:.1}K", n as f64 / 1024.0),
        n => format!("{n}B"),
    }
}

fn read_local_text(p: &PathBuf) -> Result<String> {
    let bytes = std::fs::read(p)
        .map_err(|e| AppError::io(format!("Failed to read local file {}: {e}", p.display())))?;
    String::from_utf8(bytes)
        .map_err(|_| AppError::bad_request(format!("{} is not UTF-8 text", p.display())))
}

fn read_stdin() -> Result<String> {
    let mut stdin = std::io::stdin();
    if stdin.is_terminal() {
        return Err(AppError::bad_request(
            "Provide content via --content, --from or a pipe",
        ));
    }
    let mut s = String::new();
    stdin
        .read_to_string(&mut s)
        .map_err(|e| AppError::bad_request(format!("stdin is not UTF-8 text: {e}")))?;
    Ok(s)
}

fn text_arg(inline: Option<String>, file: Option<PathBuf>, name: &str) -> Result<String> {
    match (inline, file) {
        (Some(s), _) => Ok(s),
        (None, Some(f)) => read_local_text(&f),
        (None, None) => Err(AppError::bad_request(format!("Missing {name}"))),
    }
}
