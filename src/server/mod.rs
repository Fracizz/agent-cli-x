//! 服务端：TLS 监听，每个连接一个线程、一个请求。

mod admin;
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
/// - restart / set_port：`/v1/restart` 与 `/v1/port`
pub const FEATURES: &[&str] = &["put_stream", "purge", "restart", "set_port"];

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
    /// config.toml 有误、已回退到 last-good 运行时的错误信息（info 返回）。
    pub config_error: Option<String>,
    tls: Arc<ServerConfig>,
    net: admin::Net,
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
    /// 本次连接到达的本机端口（换端口时确认请求必须来自新端口）。
    pub local_port: u16,
}

/// 启动服务端。`wait_port_secs > 0` 时端口被占用会重试这么久（升级后新进程等待旧进程退出）。
/// 后台任务执行进程入口（`agent-cli-x job-run <目录>`，由 serve 启动）。
pub fn job_run(dir: &Path) -> Result<i32> {
    jobs::run(dir)
}

pub fn serve(home: &Path, wait_port_secs: u64) -> Result<()> {
    log::init(home);
    let r = serve_inner(home, wait_port_secs);
    if let Err(e) = &r {
        crate::slog!("serve exited: {e}");
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
                    "listen on {listen} failed: port in use. An agent-cli-x serve may already be running (e.g. the autostart one); check with `agent-cli-x doctor`, or change listen in config.toml"
                )))
            }
            Err(e) => return Err(AppError::io(format!("listen on {listen} failed: {e}"))),
        }
    }
}

/// 读取并完整校验一份配置文件（与启动时相同的检查）。
fn load_full(home: &Path, path: &Path) -> Result<(Config, Workspaces)> {
    let cfg = config::load_config_file(path)?;
    let ws = Workspaces::new(&cfg, home)?;
    Ok((cfg, ws))
}

/// `serve --check`：校验 config.toml 与证书，返回监听地址。
pub fn check(home: &Path) -> Result<String> {
    let (cfg, _) = load_full(home, &config::config_path(home))?;
    tls::server_config(home)?;
    Ok(cfg.listen)
}

/// 加载配置；config.toml 有误但存在 last-good 时回退运行（返回原错误信息），避免机器失联。
fn load_or_fallback(home: &Path) -> Result<(Config, Workspaces, Option<String>)> {
    let cfg_path = config::config_path(home);
    match load_full(home, &cfg_path) {
        Ok((cfg, ws)) => {
            config::save_last_good(home);
            Ok((cfg, ws, None))
        }
        Err(e) => {
            let lg = config::last_good_path(home);
            // 配置文件不存在（未 init 或被删除）不回退
            if !cfg_path.exists() || !lg.exists() {
                return Err(e);
            }
            let Ok((cfg, ws)) = load_full(home, &lg) else {
                return Err(e);
            };
            crate::slog!(
                "warning: {}; fell back to last good config {}. Fix and restart serve (from another machine: `agent-cli-x restart <this-peer>`)",
                e.message,
                lg.display()
            );
            Ok((cfg, ws, Some(e.message)))
        }
    }
}

fn serve_inner(home: &Path, wait_port_secs: u64) -> Result<()> {
    // config.toml 有误时这里会失败，交给 load_or_fallback 处理
    let _ = config::ensure_node_id(home);
    let (cfg, ws, config_error) = load_or_fallback(home)?;
    let (tls_cfg, fp) = tls::server_config(home)?;
    upgrade::cleanup_leftovers();
    let listener = bind(&cfg.listen, wait_port_secs)?;
    // 清理过期备份可能较慢，放到后台，不拖慢重新上线
    let backup_dir = home.join("backup");
    thread::spawn(move || util::prune_older_than(&backup_dir, backup::RETAIN_DAYS));
    let auth = auth::Auth::new(home, &cfg.token);

    crate::slog!(
        "agent-cli-x {} serving (pid {})",
        http::VERSION,
        std::process::id()
    );
    crate::slog!(
        "  machine: {} ({})",
        cfg.node_name(),
        cfg.node_id.as_deref().unwrap_or("-")
    );
    crate::slog!("  listen: {}", cfg.listen);
    crate::slog!("  fingerprint: {fp}");
    let shell = platform::default_shell();
    let mem_gb = platform::total_memory_gb();
    let configured = cfg.policy.prewarm_shells;
    let warm = warm::WarmPool::new(
        shell,
        config::resolve_prewarm(configured, mem_gb),
        cfg.policy.prewarm_idle_secs,
    );
    crate::slog!("  shell:  {}", shell.name());
    if warm.size() > 0 {
        let mem = mem_gb.map_or("memory unknown".to_string(), |g| format!("memory {g} GB"));
        let how = match configured {
            None => format!("auto, {mem}"),
            Some(n) if n > warm.size() => format!("configured {n}, exceeds {mem}, capped"),
            Some(_) => "policy.prewarm_shells".to_string(),
        };
        let idle = match cfg.policy.prewarm_idle_secs {
            0 => "kept".to_string(),
            n => format!("released after {n}s idle"),
        };
        crate::slog!(
            "  warm: {} idle {} ({how}; {idle})",
            warm.size(),
            shell.name()
        );
    }
    if shell == platform::Shell::PowerShell {
        crate::slog!("  note: pwsh not found, using Windows PowerShell 5.1 (cold start ~2 s); installing PowerShell 7 speeds up exec noticeably");
    }
    if ws.is_empty() {
        crate::slog!(
            "  warning: no workspace configured, peers cannot access files or run commands (edit {})",
            config::config_path(home).display()
        );
    }
    for (n, p) in ws.list() {
        crate::slog!("  workspace: {n} = {}", p.display());
    }
    crate::slog!(
        "  token: default token + {} caller tokens",
        auth.named_count()
    );
    crate::slog!("  log: {}", log::dir(home).display());
    crate::slog!(
        "  denylist: {} denied paths, {} path policy rules, {} command rules",
        ws.deny_rules().len(),
        ws.perm_rules().len(),
        cfg.policy.deny_commands.len()
    );

    let listen = cfg.listen.clone();
    let st = Arc::new(State {
        audit: audit::Audit::new(home.join("audit")),
        jobs: jobs::Jobs::new(home.join("jobs")),
        warm: Arc::clone(&warm),
        home: home.to_path_buf(),
        auth,
        started: util::now_iso(),
        config_error,
        tls: tls_cfg,
        net: admin::Net::default(),
        cfg,
        ws,
        active: AtomicUsize::new(0),
    });
    warm.refill();
    // 监听器在独立线程上运行（换端口时会被替换），主线程只需保持进程存活
    let h = admin::spawn_accept(&st, listener, &listen)?;
    st.net.set_active(h);
    loop {
        thread::park();
    }
}

struct ActiveGuard<'a>(&'a AtomicUsize);
impl Drop for ActiveGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn handle_conn(st: Arc<State>, tcp: TcpStream) {
    let ip = tcp
        .peer_addr()
        .map(|a| a.ip().to_string())
        .unwrap_or_default();
    let local_port = tcp.local_addr().map(|a| a.port()).unwrap_or_default();
    let _ = tcp.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = tcp.set_write_timeout(Some(Duration::from_secs(120)));
    let _ = tcp.set_nodelay(true);
    let Ok(conn) = ServerConnection::new(Arc::clone(&st.tls)) else {
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
                &AppError::new("busy", "server connection limit reached, retry later"),
            );
        }
        Ok(req) => dispatch(&st, ip, local_port, req, &mut conn),
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

fn dispatch(st: &Arc<State>, ip: String, local_port: u16, req: http::Request, conn: &mut Conn) {
    let Some(caller) = authorize(st, &req) else {
        st.audit
            .log(&ip, "", "auth_fail", false, json!({"path": req.path}));
        thread::sleep(Duration::from_millis(300));
        let _ = http::write_error(conn.get_mut(), &AppError::auth("invalid token"));
        return;
    };
    let ctx = Ctx {
        st: Arc::clone(st),
        ip,
        caller: caller.name,
        no_relay: caller.no_relay,
        local_port,
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
                let _ = http::write_error(w, &AppError::bad_request(format!("invalid JSON: {e}")));
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
        "/v1/fs/find" => fs::find(&ctx, body),
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
        "/v1/restart" => {
            admin::restart(&ctx, body, w);
            return;
        }
        "/v1/port" => admin::port(&ctx, body),
        "/v1/rollback" => backup::rollback(&ctx, body),
        "/v1/purge" => backup::purge(&ctx, body),
        other => Err(AppError::not_found(format!("unknown endpoint {other}"))),
    };
    if let Err(e) = &result {
        if e.code == "internal" {
            crate::slog!("{} internal error: {}", req.path, e.message);
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
    let mut v = info_base(ctx);
    // 只在异常时出现，正常情况下不占 token
    if let Some(e) = &ctx.st.config_error {
        v["config_error"] = json!(e);
    }
    v
}

fn info_base(ctx: &Ctx) -> Value {
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
        // 重启/升级时后台任务能否继续运行（upgrade 据此决定是否需要 --force）
        "jobs_survive_restart": upgrade::jobs_survive_restart(),
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
            "idle_secs": ctx.st.cfg.policy.prewarm_idle_secs,
        },
    })
}
