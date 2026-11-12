//! 客户端：短连接 HTTPS（证书指纹固定），一次调用一个请求。

pub mod admin;
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
        .map_err(|e| AppError::connect(format!("invalid address {addr}: {e}")))?
        .next()
        .ok_or_else(|| AppError::connect(format!("cannot resolve address {addr}")))?;
    let tcp = TcpStream::connect_timeout(&sock_addr, CONNECT_TIMEOUT).map_err(|e| {
        AppError::connect(format!(
            "cannot connect to {addr}: {e} (check that `agent-cli-x serve` is running on the peer and the firewall allows it)"
        ))
    })?;
    let _ = tcp.set_read_timeout(Some(read_timeout));
    let _ = tcp.set_write_timeout(Some(Duration::from_secs(60)));
    let _ = tcp.set_nodelay(true);
    let (cfg, verifier) = tls::client_config(pin)?;
    let name = ServerName::try_from("agent-cli-x").expect("static name is valid");
    let conn = ClientConnection::new(cfg, name).map_err(|e| AppError::internal(e.to_string()))?;
    let mut s = StreamOwned::new(conn, tcp);
    while s.conn.is_handshaking() {
        if let Err(e) = s.conn.complete_io(&mut s.sock) {
            let msg = e.to_string();
            if msg.contains(tls::MISMATCH_TAG) {
                let seen = verifier.seen.lock().unwrap().clone().unwrap_or_default();
                return Err(AppError::fingerprint(format!(
                    "{addr} certificate fingerprint differs from the registered one (now {seen}). The peer may have re-run init, or it may be impersonated; \
verify with `agent-cli-x fingerprint` on the peer, then re-register with `agent-cli-x peer add`"
                )));
            }
            return Err(AppError::connect(handshake_error(addr, &e)));
        }
    }
    let fp = verifier.seen.lock().unwrap().clone().unwrap_or_default();
    Ok((s, fp))
}

/// TCP 已连上但 TLS 握手失败：按错误类型说明可能原因与下一步。
fn handshake_error(addr: &str, e: &std::io::Error) -> String {
    let base = format!("TLS handshake with {addr} failed: {e}");
    let hint = match e.kind() {
        ErrorKind::UnexpectedEof | ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted => {
            "TCP connected but the other side closed before finishing the handshake: this port may not be agent-cli-x serve (e.g. a port forward/proxy remains after the backend serve exited)"
        }
        ErrorKind::WouldBlock | ErrorKind::TimedOut => {
            "TCP connected but no TLS response: another program may hold the port, or the peer serve is stuck"
        }
        ErrorKind::InvalidData => "the reply is not TLS: this port is probably used by another program",
        _ => return base,
    };
    format!(
        "{base}. {hint}. Check the latest log in ~/.agent-cli-x/serve-logs/ on the peer (serve states the reason for config errors), or run `agent-cli-x doctor` there"
    )
}

/// JSON 响应以断连结束。提前拒绝请求时，未读的上传数据可能使 TCP 以 RST 结束；
/// 仅当已收到完整 JSON 时保留响应，残缺响应与超时仍按连接错误返回。
fn read_all<R: Read>(r: &mut R, out: &mut Vec<u8>) -> Result<()> {
    match r.read_to_end(out) {
        Ok(_) => Ok(()),
        Err(e)
            if matches!(
                e.kind(),
                ErrorKind::UnexpectedEof
                    | ErrorKind::ConnectionReset
                    | ErrorKind::ConnectionAborted
            ) && !out.is_empty()
                && serde_json::from_slice::<Value>(out).is_ok() =>
        {
            Ok(())
        }
        Err(e) => Err(map_io(e)),
    }
}

/// 写入失败后放弃待发送的 TLS 记录，让读取响应时不再重试已失败的请求体。
/// 双向 TLS 的读写序列相互独立；对端可能已发来能说明失败原因的 HTTP 响应。
fn discard_pending_writes(s: &mut Stream) {
    let mut sink = std::io::sink();
    while s.conn.wants_write() {
        if !matches!(s.conn.write_tls(&mut sink), Ok(n) if n > 0) {
            break;
        }
    }
}

fn map_io(e: std::io::Error) -> AppError {
    match e.kind() {
        ErrorKind::WouldBlock | ErrorKind::TimedOut => AppError::new(
            "timeout",
            format!("timed out waiting for peer response: {e}"),
        ),
        _ => AppError::connect(format!("connection lost: {e}")),
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
                "`{name}`: this command takes one peer at a time (multiple peers only for exec / info / upgrade / restart)"
            )));
        }
        let peers = config::load_peers(&config::home_dir())?;
        let peer = peers.peers.get(name).cloned().ok_or_else(|| {
            let known: Vec<_> = peers.peers.keys().cloned().collect();
            AppError::config(format!(
                "unknown peer `{name}` (registered: {}). Run `agent-cli-x peer add` first",
                if known.is_empty() {
                    "none".to_string()
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

    pub fn addr(&self) -> &str {
        &self.peer.addr
    }

    /// 同一对端（同一 token 与证书指纹）换一个地址。
    pub fn with_addr(&self, addr: &str) -> Self {
        let mut peer = self.peer.clone();
        peer.addr = addr.to_string();
        Self::from_peer(&self.name, peer)
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
                "[agent-cli-x: version mismatch: local {}, peer {} is {v}. Run `agent-cli-x upgrade {}` to align]",
                http::VERSION,
                self.name,
                self.name
            );
        }
    }

    /// 把服务端错误还原为 AppError；对端不认识接口且版本不同时，提示升级。
    fn remote_error(&self, v: &Value, status: u16) -> AppError {
        let mut e = http::remote_error(v, status);
        if e.code == "not_found"
            && (e.message.starts_with("未知接口") || e.message.starts_with("unknown endpoint"))
        {
            let rv = self
                .remote_version
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .unwrap_or_else(|| "?".into());
            if rv != http::VERSION {
                e.message.push_str(&format!(
                    " (peer version {rv} is older and does not support this; local {}. Upgrade the peer first)",
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
        let sent = http::write_request(&mut s, path, &self.peer.token, &bytes).map_err(map_io);
        if sent.is_err() {
            discard_pending_writes(&mut s);
        }
        let mut br = BufReader::new(s);
        let head = match http::read_response_head(&mut br) {
            Ok(head) => head,
            Err(e) => return Err(sent.err().unwrap_or(e)),
        };
        self.note_version(&head);
        // 拒绝响应比传输失败更准确；成功响应不能掩盖未发送完整的请求。
        if head.status == 200 {
            sent?;
        }
        Ok((head.status, br))
    }

    pub fn call(&self, path: &str, body: &Value, timeout: Duration) -> Result<Value> {
        let (status, mut br) = self.send(path, body, timeout)?;
        let mut data = Vec::new();
        read_all(&mut br, &mut data)?;
        let v: Value = serde_json::from_slice(&data)
            .map_err(|e| AppError::internal(format!("invalid JSON response: {e}")))?;
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
                .map_err(|e| AppError::internal(format!("invalid stream data: {e}")))?;
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
        let body_res = write_body(&mut s).and_then(|_| s.flush().map_err(map_io));
        if body_res.is_err() {
            discard_pending_writes(&mut s);
        }
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
            .map_err(|e| AppError::internal(format!("invalid JSON response: {e}")))?;
        if status != 200 {
            return Err(self.remote_error(&v, status));
        }
        body_res?;
        Ok(v)
    }

    /// 长度未知的帧流上传（cp 中转用，对端需支持 put_stream）。
    /// 发送结束后关闭写方向，供服务端在提前拒绝时结束 drain；读取响应方向保持可用。
    pub fn upload_frames(
        &self,
        path: &str,
        timeout: Duration,
        write_body: impl FnOnce(&mut Stream) -> Result<()>,
    ) -> Result<Value> {
        let (mut s, _) = connect(&self.peer.addr, Some(&self.peer.fingerprint), timeout)?;
        http::write_stream_request_head(&mut s, path, &self.peer.token).map_err(map_io)?;
        let written = write_body(&mut s);
        // 即使源端中断，也先尝试写完已经排队的 TLS 记录，保证后续 close_notify 的序号连续。
        let flushed = s.flush().map_err(map_io);
        if flushed.is_ok() {
            s.conn.send_close_notify();
            if s.flush().is_err() {
                discard_pending_writes(&mut s);
            }
        } else {
            // 丢弃过 TLS 记录后不能再发 close_notify：对端无法解密有序号缺口的记录。
            discard_pending_writes(&mut s);
        }
        let body_res = written.and(flushed);
        // 小流可能已全部发完才收到拒绝；服务端此时正 drain 到 EOF，必须始终结束写方向。
        let _ = s.sock.shutdown(std::net::Shutdown::Write);
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
            (Ok(()), _) if v.is_null() => Err(AppError::internal("invalid JSON response")),
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

#[cfg(test)]
mod response_tests {
    use super::*;
    use std::io::{self, Cursor};

    struct InterruptedResponse {
        body: Cursor<Vec<u8>>,
        error: ErrorKind,
    }

    impl Read for InterruptedResponse {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.body.read(buf)? {
                0 => Err(io::Error::from(self.error)),
                n => Ok(n),
            }
        }
    }

    fn read_response(body: &[u8], error: ErrorKind) -> Result<Vec<u8>> {
        let mut source = InterruptedResponse {
            body: Cursor::new(body.to_vec()),
            error,
        };
        let mut data = Vec::new();
        read_all(&mut source, &mut data)?;
        Ok(data)
    }

    #[test]
    fn complete_json_survives_connection_reset() {
        let body = br#"{"error":{"code":"auth","message":"invalid token"}}"#;
        for error in [
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted,
            ErrorKind::UnexpectedEof,
        ] {
            assert_eq!(read_response(body, error).unwrap(), body);
        }
    }

    #[test]
    fn incomplete_responses_and_timeouts_remain_transport_errors() {
        for body in [b"".as_slice(), br#"{"error":"#] {
            assert_eq!(
                read_response(body, ErrorKind::ConnectionReset)
                    .unwrap_err()
                    .code,
                "connect"
            );
        }
        assert_eq!(
            read_response(b"{}", ErrorKind::TimedOut).unwrap_err().code,
            "timeout"
        );
    }
}
