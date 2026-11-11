//! 极简 HTTP/1.1：每个连接只处理一个请求，响应以关闭连接结束（Connection: close）。
//! 两端都是 agent-cli-x，因此只实现自己需要的子集，同时保持可用 curl 调试。

use crate::error::{AppError, Result};
use serde_json::{json, Value};
use std::io::{BufRead, Read, Write};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_LINE: usize = 8 * 1024;
const MAX_HEADERS: usize = 64;

pub struct Request {
    #[allow(dead_code)]
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub content_length: u64,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

fn read_line<R: BufRead>(r: &mut R) -> Result<String> {
    let mut buf = Vec::new();
    let n = r
        .by_ref()
        .take(MAX_LINE as u64 + 1)
        .read_until(b'\n', &mut buf)?;
    if n == 0 {
        return Err(AppError::io("连接已关闭"));
    }
    if !buf.ends_with(b"\n") {
        return Err(AppError::bad_request("HTTP 行过长或不完整"));
    }
    while matches!(buf.last(), Some(b'\n' | b'\r')) {
        buf.pop();
    }
    String::from_utf8(buf).map_err(|_| AppError::bad_request("HTTP 头不是 UTF-8"))
}

fn read_headers<R: BufRead>(r: &mut R) -> Result<Vec<(String, String)>> {
    let mut headers = Vec::new();
    loop {
        let line = read_line(r)?;
        if line.is_empty() {
            return Ok(headers);
        }
        if headers.len() >= MAX_HEADERS {
            return Err(AppError::bad_request("HTTP 头过多"));
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
}

/// 只读取请求行与请求头；请求体由调用方按需读取（put 需要流式读取大文件）。
pub fn read_request_head<R: BufRead>(r: &mut R) -> Result<Request> {
    let line = read_line(r)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    if method.is_empty() || !path.starts_with('/') {
        return Err(AppError::bad_request("无效的 HTTP 请求行"));
    }
    let headers = read_headers(r)?;
    let content_length = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .map(|(_, v)| v.parse::<u64>())
        .transpose()
        .map_err(|_| AppError::bad_request("Content-Length 无效"))?
        .unwrap_or(0);
    Ok(Request {
        method,
        path,
        headers,
        content_length,
        body: Vec::new(),
    })
}

pub fn read_body<R: Read>(r: &mut R, len: u64, max_body: usize) -> Result<Vec<u8>> {
    if len > max_body as u64 {
        return Err(AppError::bad_request(format!(
            "请求体过大（{len} 字节，上限 {max_body}）"
        )));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body)?;
    Ok(body)
}

#[cfg(test)]
pub fn read_request<R: BufRead>(r: &mut R, max_body: usize) -> Result<Request> {
    let mut req = read_request_head(r)?;
    req.body = read_body(r, req.content_length, max_body)?;
    Ok(req)
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        408 => "Request Timeout",
        409 => "Conflict",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    }
}

pub fn write_head<W: Write>(w: &mut W, status: u16, content_type: &str) -> std::io::Result<()> {
    write!(
        w,
        "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nConnection: close\r\nX-Agent-Cli-Version: {VERSION}\r\n\r\n",
        reason(status)
    )
}

pub fn write_json<W: Write>(w: &mut W, status: u16, v: &Value) -> std::io::Result<()> {
    write_head(w, status, "application/json; charset=utf-8")?;
    w.write_all(&serde_json::to_vec(v).unwrap_or_default())?;
    w.flush()
}

pub fn error_json(e: &AppError) -> Value {
    json!({"error": {"code": e.code, "message": e.message}})
}

pub fn write_error<W: Write>(w: &mut W, e: &AppError) -> std::io::Result<()> {
    write_json(w, e.http_status(), &error_json(e))
}

pub fn write_request<W: Write>(
    w: &mut W,
    path: &str,
    token: &str,
    body: &[u8],
) -> std::io::Result<()> {
    write!(
        w,
        "POST {path} HTTP/1.1\r\nHost: agent-cli-x\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nX-Agent-Cli-Version: {VERSION}\r\n\r\n",
        body.len()
    )?;
    w.write_all(body)?;
    w.flush()
}

pub fn write_request_head<W: Write>(
    w: &mut W,
    path: &str,
    token: &str,
    len: u64,
    content_type: &str,
) -> std::io::Result<()> {
    write!(
        w,
        "POST {path} HTTP/1.1\r\nHost: agent-cli-x\r\nAuthorization: Bearer {token}\r\nContent-Type: {content_type}\r\nContent-Length: {len}\r\nX-Agent-Cli-Version: {VERSION}\r\n\r\n"
    )
}

pub fn read_response_head<R: BufRead>(r: &mut R) -> Result<u16> {
    let line = read_line(r)?;
    let status = line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| AppError::internal(format!("无效的响应行: {line}")))?;
    read_headers(r)?;
    Ok(status)
}

/// 把服务端返回的错误 JSON 还原为 AppError。
pub fn remote_error(v: &Value, status: u16) -> AppError {
    let code = v["error"]["code"].as_str().unwrap_or("internal");
    let msg = v["error"]["message"]
        .as_str()
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("服务端返回 HTTP {status}"));
    AppError::new(code, msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn request_roundtrip() {
        let mut buf = Vec::new();
        write_request(&mut buf, "/v1/info", "tok", b"{\"a\":1}").unwrap();
        let req = read_request(&mut Cursor::new(buf), 1024).unwrap();
        assert_eq!(req.path, "/v1/info");
        assert_eq!(req.header("authorization"), Some("Bearer tok"));
        assert_eq!(req.body, b"{\"a\":1}");
    }

    #[test]
    fn body_limit() {
        let mut buf = Vec::new();
        write_request(&mut buf, "/x", "t", &[b'a'; 100]).unwrap();
        assert!(read_request(&mut Cursor::new(buf), 10).is_err());
    }
}
