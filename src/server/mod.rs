//! 服务端：TLS 监听，每个连接一个线程、一个请求。

pub mod audit;
mod backup;
mod exec;
mod fs;
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
use std::time::Duration;
use workspace::Workspaces;

const MAX_CONNECTIONS: usize = 64;

pub struct State {
    pub cfg: Config,
    pub ws: Workspaces,
    pub home: PathBuf,
    pub audit: audit::Audit,
    active: AtomicUsize,
}

pub struct Ctx {
    pub st: Arc<State>,
    pub ip: String,
}

pub fn serve(home: &Path) -> Result<()> {
    let cfg = config::load_config(home)?;
    let ws = Workspaces::new(&cfg, home)?;
    let (tls_cfg, fp) = tls::server_config(home)?;
    util::prune_older_than(&home.join("backup"), backup::RETAIN_DAYS);
    let listener = TcpListener::bind(&cfg.listen)
        .map_err(|e| AppError::io(format!("监听 {} 失败: {e}", cfg.listen)))?;

    eprintln!("agent-cli-x {} 服务已启动", http::VERSION);
    eprintln!("  监听:   {}", cfg.listen);
    eprintln!("  指纹:   {fp}");
    eprintln!("  shell:  {}", platform::default_shell().name());
    if ws.is_empty() {
        eprintln!(
            "  警告:   未配置任何工作区，对端无法访问文件或执行命令（编辑 {}）",
            config::config_path(home).display()
        );
    }
    for (n, p) in ws.list() {
        eprintln!("  工作区: {n} = {}", p.display());
    }
    eprintln!(
        "  黑名单: {} 条路径规则，{} 条命令规则",
        ws.deny_rules().len(),
        cfg.policy.deny_commands.len()
    );

    let st = Arc::new(State {
        audit: audit::Audit::new(home.join("audit")),
        home: home.to_path_buf(),
        cfg,
        ws,
        active: AtomicUsize::new(0),
    });
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
    let mut s = StreamOwned::new(conn, tcp);

    let n = st.active.fetch_add(1, Ordering::SeqCst) + 1;
    let _guard = ActiveGuard(&st.active);
    let max_body = (st.cfg.policy.max_write_kb as usize * 1024) * 6 + 64 * 1024;
    let req = {
        let mut br = BufReader::new(&mut s);
        http::read_request(&mut br, max_body)
    };
    match req {
        Err(e) => {
            let _ = http::write_error(&mut s, &e);
        }
        Ok(_) if n > MAX_CONNECTIONS => {
            let _ = http::write_error(
                &mut s,
                &AppError::new("busy", "服务端并发连接已满，请稍后重试"),
            );
        }
        Ok(req) => dispatch(&st, ip, req, &mut s),
    }
    s.conn.send_close_notify();
    let _ = s.flush();
}

fn authorized(st: &State, req: &http::Request) -> bool {
    let got = req
        .header("authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    util::ct_eq(got.as_bytes(), st.cfg.token.as_bytes())
}

fn dispatch<W: Write>(st: &Arc<State>, ip: String, req: http::Request, w: &mut W) {
    if !authorized(st, &req) {
        st.audit
            .log(&ip, "auth_fail", false, json!({"path": req.path}));
        thread::sleep(Duration::from_millis(300));
        let _ = http::write_error(w, &AppError::auth("token 无效"));
        return;
    }
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
    let ctx = Ctx {
        st: Arc::clone(st),
        ip,
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
        "/v1/rollback" => backup::rollback(&ctx, body),
        other => Err(AppError::not_found(format!("未知接口 {other}"))),
    };
    if let Err(e) = &result {
        if e.code == "policy_denied" {
            ctx.st.audit.log(
                &ctx.ip,
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
        "version": http::VERSION,
        "hostname": platform::hostname(),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "shell": platform::default_shell().name(),
        "workspaces": ws,
        "default_workspace": ctx.st.ws.default_name(),
        "deny_paths": ctx.st.ws.deny_rules(),
        "deny_commands": ctx.st.cfg.policy.deny_commands,
        "default_timeout_secs": ctx.st.cfg.policy.default_timeout_secs,
        "max_timeout_secs": ctx.st.cfg.policy.max_timeout_secs,
    })
}
