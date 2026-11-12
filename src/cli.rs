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
    about = "跨机器 AI 指令工具：让不同服务器上的 AI 互相执行命令与读写文件",
    after_help = "路径格式：工作区:相对路径，例如 proj:src/main.rs（只有一个工作区时可省略前缀）。\n\
退出码：远端命令退出码原样透传；agent-cli-x 自身错误为 100 连接 / 101 鉴权 / 102 策略拒绝 / \
103 不存在 / 104 超时 / 105 冲突 / 106 指纹 / 110 参数 / 111 繁忙。\n给 AI 的完整说明：agent-cli-x skill"
)]
pub struct Cli {
    /// 以 JSON 输出结果（便于程序解析）
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 初始化本机：生成机器 ID、token、自签证书与带注释的 config.toml
    Init {
        /// 本机名称（默认主机名），对端 info / peer list 中显示
        #[arg(long)]
        name: Option<String>,
        /// 监听地址
        #[arg(long, default_value = "0.0.0.0:47821")]
        listen: String,
        /// 工作区，可重复：--workspace proj=D:/code/proj
        #[arg(long = "workspace", value_name = "NAME=PATH")]
        workspaces: Vec<String>,
        /// 覆盖已有配置（会更换 token、证书与机器 ID，已登记本机的对端需重新 peer add）
        #[arg(long)]
        force: bool,
    },
    /// 前台运行服务端（日志同时写入 ~/.agent-cli-x/serve-logs/）
    Serve {
        /// 端口被占用时最多等待的秒数（升级重启时新进程等待旧进程退出）
        #[arg(long, hide = true, default_value_t = 0)]
        wait_port: u64,
    },
    /// 显示本机的名称、ID、指纹与工作区（不联网）
    Whoami {
        /// 同时打印供其他机器登记本机的 `peer add` 命令（含默认 token）
        #[arg(long)]
        pair: bool,
    },
    /// 自检：配置、本机服务、开机自启、防火墙、各对端连通性与版本
    Doctor {
        /// 不检查对端
        #[arg(long)]
        no_peers: bool,
    },
    /// 管理访问本机的 token（按调用方分别签发，可单独吊销）
    #[command(subcommand)]
    Token(TokenCmd),
    /// 远程升级对端：上传新二进制并重启对端服务（对端可写 A,B 或 @all）
    Upgrade {
        peer: String,
        /// 要部署的二进制，默认用本机正在运行的这个（对端系统/架构需相同）
        #[arg(long)]
        binary: Option<PathBuf>,
        /// 对端有后台任务在运行时也强制升级（任务会被中断）
        #[arg(long)]
        force: bool,
    },
    /// 打印本机证书指纹（供对端 peer add 核对）
    Fingerprint,
    /// 打印给 AI 阅读的用法说明
    Skill,
    /// 管理对端节点
    #[command(subcommand)]
    Peer(PeerCmd),
    /// 查看对端信息（机器名/ID、系统、shell、工作区、黑名单）；A,B 或 @all 时显示概览
    Info { peer: String },
    /// 在对端执行命令（选项写在命令之前）；对端写 A,B 或 @all 时并行执行并分组输出
    Exec {
        peer: String,
        /// 工作目录（工作区:相对路径），默认是对端的默认工作区根目录
        #[arg(long)]
        cwd: Option<String>,
        /// pwsh / powershell / cmd / sh / bash，默认 Windows 用 pwsh，Linux 用 sh
        #[arg(long)]
        shell: Option<String>,
        /// 超时秒数（前台受 max_timeout_secs、后台受 job_timeout_secs 限制）
        #[arg(long)]
        timeout: Option<u64>,
        /// 额外环境变量，可重复：--env KEY=VALUE
        #[arg(long = "env", value_name = "KEY=VALUE")]
        envs: Vec<String>,
        /// 作为后台任务运行，立即返回任务 ID（用 job logs / job kill 管理）
        #[arg(long)]
        detach: bool,
        /// 不截断输出
        #[arg(long)]
        full: bool,
        /// 执行本地脚本文件的内容（`-` 表示标准输入），免去多层引号转义；不能与命令同时给出
        #[arg(long, value_name = "FILE", conflicts_with = "command")]
        script: Option<PathBuf>,
        /// 要执行的命令（多个参数会以空格拼接）
        #[arg(
            required_unless_present = "script",
            trailing_var_arg = true,
            allow_hyphen_values = true
        )]
        command: Vec<String>,
    },
    /// 管理后台任务
    #[command(subcommand)]
    Job(JobCmd),
    /// 列目录（遵守 .gitignore，隐藏黑名单路径）
    Ls {
        peer: String,
        path: Option<String>,
        #[arg(long, default_value_t = 1)]
        depth: usize,
    },
    /// 在对端搜索文件内容（正则，遵守 .gitignore，跳过二进制与黑名单）
    Grep {
        peer: String,
        pattern: String,
        /// 搜索范围（工作区:相对路径），默认默认工作区根目录
        path: Option<String>,
        /// 只搜匹配的文件，可重复：--glob "*.rs"
        #[arg(long)]
        glob: Vec<String>,
        /// 忽略大小写
        #[arg(short = 'i', long)]
        ignore_case: bool,
        /// 按字面量匹配（不当作正则）
        #[arg(short = 'F', long)]
        fixed: bool,
        /// 最多返回的匹配行数
        #[arg(long, default_value_t = 200)]
        max: usize,
    },
    /// 读取文本文件
    Read {
        peer: String,
        path: String,
        /// 行范围，如 100:200、100:、:50
        #[arg(long)]
        lines: Option<String>,
        /// 输出带行号
        #[arg(short = 'n', long)]
        number: bool,
    },
    /// 写入整个文件（内容来自 --content / --from / 标准输入）
    Write {
        peer: String,
        path: String,
        #[arg(long, conflicts_with = "from")]
        content: Option<String>,
        /// 从本地文件读取内容
        #[arg(long)]
        from: Option<PathBuf>,
        /// 父目录不存在时自动创建
        #[arg(long)]
        mkdir: bool,
        /// 仅当远端文件当前 sha256 等于此值时才写入（乐观锁）
        #[arg(long)]
        expect_sha: Option<String>,
    },
    /// 精确替换文件中的文本（old 必须唯一匹配）
    Edit {
        peer: String,
        path: String,
        #[arg(long, conflicts_with = "old_file")]
        old: Option<String>,
        /// 从本地文件读取 old 文本（避免命令行转义问题）
        #[arg(long)]
        old_file: Option<PathBuf>,
        #[arg(long, conflicts_with = "new_file")]
        new: Option<String>,
        /// 从本地文件读取 new 文本
        #[arg(long)]
        new_file: Option<PathBuf>,
        /// 替换全部匹配
        #[arg(long)]
        all: bool,
        #[arg(long)]
        expect_sha: Option<String>,
    },
    /// 上传本地文件或目录到对端（目标是已存在目录时放到其下，同 cp -r）
    Put {
        peer: String,
        local: PathBuf,
        /// 远端目标（工作区:相对路径）
        dest: String,
        /// 排除的文件/目录，可重复：--exclude node_modules --exclude "*.log"
        #[arg(long)]
        exclude: Vec<String>,
    },
    /// 从对端下载文件或目录（本地目标是已存在目录时放到其下）
    Get {
        peer: String,
        /// 远端源（工作区:相对路径）
        src: String,
        local: PathBuf,
        #[arg(long)]
        exclude: Vec<String>,
    },
    /// 在两个对端之间直接复制文件或目录（经本机流式转发，不落本机磁盘）
    #[command(
        after_help = "示例：agent-cli-x cp win127 dl:pkg ub40 home:\n语义同 put：目标是已存在目录时放到其下。目标端需 0.0.4 及以上，旧版本自动退回本机临时目录中转（用完即删）。"
    )]
    Cp {
        /// 源对端
        src_peer: String,
        /// 源路径（工作区:相对路径）
        src: String,
        /// 目标对端
        dst_peer: String,
        /// 目标路径（工作区:相对路径）
        dest: String,
        #[arg(long)]
        exclude: Vec<String>,
    },
    /// 删除文件或目录：默认移入对端回收站，可用 rollback 还原（需要对端路径权限含 d）
    Rm {
        peer: String,
        path: String,
        /// 删除目录及其下全部内容
        #[arg(short = 'r', long)]
        recursive: bool,
        /// 彻底删除：不进回收站、不备份，无法恢复
        #[arg(long)]
        permanent: bool,
    },
    /// 撤销一次 write / edit / put / rm
    Rollback { peer: String, op_id: String },
    /// 彻底清除一次操作留下的回收站条目和备份（释放空间，之后无法 rollback）
    Purge { peer: String, op_id: String },
}

#[derive(Subcommand)]
enum PeerCmd {
    /// 登记对端。首次不带 --fingerprint 只显示指纹供核对
    Add {
        name: String,
        /// IP:端口（端口默认 47821）
        addr: String,
        #[arg(long)]
        token: String,
        /// 对端指纹（在对端执行 `agent-cli-x fingerprint` 获取）
        #[arg(long)]
        fingerprint: Option<String>,
    },
    /// 列出已登记的对端
    List,
    /// 删除对端
    Remove { name: String },
}

#[derive(Subcommand)]
enum TokenCmd {
    /// 为一个调用方签发 token，并打印对方需要执行的 `peer add` 命令
    Add {
        name: String,
        /// 禁止借道：该调用方在本机 exec 里不能再用 agent-cli-x 访问其他节点或管理凭据
        #[arg(long)]
        no_relay: bool,
    },
    /// 修改已签发 token 的限制（不更换 token）
    Set {
        name: String,
        /// 禁止借道
        #[arg(long, required_unless_present = "allow_relay")]
        no_relay: bool,
        /// 取消禁止借道
        #[arg(long, conflicts_with = "no_relay")]
        allow_relay: bool,
    },
    /// 列出已签发的 token（名称、创建时间与限制）
    List,
    /// 吊销一个调用方的 token（serve 运行中几秒内生效）
    Remove { name: String },
    /// 更换 token（名称为 default 时更换 config.toml 中的默认 token）
    Rotate { name: String },
}

#[derive(Subcommand)]
enum JobCmd {
    /// 列出最近的后台任务
    List {
        peer: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// 查看任务输出；--follow 持续输出直到任务结束，并以任务退出码退出
    Logs {
        peer: String,
        id: String,
        #[arg(short = 'f', long)]
        follow: bool,
        /// 不截断输出
        #[arg(long)]
        full: bool,
    },
    /// 终止任务（连同整棵进程树）
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
            "当前命令运行在调用方 `{who}` 发起的 exec 中，本机禁止借道：不能再用 agent-cli-x \
             访问其他节点或管理本机凭据。请在你自己的机器上直接操作；需要访问其他节点请告知用户，\
             不要尝试绕过（读取凭据文件、清除 {} 环境变量或直连）",
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
        Cmd::Serve { wait_port } => server::serve(&config::home_dir(), wait_port).map(|_| 0),
        Cmd::Whoami { pair } => whoami(pair, json),
        // 禁止借道时不探测对端
        Cmd::Doctor { no_peers } => doctor::run(json, !no_peers && !no_relay),
        Cmd::Token(t) => token(t, json),
        Cmd::Upgrade {
            peer,
            binary,
            force,
        } => upgrade(&peer, binary, force, json),
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
                    return Err(AppError::bad_request("--detach 一次只能用于一个对端"));
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
        Cmd::Ls { peer, path, depth } => {
            let v = Client::load(&peer)?.call(
                "/v1/fs/ls",
                &json!({"path": path.unwrap_or_default(), "depth": depth}),
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
                    eprintln!("[agent-cli-x: 条目过多已截断，请缩小目录或降低 --depth]");
                }
            }
            Ok(0)
        }
        Cmd::Grep {
            peer,
            pattern,
            path,
            glob,
            ignore_case,
            fixed,
            max,
        } => {
            let v = Client::load(&peer)?.call(
                "/v1/fs/grep",
                &json!({
                    "pattern": pattern,
                    "path": path.unwrap_or_default(),
                    "glob": glob,
                    "ignore_case": ignore_case,
                    "fixed": fixed,
                    "max": max,
                }),
                CALL_TIMEOUT,
            )?;
            let matches = v["matches"].as_array().cloned().unwrap_or_default();
            if json {
                println!("{v}");
            } else {
                let ws = v["ws"].as_str().unwrap_or_default();
                for m in &matches {
                    println!(
                        "{ws}:{}:{}:{}",
                        m["path"].as_str().unwrap_or_default(),
                        m["line"],
                        m["text"].as_str().unwrap_or_default()
                    );
                }
                if v["truncated"].as_bool() == Some(true) {
                    eprintln!("[agent-cli-x: 已达 --max {max} 条上限，请缩小范围或用 --glob 过滤]");
                }
            }
            // 与 grep 一致：无匹配返回 1
            Ok(if matches.is_empty() { 1 } else { 0 })
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
            print_write_result(&v, json, "写入");
            Ok(0)
        }
        Cmd::Edit {
            peer,
            path,
            old,
            old_file,
            new,
            new_file,
            all,
            expect_sha,
        } => {
            let old = text_arg(old, old_file, "--old / --old-file")?;
            let new = text_arg(new, new_file, "--new / --new-file")?;
            let v = Client::load(&peer)?.call(
                "/v1/fs/edit",
                &json!({"path": path, "old": old, "new": new, "replace_all": all, "expect_sha": expect_sha}),
                CALL_TIMEOUT,
            )?;
            print_write_result(&v, json, "修改");
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
                    "已上传 {} 个文件（{}）到 {}",
                    v["files"],
                    human_size(v["bytes"].as_u64().unwrap_or(0)),
                    v["dest"].as_str().unwrap_or_default()
                );
                if let Some(op) = v["op_id"].as_str() {
                    println!("  op_id={op}（撤销: agent-cli-x rollback {peer} {op}）");
                }
                if v["skipped_symlinks"].as_u64().unwrap_or(0) > 0 {
                    eprintln!("[agent-cli-x: 跳过了 {} 个符号链接]", v["skipped_symlinks"]);
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
                    "已下载 {} 个文件（{}）到 {}",
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
                        Some("purged") => "已从回收站彻底删除",
                        _ => "回收站中已不存在（可能已清空或还原）",
                    };
                    println!("{action}: {}", f["path"].as_str().unwrap_or_default());
                }
                let n = v["backups"].as_u64().unwrap_or(0);
                if n > 0 {
                    println!(
                        "已删除 {n} 个备份副本（{}）",
                        human_size(v["backup_bytes"].as_u64().unwrap_or(0))
                    );
                }
                println!("操作 {op_id} 已清除，无法再 rollback");
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
                    "{} 个文件（{}）",
                    v["files"],
                    human_size(v["bytes"].as_u64().unwrap_or(0))
                )];
                if v["dirs"].as_u64().unwrap_or(0) > 0 {
                    parts.push(format!("{} 个目录", v["dirs"]));
                }
                if v["links"].as_u64().unwrap_or(0) > 0 {
                    parts.push(format!("{} 个符号链接", v["links"]));
                }
                let how = match v["method"].as_str() {
                    Some("trash") => "已移入回收站",
                    Some("permanent") => "已彻底删除",
                    _ => "已删除（已备份）",
                };
                println!(
                    "{how} {}：{}",
                    v["path"].as_str().unwrap_or_default(),
                    parts.join("、")
                );
                if let Some(op) = v["op_id"].as_str() {
                    println!("  op_id={op}（撤销: agent-cli-x rollback {peer} {op}；释放空间: agent-cli-x purge {peer} {op}）");
                }
                if let Some(e) = v["trash_error"].as_str() {
                    eprintln!("[agent-cli-x: 回收站不可用（{e}），已改为备份后删除]");
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
                        Some("deleted") => "已删除新建的文件",
                        Some("rmdir") => "已删除新建的目录",
                        Some("kept_dir") => "保留已有目录",
                        Some("recreated_dir") => "已重建被删除的目录",
                        Some("untrashed") => "已从回收站还原",
                        Some("relinked") => "已重建符号链接",
                        Some("kept_link") => "保留已有符号链接",
                        Some("link_failed") => "符号链接重建失败（可能缺少权限）",
                        _ => "已恢复为修改前内容",
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
            "{} 已存在。如需重新生成（会更换 token、证书和机器 ID）请加 --force",
            cfg_path.display()
        )));
    }
    let node_name = name.unwrap_or_else(config::default_node_name);
    config::validate_name(&node_name, "机器")?;
    let mut ws = Vec::new();
    for item in workspaces {
        let (name, path) = item.split_once('=').ok_or_else(|| {
            AppError::bad_request(format!("--workspace `{item}` 格式应为 NAME=PATH"))
        })?;
        config::validate_name(name, "工作区")?;
        let canon = dunce::canonicalize(path)
            .map_err(|e| AppError::bad_request(format!("工作区路径 `{path}` 无效: {e}")))?;
        ws.push((name.to_string(), canon.to_string_lossy().replace('\\', "/")));
    }
    std::fs::create_dir_all(&home)?;
    let token = util::random_hex(24);
    let node_id = util::uuid_v4();
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
    println!("已初始化 {}", home.display());
    println!("  机器:   {node_name} ({node_id})");
    println!("  配置:   {}", cfg_path.display());
    println!("  监听:   {listen}");
    println!("  token:  {token}");
    println!("  指纹:   {fp}");
    println!();
    println!("下一步:");
    println!("  1. 编辑配置，在 [workspaces] 中登记允许访问的目录、按需调整 deny_paths / [[policy.paths]] / deny_commands 预设");
    println!("  2. 运行 `agent-cli-x serve`");
    println!("  3. 在要访问本机的机器上执行（为每个调用方单独签发 token 可用 `agent-cli-x token add <名称>`）:");
    println!("     {}", pair_command(&node_name, listen, &token, &fp));
    println!("  4. 配置开机自启（见 README「Start at boot」），完成后用 `agent-cli-x doctor` 自检");
    if cfg!(windows) {
        println!("  防火墙（管理员 PowerShell 执行一次）:");
        println!("     netsh advfirewall firewall add rule name=\"agent-cli-x\" dir=in action=allow protocol=TCP localport={port}");
    } else {
        println!("  防火墙（如启用了 ufw）: sudo ufw allow {port}/tcp");
    }
    Ok(0)
}

/// 其他机器登记本机所需的 `peer add` 命令。监听全部网卡时用本机主 IP。
fn pair_command(node_name: &str, listen: &str, token: &str, fp: &str) -> String {
    let port = listen.rsplit(':').next().unwrap_or_default();
    let host = listen[..listen.len() - port.len()].trim_end_matches(':');
    let ip = match host {
        "" | "0.0.0.0" | "[::]" => util::local_ip().unwrap_or_else(|| "<本机IP>".into()),
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
        println!("  监听: {}", cfg.listen);
        println!("  指纹: {fp}");
        for (n, p) in &cfg.workspaces {
            println!("  工作区 {n} = {p}");
        }
        if pair {
            println!("  在要访问本机的机器上执行:");
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
            println!("已{verb} token `{name}`: {tok}");
            println!("在调用方执行（token 只显示这一次）:");
            println!("  {cmd}");
        }
    };
    match cmd {
        TokenCmd::Add { name, no_relay } => {
            config::validate_name(&name, "token")?;
            if name == DEFAULT_TOKEN_NAME || tokens.tokens.contains_key(&name) {
                return Err(AppError::conflict(format!(
                    "token `{name}` 已存在，更换请用 `agent-cli-x token rotate {name}`"
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
            issued(&name, &tok, "签发");
            if no_relay && !json {
                println!("该调用方已禁止借道：它在本机 exec 里不能再调用 agent-cli-x 访问其他节点");
            }
        }
        TokenCmd::Set {
            name,
            no_relay,
            allow_relay: _,
        } => {
            if name == DEFAULT_TOKEN_NAME {
                return Err(AppError::bad_request(
                    "默认 token 不支持限制，请为该调用方单独 `token add <名称> --no-relay`",
                ));
            }
            let Some(e) = tokens.tokens.get_mut(&name) else {
                return Err(AppError::not_found(format!("没有名为 `{name}` 的 token")));
            };
            e.no_relay = no_relay;
            config::save_tokens(&home, &tokens)?;
            if json {
                println!("{}", json!({"name": name, "no_relay": no_relay}));
            } else if no_relay {
                println!("token `{name}` 已禁止借道（运行中的 serve 几秒内生效）");
            } else {
                println!("token `{name}` 已取消禁止借道（运行中的 serve 几秒内生效）");
            }
        }
        TokenCmd::Rotate { name } => {
            let tok = util::random_hex(24);
            if name == DEFAULT_TOKEN_NAME {
                config::replace_config_token(&home, &tok)?;
            } else {
                let Some(e) = tokens.tokens.get_mut(&name) else {
                    return Err(AppError::not_found(format!("没有名为 `{name}` 的 token")));
                };
                e.sha256 = util::sha256_hex(tok.as_bytes());
                e.created = util::now_iso();
                config::save_tokens(&home, &tokens)?;
            }
            issued(&name, &tok, "更换");
            if !json {
                println!("旧 token 已作废（运行中的 serve 几秒内生效），用旧 token 登记的调用方需重新 peer add");
            }
        }
        TokenCmd::Remove { name } => {
            if name == DEFAULT_TOKEN_NAME {
                return Err(AppError::bad_request(
                    "默认 token 不能删除，可用 `agent-cli-x token rotate default` 更换",
                ));
            }
            if tokens.tokens.remove(&name).is_none() {
                return Err(AppError::not_found(format!("没有名为 `{name}` 的 token")));
            }
            config::save_tokens(&home, &tokens)?;
            if json {
                println!("{}", json!({"removed": name}));
            } else {
                println!("已吊销 token `{name}`（运行中的 serve 几秒内生效）");
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
                    let flag = if e.no_relay { "\t禁止借道" } else { "" };
                    println!("{n}\t{}{flag}", e.created);
                }
            }
        }
    }
    Ok(0)
}

fn upgrade(spec: &str, binary: Option<PathBuf>, force: bool, json: bool) -> Result<i32> {
    client::disable_version_warning();
    let peers = multi::resolve(spec)?;
    let mut results = Vec::new();
    let mut code = 0;
    // 逐台升级：出问题时影响面最小，也便于看清是哪一台失败
    for p in &peers {
        if !json {
            eprintln!("[agent-cli-x: 正在升级 {p} …]");
        }
        let r =
            Client::load(p).and_then(|c| client::upgrade::upgrade(&c, binary.as_deref(), force));
        match r {
            Ok(v) => {
                if !json {
                    println!(
                        "{p}: {} → {}（{}，用时 {:.1}s）",
                        v["old_version"].as_str().unwrap_or("?"),
                        v["new_version"].as_str().unwrap_or("?"),
                        if v["restart"] == "exit" {
                            "由服务管理器重启"
                        } else {
                            "已自动重启"
                        },
                        v["ms"].as_u64().unwrap_or(0) as f64 / 1000.0
                    );
                }
                results.push(v);
            }
            Err(e) => {
                if code == 0 {
                    code = e.exit_code();
                }
                if !json {
                    println!("{p}: 升级失败 {e}");
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

fn peer(cmd: PeerCmd, json: bool) -> Result<i32> {
    let home = config::home_dir();
    match cmd {
        PeerCmd::Add {
            name,
            addr,
            token,
            fingerprint,
        } => {
            config::validate_name(&name, "对端")?;
            let addr = config::normalize_addr(&addr);
            let seen = client::discover_fingerprint(&addr)?;
            let Some(want) = fingerprint else {
                return Err(AppError::fingerprint(format!(
                    "{addr} 的证书指纹为 {seen}\n请在对端执行 `agent-cli-x fingerprint` 核对一致后，追加参数重新登记:\n  --fingerprint {seen}"
                )));
            };
            if !want.trim().eq_ignore_ascii_case(&seen) {
                return Err(AppError::fingerprint(format!(
                    "指纹不一致：期望 {want}，实际 {seen}。请勿登记，确认对端身份后再试"
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
                        "[agent-cli-x: 注意，这台机器已以 `{other}` 的名称登记过（相同 node_id）]"
                    );
                }
            }
            peers.peers.insert(name.clone(), p);
            config::save_peers(&home, &peers)?;
            if json {
                println!("{}", json!({"added": name, "info": info}));
            } else {
                println!("已登记对端 {name} ({addr})");
                print_info(&name, &info);
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
                    eprintln!("[agent-cli-x: 已联网补全 {} 的系统信息]", names.join(", "));
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
                    "尚未登记对端。使用 `agent-cli-x peer add <名称> <IP:端口> --token <token>`"
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
                return Err(AppError::not_found(format!("未登记对端 `{name}`")));
            }
            config::save_peers(&home, &peers)?;
            println!("已删除对端 {name}");
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
            .ok_or_else(|| AppError::bad_request(format!("--env `{kv}` 格式应为 KEY=VALUE")))?;
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
        return Err(AppError::bad_request("脚本内容为空"));
    }
    if shell.is_some_and(|s| s.eq_ignore_ascii_case("cmd")) && text.trim_end().contains('\n') {
        return Err(AppError::bad_request(
            "cmd 不支持多行 --script，请改用 pwsh / powershell，或先用 write 写成 .bat 再执行",
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
        .ok_or_else(|| AppError::connect("连接中断，未收到命令退出码（命令可能仍在对端运行）"))?;
    Ok(out.finish(&exit))
}

fn start_job(peer: &str, body: &Value, json: bool) -> Result<i32> {
    let v = Client::load(peer)?.call("/v1/jobs/start", body, CALL_TIMEOUT)?;
    if json {
        println!("{v}");
    } else {
        let id = v["job_id"].as_str().unwrap_or_default();
        println!(
            "已在后台启动任务 {id}（工作目录 {}）",
            v["cwd"].as_str().unwrap_or_default()
        );
        println!("  查看输出: agent-cli-x job logs {peer} {id} --follow");
        println!("  终止任务: agent-cli-x job kill {peer} {id}");
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
                println!("没有后台任务");
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
                        println!("已发送终止请求: {id}（尚未确认结束，稍后用 job list 查看）")
                    }
                    _ => println!("已终止任务: {id}"),
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
            let end = end.ok_or_else(|| AppError::connect("连接中断，未收到任务状态"))?;
            let status = end["status"].as_str().unwrap_or_default().to_string();
            match status.as_str() {
                "running" => {
                    out.finish(&json!({"code": 0}));
                    eprintln!("[agent-cli-x: 任务仍在运行；用 --follow 等待结束]");
                    Ok(0)
                }
                "lost" => {
                    out.finish(&json!({"code": 1}));
                    eprintln!("[agent-cli-x: 对端服务重启过，无法得知该任务的结果]");
                    Ok(1)
                }
                "killed" => {
                    out.finish(&json!({"code": 137}));
                    eprintln!("[agent-cli-x: 任务已被终止]");
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
                    "\r[agent-cli-x: 已转发 {files} 个文件（{}）]   ",
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
                "[agent-cli-x: {dst_peer} 版本 {} 不支持直传，已经本机临时目录中转（已删除）；升级对端后可直传]",
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
            "已复制 {} 个文件（{}，{:.1}s，{}/s）: {src_peer} {src} → {dst_peer} {}",
            v["files"],
            human_size(bytes),
            ms as f64 / 1000.0,
            human_size(bytes * 1000 / ms.max(1)),
            v["dest"].as_str().unwrap_or_default()
        );
        if let Some(op) = v["op_id"].as_str() {
            println!("  op_id={op}（撤销: agent-cli-x rollback {dst_peer} {op}）");
        }
    }
    Ok(0)
}

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
                        AppError::bad_request(format!("--lines `{spec}` 格式应为 起:止"))
                    })
                }
            };
            (p(a)?, p(b)?)
        }
    };
    let v = Client::load(peer)?.call(
        "/v1/fs/read",
        &json!({"path": path, "start": start, "end": end}),
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
            "\n[agent-cli-x: 显示第 {first}-{last} 行，共 {} 行{}；继续读取用 --lines {}:]",
            total.map(|t| t.to_string()).unwrap_or_else(|| "?".into()),
            if v["truncated"].as_bool() == Some(true) {
                "，已达单次读取上限"
            } else {
                ""
            },
            last + 1
        );
    }
    Ok(0)
}

fn print_info(name: &str, v: &Value) {
    println!(
        "{name} → 机器 {} ({})",
        v["node_name"].as_str().unwrap_or("?"),
        v["node_id"].as_str().unwrap_or("-")
    );
    let ver = v["version"].as_str().unwrap_or("?");
    println!(
        "  主机 {}  {}/{}  agent-cli-x {ver}{}  默认 shell: {}",
        v["hostname"].as_str().unwrap_or("?"),
        v["os"].as_str().unwrap_or("?"),
        v["arch"].as_str().unwrap_or("?"),
        if ver != http::VERSION {
            format!("（本机 {}，可用 upgrade 统一）", http::VERSION)
        } else {
            String::new()
        },
        v["shell"].as_str().unwrap_or("?")
    );
    if let Some(user) = v["exec_user"].as_str() {
        let elevated = match (v["exec_elevated"] == true, v["os"] == "windows") {
            (true, true) => "，管理员",
            (true, false) => "，root",
            _ => "",
        };
        println!(
            "  exec 身份: {user}{elevated}（exec 不受下方路径权限/禁止访问约束，只受命令黑名单约束）"
        );
    }
    if v["caller_no_relay"] == true {
        println!(
            "  本机对你禁止借道: 你（{}）在本机 exec 里不能再调用 agent-cli-x 访问其他节点",
            v["caller"].as_str().unwrap_or("?")
        );
    }
    let def = v["default_workspace"].as_str();
    for w in v["workspaces"].as_array().into_iter().flatten() {
        let n = w["name"].as_str().unwrap_or_default();
        println!(
            "  工作区 {n}{} = {}",
            if Some(n) == def { "（默认）" } else { "" },
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
        println!("  禁止访问: {}", deny.join(", "));
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
        println!("  路径权限（r 读 c 建 u 改 d 删）: {}", rules.join(", "));
    }
}

fn print_write_result(v: &Value, json: bool, verb: &str) {
    if json {
        println!("{v}");
        return;
    }
    let mut extra = String::new();
    if let Some(n) = v["replacements"].as_u64() {
        extra = format!(
            "，替换 {n} 处，起始第 {} 行",
            v["line"].as_u64().unwrap_or(0)
        );
    } else if let Some(b) = v["bytes"].as_u64() {
        extra = format!(
            "，{b} 字节{}",
            if v["created"] == true {
                "，新建"
            } else {
                ""
            }
        );
    }
    println!(
        "已{verb} {}{extra}\n  op_id={}（撤销: agent-cli-x rollback <对端> {}）\n  sha256={}",
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
        .map_err(|e| AppError::io(format!("读取本地文件 {} 失败: {e}", p.display())))?;
    String::from_utf8(bytes)
        .map_err(|_| AppError::bad_request(format!("{} 不是 UTF-8 文本", p.display())))
}

fn read_stdin() -> Result<String> {
    let mut stdin = std::io::stdin();
    if stdin.is_terminal() {
        return Err(AppError::bad_request(
            "请通过 --content、--from 或管道提供内容",
        ));
    }
    let mut s = String::new();
    stdin
        .read_to_string(&mut s)
        .map_err(|e| AppError::bad_request(format!("标准输入不是 UTF-8 文本: {e}")))?;
    Ok(s)
}

fn text_arg(inline: Option<String>, file: Option<PathBuf>, name: &str) -> Result<String> {
    match (inline, file) {
        (Some(s), _) => Ok(s),
        (None, Some(f)) => read_local_text(&f),
        (None, None) => Err(AppError::bad_request(format!("缺少 {name}"))),
    }
}
