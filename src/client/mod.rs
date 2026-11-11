//! 客户端：短连接 HTTPS（证书指纹固定），一次调用一个请求。

pub mod output;
pub mod transfer;

use crate::config::{self, Peer};
use crate::error::{AppError, Result};
use crate::{http, tls};
use rustls::pki_types::ServerName;
use rustls::{ClientConnection, StreamOwned};
use serde_json::Value;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

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
    peer: Peer,
}

impl Client {
    pub fn load(name: &str) -> Result<Self> {
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
        Ok(Self { peer })
    }

    pub fn from_peer(peer: Peer) -> Self {
        Self { peer }
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
        let status = http::read_response_head(&mut br)?;
        Ok((status, br))
    }

    pub fn call(&self, path: &str, body: &Value, timeout: Duration) -> Result<Value> {
        let (status, mut br) = self.send(path, body, timeout)?;
        let mut data = Vec::new();
        read_all(&mut br, &mut data)?;
        let v: Value = serde_json::from_slice(&data)
            .map_err(|e| AppError::internal(format!("响应不是有效 JSON: {e}")))?;
        if status != 200 {
            return Err(http::remote_error(&v, status));
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
            return Err(http::remote_error(&v, status));
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
            Ok(st) => st,
            Err(e) => return Err(body_res.err().unwrap_or(e)),
        };
        let mut data = Vec::new();
        read_all(&mut br, &mut data)?;
        let v: Value = serde_json::from_slice(&data)
            .map_err(|e| AppError::internal(format!("响应不是有效 JSON: {e}")))?;
        if status != 200 {
            return Err(http::remote_error(&v, status));
        }
        body_res?;
        Ok(v)
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
            return Err(http::remote_error(&v, status));
        }
        Ok(br)
    }
}

/// 只做握手拿指纹，不发送任何凭据。
pub fn discover_fingerprint(addr: &str) -> Result<String> {
    let (mut s, fp) = connect(addr, None, CONNECT_TIMEOUT)?;
    s.conn.send_close_notify();
    let _ = s.flush();
    Ok(fp)
}
