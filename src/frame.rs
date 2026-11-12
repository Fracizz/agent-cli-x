//! put / get 的传输帧：一行 JSON 头 + 原始字节，依次排列，无需额外依赖。
//!
//! ```text
//! {"kind":"d","name":"dist"}            ← 元信息（get 响应 / put 请求的第一行）
//! {"p":"","k":"d"}                       ← 目录（p 为相对源根目录的路径，"" 表示根本身）
//! {"p":"a/b.txt","k":"f","n":12}         ← 文件头，后面紧跟 12 字节内容
//! {"end":true,"files":1,"bytes":12}      ← 结束
//! ```

use crate::error::{AppError, Result};
use serde_json::Value;
use std::io::{self, BufRead, Read, Write};

const MAX_HEADER: u64 = 64 * 1024;

pub fn header_bytes(v: &Value) -> Vec<u8> {
    let mut line = serde_json::to_vec(v).unwrap_or_default();
    line.push(b'\n');
    line
}

pub fn write_header<W: Write>(w: &mut W, v: &Value) -> io::Result<()> {
    w.write_all(&header_bytes(v))
}

pub fn read_header<R: BufRead>(r: &mut R) -> Result<Value> {
    let mut buf = Vec::new();
    let n = r.by_ref().take(MAX_HEADER).read_until(b'\n', &mut buf)?;
    if n == 0 || !buf.ends_with(b"\n") {
        return Err(AppError::io("transfer stream ended unexpectedly"));
    }
    serde_json::from_slice(&buf)
        .map_err(|e| AppError::bad_request(format!("invalid transfer frame: {e}")))
}

/// 恰好复制 n 字节；源不足 n 字节视为传输中断。
pub fn copy_exact<R: Read, W: Write>(r: &mut R, w: &mut W, n: u64) -> Result<()> {
    let copied = io::copy(&mut r.by_ref().take(n), w)?;
    if copied != n {
        return Err(AppError::io(format!(
            "transfer interrupted: expected {n} bytes, got {copied}"
        )));
    }
    Ok(())
}

/// 校验帧里的相对路径：`/` 分隔，不得为绝对路径、不得含 `..` 或 `:`。返回各段。
pub fn safe_rel(p: &str) -> Result<Vec<String>> {
    let s = p.replace('\\', "/");
    let b = s.as_bytes();
    if s.starts_with('/') || (b.len() >= 2 && b[1] == b':') {
        return Err(AppError::denied(format!(
            "transfer path `{p}` must not be absolute"
        )));
    }
    let mut out = Vec::new();
    for part in s.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                return Err(AppError::denied(format!(
                    "transfer path `{p}` contains `..`"
                )))
            }
            x if x.contains(':') || x.contains('\0') => {
                return Err(AppError::denied(format!(
                    "transfer path `{p}` contains illegal characters"
                )))
            }
            x => out.push(x.to_string()),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;

    #[test]
    fn frame_roundtrip() {
        let mut buf = Vec::new();
        write_header(&mut buf, &json!({"p": "a.txt", "k": "f", "n": 3})).unwrap();
        buf.extend_from_slice(b"abc");
        write_header(&mut buf, &json!({"end": true})).unwrap();
        let mut r = Cursor::new(buf);
        let h = read_header(&mut r).unwrap();
        let mut out = Vec::new();
        copy_exact(&mut r, &mut out, h["n"].as_u64().unwrap()).unwrap();
        assert_eq!(out, b"abc");
        assert_eq!(read_header(&mut r).unwrap()["end"], true);
    }

    #[test]
    fn rel_rules() {
        assert_eq!(safe_rel("a/b\\c").unwrap(), vec!["a", "b", "c"]);
        assert!(safe_rel("").unwrap().is_empty());
        for bad in ["../x", "/etc", "C:/x", "a/../../b", "a:b"] {
            assert!(safe_rel(bad).is_err(), "{bad}");
        }
    }
}
