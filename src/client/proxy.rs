//! `proxy`：命令运行期间在本机开一个临时 HTTP 代理，流量经对端出去。
//!
//! 代理只监听 127.0.0.1，带随机密码，仅通过环境变量告诉子进程；子进程结束即失效。

use super::Client;
use crate::error::{AppError, Result};
use crate::{tunnel, util};
use rustls::StreamOwned;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// 本机同时打开的隧道数，超出的连接排队（与对端上限一致）。
const MAX_ACTIVE: usize = 48;
const CALL: Duration = Duration::from_secs(30);
/// 对端隧道占满时的重试次数与间隔。
const BUSY_RETRIES: u32 = 40;
const BUSY_WAIT: Duration = Duration::from_millis(500);
/// 最多向 stderr 报告几种不同的连接错误。
const MAX_REPORTS: usize = 5;
const MAX_REPORT_CHARS: usize = 300;
const MAX_LINE: u64 = 16 * 1024;
const MAX_HEADERS: usize = 100;

struct Shared {
    client: Client,
    /// 期望的 `Proxy-Authorization` 值。
    auth: String,
    active: AtomicUsize,
    connections: AtomicUsize,
    failures: AtomicUsize,
    bytes: tunnel::Counters,
    reported: Mutex<Vec<String>>,
}

impl Shared {
    fn new(client: Client, auth: String) -> Self {
        Self {
            client,
            auth,
            active: AtomicUsize::new(0),
            connections: AtomicUsize::new(0),
            failures: AtomicUsize::new(0),
            bytes: tunnel::Counters::default(),
            reported: Mutex::new(Vec::new()),
        }
    }
}

/// 运行 `command`，返回它的退出码。
pub fn run(client: Client, command: &[String], json_output: bool) -> Result<i32> {
    preflight(&client)?;
    let program = command
        .first()
        .ok_or_else(|| AppError::bad_request("missing command after `--`"))?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    let password = util::random_hex(16);
    let url = format!("http://acx:{password}@{addr}");
    let name = client.name.clone();
    let auth = format!(
        "Basic {}",
        util::base64(format!("acx:{password}").as_bytes())
    );
    let shared = Arc::new(Shared::new(client, auth));
    {
        let shared = Arc::clone(&shared);
        thread::Builder::new().spawn(move || accept(&shared, listener))?;
    }

    let mut cmd = Command::new(resolve_program(program));
    cmd.args(&command[1..]);
    for key in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
        // npm 的 .npmrc 里配置的代理优先级高于 HTTP_PROXY
        "npm_config_proxy",
        "npm_config_https_proxy",
    ] {
        cmd.env(key, &url);
    }
    let no_proxy = no_proxy(
        std::env::var("NO_PROXY")
            .or_else(|_| std::env::var("no_proxy"))
            .ok(),
    );
    cmd.env("NO_PROXY", &no_proxy).env("no_proxy", &no_proxy);
    // Node 24+ 的 fetch / http 默认忽略代理环境变量
    cmd.env("NODE_USE_ENV_PROXY", "1");
    let status = cmd.status().map_err(|e| {
        let msg = format!(
            "cannot run `{program}`: {e} (shell built-ins, aliases and scripts need a shell: -- pwsh -c \"...\" / -- sh -c '...')"
        );
        match e.kind() {
            std::io::ErrorKind::NotFound => AppError::not_found(msg),
            _ => AppError::io(msg),
        }
    })?;

    let n = shared.connections.load(Ordering::SeqCst);
    let failed = shared.failures.load(Ordering::SeqCst);
    let down = shared.bytes.received.load(Ordering::SeqCst);
    let up = shared.bytes.sent.load(Ordering::SeqCst);
    if json_output {
        eprintln!(
            "{}",
            json!({"peer": name, "connections": n, "failed": failed, "bytes_down": down, "bytes_up": up})
        );
    } else if n + failed == 0 {
        eprintln!("[agent-cli-x proxy via {name}: no connections; the command needed no network or ignores HTTP_PROXY/HTTPS_PROXY]");
    } else {
        let failed = match failed {
            0 => String::new(),
            f => format!(", {f} failed"),
        };
        eprintln!(
            "[agent-cli-x proxy via {name}: {n} connections{failed}, {} down, {} up]",
            size(down),
            size(up)
        );
    }
    Ok(exit_code(status))
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    status.code().unwrap_or(1)
}

fn size(n: u64) -> String {
    match n {
        0..=1023 => format!("{n}B"),
        1024..=1048575 => format!("{:.1}KB", n as f64 / 1024.0),
        _ => format!("{:.1}MB", n as f64 / 1048576.0),
    }
}

/// 启动命令前确认对端支持且允许代理，避免命令跑到一半才逐个连接失败。
fn preflight(client: &Client) -> Result<()> {
    let info = client.info(CALL)?;
    let name = &client.name;
    let supported = info["features"]
        .as_array()
        .is_some_and(|f| f.iter().any(|v| v == "net_proxy"));
    if !supported {
        return Err(AppError::new(
            "unsupported",
            format!(
                "peer {name} (agent-cli-x {}) has no proxy support; run `agent-cli-x upgrade {name}` first",
                info["version"].as_str().unwrap_or("?")
            ),
        ));
    }
    if info["net_proxy"] != Value::Bool(true) {
        return Err(AppError::denied(format!(
            "network proxy is disabled on {name}; its owner must set [policy] net_proxy = true in config.toml, then restart serve"
        )));
    }
    if info["caller_no_relay"] == Value::Bool(true) {
        return Err(AppError::denied(format!(
            "your token on {name} is --no-relay, so it can't be used as a network proxy; ask the user"
        )));
    }
    Ok(())
}

/// 本机地址始终直连；保留调用环境里已有的直连列表。
fn no_proxy(existing: Option<String>) -> String {
    let mut list: Vec<String> = existing
        .iter()
        .flat_map(|v| v.split(','))
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .collect();
    for local in ["localhost", "127.0.0.1", "::1"] {
        if !list.iter().any(|v| v == local) {
            list.push(local.to_string());
        }
    }
    list.join(",")
}

/// Windows 上 `npm`、`pnpm` 等是 `.cmd`，系统只会自动补 `.exe`，这里按 PATHEXT 查找。
#[cfg(windows)]
fn resolve_program(name: &str) -> PathBuf {
    if name.contains(['/', '\\']) {
        return name.into();
    }
    let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
    let has_ext = std::path::Path::new(name).extension().is_some();
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        if has_ext && dir.join(name).is_file() {
            return dir.join(name);
        }
        for ext in exts.split(';').filter(|e| !e.is_empty()) {
            let candidate = dir.join(format!("{name}{}", ext.to_ascii_lowercase()));
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    name.into()
}

#[cfg(not(windows))]
fn resolve_program(name: &str) -> PathBuf {
    name.into()
}

fn accept(shared: &Arc<Shared>, listener: TcpListener) {
    for local in listener.incoming() {
        let Ok(local) = local else {
            thread::sleep(Duration::from_millis(100));
            continue;
        };
        while shared.active.load(Ordering::SeqCst) >= MAX_ACTIVE {
            thread::sleep(Duration::from_millis(20));
        }
        shared.active.fetch_add(1, Ordering::SeqCst);
        let worker = Arc::clone(shared);
        let spawned = thread::Builder::new().spawn(move || {
            handle(&worker, local);
            worker.active.fetch_sub(1, Ordering::SeqCst);
        });
        if spawned.is_err() {
            shared.active.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

struct Head {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
}

impl Head {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

fn read_head<R: BufRead>(r: &mut R) -> Option<Head> {
    let mut line = || {
        let mut buf = Vec::new();
        r.by_ref().take(MAX_LINE).read_until(b'\n', &mut buf).ok()?;
        if !buf.ends_with(b"\n") {
            return None;
        }
        Some(String::from_utf8_lossy(&buf).trim_end().to_string())
    };
    let first = line()?;
    let mut parts = first.split_whitespace();
    let (method, target) = (parts.next()?.to_string(), parts.next()?.to_string());
    let mut headers = Vec::new();
    loop {
        let l = line()?;
        if l.is_empty() {
            return Some(Head {
                method,
                target,
                headers,
            });
        }
        if headers.len() >= MAX_HEADERS {
            return None;
        }
        let (k, v) = l.split_once(':')?;
        headers.push((k.trim().to_string(), v.trim().to_string()));
    }
}

/// `host:port`，IPv6 写作 `[::1]:443`；`default` 用于省略端口的普通 HTTP 请求。
fn split_authority(authority: &str, default: Option<u16>) -> Option<(String, u16)> {
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !p.contains(']') => (h, Some(p.parse::<u16>().ok()?)),
        _ => (authority, default),
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    (!host.is_empty()).then_some((host.to_string(), port?))
}

/// 把发给代理的绝对地址请求改写为发给目标服务器的请求。
/// 强制 `Connection: close`：同一连接上的下一个请求可能指向别的主机。
fn rewrite(head: &Head) -> Option<(String, u16, Vec<u8>)> {
    let rest = head.target.strip_prefix("http://")?;
    let at = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, path) = rest.split_at(at);
    let authority = authority.rsplit('@').next()?;
    let (host, port) = split_authority(authority, Some(80))?;
    let slash = if path.starts_with('/') { "" } else { "/" };
    let mut out = format!("{} {slash}{path} HTTP/1.1\r\n", head.method);
    if head.header("host").is_none() {
        out.push_str(&format!("Host: {authority}\r\n"));
    }
    for (k, v) in &head.headers {
        let hop = [
            "proxy-authorization",
            "proxy-connection",
            "connection",
            "keep-alive",
        ];
        if !hop.iter().any(|h| k.eq_ignore_ascii_case(h)) {
            out.push_str(&format!("{k}: {v}\r\n"));
        }
    }
    out.push_str("Connection: close\r\n\r\n");
    Some((host, port, out.into_bytes()))
}

fn reply(mut local: &TcpStream, status: &str, extra: &str, body: &str) {
    let _ = write!(
        local,
        "HTTP/1.1 {status}\r\n{extra}Content-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
}

fn handle(shared: &Shared, local: TcpStream) {
    let _ = local.set_read_timeout(Some(CALL));
    let _ = local.set_nodelay(true);
    let mut br = BufReader::new(&local);
    // 部分客户端先不带凭据请求，收到 407 后在同一连接上重试
    let mut challenged = 0;
    let head = loop {
        let Some(head) = read_head(&mut br) else {
            return;
        };
        if head.header("proxy-authorization") == Some(shared.auth.as_str()) {
            break head;
        }
        let has_body = head.header("content-length").is_some_and(|v| v != "0")
            || head.header("transfer-encoding").is_some();
        challenged += 1;
        let close = has_body || challenged >= 3;
        reply(
            &local,
            "407 Proxy Authentication Required",
            if close {
                "Proxy-Authenticate: Basic realm=\"agent-cli-x\"\r\nConnection: close\r\n"
            } else {
                "Proxy-Authenticate: Basic realm=\"agent-cli-x\"\r\n"
            },
            "",
        );
        if close {
            return;
        }
    };
    let connect = head.method.eq_ignore_ascii_case("CONNECT");
    let target = if connect {
        split_authority(&head.target, None).map(|(h, p)| (h, p, Vec::new()))
    } else {
        rewrite(&head)
    };
    let Some((host, port, mut to_peer)) = target else {
        reply(
            &local,
            "400 Bad Request",
            "Connection: close\r\n",
            "unsupported proxy request",
        );
        return;
    };
    let remote = match open(&shared.client, &host, port) {
        Ok(r) => r,
        Err(e) => {
            shared.failures.fetch_add(1, Ordering::SeqCst);
            report(shared, &format!("{host}:{port}: {e}"));
            let status = if e.code == "policy_denied" {
                "403 Forbidden"
            } else {
                "502 Bad Gateway"
            };
            reply(&local, status, "Connection: close\r\n", &e.to_string());
            return;
        }
    };
    shared.connections.fetch_add(1, Ordering::SeqCst);
    if connect
        && (&local)
            .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
            .is_err()
    {
        return;
    }
    to_peer.extend_from_slice(br.buffer());
    let early = remote.buffer().to_vec();
    let StreamOwned { conn, sock } = remote.into_inner();
    let _ = tunnel::pump(
        conn.into(),
        &sock,
        &local,
        &to_peer,
        &early,
        tunnel::LIMITS,
        &shared.bytes,
    );
}

fn open(client: &Client, host: &str, port: u16) -> Result<BufReader<super::Stream>> {
    let body = json!({"host": host, "port": port});
    let mut tries = 0;
    loop {
        match client.download("/v1/connect", &body, CALL) {
            Err(e) if e.code == "busy" && tries < BUSY_RETRIES => {
                tries += 1;
                thread::sleep(BUSY_WAIT);
            }
            other => return other,
        }
    }
}

fn report(shared: &Shared, message: &str) {
    let message: String = message.chars().take(MAX_REPORT_CHARS).collect();
    let mut seen = shared.reported.lock().unwrap_or_else(|e| e.into_inner());
    if seen.len() > MAX_REPORTS || seen.contains(&message) {
        return;
    }
    if seen.len() == MAX_REPORTS {
        eprintln!("[agent-cli-x proxy: further connection errors suppressed]");
    } else {
        eprintln!("[agent-cli-x proxy: {message}]");
    }
    seen.push(message);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(text: &str) -> Head {
        read_head(&mut text.as_bytes()).unwrap()
    }

    #[test]
    fn authority_forms() {
        let ok = |a, d| split_authority(a, d).unwrap();
        assert_eq!(ok("github.com:443", None), ("github.com".into(), 443));
        assert_eq!(ok("[2001:db8::1]:443", None), ("2001:db8::1".into(), 443));
        assert_eq!(ok("[2001:db8::1]", Some(80)), ("2001:db8::1".into(), 80));
        assert_eq!(ok("example.com", Some(80)), ("example.com".into(), 80));
        for bad in ["example.com", "host:99999", ":443"] {
            assert!(split_authority(bad, None).is_none(), "{bad}");
        }
    }

    #[test]
    fn plain_http_is_rewritten_without_proxy_headers() {
        let h = head("GET http://u:p@example.com:8080?q=1 HTTP/1.1\r\nHost: example.com:8080\r\nProxy-Authorization: Basic x\r\nProxy-Connection: keep-alive\r\nAccept: */*\r\n\r\n");
        let (host, port, bytes) = rewrite(&h).unwrap();
        assert_eq!((host.as_str(), port), ("example.com", 8080));
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "GET /?q=1 HTTP/1.1\r\nHost: example.com:8080\r\nAccept: */*\r\nConnection: close\r\n\r\n"
        );
        assert!(rewrite(&head("GET ftp://x/ HTTP/1.1\r\n\r\n")).is_none());
    }

    /// 没有正确凭据的本机连接拿不到隧道（这里的对端地址无人监听，也证明没有发起连接）。
    #[test]
    fn requests_without_the_password_are_challenged_then_dropped() {
        let peer = toml::from_str(
            "addr = \"127.0.0.1:1\"
token = \"t\"
fingerprint = \"f\"",
        )
        .unwrap();
        let shared = Shared::new(Client::from_peer("b", peer), "Basic right".into());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut app = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (local, _) = listener.accept().unwrap();
        let request = "CONNECT example.com:443 HTTP/1.1
Proxy-Authorization: Basic wrong

";
        app.write_all(request.repeat(3).as_bytes()).unwrap();
        handle(&shared, local);
        let mut reply = String::new();
        app.read_to_string(&mut reply).unwrap();
        assert_eq!(
            reply.matches("407 Proxy Authentication Required").count(),
            3,
            "{reply}"
        );
        assert!(reply.contains("Connection: close"), "{reply}");
        assert_eq!(shared.connections.load(Ordering::SeqCst), 0);
        assert_eq!(shared.failures.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn no_proxy_keeps_existing_entries() {
        assert_eq!(no_proxy(None), "localhost,127.0.0.1,::1");
        assert_eq!(
            no_proxy(Some("corp.local, localhost".into())),
            "corp.local,localhost,127.0.0.1,::1"
        );
    }
}
