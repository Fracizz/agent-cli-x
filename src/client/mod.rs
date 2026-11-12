//! 客户端：短连接 HTTPS（证书指纹固定），一次调用一个请求。

pub mod multi;
pub mod output;
pub mod transfer;
pub mod upgrade;

use crate::config::{self, Peer};
use crate::error::{AppError, Result};
use crate::{http, tls};
use rustls::pki_types::ServerName;
use rustls::{ClientConnection, StreamOwned};
use serde_json::Value;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

/// 每个进程只提示一次版本不一致。
static VERSION_WARN: AtomicBool = AtomicBool::new(true);

/// 关闭版本不一致提示（upgrade / doctor 会自己报告版本）。
pub fn disable_version_warning() {
    VERSION_WARN.store(false, Ordering::SeqCst);
}

pub type Stream = StreamOwned<ClientConnection, TcpStream>;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// 建立 TLS 连接并完成握手，返回连接与对端证书指纹。
pub fn connect(addr: &str, pin: Option<&str>, read_timeout: Duration) -> Result<(Stream, String)> {
    let sock_addr = addr
        .to_socket_addrs()
        .map_err(|e| AppError::connect(format!("地址 {addr} 无效: {e}")))?
        .next()
        .ok_or_else(|| AppError::connect(format!("地址 {addr} 无法解析")))?;
    let tcp = TcpStream::connect_timeout(&sock_addr, CONNECT_TIMEOUT).map_err(|e| {
        AppError::connect(format!(
            "无法连接 {addr}: {e}（检查对端 `agent-cli-x serve` 是否运行、防火墙是否放行）"
        ))
    })?;
    let _ = tcp.set_read_timeout(Some(read_timeout));
    let _ = tcp.set_write_timeout(Some(Duration::from_secs(60)));
    let _ = tcp.set_nodelay(true);
    let (cfg, verifier) = tls::client_config(pin)?;
    let name = ServerName::try_from("agent-cli-x").expect("静态名称合法");
    let conn = ClientConnection::new(cfg, name).map_err(|e| AppError::internal(e.to_string()))?;
    let mut s = StreamOwned::new(conn, tcp);
    while s.conn.is_handshaking() {
        if let Err(e) = s.conn.complete_io(&mut s.sock) {
            let msg = e.to_string();
            if msg.contains(tls::MISMATCH_TAG) {
                let seen = verifier.seen.lock().unwrap().clone().unwrap_or_default();
                return Err(AppError::fingerprint(format!(
                    "{addr} 的证书指纹与登记的不一致（当前 {seen}）。可能是对端重新 init 过，也可能是被冒充；\
请在对端执行 `agent-cli-x fingerprint` 核对后，用 `agent-cli-x peer add` 重新登记"
                )));
            }
            return Err(AppError::connect(format!(
                "与 {addr} 的 TLS 握手失败: {msg}"
            )));
        }
    }
    let fp = verifier.seen.lock().unwrap().clone().unwrap_or_default();
    Ok((s, fp))
}

/// 读到 EOF；对端未发送 close_notify 导致的 UnexpectedEof 视为正常结束。
fn read_all<R: Read>(r: &mut R, out: &mut Vec<u8>) -> Result<()> {
    match r.read_to_end(out) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => Ok(()),
        Err(e) => Err(map_io(e)),
    }
}

fn map_io(e: std::io::Error) -> AppError {
    match e.kind() {
        ErrorKind::WouldBlock | ErrorKind::TimedOut => {
            AppError::new("timeout", format!("等待对端响应超时: {e}"))
        }
        _ => AppError::connect(format!("连接中断: {e}")),
    }
}

pub struct Client {
    pub name: String,
    peer: Peer,
    /// 最近一次响应里对端报告的版本。
    remote_version: Mutex<Option<String>>,
}

impl Client {
    pub fn load(name: &str) -> Result<Self> {
        if multi::is_multi(name) {
            return Err(AppError::bad_request(format!(
                "`{name}`：此命令一次只能操作一个对端（多个对端仅支持 exec / info / upgrade）"
            )));
        }
        let peers = config::load_peers(&config::home_dir())?;
        let peer = peers.peers.get(name).cloned().ok_or_else(|| {
            let known: Vec<_> = peers.peers.keys().cloned().collect();
            AppError::config(format!(
                "未知对端 `{name}`（已登记: {}）。请先执行 `agent-cli-x peer add`",
                if known.is_empty() {
                    "无".to_string()
                } else {
                    known.join(", ")
                }
            ))
        })?;
        Ok(Self::from_peer(name, peer))
    }

    pub fn from_peer(name: &str, peer: Peer) -> Self {
        Self {
            name: name.to_string(),
            peer,
            remote_version: Mutex::new(None),
        }
    }

    fn note_version(&self, head: &http::ResponseHead) {
        let Some(v) = head.header(http::VERSION_HEADER) else {
            return;
        };
        *self
            .remote_version
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(v.to_string());
        if v != http::VERSION && VERSION_WARN.swap(false, Ordering::SeqCst) {
            eprintln!(
                "[agent-cli-x: 版本不一致：本机 {}，对端 {} 为 {v}。建议执行 `agent-cli-x upgrade {}` 统一版本]",
                http::VERSION,
                self.name,
                self.name
            );
        }
    }

    /// 把服务端错误还原为 AppError；对端不认识接口且版本不同时，提示升级。
    fn remote_error(&self, v: &Value, status: u16) -> AppError {
        let mut e = http::remote_error(v, status);
        if e.code == "not_found" && e.message.starts_with("未知接口") {
            let rv = self
                .remote_version
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .unwrap_or_else(|| "?".into());
            if rv != http::VERSION {
                e.message.push_str(&format!(
                    "（对端版本 {rv} 较旧，不支持此操作；本机 {}。请先升级对端）",
                    http::VERSION
                ));
            }
        }
        e
    }

    fn send(
        &self,
        path: &str,
        body: &Value,
        timeout: Duration,
    ) -> Result<(u16, BufReader<Stream>)> {
        let (mut s, _) = connect(&self.peer.addr, Some(&self.peer.fingerprint), timeout)?;
        let bytes = serde_json::to_vec(body).unwrap_or_default();
        http::write_request(&mut s, path, &self.peer.token, &bytes).map_err(map_io)?;
        let mut br = BufReader::new(s);
        let head = http::read_response_head(&mut br)?;
        self.note_version(&head);
        Ok((head.status, br))
    }

    pub fn call(&self, path: &str, body: &Value, timeout: Duration) -> Result<Value> {
        let (status, mut br) = self.send(path, body, timeout)?;
        let mut data = Vec::new();
        read_all(&mut br, &mut data)?;
        let v: Value = serde_json::from_slice(&data)
            .map_err(|e| AppError::internal(format!("响应不是有效 JSON: {e}")))?;
        if status != 200 {
            return Err(self.remote_error(&v, status));
        }
        Ok(v)
    }

    /// NDJSON 流式接口：每行回调一次。
    pub fn stream(
        &self,
        path: &str,
        body: &Value,
        timeout: Duration,
        mut on_line: impl FnMut(Value) -> Result<()>,
    ) -> Result<()> {
        let (status, mut br) = self.send(path, body, timeout)?;
        if status != 200 {
            let mut data = Vec::new();
            read_all(&mut br, &mut data)?;
            let v: Value = serde_json::from_slice(&data).unwrap_or(Value::Null);
            return Err(self.remote_error(&v, status));
        }
        let mut line = Vec::new();
        loop {
            line.clear();
            match br.read_until(b'\n', &mut line) {
                Ok(0) => return Ok(()),
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(()),
                Err(e) => return Err(map_io(e)),
            }
            if line.iter().all(|b| b.is_ascii_whitespace()) {
                continue;
            }
            let v: Value = serde_json::from_slice(&line)
                .map_err(|e| AppError::internal(format!("流数据无效: {e}")))?;
            on_line(v)?;
        }
    }

    /// 流式上传：先发请求头，再由 `write_body` 写入恰好 `len` 字节的请求体。
    pub fn upload(
        &self,
        path: &str,
        len: u64,
        timeout: Duration,
        write_body: impl FnOnce(&mut Stream) -> Result<()>,
    ) -> Result<Value> {
        let (mut s, _) = connect(&self.peer.addr, Some(&self.peer.fingerprint), timeout)?;
        http::write_request_head(
            &mut s,
            path,
            &self.peer.token,
            len,
            "application/octet-stream",
        )
        .map_err(map_io)?;
        let body_res = write_body(&mut s);
        let _ = s.flush();
        // 服务端可能中途拒绝并关闭连接：即使写入失败，也优先读取它返回的错误
        let mut br = BufReader::new(s);
        let status = match http::read_response_head(&mut br) {
            Ok(head) => {
                self.note_version(&head);
                head.status
            }
            Err(e) => return Err(body_res.err().unwrap_or(e)),
        };
        let mut data = Vec::new();
        read_all(&mut br, &mut data)?;
        let v: Value = serde_json::from_slice(&data)
            .map_err(|e| AppError::internal(format!("响应不是有效 JSON: {e}")))?;
        if status != 200 {
            return Err(self.remote_error(&v, status));
        }
        body_res?;
        Ok(v)
    }

    /// 长度未知的帧流上传（cp 中转用，对端需支持 put_stream）。
    /// `write_body` 出错时（例如源端中断）主动结束发送，让服务端尽快回错误（含可撤销的 op_id）。
    pub fn upload_frames(
        &self,
        path: &str,
        timeout: Duration,
        write_body: impl FnOnce(&mut Stream) -> Result<()>,
    ) -> Result<Value> {
        let (mut s, _) = connect(&self.peer.addr, Some(&self.peer.fingerprint), timeout)?;
        http::write_stream_request_head(&mut s, path, &self.peer.token).map_err(map_io)?;
        let body_res = write_body(&mut s);
        let _ = s.flush();
        if body_res.is_err() {
            s.conn.send_close_notify();
            let _ = s.flush();
            let _ = s.sock.shutdown(std::net::Shutdown::Write);
        }
        let mut br = BufReader::new(s);
        let head = match http::read_response_head(&mut br) {
            Ok(h) => h,
            Err(e) => return Err(body_res.err().unwrap_or(e)),
        };
        self.note_version(&head);
        let mut data = Vec::new();
        read_all(&mut br, &mut data)?;
        let v: Value = serde_json::from_slice(&data).unwrap_or(Value::Null);
        match (body_res, head.status) {
            // 服务端已回错误（例如权限不足提前拒绝）：以服务端为准
            (_, status) if status != 200 => Err(self.remote_error(&v, status)),
            (Err(e), _) => Err(e),
            (Ok(()), _) if v.is_null() => Err(AppError::internal("响应不是有效 JSON")),
            (Ok(()), _) => Ok(v),
        }
    }

    /// 流式下载：返回定位在响应体开头的读取器。
    pub fn download(
        &self,
        path: &str,
        body: &Value,
        timeout: Duration,
    ) -> Result<BufReader<Stream>> {
        let (status, mut br) = self.send(path, body, timeout)?;
        if status != 200 {
            let mut data = Vec::new();
            read_all(&mut br, &mut data)?;
            let v: Value = serde_json::from_slice(&data).unwrap_or(Value::Null);
            return Err(self.remote_error(&v, status));
        }
        Ok(br)
    }
}

/// 上传过程中服务端是否已经发回应用数据或关闭了连接（不消费数据）。
/// 帧流上传成功时服务端只在读完结束帧后才响应，提前到达的应用数据必然是错误响应，
/// 调用方应停止发送、转去读取它。不能直接 peek socket：TLS 1.3 服务端握手后会主动
/// 发送会话票据，那不是响应；这里交给 rustls 解析后只看明文。
pub fn peer_replied(s: &mut Stream) -> bool {
    if s.sock.set_nonblocking(true).is_err() {
        return false;
    }
    let read = s.conn.read_tls(&mut s.sock);
    let _ = s.sock.set_nonblocking(false);
    match read {
        // 对端关闭了连接
        Ok(0) => true,
        Ok(_) => match s.conn.process_new_packets() {
            Ok(st) => st.plaintext_bytes_to_read() > 0 || st.peer_has_closed(),
            Err(_) => true,
        },
        Err(_) => false,
    }
}

/// 只做握手拿指纹，不发送任何凭据。
pub fn discover_fingerprint(addr: &str) -> Result<String> {
    let (mut s, fp) = connect(addr, None, CONNECT_TIMEOUT)?;
    s.conn.send_close_notify();
    let _ = s.flush();
    Ok(fp)
}
