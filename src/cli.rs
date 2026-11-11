//! 命令行入口。

use crate::client::{self, output::ExecOutput, Client};
use crate::config::{self, Peer};
use crate::error::{AppError, Result};
use crate::{server, tls, util};
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
        #[arg(long, default_value = "0.0.0.0:7700")]
        listen: String,
        /// 工作区，可重复：--workspace proj=D:/code/proj
        #[arg(long = "workspace", value_name = "NAME=PATH")]
        workspaces: Vec<String>,
        /// 覆盖已有配置（会更换 token、证书与机器 ID，已登记本机的对端需重新 peer add）
        #[arg(long)]
        force: bool,
    },
    /// 前台运行服务端
    Serve,
    /// 显示本机的名称、ID、指纹与工作区（不联网）
    Whoami,
    /// 打印本机证书指纹（供对端 peer add 核对）
    Fingerprint,
    /// 打印给 AI 阅读的用法说明
    Skill,
    /// 管理对端节点
    #[command(subcommand)]
    Peer(PeerCmd),
    /// 查看对端信息（机器名/ID、系统、shell、工作区、黑名单）
    Info { peer: String },
    /// 在对端执行命令（选项写在命令之前）
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
        /// 要执行的命令（多个参数会以空格拼接）
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
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
    /// 撤销一次 write / edit / put
    Rollback { peer: String, op_id: String },
}

#[derive(Subcommand)]
enum PeerCmd {
    /// 登记对端。首次不带 --fingerprint 只显示指纹供核对
    Add {
        name: String,
        /// IP:端口（端口默认 7700）
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

pub fn run() -> Result<i32> {
    let cli = Cli::parse();
    let json = cli.json;
    match cli.cmd {
        Cmd::Init {
            name,
            listen,
            workspaces,
            force,
        } => init(name, &listen, &workspaces, force),
        Cmd::Serve => server::serve(&config::home_dir()).map(|_| 0),
        Cmd::Whoami => whoami(json),
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
        Cmd::Info { peer } => {
            let v = Client::load(&peer)?.call("/v1/info", &json!({}), CALL_TIMEOUT)?;
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
            command,
        } => {
            let body = exec_body(cwd, shell, timeout, envs, command.join(" "))?;
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
                    let action = if f["action"] == "deleted" {
                        "已删除新建的文件"
                    } else {
                        "已恢复为修改前内容"
                    };
                    println!("{action}: {}", f["path"].as_str().unwrap_or_default());
                }
            }
            Ok(0)
        }
    }
}

fn init(name: Option<String>, listen: &str, workspaces: &[String], force: bool) -> Result<i32> {
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
    let port = listen.rsplit(':').next().unwrap_or("7700");
    println!("已初始化 {}", home.display());
    println!("  机器:   {node_name} ({node_id})");
    println!("  配置:   {}", cfg_path.display());
    println!("  监听:   {listen}");
    println!("  token:  {token}");
    println!("  指纹:   {fp}");
    println!();
    println!("下一步:");
    println!("  1. 编辑配置，在 [workspaces] 中登记允许访问的目录、按需调整 deny_paths 黑名单");
    println!("  2. 运行 `agent-cli-x serve`");
    println!("  3. 在对端执行: agent-cli-x peer add {node_name} <本机IP>:{port} --token {token} --fingerprint {fp}");
    if cfg!(windows) {
        println!("  防火墙（管理员 PowerShell 执行一次）:");
        println!("     netsh advfirewall firewall add rule name=\"agent-cli-x\" dir=in action=allow protocol=TCP localport={port}");
    } else {
        println!("  防火墙（如启用了 ufw）: sudo ufw allow {port}/tcp");
    }
    Ok(0)
}

fn whoami(json: bool) -> Result<i32> {
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
    }
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
                node_name: None,
                node_id: None,
            };
            let info = Client::from_peer(p.clone()).call("/v1/info", &json!({}), CALL_TIMEOUT)?;
            p.node_name = info["node_name"].as_str().map(|s| s.to_string());
            p.node_id = info["node_id"].as_str().map(|s| s.to_string());
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
            let peers = config::load_peers(&home)?;
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
                    println!(
                        "{n}\t{}\t{} ({})",
                        p.addr,
                        p.node_name.as_deref().unwrap_or("-"),
                        p.node_id.as_deref().unwrap_or("-")
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
                println!("已发送终止请求: {id}");
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
    println!(
        "  主机 {}  {}/{}  agent-cli-x {}  默认 shell: {}",
        v["hostname"].as_str().unwrap_or("?"),
        v["os"].as_str().unwrap_or("?"),
        v["arch"].as_str().unwrap_or("?"),
        v["version"].as_str().unwrap_or("?"),
        v["shell"].as_str().unwrap_or("?")
    );
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
