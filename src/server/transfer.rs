//! put / get：文件或目录传输（格式见 frame.rs）。
//! 语义同 `cp -r`：目标是已存在目录时，源放到它下面；否则目标就是源的新路径。

use super::backup::Op;
use super::Ctx;
use crate::error::{AppError, Result};
use crate::{frame, http, util};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::Deserialize;
use serde_json::{json, Value};
use std::fs::{self, File};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};

fn join_rel(a: &str, b: &str) -> String {
    match (a.is_empty(), b.is_empty()) {
        (true, _) => b.to_string(),
        (_, true) => a.to_string(),
        _ => format!("{a}/{b}"),
    }
}

/// 名称必须是单个合法路径段。
fn single_name(v: &Value) -> Result<String> {
    let name = v.as_str().unwrap_or_default();
    let parts = frame::safe_rel(name)?;
    if parts.len() != 1 {
        return Err(AppError::bad_request(format!("名称 `{name}` 无效")));
    }
    Ok(parts[0].clone())
}

pub fn exclude_set(globs: &[String]) -> Result<GlobSet> {
    let mut b = GlobSetBuilder::new();
    for g in globs {
        let glob = GlobBuilder::new(g)
            .literal_separator(false)
            .case_insensitive(cfg!(windows))
            .build()
            .map_err(|e| AppError::bad_request(format!("--exclude `{g}` 无效: {e}")))?;
        b.add(glob);
    }
    b.build().map_err(|e| AppError::bad_request(e.to_string()))
}

/// 排除规则匹配相对路径本身或其中任一路径段（`node_modules` 可排除任意层级的该目录）。
pub fn excluded(set: &GlobSet, rel: &str) -> bool {
    !set.is_empty() && (set.is_match(rel) || rel.split('/').any(|seg| set.is_match(seg)))
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

// ---------------- put ----------------

pub fn put<S: Read + Write>(ctx: &Ctx, len: u64, conn: &mut BufReader<S>) {
    let mut op: Option<Op> = None;
    let res = put_inner(ctx, len, conn, &mut op);
    let op_id = op.as_ref().filter(|o| !o.is_empty()).map(|o| o.id.clone());
    if let Some(o) = op {
        o.finish();
    }
    let w = conn.get_mut();
    match res {
        Ok(mut v) => {
            v["op_id"] = json!(op_id);
            ctx.st
                .audit
                .log(&ctx.ip, &ctx.caller, "put", true, v.clone());
            let _ = http::write_json(w, 200, &v);
        }
        Err(mut e) => {
            if let Some(id) = &op_id {
                e.message
                    .push_str(&format!("（已写入的部分可用 rollback {id} 撤销）"));
            }
            ctx.st.audit.log(
                &ctx.ip,
                &ctx.caller,
                "put",
                false,
                json!({"error": e.to_string()}),
            );
            let _ = http::write_error(w, &e);
        }
    }
}

fn put_inner<S: Read>(
    ctx: &Ctx,
    len: u64,
    conn: &mut BufReader<S>,
    op: &mut Option<Op>,
) -> Result<Value> {
    let max = ctx.st.cfg.policy.max_transfer_mb * 1024 * 1024;
    if len > max + 1024 * 1024 {
        return Err(AppError::bad_request(format!(
            "传输量超过 max_transfer_mb（{} MB）",
            ctx.st.cfg.policy.max_transfer_mb
        )));
    }
    let mut body = conn.by_ref().take(len);
    let meta = frame::read_header(&mut body)?;
    let dest = meta["dest"].as_str().unwrap_or_default().to_string();
    let kind = meta["kind"].as_str().unwrap_or_default().to_string();
    let name = single_name(&meta["name"])?;
    let dest_r = ctx.st.ws.resolve(&dest)?;
    let base_rel = if dest_r.exists && dest_r.abs.is_dir() {
        join_rel(&dest_r.rel, &name)
    } else {
        dest_r.rel.clone()
    };
    if base_rel.is_empty() {
        return Err(AppError::bad_request("不能覆盖工作区根目录本身"));
    }
    if kind != "f" && kind != "d" {
        return Err(AppError::bad_request("未知的源类型"));
    }
    let ws = dest_r.ws.clone();
    *op = Some(Op::new(&ctx.st.home)?);
    let (mut files, mut dirs, mut bytes) = (0u64, 0u64, 0u64);
    loop {
        let h = frame::read_header(&mut body)?;
        if h["end"] == true {
            break;
        }
        let parts = frame::safe_rel(h["p"].as_str().unwrap_or_default())?;
        let r = ctx
            .st
            .ws
            .resolve(&format!("{ws}:{}", join_rel(&base_rel, &parts.join("/"))))?;
        match h["k"].as_str() {
            Some("d") => {
                if r.abs.is_file() {
                    return Err(AppError::conflict(format!(
                        "{} 已存在同名文件",
                        r.display()
                    )));
                }
                match op.as_mut() {
                    Some(o) => o.create_dirs(&r.root, &r.abs, &r.ws)?,
                    None => fs::create_dir_all(&r.abs)?,
                }
                dirs += 1;
            }
            Some("f") => {
                let n = h["n"]
                    .as_u64()
                    .ok_or_else(|| AppError::bad_request("文件帧缺少大小"))?;
                bytes += n;
                if bytes > max {
                    return Err(AppError::bad_request("传输量超过 max_transfer_mb"));
                }
                if r.abs.is_dir() {
                    return Err(AppError::conflict(format!("{} 是目录", r.display())));
                }
                let parent = r
                    .abs
                    .parent()
                    .ok_or_else(|| AppError::bad_request("无效路径"))?
                    .to_path_buf();
                match op.as_mut() {
                    Some(o) => o.create_dirs(&r.root, &parent, &r.ws)?,
                    None => fs::create_dir_all(&parent)?,
                }
                let fname = r
                    .abs
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                let tmp = parent.join(format!(".{fname}.agent-cli-x-{}.tmp", util::random_hex(4)));
                let written = (|| -> Result<()> {
                    let mut f = File::create(&tmp)?;
                    frame::copy_exact(&mut body, &mut f, n)?;
                    f.sync_all()?;
                    Ok(())
                })();
                if let Err(e) = written {
                    let _ = fs::remove_file(&tmp);
                    return Err(e);
                }
                if let Some(o) = op.as_mut() {
                    o.add(&r.abs, &r.display())?;
                }
                if let Err(e) = fs::rename(&tmp, &r.abs) {
                    let _ = fs::remove_file(&tmp);
                    return Err(e.into());
                }
                if h["x"] == true {
                    set_exec(&r.abs);
                }
                files += 1;
            }
            _ => return Err(AppError::bad_request("未知的条目类型")),
        }
    }
    Ok(json!({"dest": format!("{ws}:{base_rel}"), "files": files, "dirs": dirs, "bytes": bytes}))
}

// ---------------- get ----------------

#[derive(Deserialize)]
struct GetReq {
    src: String,
    #[serde(default)]
    exclude: Vec<String>,
}

struct Entry {
    rel: String,
    abs: PathBuf,
    dir: bool,
}

#[cfg(unix)]
fn is_exec(m: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    m.permissions().mode() & 0o111 != 0
}
#[cfg(not(unix))]
fn is_exec(_m: &fs::Metadata) -> bool {
    false
}

pub fn get<W: Write>(ctx: &Ctx, body: Value, w: &mut W) {
    let prep = (|| -> Result<(String, String, Vec<Entry>)> {
        let req: GetReq =
            serde_json::from_value(body).map_err(|e| AppError::bad_request(e.to_string()))?;
        let r = ctx.st.ws.resolve(&req.src)?;
        if !r.exists {
            return Err(AppError::not_found(format!("不存在: {}", r.display())));
        }
        let ex = exclude_set(&req.exclude)?;
        let name = r
            .abs
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| r.ws.clone());
        let mut entries = Vec::new();
        if r.abs.is_file() {
            entries.push(Entry {
                rel: String::new(),
                abs: r.abs.clone(),
                dir: false,
            });
            return Ok(("f".into(), name, entries));
        }
        entries.push(Entry {
            rel: String::new(),
            abs: r.abs.clone(),
            dir: true,
        });
        let mut stack = vec![(r.abs.clone(), String::new())];
        while let Some((dir, rel)) = stack.pop() {
            let mut children: Vec<_> = fs::read_dir(&dir)?.flatten().collect();
            children.sort_by_key(|e| e.file_name());
            for c in children {
                let Ok(ft) = c.file_type() else { continue };
                if ft.is_symlink() {
                    continue;
                }
                let crel = join_rel(&rel, &c.file_name().to_string_lossy());
                let cabs = c.path();
                if excluded(&ex, &crel) || ctx.st.ws.denied_by(&r.ws, &r.root, &cabs).is_some() {
                    continue;
                }
                if ft.is_dir() {
                    entries.push(Entry {
                        rel: crel.clone(),
                        abs: cabs.clone(),
                        dir: true,
                    });
                    stack.push((cabs, crel));
                } else if ft.is_file() {
                    entries.push(Entry {
                        rel: crel,
                        abs: cabs,
                        dir: false,
                    });
                }
            }
        }
        Ok(("d".into(), name, entries))
    })();
    let (kind, name, entries) = match prep {
        Ok(x) => x,
        Err(e) => {
            let _ = http::write_error(w, &e);
            return;
        }
    };
    if http::write_head(w, 200, "application/octet-stream").is_err() {
        return;
    }
    let max = ctx.st.cfg.policy.max_transfer_mb * 1024 * 1024;
    let res = (|| -> Result<(u64, u64)> {
        frame::write_header(w, &json!({"kind": kind, "name": name}))?;
        let (mut files, mut bytes) = (0u64, 0u64);
        for e in &entries {
            if e.dir {
                frame::write_header(w, &json!({"p": e.rel, "k": "d"}))?;
                continue;
            }
            let Ok(mut f) = File::open(&e.abs) else {
                continue;
            };
            let m = f.metadata()?;
            let n = m.len();
            bytes += n;
            if bytes > max {
                return Err(AppError::bad_request(format!(
                    "传输量超过 max_transfer_mb（{} MB），请用 --exclude 缩小范围",
                    ctx.st.cfg.policy.max_transfer_mb
                )));
            }
            frame::write_header(w, &json!({"p": e.rel, "k": "f", "n": n, "x": is_exec(&m)}))?;
            frame::copy_exact(&mut f, w, n)?;
            files += 1;
        }
        frame::write_header(w, &json!({"end": true, "files": files, "bytes": bytes}))?;
        w.flush()?;
        Ok((files, bytes))
    })();
    match res {
        Ok((files, bytes)) => ctx.st.audit.log(
            &ctx.ip,
            &ctx.caller,
            "get",
            true,
            json!({"files": files, "bytes": bytes, "name": name}),
        ),
        Err(e) => {
            let _ = frame::write_header(w, &http::error_json(&e));
            ctx.st.audit.log(
                &ctx.ip,
                &ctx.caller,
                "get",
                false,
                json!({"error": e.to_string()}),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclude_matches_segments() {
        let set = exclude_set(&["node_modules".into(), "*.log".into()]).unwrap();
        assert!(excluded(&set, "web/node_modules/x.js"));
        assert!(excluded(&set, "a/b/run.log"));
        assert!(!excluded(&set, "src/main.rs"));
        assert!(!excluded(&exclude_set(&[]).unwrap(), "x"));
    }
}
