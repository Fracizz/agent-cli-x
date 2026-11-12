//! 服务端：TLS 监听，每个连接一个线程、一个请求。

pub mod audit;
mod auth;
mod backup;
mod exec;
mod fs;
mod jobs;
pub mod log;
mod recycle;
pub mod transfer;
mod upgrade;
mod warm;
pub mod workspace;

use crate::config::{self, Config};
use crate::error::{AppError, Result};
use crate::{http, platform, tls, util};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use serde_json::{json, Value};
use std::io::{BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use workspace::Workspaces;

const MAX_CONNECTIONS: usize = 64;

/// 本版本服务端支持的可选能力，客户端据此决定是否使用新接口（info 返回）。
/// - put_stream：put 接受长度未知的帧流（cp 在两个对端之间直接中转）
/// - purge：按 op_id 彻底清除回收站条目与备份
pub const FEATURES: &[&str] = &["put_stream", "purge"];

pub struct State {
    pub cfg: Config,
    pub ws: Workspaces,
    pub home: PathBuf,
    pub audit: audit::Audit,
    pub jobs: jobs::Jobs,
    pub warm: Arc<warm::WarmPool>,
    pub auth: auth::Auth,
    /// 本进程启动时间（info 返回，供 upgrade 判断服务是否已重启）。
    pub started: String,
    active: AtomicUsize,
}

type Conn = BufReader<StreamOwned<ServerConnection, TcpStream>>;

pub struct Ctx {
    pub st: Arc<State>,
    pub ip: String,
    /// 通过鉴权的 token 名称（默认 token 为 `default`）。
    pub caller: String,
    /// 该调用方被禁止借道（`token add --no-relay`）：exec 子进程会带上标记环境变量。
    pub no_relay: bool,
}

/// 启动服务端。`wait_port_secs > 0` 时端口被占用会重试这么久（升级后新进程等待旧进程退出）。
pub fn serve(home: &Path, wait_port_secs: u64) -> Result<()> {
    log::init(home);
    let r = serve_inner(home, wait_port_secs);
    if let Err(e) = &r {
        crate::slog!("serve 退出: {e}");
    }
    r
}

fn bind(listen: &str, wait_secs: u64) -> Result<TcpListener> {
    let deadline = Instant::now() + Duration::from_secs(wait_secs);
    loop {
        match TcpListener::bind(listen) {
            Ok(l) => return Ok(l),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(200));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                return Err(AppError::io(format!(
                    "监听 {listen} 失败: 端口已被占用。可能已有一个 agent-cli-x serve 在运行（例如开机自启的那个），可用 `agent-cli-x doctor` 检查；或在 config.toml 中修改 listen"
                )))
            }
            Err(e) => return Err(AppError::io(format!("监听 {listen} 失败: {e}"))),
        }
    }
}

fn serve_inner(home: &Path, wait_port_secs: u64) -> Result<()> {
    config::ensure_node_id(home)?;
    let cfg = config::load_config(home)?;
    let ws = Workspaces::new(&cfg, home)?;
    let (tls_cfg, fp) = tls::server_config(home)?;
    util::prune_older_than(&home.join("backup"), backup::RETAIN_DAYS);
    upgrade::cleanup_leftovers();
    let listener = bind(&cfg.listen, wait_port_secs)?;
    let auth = auth::Auth::new(home, &cfg.token);

    crate::slog!(
        "agent-cli-x {} 服务已启动（pid {}）",
        http::VERSION,
        std::process::id()
    );
    crate::slog!(
        "  机器:   {} ({})",
        cfg.node_name(),
        cfg.node_id.as_deref().unwrap_or("-")
    );
    crate::slog!("  监听:   {}", cfg.listen);
    crate::slog!("  指纹:   {fp}");
    let shell = platform::default_shell();
    let mem_gb = platform::total_memory_gb();
    let configured = cfg.policy.prewarm_shells;
    let warm = warm::WarmPool::new(shell, config::resolve_prewarm(configured, mem_gb));
    crate::slog!("  shell:  {}", shell.name());
    if warm.size() > 0 {
        let mem = mem_gb.map_or("内存未知".to_string(), |g| format!("内存 {g} GB"));
        let how = match configured {
            None => format!("自动，{mem}"),
            Some(n) if n > warm.size() => format!("配置 {n}，超过{mem}，已限制"),
            Some(_) => "policy.prewarm_shells".to_string(),
        };
        crate::slog!("  预热:   {} 个空闲 {}（{how}）", warm.size(), shell.name());
    }
    if shell == platform::Shell::PowerShell {
        crate::slog!("  提示:   未找到 pwsh，正在使用 Windows PowerShell 5.1（冷启动约 2 秒）；安装 PowerShell 7 可明显加快 exec");
    }
    if ws.is_empty() {
        crate::slog!(
            "  警告:   未配置任何工作区，对端无法访问文件或执行命令（编辑 {}）",
            config::config_path(home).display()
        );
    }
    for (n, p) in ws.list() {
        crate::slog!("  工作区: {n} = {}", p.display());
    }
    crate::slog!(
        "  token:  默认 token + {} 个调用方 token",
        auth.named_count()
    );
    crate::slog!("  日志:   {}", log::dir(home).display());
    crate::slog!(
        "  黑名单: {} 条禁止路径，{} 条路径权限规则，{} 条命令规则",
        ws.deny_rules().len(),
        ws.perm_rules().len(),
        cfg.policy.deny_commands.len()
    );

    let st = Arc::new(State {
        audit: audit::Audit::new(home.join("audit")),
        jobs: jobs::Jobs::new(home.join("jobs")),
        warm: Arc::clone(&warm),
        home: home.to_path_buf(),
        auth,
        started: util::now_iso(),
        cfg,
        ws,
        active: AtomicUsize::new(0),
    });
    warm.refill();
    for stream in listener.incoming() {
        let Ok(tcp) = stream else { continue };
        let st = Arc::clone(&st);
        let tls_cfg = Arc::clone(&tls_cfg);
        thread::spawn(move || handle_conn(st, tls_cfg, tcp));
    }
    Ok(())
}

struct ActiveGuard<'a>(&'a AtomicUsize);
impl Drop for ActiveGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn handle_conn(st: Arc<State>, tls_cfg: Arc<ServerConfig>, tcp: TcpStream) {
    let ip = tcp
        .peer_addr()
        .map(|a| a.ip().to_string())
        .unwrap_or_default();
    let _ = tcp.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = tcp.set_write_timeout(Some(Duration::from_secs(120)));
    let _ = tcp.set_nodelay(true);
    let Ok(conn) = ServerConnection::new(tls_cfg) else {
        return;
    };
    let mut conn: Conn = BufReader::new(StreamOwned::new(conn, tcp));

    let n = st.active.fetch_add(1, Ordering::SeqCst) + 1;
    let _guard = ActiveGuard(&st.active);
    let max_body = (st.cfg.policy.max_write_kb as usize * 1024) * 6 + 64 * 1024;
    // put 的请求体是大文件流，由 transfer::put 边读边写；其余接口一次读完
    let req = http::read_request_head(&mut conn).and_then(|mut req| {
        if !is_streaming_upload(&req.path) {
            req.body = http::read_body(&mut conn, req.content_length, max_body)?;
        }
        Ok(req)
    });
    match req {
        Err(e) => {
            let _ = http::write_error(conn.get_mut(), &e);
        }
        Ok(_) if n > MAX_CONNECTIONS => {
            let _ = http::write_error(
                conn.get_mut(),
                &AppError::new("busy", "服务端并发连接已满，请稍后重试"),
            );
        }
        Ok(req) => dispatch(&st, ip, req, &mut conn),
    }
    let s = conn.get_mut();
    s.conn.send_close_notify();
    let _ = s.flush();
}

/// 请求体是大块二进制流、由处理函数边读边写的接口。
fn is_streaming_upload(path: &str) -> bool {
    matches!(path, "/v1/fs/put" | "/v1/upgrade")
}

/// 鉴权通过时返回调用方。
fn authorize(st: &State, req: &http::Request) -> Option<auth::Caller> {
    let got = req
        .header("authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    st.auth.check(got)
}

fn dispatch(st: &Arc<State>, ip: String, req: http::Request, conn: &mut Conn) {
    let Some(caller) = authorize(st, &req) else {
        st.audit
            .log(&ip, "", "auth_fail", false, json!({"path": req.path}));
        thread::sleep(Duration::from_millis(300));
        let _ = http::write_error(conn.get_mut(), &AppError::auth("token 无效"));
        return;
    };
    let ctx = Ctx {
        st: Arc::clone(st),
        ip,
        caller: caller.name,
        no_relay: caller.no_relay,
    };
    match req.path.as_str() {
        "/v1/fs/put" => {
            // 带 X-Agent-Cli-Stream: frames 时请求体长度未知（cp 中转），按帧读到结束帧为止
            let len = match req.header(http::STREAM_HEADER) {
                Some(v) if v.eq_ignore_ascii_case("frames") => None,
                _ => Some(req.content_length),
            };
            return transfer::put(&ctx, len, conn);
        }
        "/v1/upgrade" => return upgrade::handle(&ctx, req.content_length, conn),
        _ => {}
    }
    let w = conn.get_mut();
    let body: Value = if req.body.is_empty() {
        json!({})
    } else {
        match serde_json::from_slice(&req.body) {
            Ok(v) => v,
            Err(e) => {
                let _ = http::write_error(w, &AppError::bad_request(format!("JSON 无效: {e}")));
                return;
            }
        }
    };
    let result = match req.path.as_str() {
        "/v1/info" => Ok(info(&ctx)),
        "/v1/exec" => {
            exec::run(&ctx, body, w);
            return;
        }
        "/v1/fs/ls" => fs::ls(&ctx, body),
        "/v1/fs/read" => fs::read(&ctx, body),
        "/v1/fs/write" => fs::write(&ctx, body),
        "/v1/fs/edit" => fs::edit(&ctx, body),
        "/v1/fs/rm" => fs::rm(&ctx, body),
        "/v1/fs/grep" => fs::grep(&ctx, body),
        "/v1/fs/get" => {
            transfer::get(&ctx, body, w);
            return;
        }
        "/v1/jobs/start" => jobs::start(&ctx, body),
        "/v1/jobs/list" => jobs::list(&ctx, body),
        "/v1/jobs/kill" => jobs::kill(&ctx, body),
        "/v1/jobs/log" => {
            jobs::log(&ctx, body, w);
            return;
        }
        "/v1/rollback" => backup::rollback(&ctx, body),
        "/v1/purge" => backup::purge(&ctx, body),
        other => Err(AppError::not_found(format!("未知接口 {other}"))),
    };
    if let Err(e) = &result {
        if e.code == "internal" {
            crate::slog!("{} 内部错误: {}", req.path, e.message);
        }
        if e.code == "policy_denied" {
            ctx.st.audit.log(
                &ctx.ip,
                &ctx.caller,
                "denied",
                false,
                json!({"path": req.path, "error": e.message}),
            );
        }
    }
    let _ = match result {
        Ok(v) => http::write_json(w, 200, &v),
        Err(e) => http::write_error(w, &e),
    };
}

fn info(ctx: &Ctx) -> Value {
    let ws: Vec<Value> = ctx
        .st
        .ws
        .list()
        .map(|(n, p)| json!({"name": n, "path": p.to_string_lossy()}))
        .collect();
    json!({
        "node_name": ctx.st.cfg.node_name(),
        "node_id": ctx.st.cfg.node_id,
        "version": http::VERSION,
        "pid": std::process::id(),
        "started": ctx.st.started,
        "running_jobs": ctx.st.jobs.running_count(),
        "hostname": platform::hostname(),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "shell": platform::default_shell().name(),
        // exec 以 serve 进程的身份运行：路径权限只约束文件类接口，不约束 exec
        "exec_user": platform::current_user(),
        "exec_elevated": platform::is_elevated(),
        "features": FEATURES,
        // 本次调用方及其限制：no_relay 时在本机 exec 里不能再调用 agent-cli-x 访问其他节点
        "caller": ctx.caller,
        "caller_no_relay": ctx.no_relay,
        "workspaces": ws,
        "default_workspace": ctx.st.ws.default_name(),
        "deny_paths": ctx.st.ws.deny_rules(),
        "path_rules": ctx.st.ws.perm_rules().into_iter()
            .map(|(path, allow)| json!({"path": path, "allow": allow}))
            .collect::<Vec<_>>(),
        "deny_commands": ctx.st.cfg.policy.deny_commands,
        "default_timeout_secs": ctx.st.cfg.policy.default_timeout_secs,
        "max_timeout_secs": ctx.st.cfg.policy.max_timeout_secs,
        "prewarm": {
            "shell": ctx.st.warm.shell().map(|s| s.name()),
            "size": ctx.st.warm.size(),
            "auto": ctx.st.cfg.policy.prewarm_shells.is_none(),
            "idle": ctx.st.warm.idle_count(),
        },
    })
}
