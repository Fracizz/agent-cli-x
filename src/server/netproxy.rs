//! `/v1/connect`：让调用方经本机访问网络（`proxy` 子命令），默认关闭。

use super::{Conn, Ctx};
use crate::error::{AppError, Result};
use crate::{http, tunnel};
use rustls::StreamOwned;
use serde::Deserialize;
use serde_json::json;
use std::io::Write;
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// 同时存在的隧道上限：低于总连接上限，给 exec 等普通请求留余量。
const MAX_TUNNELS: usize = 48;
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_DEADLINE: Duration = Duration::from_secs(20);

static ACTIVE: AtomicUsize = AtomicUsize::new(0);

struct Permit;
impl Permit {
    fn acquire() -> Result<Self> {
        // 先占位再判断：不用 fetch_update（新版标准库改名为 try_update，最低支持版本还没有）
        if ACTIVE.fetch_add(1, Ordering::SeqCst) >= MAX_TUNNELS {
            ACTIVE.fetch_sub(1, Ordering::SeqCst);
            return Err(AppError::new(
                "busy",
                format!("{MAX_TUNNELS} proxy connections already open on this peer; retry later"),
            ));
        }
        Ok(Self)
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        ACTIVE.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Deserialize)]
struct Target {
    host: String,
    port: u16,
}

/// 接管整条连接：成功时回 200，之后连接里就是目标地址的原始字节流。
pub fn serve(ctx: Ctx, body: &[u8], mut conn: Conn) {
    match open(&ctx, body) {
        Err(e) => {
            let _ = http::write_error(conn.get_mut(), &e);
            let s = conn.get_mut();
            s.conn.send_close_notify();
            let _ = s.flush();
        }
        Ok((target, _permit)) => {
            let w = conn.get_mut();
            if http::write_head(w, 200, "application/octet-stream")
                .and_then(|_| w.flush())
                .is_err()
            {
                return;
            }
            let early = conn.buffer().to_vec();
            let StreamOwned { conn, sock } = conn.into_inner();
            // 任一端中断都属于正常结束，不单独记录
            let _ = tunnel::pump(
                conn.into(),
                &sock,
                &target,
                &[],
                &early,
                tunnel::LIMITS,
                &tunnel::Counters::default(),
            );
        }
    }
}

fn open(ctx: &Ctx, body: &[u8]) -> Result<(TcpStream, Permit)> {
    let audit = |ok: bool, detail| {
        ctx.st
            .audit
            .log(&ctx.ip, &ctx.caller, "net_connect", ok, detail)
    };
    let policy = &ctx.st.cfg.policy;
    let refused = if !policy.net_proxy {
        Some("the network proxy is disabled on this peer; its owner must set [policy] net_proxy = true in config.toml, then restart serve".to_string())
    } else if ctx.no_relay {
        Some(format!(
            "caller `{}` is not allowed to relay (token --no-relay), so it can't use this machine as a network proxy",
            ctx.caller
        ))
    } else {
        None
    };
    if let Some(reason) = refused {
        audit(false, json!({"error": reason}));
        return Err(AppError::denied(reason));
    }
    let t: Target = serde_json::from_slice(body)
        .map_err(|e| AppError::bad_request(format!("bad proxy target: {e}")))?;
    if t.port == 0
        || t.host.is_empty()
        || t.host.len() > 255
        || t.host.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(AppError::bad_request("bad proxy target host/port"));
    }
    let permit = Permit::acquire()?;
    match dial(&t.host, t.port, policy.net_proxy_local) {
        Ok((stream, addr)) => {
            audit(
                true,
                json!({"host": t.host, "port": t.port, "addr": addr.to_string()}),
            );
            Ok((stream, permit))
        }
        Err(e) => {
            audit(
                false,
                json!({"host": t.host, "port": t.port, "error": e.message}),
            );
            Err(e)
        }
    }
}

/// 回环、链路本地（含云主机元数据地址）、未指定、组播与广播地址。
fn is_local_range(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v) => {
            v.is_loopback()
                || v.octets()[0] == 0
                || v.is_link_local()
                || v.is_multicast()
                || v.is_broadcast()
        }
        IpAddr::V6(v) => {
            v.is_loopback()
                || v.is_unspecified()
                || v.is_multicast()
                || (v.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

fn dial(host: &str, port: u16, allow_local: bool) -> Result<(TcpStream, SocketAddr)> {
    let target = format!("{host}:{port}");
    let addrs = (host, port)
        .to_socket_addrs()
        .map_err(|e| AppError::connect(format!("this peer cannot resolve {target}: {e}")))?;
    let local = || {
        AppError::denied(format!(
            "{target} is a loopback, link-local or own address of this peer; refused (policy.net_proxy_local = true allows it)"
        ))
    };
    let deadline = Instant::now() + CONNECT_DEADLINE;
    let mut last = AppError::connect(format!("this peer cannot resolve {target}"));
    for addr in addrs {
        if !allow_local && is_local_range(addr.ip()) {
            last = local();
            continue;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        match TcpStream::connect_timeout(&addr, left.min(ATTEMPT_TIMEOUT)) {
            Ok(s) => {
                // 连到本机任一网卡地址时，系统选用的源地址与目标相同
                if !allow_local && s.local_addr().is_ok_and(|l| l.ip() == addr.ip()) {
                    last = local();
                    continue;
                }
                let _ = s.set_nodelay(true);
                return Ok((s, addr));
            }
            Err(e) => {
                last = AppError::connect(format!("this peer cannot connect to {target}: {e}"));
            }
        }
    }
    Err(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_ranges_are_refused() {
        for ip in [
            "127.0.0.1",
            "0.0.0.0",
            "0.1.2.3",
            "169.254.169.254",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fe80::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(is_local_range(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "8.8.8.8",
            "192.168.10.1",
            "10.0.0.1",
            "198.18.0.5",
            "2001:db8::1",
        ] {
            assert!(!is_local_range(ip.parse().unwrap()), "{ip}");
        }
    }
}
