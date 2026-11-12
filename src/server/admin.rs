//! 运维接口：重启服务（/v1/restart）、不停机更换监听端口（/v1/port）与重启前的配置预检。
//!
//! 换端口分两步，先通后切：
//! 1. `open`：在新端口上额外开一个监听器（旧端口照常服务），120 秒内未确认自动关闭；
//! 2. `commit`：必须**经由新端口**发来，证明调用方连得上新端口，再写入 config.toml 并关闭旧端口。
//!
//! 新端口被外部防火墙/安全组拦住时 commit 到不了，旧端口保持不变，不会失联。

use super::{upgrade, Ctx, State};
use crate::config;
use crate::error::{AppError, Result};
use crate::{firewall, http, platform, tls};
use serde_json::{json, Value};
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// 新端口等待确认的时间。
const PENDING_SECS: u64 = 120;

/// 一个监听器：设置 stop 后唤醒阻塞中的 accept，监听线程退出并释放端口。
pub struct ListenerHandle {
    pub addr: String,
    local: SocketAddr,
    stop: Arc<AtomicBool>,
}

impl ListenerHandle {
    fn close(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let mut a = self.local;
        if a.ip().is_unspecified() {
            a.set_ip(match a.ip() {
                IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
                IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
            });
        }
        let _ = TcpStream::connect_timeout(&a, Duration::from_secs(2));
    }
}

struct Pending {
    id: u64,
    port: u16,
    handle: ListenerHandle,
}

#[derive(Default)]
struct NetInner {
    active: Option<ListenerHandle>,
    pending: Option<Pending>,
}

/// 当前监听器与待确认的新端口。
#[derive(Default)]
pub struct Net {
    inner: Mutex<NetInner>,
    next_id: AtomicU64,
}

impl Net {
    fn lock(&self) -> std::sync::MutexGuard<'_, NetInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set_active(&self, h: ListenerHandle) {
        self.lock().active = Some(h);
    }

    fn listen(&self) -> String {
        self.lock()
            .active
            .as_ref()
            .map(|h| h.addr.clone())
            .unwrap_or_default()
    }
}

/// 在独立线程上接受连接。
pub fn spawn_accept(st: &Arc<State>, listener: TcpListener, addr: &str) -> Result<ListenerHandle> {
    let local = listener.local_addr()?;
    let stop = Arc::new(AtomicBool::new(false));
    let (st, flag) = (Arc::clone(st), Arc::clone(&stop));
    thread::spawn(move || {
        for stream in listener.incoming() {
            if flag.load(Ordering::SeqCst) {
                break;
            }
            let Ok(tcp) = stream else { continue };
            let st = Arc::clone(&st);
            thread::spawn(move || super::handle_conn(st, tcp));
        }
    });
    Ok(ListenerHandle {
        addr: addr.to_string(),
        local,
        stop,
    })
}

/// 用当前代码完整校验一遍配置（与 serve 启动时相同），不通过则说明重启后起不来。
pub fn preflight(home: &Path) -> Result<()> {
    let r = (|| {
        let cfg = config::load_config(home)?;
        super::workspace::Workspaces::new(&cfg, home)?;
        tls::server_config(home)?;
        Ok(())
    })();
    r.map_err(|e: AppError| {
        AppError::config(format!(
            "{}\n配置有误，重启后服务将无法启动，已取消。修复后再试（在该机器上可用 `agent-cli-x serve --check` 校验）",
            e.message.trim_end()
        ))
    })
}

/// `/v1/restart`：预检配置 → 返回响应 → 重启自身。
pub fn restart<W: Write>(ctx: &Ctx, body: Value, w: &mut W) {
    let force = body["force"].as_bool().unwrap_or(false);
    let res = upgrade::jobs_on_restart(ctx, force, "重启").and_then(|(interrupted, kept)| {
        preflight(&ctx.st.home)?;
        Ok(json!({
            "pid": std::process::id(),
            "restart": upgrade::restart_mode(),
            "interrupted_jobs": interrupted,
            "kept_jobs": kept,
        }))
    });
    ctx.st.audit.log(
        &ctx.ip,
        &ctx.caller,
        "restart",
        res.is_ok(),
        res.as_ref()
            .map_or_else(|e| json!({"error": e.message}), |v| v.clone()),
    );
    match res {
        Ok(v) => {
            crate::slog!("由 {}@{} 请求重启", ctx.caller, ctx.ip);
            let _ = http::write_json(w, 200, &v);
            let _ = w.flush();
            upgrade::schedule_restart();
        }
        Err(e) => {
            let _ = http::write_error(w, &e);
        }
    }
}

/// `/v1/port`：`{"action": "open"|"commit"|"abort", "port": N}`。
pub fn port(ctx: &Ctx, body: Value) -> Result<Value> {
    let action = body["action"].as_str().unwrap_or_default();
    if action == "abort" {
        let p = ctx.st.net.lock().pending.take();
        if let Some(p) = &p {
            p.handle.close();
            crate::slog!("已撤销新端口 {}", p.handle.addr);
        }
        return Ok(json!({"aborted": p.is_some()}));
    }
    let port = body["port"]
        .as_u64()
        .filter(|p| (1..=65535).contains(p))
        .ok_or_else(|| AppError::bad_request("port 无效（1-65535）"))? as u16;
    match action {
        "open" => open(ctx, port),
        "commit" => commit(ctx, port),
        other => Err(AppError::bad_request(format!("未知 action `{other}`"))),
    }
}

fn open(ctx: &Ctx, port: u16) -> Result<Value> {
    let st = &ctx.st;
    if let Some(e) = &st.config_error {
        return Err(AppError::conflict(format!(
            "该机器正在用上次可用的配置运行（config.toml 有误: {e}），先修复 config.toml 再改端口"
        )));
    }
    let cur = st.net.listen();
    let addr = config::with_port(&cur, port);
    if addr == cur {
        return Err(AppError::bad_request(format!("已经在监听 {cur}")));
    }
    let listener = TcpListener::bind(&addr).map_err(|e| match e.kind() {
        std::io::ErrorKind::AddrInUse => {
            AppError::conflict(format!("端口 {port} 已被其他程序占用，换一个端口"))
        }
        // Windows 对系统保留（Hyper-V 等排除范围）或被独占的端口返回 WSAEACCES
        std::io::ErrorKind::PermissionDenied => AppError::conflict(format!(
            "端口 {port} 被系统保留或被其他程序独占，换一个端口\
（Windows 可用 `netsh int ipv4 show excludedportrange protocol=tcp` 查看保留范围）"
        )),
        _ => AppError::io(format!("监听 {addr} 失败: {e}")),
    })?;
    let handle = spawn_accept(st, listener, &addr)?;
    let id = st.net.next_id.fetch_add(1, Ordering::SeqCst);
    if let Some(old) = st.net.lock().pending.replace(Pending { id, port, handle }) {
        old.handle.close();
    }
    // 超时未确认：关闭新端口，旧端口不受影响
    let st2 = Arc::clone(st);
    thread::spawn(move || {
        thread::sleep(Duration::from_secs(PENDING_SECS));
        let mut g = st2.net.lock();
        if g.pending.as_ref().is_some_and(|p| p.id == id) {
            if let Some(p) = g.pending.take() {
                p.handle.close();
                crate::slog!("新端口 {} 超时未确认，已关闭", p.handle.addr);
            }
        }
    });

    let mut fw = firewall::check(port);
    if fw.blocked() && platform::is_elevated() && firewall::add_program_rule() {
        fw = firewall::Status::Added;
        crate::slog!("已添加防火墙入站规则 agent-cli-x（按程序放行）");
    }
    crate::slog!(
        "已在 {addr} 上开始监听，等待确认（{}@{}）",
        ctx.caller,
        ctx.ip
    );
    let mut v = json!({
        "listen": addr,
        "old_listen": cur,
        "firewall": fw.name(),
        "expires_secs": PENDING_SECS,
    });
    if fw.blocked() {
        v["firewall_cmd"] = json!(firewall::program_rule_cmd());
    }
    Ok(v)
}

fn commit(ctx: &Ctx, port: u16) -> Result<Value> {
    let st = &ctx.st;
    let mut g = st.net.lock();
    if !g.pending.as_ref().is_some_and(|p| p.port == port) {
        return Err(AppError::conflict(format!(
            "没有待确认的端口 {port}（可能已超过 {PENDING_SECS} 秒）。请重新执行 set-port"
        )));
    }
    if ctx.local_port != port {
        return Err(AppError::conflict(format!(
            "确认请求必须经由新端口 {port} 发送（本次来自端口 {}）",
            ctx.local_port
        )));
    }
    let p = g.pending.take().expect("上面已检查");
    if let Err(e) = config::replace_config_listen(&st.home, &p.handle.addr) {
        p.handle.close();
        return Err(e);
    }
    config::save_last_good(&st.home);
    let new_addr = p.handle.addr.clone();
    let old = g.active.replace(p.handle);
    drop(g);
    let old_addr = old.as_ref().map(|h| h.addr.clone()).unwrap_or_default();
    if let Some(h) = old {
        // 已建立的连接不受影响，只是不再接受新连接
        h.close();
    }
    crate::slog!(
        "监听地址已由 {old_addr} 改为 {new_addr}（{}@{}），已写入 config.toml",
        ctx.caller,
        ctx.ip
    );
    ctx.st.audit.log(
        &ctx.ip,
        &ctx.caller,
        "set_port",
        true,
        json!({"old": old_addr, "new": new_addr}),
    );
    Ok(json!({"listen": new_addr, "old_listen": old_addr}))
}
