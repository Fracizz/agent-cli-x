//! put / get 的客户端部分：本地遍历与落盘（格式见 frame.rs）。

use super::Client;
use crate::error::{AppError, Result};
use crate::frame;
use crate::http;
use crate::server::transfer::{exclude_set, excluded};
use serde_json::{json, Value};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(300);

#[cfg(unix)]
fn is_exec(m: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    m.permissions().mode() & 0o111 != 0
}
#[cfg(not(unix))]
fn is_exec(_m: &fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn set_exec(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(m) = fs::metadata(p) {
        let mut perm = m.permissions();
        perm.set_mode(perm.mode() | 0o111);
        let _ = fs::set_permissions(p, perm);
    }
}
#[cfg(not(unix))]
fn set_exec(_p: &Path) {}

struct Item {
    header: Vec<u8>,
    file: Option<(PathBuf, u64)>,
}

pub fn put(client: &Client, local: &Path, dest: &str, exclude: &[String]) -> Result<Value> {
    let canon = dunce::canonicalize(local)
        .map_err(|e| AppError::not_found(format!("invalid local path {}: {e}", local.display())))?;
    let name = canon
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .ok_or_else(|| {
            AppError::bad_request("cannot determine source name (cannot upload a drive root)")
        })?;
    let ex = exclude_set(exclude)?;
    let mut items = Vec::new();
    let mut skipped_links = 0usize;
    let kind = if canon.is_file() {
        let m = fs::metadata(&canon)?;
        items.push(Item {
            header: frame::header_bytes(
                &json!({"p": "", "k": "f", "n": m.len(), "x": is_exec(&m)}),
            ),
            file: Some((canon.clone(), m.len())),
        });
        "f"
    } else {
        items.push(Item {
            header: frame::header_bytes(&json!({"p": "", "k": "d"})),
            file: None,
        });
        let mut stack = vec![(canon.clone(), String::new())];
        while let Some((dir, rel)) = stack.pop() {
            let mut children: Vec<_> = fs::read_dir(&dir)?.flatten().collect();
            children.sort_by_key(|e| e.file_name());
            for c in children {
                let Ok(ft) = c.file_type() else { continue };
                let crel = if rel.is_empty() {
                    c.file_name().to_string_lossy().to_string()
                } else {
                    format!("{rel}/{}", c.file_name().to_string_lossy())
                };
                if ft.is_symlink() {
                    skipped_links += 1;
                    continue;
                }
                if excluded(&ex, &crel) {
                    continue;
                }
                if ft.is_dir() {
                    items.push(Item {
                        header: frame::header_bytes(&json!({"p": crel, "k": "d"})),
                        file: None,
                    });
                    stack.push((c.path(), crel));
                } else if ft.is_file() {
                    let m = c.metadata()?;
                    items.push(Item {
                        header: frame::header_bytes(
                            &json!({"p": crel, "k": "f", "n": m.len(), "x": is_exec(&m)}),
                        ),
                        file: Some((c.path(), m.len())),
                    });
                }
            }
        }
        "d"
    };
    let meta = frame::header_bytes(&json!({"dest": dest, "kind": kind, "name": name}));
    let end = frame::header_bytes(&json!({"end": true}));
    let len = meta.len() as u64
        + end.len() as u64
        + items
            .iter()
            .map(|i| i.header.len() as u64 + i.file.as_ref().map(|f| f.1).unwrap_or(0))
            .sum::<u64>();
    let mut v = client.upload("/v1/fs/put", len, TIMEOUT, |s| {
        s.write_all(&meta)?;
        for it in &items {
            s.write_all(&it.header)?;
            if let Some((p, n)) = &it.file {
                let mut f = File::open(p)?;
                frame::copy_exact(&mut f, s, *n).map_err(|e| {
                    AppError::io(format!(
                        "{} was modified during upload: {}",
                        p.display(),
                        e.message
                    ))
                })?;
            }
        }
        s.write_all(&end)?;
        Ok(())
    })?;
    v["skipped_symlinks"] = json!(skipped_links);
    Ok(v)
}

pub fn get(client: &Client, src: &str, local: &Path, exclude: &[String]) -> Result<Value> {
    let mut br = client.download(
        "/v1/fs/get",
        &json!({"src": src, "exclude": exclude}),
        TIMEOUT,
    )?;
    let meta = frame::read_header(&mut br)?;
    if meta.get("error").is_some() {
        return Err(http::remote_error(&meta, 500));
    }
    let kind = meta["kind"].as_str().unwrap_or_default().to_string();
    let parts = frame::safe_rel(meta["name"].as_str().unwrap_or_default())?;
    if parts.len() != 1 {
        return Err(AppError::bad_request("invalid name returned by peer"));
    }
    let base = if local.is_dir() {
        local.join(&parts[0])
    } else {
        local.to_path_buf()
    };
    if kind == "f" && base.is_dir() {
        return Err(AppError::conflict(format!(
            "local {} is a directory",
            base.display()
        )));
    }
    if kind == "d" && base.is_file() {
        return Err(AppError::conflict(format!(
            "local {} is a file",
            base.display()
        )));
    }
    let (mut files, mut bytes) = (0u64, 0u64);
    loop {
        let h = frame::read_header(&mut br)?;
        if h.get("error").is_some() {
            let mut e = http::remote_error(&h, 500);
            e.message
                .push_str(" (partially downloaded files kept locally)");
            return Err(e);
        }
        if h["end"] == true {
            break;
        }
        let rel = frame::safe_rel(h["p"].as_str().unwrap_or_default())?;
        let target = rel.iter().fold(base.clone(), |p, c| p.join(c));
        match h["k"].as_str() {
            Some("d") => fs::create_dir_all(&target)?,
            Some("f") => {
                let n = h["n"]
                    .as_u64()
                    .ok_or_else(|| AppError::bad_request("file frame missing size"))?;
                let parent = target
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| PathBuf::from("."));
                fs::create_dir_all(&parent)?;
                let fname = target
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                let tmp = parent.join(format!(
                    ".{fname}.agent-cli-x-{}.tmp",
                    crate::util::random_hex(4)
                ));
                let written = (|| -> Result<()> {
                    let mut f = File::create(&tmp)?;
                    frame::copy_exact(&mut br, &mut f, n)?;
                    f.sync_all()?;
                    Ok(())
                })();
                if let Err(e) = written {
                    let _ = fs::remove_file(&tmp);
                    return Err(e);
                }
                fs::rename(&tmp, &target).inspect_err(|_| {
                    let _ = fs::remove_file(&tmp);
                })?;
                if h["x"] == true {
                    set_exec(&target);
                }
                files += 1;
                bytes += n;
            }
            _ => return Err(AppError::bad_request("unknown entry type")),
        }
    }
    Ok(json!({"dest": base.to_string_lossy(), "files": files, "bytes": bytes}))
}

/// 对端到对端复制：从 `src` 下载的帧流直接转发给 `dst` 的 put，本机不落盘。
/// 目标端需支持 put_stream（0.0.4 起）；调用方负责事先确认。
pub fn relay(
    src: &Client,
    src_path: &str,
    dst: &Client,
    dest: &str,
    exclude: &[String],
    mut progress: impl FnMut(u64, u64),
) -> Result<Value> {
    let mut br = src.download(
        "/v1/fs/get",
        &json!({"src": src_path, "exclude": exclude}),
        TIMEOUT,
    )?;
    let meta = frame::read_header(&mut br)?;
    if meta.get("error").is_some() {
        return Err(http::remote_error(&meta, 500));
    }
    let kind = meta["kind"].as_str().unwrap_or_default().to_string();
    let name = meta["name"].as_str().unwrap_or_default().to_string();
    if frame::safe_rel(&name)?.len() != 1 {
        return Err(AppError::bad_request("invalid name returned by source"));
    }
    let (mut files, mut bytes) = (0u64, 0u64);
    // source error时记下原因：目标端随后也会因流被截断而报错，两者合并告诉用户
    let mut src_err: Option<AppError> = None;
    let res = dst.upload_frames("/v1/fs/put", TIMEOUT, |s| {
        frame::write_header(s, &json!({"dest": dest, "kind": kind, "name": name}))?;
        loop {
            let h = match frame::read_header(&mut br) {
                Ok(h) => h,
                Err(e) => {
                    src_err = Some(AppError::io(format!("source {}: {}", src.name, e.message)));
                    return Err(AppError::io("source interrupted"));
                }
            };
            if h.get("error").is_some() {
                let mut e = http::remote_error(&h, 500);
                e.message = format!("source {}: {}", src.name, e.message);
                src_err = Some(e);
                return Err(AppError::io("source error"));
            }
            if super::peer_replied(s) {
                return Err(AppError::io("destination ended receiving early"));
            }
            if h["end"] == true {
                frame::write_header(s, &json!({"end": true}))?;
                return Ok(());
            }
            frame::safe_rel(h["p"].as_str().unwrap_or_default())?;
            match h["k"].as_str() {
                Some("d") => frame::write_header(s, &json!({"p": h["p"], "k": "d"}))?,
                Some("f") => {
                    let n = h["n"]
                        .as_u64()
                        .ok_or_else(|| AppError::bad_request("file frame missing size"))?;
                    frame::write_header(
                        s,
                        &json!({"p": h["p"], "k": "f", "n": n, "x": h["x"] == true}),
                    )?;
                    // 逐块转发：区分是源端读不到，还是写往目标端失败
                    let mut left = n;
                    let mut buf = vec![0u8; 256 * 1024];
                    while left > 0 {
                        let want = left.min(buf.len() as u64) as usize;
                        let got = match br.read(&mut buf[..want]) {
                            Ok(0) | Err(_) => {
                                src_err = Some(AppError::io(format!(
                                    "source {} transfer interrupted: {left} bytes of the file missing",
                                    src.name
                                )));
                                return Err(AppError::io("source interrupted"));
                            }
                            Ok(k) => k,
                        };
                        if super::peer_replied(s) {
                            return Err(AppError::io("destination ended receiving early"));
                        }
                        s.write_all(&buf[..got])?;
                        left -= got as u64;
                    }
                    files += 1;
                    bytes += n;
                    progress(files, bytes);
                }
                _ => return Err(AppError::bad_request("unknown entry type")),
            }
        }
    });
    match (res, src_err) {
        (Ok(v), _) => Ok(v),
        (Err(dst_e), Some(mut e)) => {
            e.message
                .push_str(&format!("; destination {}: {}", dst.name, dst_e.message));
            Err(e)
        }
        (Err(e), None) => Err(e),
    }
}
