//! 文件接口：ls / read / write / edit / rm。所有路径都经过 Workspaces 解析（边界 + CRUD 权限）。

use super::{backup, Ctx};
use crate::error::{AppError, Result};
use crate::util;
use serde::Deserialize;
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::sync::Arc;

fn parse<T: for<'de> Deserialize<'de>>(body: Value) -> Result<T> {
    serde_json::from_value(body).map_err(|e| AppError::bad_request(format!("参数错误: {e}")))
}

// ---------------- ls ----------------

#[derive(Deserialize)]
struct LsReq {
    #[serde(default)]
    path: String,
    #[serde(default = "one")]
    depth: usize,
    #[serde(default = "max_entries")]
    max: usize,
}
fn one() -> usize {
    1
}
fn max_entries() -> usize {
    500
}

pub fn ls(ctx: &Ctx, body: Value) -> Result<Value> {
    let req: LsReq = parse(body)?;
    let r = ctx.st.ws.resolve(&req.path)?;
    if !r.abs.is_dir() {
        return Err(AppError::bad_request(format!("{} 不是目录", r.display())));
    }
    let depth = req.depth.clamp(1, 20);
    let max = req.max.clamp(1, 20_000);
    let st = Arc::clone(&ctx.st);
    let ws_name = r.ws.clone();
    let root = r.root.clone();
    let walker = ignore::WalkBuilder::new(&r.abs)
        .max_depth(Some(depth))
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(false)
        .require_git(false)
        .parents(true)
        .filter_entry(move |e| {
            e.file_name() != ".git" && st.ws.denied_by(&ws_name, &root, e.path()).is_none()
        })
        .build();
    let mut entries = Vec::new();
    let mut truncated = false;
    for item in walker.flatten() {
        if item.depth() == 0 {
            continue;
        }
        if entries.len() >= max {
            truncated = true;
            break;
        }
        let rel = item
            .path()
            .strip_prefix(&r.root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        let ft = item.file_type();
        let (kind, size) = if ft.is_some_and(|t| t.is_symlink()) {
            ("l", 0)
        } else if ft.is_some_and(|t| t.is_dir()) {
            ("d", 0)
        } else {
            ("f", item.metadata().map(|m| m.len()).unwrap_or(0))
        };
        entries.push(json!({"path": rel, "kind": kind, "size": size}));
    }
    entries.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
    ctx.st.audit.log(
        &ctx.ip,
        &ctx.caller,
        "ls",
        true,
        json!({"path": r.display(), "count": entries.len()}),
    );
    Ok(json!({"ws": r.ws, "path": r.rel, "entries": entries, "truncated": truncated}))
}

// ---------------- grep ----------------

#[derive(Deserialize)]
struct GrepReq {
    pattern: String,
    #[serde(default)]
    path: String,
    /// 只搜索匹配这些 glob 的文件，如 "*.rs"、"src/**/*.ts"
    #[serde(default)]
    glob: Vec<String>,
    #[serde(default)]
    ignore_case: bool,
    /// 按字面量匹配（不当作正则）
    #[serde(default)]
    fixed: bool,
    #[serde(default = "grep_max")]
    max: usize,
}
fn grep_max() -> usize {
    200
}

const GREP_FILE_LIMIT: u64 = 8 * 1024 * 1024;
const GREP_LINE_CHARS: usize = 300;

pub fn grep(ctx: &Ctx, body: Value) -> Result<Value> {
    let req: GrepReq = parse(body)?;
    if req.pattern.is_empty() {
        return Err(AppError::bad_request("pattern 不能为空"));
    }
    let pat = if req.fixed {
        regex::escape(&req.pattern)
    } else {
        req.pattern.clone()
    };
    let re = regex::bytes::RegexBuilder::new(&pat)
        .case_insensitive(req.ignore_case)
        .size_limit(8 * 1024 * 1024)
        .build()
        .map_err(|e| AppError::bad_request(format!("正则无效: {e}")))?;
    let r = ctx.st.ws.resolve(&req.path)?;
    if !r.exists {
        return Err(AppError::not_found(format!("路径不存在: {}", r.display())));
    }
    let max = req.max.clamp(1, 5000);

    let st = Arc::clone(&ctx.st);
    let ws_name = r.ws.clone();
    let root = r.root.clone();
    let mut builder = ignore::WalkBuilder::new(&r.abs);
    builder
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(false)
        .require_git(false)
        .parents(true)
        .filter_entry(move |e| {
            e.file_name() != ".git" && st.ws.denied_by(&ws_name, &root, e.path()).is_none()
        });
    if !req.glob.is_empty() {
        let mut ob = ignore::overrides::OverrideBuilder::new(&r.abs);
        for g in &req.glob {
            ob.add(g)
                .map_err(|e| AppError::bad_request(format!("glob `{g}` 无效: {e}")))?;
        }
        builder.overrides(
            ob.build()
                .map_err(|e| AppError::bad_request(format!("glob 无效: {e}")))?,
        );
    }

    let mut matches = Vec::new();
    let mut files_scanned = 0usize;
    let mut truncated = false;
    'walk: for item in builder.build().flatten() {
        if !item.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if item.metadata().map(|m| m.len()).unwrap_or(0) > GREP_FILE_LIMIT {
            continue;
        }
        let Ok(data) = fs::read(item.path()) else {
            continue;
        };
        if data[..data.len().min(8192)].contains(&0) {
            continue;
        }
        files_scanned += 1;
        let rel = item
            .path()
            .strip_prefix(&r.root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        for (i, line) in data.split(|&b| b == b'\n').enumerate() {
            if !re.is_match(line) {
                continue;
            }
            if matches.len() >= max {
                truncated = true;
                break 'walk;
            }
            let text = String::from_utf8_lossy(line);
            let text = text.trim_end_matches('\r');
            let text: String = if text.chars().count() > GREP_LINE_CHARS {
                text.chars().take(GREP_LINE_CHARS).collect::<String>() + "…"
            } else {
                text.to_string()
            };
            matches.push(json!({"path": rel, "line": i + 1, "text": text}));
        }
    }
    ctx.st.audit.log(
        &ctx.ip,
        &ctx.caller,
        "grep",
        true,
        json!({"path": r.display(), "pattern": req.pattern, "matches": matches.len()}),
    );
    Ok(json!({
        "ws": r.ws,
        "matches": matches,
        "files_scanned": files_scanned,
        "truncated": truncated,
    }))
}

// ---------------- read ----------------

#[derive(Deserialize)]
struct ReadReq {
    path: String,
    #[serde(default)]
    start: Option<usize>,
    #[serde(default)]
    end: Option<usize>,
    /// 本次最多返回的 KB 数（不超过 max_read_kb）；旧版客户端不传
    #[serde(default)]
    max_kb: Option<u64>,
}

/// 统计总行数 / 计算 sha256 时最多扫描的文件大小。
const FULL_SCAN_LIMIT: u64 = 64 * 1024 * 1024;

pub fn read(ctx: &Ctx, body: Value) -> Result<Value> {
    let req: ReadReq = parse(body)?;
    let r = ctx.st.ws.resolve(&req.path)?;
    if !r.exists || !r.abs.is_file() {
        return Err(AppError::not_found(format!("文件不存在: {}", r.display())));
    }
    let max_kb = match req.max_kb {
        Some(k) => k.clamp(1, ctx.st.cfg.policy.max_read_kb),
        None => ctx.st.cfg.policy.max_read_kb,
    };
    let max_bytes = (max_kb * 1024) as usize;
    let size = fs::metadata(&r.abs)?.len();
    let mut file = fs::File::open(&r.abs)?;

    let mut head = vec![0u8; 8192.min(size as usize)];
    file.read_exact(&mut head)?;
    if head.contains(&0) {
        return Err(AppError::bad_request(format!(
            "{} 看起来是二进制文件，read 只支持文本",
            r.display()
        )));
    }
    drop(file);

    let start = req.start.unwrap_or(1).max(1);
    let end = req.end.unwrap_or(usize::MAX).max(start);
    let full_scan = size <= FULL_SCAN_LIMIT;
    let mut reader = BufReader::new(fs::File::open(&r.abs)?);
    let mut hasher = ring::digest::Context::new(&ring::digest::SHA256);
    let mut content: Vec<u8> = Vec::new();
    let mut line_no = 0usize;
    let mut last_line = 0usize;
    let mut truncated = false;
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let n = reader.read_until(b'\n', &mut buf)?;
        if n == 0 {
            break;
        }
        line_no += 1;
        if full_scan {
            hasher.update(&buf);
        }
        if line_no >= start && line_no <= end && !truncated {
            if content.len() + buf.len() > max_bytes {
                truncated = true;
            } else {
                content.extend_from_slice(&buf);
                last_line = line_no;
            }
        }
        if !full_scan && (truncated || line_no >= end) {
            break;
        }
    }
    let mut text = String::from_utf8_lossy(&content).into_owned();
    if start == 1 && text.starts_with('\u{feff}') {
        text.remove(0);
    }
    let sha = full_scan.then(|| util::hex(hasher.finish().as_ref()));
    ctx.st.audit.log(
        &ctx.ip,
        &ctx.caller,
        "read",
        true,
        json!({"path": r.display(), "bytes": content.len()}),
    );
    Ok(json!({
        "path": r.display(),
        "content": text,
        "start": start,
        "end": last_line,
        "total_lines": if full_scan { Some(line_no) } else { None },
        "truncated": truncated,
        "sha256": sha,
        "size": size,
    }))
}

// ---------------- write ----------------

#[derive(Deserialize)]
struct WriteReq {
    path: String,
    content: String,
    #[serde(default)]
    mkdir: bool,
    #[serde(default)]
    expect_sha: Option<String>,
}

fn check_expect_sha(abs: &std::path::Path, expect: &Option<String>) -> Result<()> {
    if let Some(want) = expect {
        let cur = fs::read(abs)
            .map(|b| util::sha256_hex(&b))
            .unwrap_or_default();
        if !cur.eq_ignore_ascii_case(want.trim_start_matches("sha256:")) {
            return Err(AppError::conflict(format!(
                "文件已被修改（当前 sha256={}），请重新读取后再改",
                if cur.is_empty() { "<不存在>" } else { &cur }
            )));
        }
    }
    Ok(())
}

pub fn write(ctx: &Ctx, body: Value) -> Result<Value> {
    let req: WriteReq = parse(body)?;
    let max = ctx.st.cfg.policy.max_write_kb * 1024;
    if req.content.len() as u64 > max {
        return Err(AppError::bad_request(format!(
            "内容超过 max_write_kb（{max} 字节）"
        )));
    }
    let r = ctx.st.ws.resolve_write(&req.path)?;
    if r.rel.is_empty() || r.abs.is_dir() {
        return Err(AppError::bad_request(format!("{} 是目录", r.display())));
    }
    let parent = r
        .abs
        .parent()
        .ok_or_else(|| AppError::bad_request("无效路径"))?;
    let need_mkdir = !parent.is_dir();
    if need_mkdir && !req.mkdir {
        return Err(AppError::not_found(format!(
            "父目录不存在: {}（加 --mkdir 自动创建）",
            r.display()
        )));
    }
    check_expect_sha(&r.abs, &req.expect_sha)?;
    let mut op = backup::Op::new(&ctx.st.home)?;
    if need_mkdir {
        // 记录新建的目录，rollback 时一并删除
        op.create_dirs(&r.root, parent, &r.ws)?;
    }
    op.add(&r.abs, &r.display())?;
    let op_id = op.id.clone();
    util::atomic_write(&r.abs, req.content.as_bytes())?;
    let sha = util::sha256_hex(req.content.as_bytes());
    ctx.st.audit.log(
        &ctx.ip,
        &ctx.caller,
        "write",
        true,
        json!({"path": r.display(), "op_id": op_id, "bytes": req.content.len()}),
    );
    Ok(
        json!({"path": r.display(), "op_id": op_id, "sha256": sha, "bytes": req.content.len(), "created": !r.exists}),
    )
}

// ---------------- edit ----------------

#[derive(Deserialize)]
struct EditReq {
    path: String,
    old: String,
    new: String,
    #[serde(default)]
    replace_all: bool,
    #[serde(default)]
    expect_sha: Option<String>,
}

pub struct EditOutcome {
    pub text: String,
    pub replacements: usize,
    pub first_line: usize,
}

/// 纯函数：按文件换行风格归一 old/new 后精确替换。
pub fn apply_edit(original: &str, old: &str, new: &str, replace_all: bool) -> Result<EditOutcome> {
    if old.is_empty() {
        return Err(AppError::bad_request("old 不能为空"));
    }
    let crlf = original.contains("\r\n");
    let fix = |s: &str| {
        let lf = s.replace("\r\n", "\n");
        if crlf {
            lf.replace('\n', "\r\n")
        } else {
            lf
        }
    };
    let (old, new) = (fix(old), fix(new));
    let count = original.matches(&old).count();
    if count == 0 {
        return Err(AppError::conflict(
            "未找到 old 文本（需逐字匹配，含缩进与空白），请先 read 确认当前内容",
        ));
    }
    if count > 1 && !replace_all {
        return Err(AppError::conflict(format!(
            "old 文本匹配到 {count} 处，请扩大上下文使其唯一，或使用 --all 全部替换"
        )));
    }
    let idx = original.find(&old).unwrap_or(0);
    let first_line = original[..idx].matches('\n').count() + 1;
    let text = if replace_all {
        original.replace(&old, &new)
    } else {
        original.replacen(&old, &new, 1)
    };
    Ok(EditOutcome {
        text,
        replacements: if replace_all { count } else { 1 },
        first_line,
    })
}

pub fn edit(ctx: &Ctx, body: Value) -> Result<Value> {
    let req: EditReq = parse(body)?;
    let r = ctx.st.ws.resolve_write(&req.path)?;
    if !r.exists || !r.abs.is_file() {
        return Err(AppError::not_found(format!("文件不存在: {}", r.display())));
    }
    let bytes = fs::read(&r.abs)?;
    let (bom, body_bytes) = match bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        Some(rest) => (true, rest),
        None => (false, &bytes[..]),
    };
    let original = std::str::from_utf8(body_bytes).map_err(|_| {
        AppError::bad_request(format!(
            "{} 不是 UTF-8 文本，edit 不支持（可用 write 整体覆盖）",
            r.display()
        ))
    })?;
    if let Some(want) = &req.expect_sha {
        let cur = util::sha256_hex(&bytes);
        if !cur.eq_ignore_ascii_case(want.trim_start_matches("sha256:")) {
            return Err(AppError::conflict(format!(
                "文件已被修改（当前 sha256={cur}），请重新读取后再改"
            )));
        }
    }
    let out = apply_edit(original, &req.old, &req.new, req.replace_all)?;
    let mut data = Vec::with_capacity(out.text.len() + 3);
    if bom {
        data.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    }
    data.extend_from_slice(out.text.as_bytes());
    let mut op = backup::Op::new(&ctx.st.home)?;
    op.add(&r.abs, &r.display())?;
    let op_id = op.id.clone();
    util::atomic_write(&r.abs, &data)?;
    let sha = util::sha256_hex(&data);
    ctx.st.audit.log(
        &ctx.ip,
        &ctx.caller,
        "edit",
        true,
        json!({"path": r.display(), "op_id": op_id, "replacements": out.replacements}),
    );
    Ok(json!({
        "path": r.display(),
        "op_id": op_id,
        "sha256": sha,
        "replacements": out.replacements,
        "line": out.first_line,
    }))
}

// ---------------- rm ----------------

#[derive(Deserialize)]
struct RmReq {
    path: String,
    /// 删除目录（含其下全部内容）时必须为 true
    #[serde(default)]
    recursive: bool,
    /// 彻底删除：不进回收站、不备份，无法 rollback
    #[serde(default)]
    permanent: bool,
}

/// 单次 rm 最多涉及的条目数（权限检查与备份都要逐个处理）。
const RM_MAX_ENTRIES: usize = 100_000;

enum RmKind {
    File,
    Dir,
    Link,
}

/// 删除文件或目录。先逐个检查删除权限（任何一项不允许就整体拒绝），再按方式删除：
/// - 默认移入系统回收站，rollback 时从回收站还原；
/// - 回收站不可用（或配置关闭）时退回“备份后删除”，同样可 rollback；
/// - `permanent` 时直接彻底删除，不可恢复。
///
/// 符号链接只删除链接本身，不跟随。
pub fn rm(ctx: &Ctx, body: Value) -> Result<Value> {
    let req: RmReq = parse(body)?;
    let ws = &ctx.st.ws;
    let r = ws.resolve_entry(&req.path)?;
    if r.rel.is_empty() {
        return Err(AppError::bad_request("不能删除工作区根目录"));
    }
    if !r.exists {
        return Err(AppError::not_found(format!("路径不存在: {}", r.display())));
    }
    let meta = fs::symlink_metadata(&r.abs)?;
    let top_is_dir = meta.is_dir();
    if top_is_dir && !req.recursive {
        return Err(AppError::bad_request(format!(
            "{} 是目录，删除目录请加 --recursive（-r）",
            r.display()
        )));
    }

    // 收集要删除的全部条目（目录在前、先父后子），同时检查删除权限
    let mut items: Vec<(std::path::PathBuf, String, RmKind)> = Vec::new();
    let mut total: u64 = 0;
    let mut stack = vec![(r.abs.clone(), r.rel.clone(), meta)];
    while let Some((abs, rel, m)) = stack.pop() {
        let display = format!("{}:{rel}", r.ws);
        ws.check_abs(
            &r.ws,
            &r.root,
            &abs,
            &display,
            super::workspace::Access::Delete,
        )?;
        let kind = if m.file_type().is_symlink() {
            RmKind::Link
        } else if m.is_dir() {
            let mut children: Vec<_> = fs::read_dir(&abs)?.flatten().collect();
            children.sort_by_key(|e| e.file_name());
            for c in children.into_iter().rev() {
                let cm = fs::symlink_metadata(c.path())?;
                let crel = format!("{rel}/{}", c.file_name().to_string_lossy());
                stack.push((c.path(), crel, cm));
            }
            RmKind::Dir
        } else {
            total += m.len();
            RmKind::File
        };
        items.push((abs, display, kind));
        if items.len() > RM_MAX_ENTRIES {
            return Err(AppError::bad_request(format!(
                "要删除的条目超过 {RM_MAX_ENTRIES} 个，请缩小范围"
            )));
        }
    }
    let count = |k: fn(&RmKind) -> bool| items.iter().filter(|(_, _, x)| k(x)).count() as u64;
    let files = count(|k| matches!(k, RmKind::File));
    let dirs = count(|k| matches!(k, RmKind::Dir));
    let links = count(|k| matches!(k, RmKind::Link));

    let mut op_id: Option<String> = None;
    let mut trash_error: Option<String> = None;
    let method = if req.permanent {
        delete_items(&items)?;
        "permanent"
    } else {
        let trashed = if ctx.st.cfg.policy.rm_use_trash {
            let since = super::recycle::now_secs();
            match super::recycle::move_to_trash(&r.abs) {
                Ok(()) => {
                    let until = super::recycle::now_secs();
                    let mut op = backup::Op::new(&ctx.st.home)?;
                    op.add_trashed(&r.abs, &r.display(), since, until)?;
                    op_id = Some(op.id.clone());
                    op.finish();
                    true
                }
                Err(e) => {
                    trash_error = Some(e);
                    false
                }
            }
        } else {
            false
        };
        if trashed {
            "trash"
        } else {
            // 回收站不可用：先备份全部条目再删除
            let max_bytes = ctx.st.cfg.policy.max_transfer_mb * 1024 * 1024;
            if total > max_bytes {
                return Err(AppError::bad_request(format!(
                    "回收站不可用{}，且要删除的文件总量超过 max_transfer_mb（{} MB）无法备份；\
                     请缩小范围、调大该配置，或确认后使用 --permanent 彻底删除",
                    trash_error
                        .as_deref()
                        .map(|e| format!("（{e}）"))
                        .unwrap_or_default(),
                    ctx.st.cfg.policy.max_transfer_mb
                )));
            }
            let mut op = backup::Op::new(&ctx.st.home)?;
            for (abs, display, kind) in &items {
                match kind {
                    RmKind::Dir => op.add_removed_dir(abs, display)?,
                    RmKind::Link => op.add_removed_link(abs, display)?,
                    RmKind::File => op.add(abs, display)?,
                }
            }
            op_id = Some(op.id.clone());
            delete_items(&items)?;
            op.finish();
            "backup"
        }
    };
    ctx.st.audit.log(
        &ctx.ip,
        &ctx.caller,
        "rm",
        true,
        json!({"path": r.display(), "method": method, "op_id": op_id, "files": files, "dirs": dirs, "bytes": total}),
    );
    Ok(json!({
        "path": r.display(),
        "method": method,
        "op_id": op_id,
        "trash_error": trash_error,
        "files": files,
        "dirs": dirs,
        "links": links,
        "bytes": total,
    }))
}

/// 直接删除收集到的条目：先删文件与链接，再从最深处删目录。
fn delete_items(items: &[(std::path::PathBuf, String, RmKind)]) -> Result<()> {
    for (abs, _, kind) in items {
        match kind {
            RmKind::File => fs::remove_file(abs)?,
            RmKind::Link => remove_link(abs)?,
            RmKind::Dir => {}
        }
    }
    for (abs, _, kind) in items.iter().rev() {
        if let RmKind::Dir = kind {
            fs::remove_dir(abs)?;
        }
    }
    Ok(())
}

/// 删除符号链接 / junction 本身。Windows 上指向目录的链接要用 remove_dir。
fn remove_link(abs: &std::path::Path) -> std::io::Result<()> {
    match fs::remove_file(abs) {
        Ok(()) => Ok(()),
        Err(e) if cfg!(windows) => fs::remove_dir(abs).map_err(|_| e),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_unique_match() {
        let o = apply_edit("a\nb\nc\n", "b\n", "B\n", false).unwrap();
        assert_eq!(o.text, "a\nB\nc\n");
        assert_eq!(o.first_line, 2);
    }

    #[test]
    fn edit_crlf_normalized() {
        let o = apply_edit("a\r\nb\r\nc\r\n", "a\nb", "x\ny", false).unwrap();
        assert_eq!(o.text, "x\r\ny\r\nc\r\n");
    }

    #[test]
    fn edit_ambiguous_and_missing() {
        assert_eq!(
            apply_edit("x x", "x", "y", false).err().unwrap().code,
            "conflict"
        );
        assert_eq!(apply_edit("x x", "x", "y", true).unwrap().text, "y y");
        assert_eq!(
            apply_edit("abc", "zzz", "y", false).err().unwrap().code,
            "conflict"
        );
        assert_eq!(
            apply_edit("abc", "", "y", false).err().unwrap().code,
            "bad_request"
        );
    }
}
