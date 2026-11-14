//! 文件接口：ls / find / grep / read / write / edit / rm。所有路径都经过 Workspaces 解析（边界 + CRUD 权限）。

use super::workspace::Resolved;
use super::{backup, Ctx};
use crate::client::output::{clip_line, clip_line_bytes, MAX_LINE_CHARS};
use crate::error::{AppError, Result};
use crate::util;
use serde::Deserialize;
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::sync::Arc;

fn parse<T: for<'de> Deserialize<'de>>(body: Value) -> Result<T> {
    serde_json::from_value(body).map_err(|e| AppError::bad_request(format!("bad params: {e}")))
}

// Leave room for response metadata inside a 64KB page.
const PAGE_BYTES: usize = 60 * 1024;

// ---------------- ls ----------------

#[derive(Deserialize)]
struct LsReq {
    #[serde(default)]
    path: String,
    #[serde(default = "one")]
    depth: usize,
    #[serde(default = "max_entries")]
    max: usize,
    #[serde(default)]
    offset: usize,
    #[serde(default)]
    full: bool,
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
        return Err(AppError::bad_request(format!(
            "{} is not a directory",
            r.display()
        )));
    }
    let depth = req.depth.clamp(1, 20);
    let max = req.max.clamp(1, 20_000);
    let metadata_bytes = serde_json::to_vec(&json!({"ws": r.ws, "path": r.rel}))
        .map_err(|e| AppError::internal(e.to_string()))?
        .len();
    let page_bytes = PAGE_BYTES.saturating_sub(metadata_bytes);
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
        .sort_by_file_name(|a, b| a.cmp(b))
        .filter_entry(move |e| {
            e.file_name() != ".git" && st.ws.denied_by(&ws_name, &root, e.path()).is_none()
        })
        .build();
    let mut entries = Vec::new();
    let mut truncated = false;
    let mut bytes = 2;
    let mut seen = 0usize;
    let mut omitted_chars = 0usize;
    for item in walker.flatten() {
        if item.depth() == 0 {
            continue;
        }
        if seen < req.offset {
            seen += 1;
            continue;
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
        let (path, cut) = if req.full {
            (rel, 0)
        } else {
            clip_line(&rel, MAX_LINE_CHARS)
        };
        let mut entry = json!({"path": path, "kind": kind, "size": size});
        if cut > 0 {
            entry["omitted_chars"] = json!(cut);
        }
        let cost = serde_json::to_vec(&entry)
            .map_err(|e| AppError::internal(e.to_string()))?
            .len()
            + 1;
        let cost = cost.max(path_display_bytes(&entry["path"]) + 48);
        if !req.full && entries.is_empty() && bytes + cost > page_bytes {
            return Err(AppError::bad_request(
                "entry exceeds page byte limit; add --full to retrieve its complete path",
            ));
        }
        if entries.len() >= max || (!req.full && bytes + cost > page_bytes) {
            truncated = true;
            break;
        }
        bytes += cost;
        omitted_chars += cut;
        entries.push(entry);
    }
    entries.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
    ctx.st.audit.log(
        &ctx.ip,
        &ctx.caller,
        "ls",
        true,
        json!({"path": r.display(), "count": entries.len()}),
    );
    let next = truncated.then(|| req.offset.saturating_add(entries.len()));
    Ok(json!({"ws": r.ws, "path": r.rel, "entries": entries,
        "truncated": truncated || omitted_chars > 0,
        "omitted_entries_at_least": usize::from(truncated),
        "omitted_chars": omitted_chars, "next_offset": next}))
}

// ---------------- 共用：遍历 ----------------

/// 与 ls 相同的遍历规则：遵守 .gitignore、跳过 .git 与黑名单路径；
/// 按文件名排序，保证分页（grep --offset）在多次调用之间结果一致。
fn walker(ctx: &Ctx, r: &Resolved, globs: &[String]) -> Result<ignore::Walk> {
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
        .sort_by_file_name(|a, b| a.cmp(b))
        .filter_entry(move |e| {
            e.file_name() != ".git" && st.ws.denied_by(&ws_name, &root, e.path()).is_none()
        });
    if !globs.is_empty() {
        let mut ob = ignore::overrides::OverrideBuilder::new(&r.abs);
        for g in globs {
            ob.add(g)
                .map_err(|e| AppError::bad_request(format!("invalid glob `{g}`: {e}")))?;
        }
        builder.overrides(
            ob.build()
                .map_err(|e| AppError::bad_request(format!("invalid glob: {e}")))?,
        );
    }
    Ok(builder.build())
}

fn rel_of(item: &ignore::DirEntry, root: &std::path::Path) -> String {
    item.path()
        .strip_prefix(root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default()
}

// ---------------- find ----------------

#[derive(Deserialize)]
struct FindReq {
    /// glob，相对搜索目录；不含 `/` 时匹配任意层级（与 grep --glob 相同的 gitignore 语义）
    pattern: String,
    #[serde(default)]
    path: String,
    #[serde(default = "find_max")]
    max: usize,
}
fn find_max() -> usize {
    200
}

/// 匹配数超过它就停止遍历（total 只是下限）。
const FIND_SCAN_LIMIT: usize = 100_000;

/// 按文件名模式查找文件，最近修改的排在前面。
pub fn find(ctx: &Ctx, body: Value) -> Result<Value> {
    let req: FindReq = parse(body)?;
    if req.pattern.trim().is_empty() {
        return Err(AppError::bad_request("pattern must not be empty"));
    }
    let r = ctx.st.ws.resolve(&req.path)?;
    if !r.abs.is_dir() {
        return Err(AppError::bad_request(format!(
            "{} is not a directory",
            r.display()
        )));
    }
    let max = req.max.clamp(1, 20_000);
    let mut found: Vec<(std::time::SystemTime, String)> = Vec::new();
    let mut capped = false;
    for item in walker(ctx, &r, std::slice::from_ref(&req.pattern))?.flatten() {
        if !item.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if found.len() >= FIND_SCAN_LIMIT {
            capped = true;
            break;
        }
        let mtime = item
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or(std::time::UNIX_EPOCH);
        found.push((mtime, rel_of(&item, &r.root)));
    }
    let total = found.len();
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let paths: Vec<String> = found.into_iter().take(max).map(|(_, p)| p).collect();
    ctx.st.audit.log(
        &ctx.ip,
        &ctx.caller,
        "find",
        true,
        json!({"path": r.display(), "pattern": req.pattern, "count": total}),
    );
    Ok(json!({
        "ws": r.ws,
        "paths": paths,
        "total": total,
        "total_capped": capped,
        "truncated": total > max,
    }))
}

// ---------------- grep ----------------

#[derive(Deserialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum GrepMode {
    /// 输出匹配行（可带上下文）
    #[default]
    Lines,
    /// 只输出含匹配的文件
    Files,
    /// 每个文件的匹配行数
    Count,
}

impl GrepMode {
    fn name(self) -> &'static str {
        match self {
            GrepMode::Lines => "lines",
            GrepMode::Files => "files",
            GrepMode::Count => "count",
        }
    }
}

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
    /// lines 模式下限制匹配行数；files / count 模式下限制文件数
    #[serde(default = "grep_max")]
    max: usize,
    /// 跳过前 N 个结果（匹配行或文件），用于截断后继续
    #[serde(default)]
    offset: usize,
    /// 取消字节与单行上限（条数仍受 max 限制）
    #[serde(default)]
    full: bool,
    #[serde(default)]
    mode: GrepMode,
    /// 匹配行前后的上下文行数（仅 lines 模式）
    #[serde(default)]
    before: usize,
    #[serde(default)]
    after: usize,
}
fn grep_max() -> usize {
    200
}

const GREP_FILE_LIMIT: u64 = 8 * 1024 * 1024;
const GREP_LINE_CHARS: usize = 300;
const GREP_MAX_CONTEXT: usize = 50;

/// 跨文件累积的 grep 结果与配额。
struct GrepAcc {
    mode: GrepMode,
    max: usize,
    offset: usize,
    full: bool,
    before: usize,
    after: usize,
    ws_len: usize,
    /// 已遇到的结果数（含被 offset 跳过的）
    seen: usize,
    /// 实际返回的结果数（不含上下文行）
    returned: usize,
    bytes: usize,
    omitted_chars: usize,
    items: Vec<Value>,
    truncated: bool,
}

impl GrepAcc {
    fn new(req: &GrepReq, ws_len: usize) -> Self {
        let lines = req.mode == GrepMode::Lines;
        let ctx = |n: usize| if lines { n.min(GREP_MAX_CONTEXT) } else { 0 };
        GrepAcc {
            mode: req.mode,
            max: req.max.clamp(1, 5000),
            offset: req.offset,
            full: req.full,
            before: ctx(req.before),
            after: ctx(req.after),
            ws_len,
            seen: 0,
            returned: 0,
            bytes: 2,
            omitted_chars: 0,
            items: Vec::new(),
            truncated: false,
        }
    }

    /// 构造一条结果：(条目, 占用字节, 被截掉的字符数)。
    fn entry(
        &self,
        rel: &str,
        line: Option<(usize, &[u8], bool)>,
        count: Option<usize>,
    ) -> (Value, usize, usize) {
        let (path, mut cut) = if self.full {
            (rel.to_string(), 0)
        } else {
            clip_line(rel, MAX_LINE_CHARS)
        };
        let mut e = json!({"path": path});
        if let Some((idx, raw, is_ctx)) = line {
            let text = String::from_utf8_lossy(raw);
            let text = text.trim_end_matches('\r');
            let (text, c) = if self.full {
                (text.to_string(), 0)
            } else {
                clip_line(text, GREP_LINE_CHARS)
            };
            cut += c;
            e["line"] = json!(idx + 1);
            e["text"] = json!(text);
            if is_ctx {
                e["ctx"] = json!(true);
            }
        }
        if let Some(n) = count {
            e["count"] = json!(n);
        }
        if cut > 0 {
            e["omitted_chars"] = json!(cut);
        }
        let cost = serde_json::to_vec(&e).map_or(0, |v| v.len()) + 1;
        let cost = cost.max(
            path_display_bytes(&e["path"])
                + e["text"].as_str().map_or(0, str::len)
                + self.ws_len
                + 40,
        );
        (e, cost, cut)
    }

    /// 尝试收下一组条目（一个匹配及其前置上下文）；条数或字节超限返回 false。
    fn commit(&mut self, group: Vec<(Value, usize, usize)>) -> bool {
        let cost: usize = group.iter().map(|g| g.1).sum();
        if self.returned >= self.max || (!self.full && self.bytes + cost > PAGE_BYTES) {
            self.truncated = true;
            return false;
        }
        self.bytes += cost;
        self.returned += 1;
        for (e, _, cut) in group {
            self.omitted_chars += cut;
            self.items.push(e);
        }
        true
    }

    /// 处理一个文件；返回 false 表示配额已满，应停止遍历。
    fn scan(&mut self, rel: &str, data: &[u8], re: &regex::bytes::Regex) -> bool {
        let mut lines: Vec<&[u8]> = data.split(|&b| b == b'\n').collect();
        if data.ends_with(b"\n") {
            lines.pop();
        }
        if self.mode != GrepMode::Lines {
            let n = if self.mode == GrepMode::Files {
                usize::from(lines.iter().any(|l| re.is_match(l)))
            } else {
                lines.iter().filter(|l| re.is_match(l)).count()
            };
            if n == 0 {
                return true;
            }
            if self.seen < self.offset {
                self.seen += 1;
                return true;
            }
            let item = self.entry(rel, None, (self.mode == GrepMode::Count).then_some(n));
            return self.commit(vec![item]);
        }
        // emitted：已输出到第几行（下标 + 1），上下文重叠时不重复输出
        let mut emitted = 0usize;
        let mut after_until = 0usize;
        for (i, line) in lines.iter().enumerate() {
            if re.is_match(line) {
                if self.seen < self.offset {
                    self.seen += 1;
                    continue;
                }
                let from = i.saturating_sub(self.before).max(emitted);
                let mut group: Vec<_> = (from..i)
                    .map(|j| self.entry(rel, Some((j, lines[j], true)), None))
                    .collect();
                group.push(self.entry(rel, Some((i, line, false)), None));
                if !self.commit(group) {
                    return false;
                }
                emitted = i + 1;
                after_until = i + 1 + self.after;
            } else if i < after_until && i >= emitted {
                if !self.full && self.bytes >= PAGE_BYTES {
                    continue;
                }
                let (e, cost, cut) = self.entry(rel, Some((i, line, true)), None);
                self.bytes += cost;
                self.omitted_chars += cut;
                self.items.push(e);
                emitted = i + 1;
            }
        }
        true
    }
}

pub fn grep(ctx: &Ctx, body: Value) -> Result<Value> {
    let req: GrepReq = parse(body)?;
    if req.pattern.is_empty() {
        return Err(AppError::bad_request("pattern must not be empty"));
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
        .map_err(|e| AppError::bad_request(format!("invalid regex: {e}")))?;
    let r = ctx.st.ws.resolve(&req.path)?;
    if !r.exists {
        return Err(AppError::not_found(format!(
            "path not found: {}",
            r.display()
        )));
    }
    let mut acc = GrepAcc::new(&req, r.ws.len());
    let mut files_scanned = 0usize;
    for item in walker(ctx, &r, &req.glob)?.flatten() {
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
        if !acc.scan(&rel_of(&item, &r.root), &data, &re) {
            break;
        }
    }
    ctx.st.audit.log(
        &ctx.ip,
        &ctx.caller,
        "grep",
        true,
        json!({"path": r.display(), "pattern": req.pattern, "matches": acc.returned}),
    );
    let next = acc
        .truncated
        .then(|| req.offset.saturating_add(acc.returned));
    Ok(json!({
        "ws": r.ws,
        "mode": req.mode.name(),
        "matches": acc.items,
        "count": acc.returned,
        "files_scanned": files_scanned,
        "truncated": acc.truncated || acc.omitted_chars > 0,
        "omitted_matches_at_least": usize::from(acc.truncated),
        "omitted_chars": acc.omitted_chars,
        "next_offset": next,
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
    #[serde(default)]
    windowed: bool,
}

/// 统计总行数 / 计算 sha256 时最多扫描的文件大小。
const FULL_SCAN_LIMIT: u64 = 64 * 1024 * 1024;

fn path_display_bytes(path: &Value) -> usize {
    path.as_str()
        .unwrap_or_default()
        .escape_debug()
        .map(char::len_utf8)
        .sum()
}

/// Map a lossy UTF-8 prefix back to its original byte length for omission accounting.
fn source_prefix_bytes(mut raw: &[u8], mut chars: usize) -> usize {
    let mut consumed = 0;
    while chars > 0 && !raw.is_empty() {
        let (valid, invalid) = match std::str::from_utf8(raw) {
            Ok(text) => (text, 0),
            Err(e) => (
                std::str::from_utf8(&raw[..e.valid_up_to()]).expect("valid prefix"),
                e.error_len().unwrap_or(raw.len() - e.valid_up_to()),
            ),
        };
        let count = valid.chars().count();
        if chars <= count {
            return consumed
                + valid
                    .char_indices()
                    .nth(chars)
                    .map_or(valid.len(), |(i, _)| i);
        }
        consumed += valid.len();
        chars -= count;
        if invalid == 0 {
            break;
        }
        consumed += invalid;
        chars -= 1;
        raw = &raw[valid.len() + invalid..];
    }
    consumed
}

/// Consume one complete line while keeping only a bounded prefix in memory.
fn read_line_prefix<R: BufRead>(
    reader: &mut R,
    prefix: &mut Vec<u8>,
    limit: usize,
    hasher: &mut ring::digest::Context,
    hash: bool,
) -> std::io::Result<(usize, bool)> {
    prefix.clear();
    let mut total = 0usize;
    loop {
        let chunk = reader.fill_buf()?;
        if chunk.is_empty() {
            return Ok((total, false));
        }
        let newline = chunk.iter().position(|&b| b == b'\n');
        let n = newline.map_or(chunk.len(), |i| i + 1);
        if hash {
            hasher.update(&chunk[..n]);
        }
        let keep = n.min(limit.saturating_sub(prefix.len()));
        prefix.extend_from_slice(&chunk[..keep]);
        total = total.saturating_add(n);
        reader.consume(n);
        if newline.is_some() {
            return Ok((total, true));
        }
    }
}

pub fn read(ctx: &Ctx, body: Value) -> Result<Value> {
    let req: ReadReq = parse(body)?;
    let r = ctx.st.ws.resolve(&req.path)?;
    if !r.exists || !r.abs.is_file() {
        return Err(AppError::not_found(format!(
            "file not found: {}",
            r.display()
        )));
    }
    let windowed = req.windowed || req.end.is_none();
    let policy_kb = ctx.st.cfg.policy.max_read_kb.max(1);
    let default_kb = if windowed {
        policy_kb.min(64)
    } else {
        policy_kb
    };
    let max_kb = req.max_kb.unwrap_or(default_kb).clamp(1, default_kb);
    let max_bytes = max_kb.saturating_mul(1024).min(usize::MAX as u64) as usize;
    let size = fs::metadata(&r.abs)?.len();
    let mut file = fs::File::open(&r.abs)?;
    let mut head = vec![0u8; 8192.min(size as usize)];
    file.read_exact(&mut head)?;
    if head.contains(&0) {
        return Err(AppError::bad_request(format!(
            "{} looks binary; read supports text only",
            r.display()
        )));
    }
    drop(file);
    let start = req.start.unwrap_or(1).max(1);
    let end = req
        .end
        .unwrap_or_else(|| start.saturating_add(1999))
        .max(start);
    let full_scan = size <= FULL_SCAN_LIMIT;
    let mut reader = BufReader::new(fs::File::open(&r.abs)?);
    let mut hasher = ring::digest::Context::new(&ring::digest::SHA256);
    let mut content = String::new();
    let mut line_no = 0usize;
    let mut last_line = start - 1;
    let mut page_full = false;
    let mut clipped_lines = 0usize;
    let mut omitted_bytes = 0usize;
    let mut buf = Vec::new();
    loop {
        let selected = line_no + 1 >= start && line_no < end && !page_full;
        let limit = if !selected {
            0
        } else if windowed {
            (MAX_LINE_CHARS * 4 + 4).min(max_bytes.saturating_add(1))
        } else {
            max_bytes.saturating_add(1)
        };
        let (n, newline) = read_line_prefix(&mut reader, &mut buf, limit, &mut hasher, full_scan)?;
        if n == 0 {
            break;
        }
        line_no += 1;
        if selected {
            if !windowed && n > max_bytes {
                return Err(AppError::bad_request(format!(
                    "line {line_no} exceeds max_read_kb ({max_kb} KB); download full text with `agent-cli-x get <peer> {} <local>`", r.display()
                )));
            }
            if buf.ends_with(b"\n") {
                buf.pop();
            }
            let mut text = String::from_utf8_lossy(&buf).into_owned();
            let bom = start == 1 && line_no == 1 && text.starts_with('\u{feff}');
            if bom {
                text.remove(0);
            }
            let (mut shown, cut) = if windowed {
                clip_line_bytes(
                    &text,
                    MAX_LINE_CHARS,
                    max_bytes.saturating_sub(usize::from(newline)),
                )
            } else {
                (text.clone(), 0)
            };
            let kept_chars = text.chars().count().saturating_sub(cut);
            let kept_bytes = source_prefix_bytes(&buf[if bom { 3 } else { 0 }..], kept_chars);
            let omitted = n
                .saturating_sub(usize::from(newline) + if bom { 3 } else { 0 })
                .saturating_sub(kept_bytes);
            if newline {
                shown.push('\n');
            }
            if !windowed && shown.len() > max_bytes {
                return Err(AppError::bad_request(format!(
                    "decoded line {line_no} exceeds max_read_kb ({max_kb} KB); download full text with `agent-cli-x get <peer> {} <local>`", r.display()
                )));
            }
            if content.len().saturating_add(shown.len()) > max_bytes {
                page_full = true;
            } else {
                content.push_str(&shown);
                last_line = line_no;
                omitted_bytes = omitted_bytes.saturating_add(omitted);
                clipped_lines += usize::from(omitted > 0);
            }
        }
        if !full_scan && (page_full || line_no >= end) {
            break;
        }
    }
    let has_more = if full_scan {
        last_line < line_no
    } else {
        page_full || !reader.fill_buf()?.is_empty()
    };
    let next_start = has_more.then(|| last_line.saturating_add(1));
    let sha = full_scan.then(|| util::hex(hasher.finish().as_ref()));
    ctx.st.audit.log(
        &ctx.ip,
        &ctx.caller,
        "read",
        true,
        json!({"path": r.display(), "bytes": content.len()}),
    );
    Ok(json!({
        "path": r.display(), "content": content, "start": start, "end": last_line,
        "total_lines": if full_scan { Some(line_no) } else { None },
        "truncated": has_more || clipped_lines > 0,
        "omitted_lines": if full_scan { Some(line_no.saturating_sub(last_line)) } else { None },
        "omitted_bytes": omitted_bytes, "clipped_lines": clipped_lines,
        "full_text": (clipped_lines > 0).then(|| format!("agent-cli-x get <peer> {} <local>", r.display())),
        "next_start": next_start, "sha256": sha, "size": size,
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
                "file modified (current sha256={}); re-read before editing",
                if cur.is_empty() { "<missing>" } else { &cur }
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
            "content exceeds max_write_kb ({max} bytes)"
        )));
    }
    let r = ctx.st.ws.resolve_write(&req.path)?;
    if r.rel.is_empty() || r.abs.is_dir() {
        return Err(AppError::bad_request(format!(
            "{} is a directory",
            r.display()
        )));
    }
    let parent = r
        .abs
        .parent()
        .ok_or_else(|| AppError::bad_request("invalid path"))?;
    let need_mkdir = !parent.is_dir();
    if need_mkdir && !req.mkdir {
        return Err(AppError::not_found(format!(
            "parent dir not found: {} (add --mkdir to create)",
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
    #[serde(default)]
    old: Option<String>,
    #[serde(default)]
    new: Option<String>,
    #[serde(default)]
    replace_all: bool,
    /// 批量修改：按顺序作用于同一文件，全部成功才写入
    #[serde(default)]
    edits: Option<Vec<EditItem>>,
    #[serde(default)]
    expect_sha: Option<String>,
}

/// 批量修改中的一项。接受 Claude Code Edit 工具的字段名作为别名，减少 AI 写错字段重试。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditItem {
    #[serde(alias = "old_string")]
    pub old: String,
    #[serde(alias = "new_string")]
    pub new: String,
    #[serde(default, alias = "replace_all")]
    pub all: bool,
}

pub struct EditOutcome {
    pub text: String,
    pub replacements: usize,
    pub first_line: usize,
}

/// 纯函数：按文件换行风格归一 old/new 后精确替换。
pub fn apply_edit(original: &str, old: &str, new: &str, replace_all: bool) -> Result<EditOutcome> {
    if old.is_empty() {
        return Err(AppError::bad_request("old must not be empty"));
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
            "old text not found (must match exactly incl. indentation/whitespace); read first to confirm current content",
        ));
    }
    if count > 1 && !replace_all {
        return Err(AppError::conflict(format!(
            "old text matches {count} places; add context to make it unique, or use --all"
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

/// 依次应用多项修改（后一项看到前一项的结果），任何一项失败则整体失败。
/// 返回的 first_line 是第一项修改的起始行。
pub fn apply_edits(original: &str, edits: &[EditItem]) -> Result<EditOutcome> {
    if edits.is_empty() {
        return Err(AppError::bad_request("edits must not be empty"));
    }
    let mut text = original.to_string();
    let (mut replacements, mut first_line) = (0, 0);
    for (i, e) in edits.iter().enumerate() {
        let out = apply_edit(&text, &e.old, &e.new, e.all).map_err(|err| {
            if edits.len() == 1 {
                err
            } else {
                AppError::new(
                    &err.code,
                    format!(
                        "edit #{} of {}: {} (nothing written)",
                        i + 1,
                        edits.len(),
                        err.message
                    ),
                )
            }
        })?;
        if i == 0 {
            first_line = out.first_line;
        }
        replacements += out.replacements;
        text = out.text;
    }
    Ok(EditOutcome {
        text,
        replacements,
        first_line,
    })
}

pub fn edit(ctx: &Ctx, body: Value) -> Result<Value> {
    let req: EditReq = parse(body)?;
    let edits = match (req.edits, req.old, req.new) {
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
            return Err(AppError::bad_request(
                "use either edits or old/new, not both",
            ))
        }
        (Some(list), None, None) => list,
        (None, Some(old), Some(new)) => vec![EditItem {
            old,
            new,
            all: req.replace_all,
        }],
        (None, _, _) => return Err(AppError::bad_request("missing old/new or edits")),
    };
    let r = ctx.st.ws.resolve_write(&req.path)?;
    if !r.exists || !r.abs.is_file() {
        return Err(AppError::not_found(format!(
            "file not found: {}",
            r.display()
        )));
    }
    let bytes = fs::read(&r.abs)?;
    let (bom, body_bytes) = match bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        Some(rest) => (true, rest),
        None => (false, &bytes[..]),
    };
    let original = std::str::from_utf8(body_bytes).map_err(|_| {
        AppError::bad_request(format!(
            "{} is not UTF-8 text; edit unsupported (use write to overwrite)",
            r.display()
        ))
    })?;
    if let Some(want) = &req.expect_sha {
        let cur = util::sha256_hex(&bytes);
        if !cur.eq_ignore_ascii_case(want.trim_start_matches("sha256:")) {
            return Err(AppError::conflict(format!(
                "file modified (current sha256={cur}); re-read before editing"
            )));
        }
    }
    let out = apply_edits(original, &edits)?;
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
        return Err(AppError::bad_request("cannot delete workspace root"));
    }
    if !r.exists {
        return Err(AppError::not_found(format!(
            "path not found: {}",
            r.display()
        )));
    }
    let meta = fs::symlink_metadata(&r.abs)?;
    let top_is_dir = meta.is_dir();
    if top_is_dir && !req.recursive {
        return Err(AppError::bad_request(format!(
            "{} is a directory; add --recursive (-r) to delete directories",
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
                "more than {RM_MAX_ENTRIES} entries to delete; narrow the scope"
            )));
        }
    }
    let count = |k: fn(&RmKind) -> bool| items.iter().filter(|(_, _, x)| k(x)).count() as u64;
    let files = count(|k| matches!(k, RmKind::File));
    let dirs = count(|k| matches!(k, RmKind::Dir));
    let links = count(|k| matches!(k, RmKind::Link));

    let mut op_id: Option<String> = None;
    let mut trash_error: Option<String> = None;
    let mut trash_warning: Option<&str> = None;
    let method = if req.permanent {
        delete_items(&items)?;
        "permanent"
    } else {
        let trashed = if ctx.st.cfg.policy.rm_use_trash {
            let mut op = backup::Op::new(&ctx.st.home)?;
            match super::recycle::move_to_trash(&r.abs) {
                Ok(receipt) => {
                    if !receipt.identified() {
                        trash_warning = Some("recycle-bin entry could not be identified; restore or delete it manually in the system recycle bin");
                    }
                    op.add_trashed(&r.abs, &r.display(), &receipt)?;
                    op_id = Some(op.id.clone());
                    op.finish();
                    true
                }
                Err(e) => {
                    op.finish();
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
                    "recycle bin unavailable{}, and files to delete exceed max_transfer_mb ({} MB), cannot back up; \
                     narrow the scope, raise the setting, or confirm and use --permanent",
                    trash_error
                        .as_deref()
                        .map(|e| format!(" ({e})"))
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
        "trash_warning": trash_warning,
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
    fn lossy_prefix_accounting_uses_source_bytes() {
        assert_eq!(source_prefix_bytes(b"a\xff\xffb", 3), 3);
        assert_eq!(source_prefix_bytes("汉字".as_bytes(), 1), 3);
        assert_eq!(source_prefix_bytes(b"\xf0\x9f", 1), 2);
        assert_eq!(source_prefix_bytes(b"abc", 0), 0);
    }

    #[test]
    fn bounded_line_prefix_consumes_and_hashes_complete_lines() {
        let data = "汉".repeat(3000) + "\nnext\n";
        let mut reader = BufReader::with_capacity(7, data.as_bytes());
        let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
        let mut prefix = Vec::new();
        let (bytes, nl) = read_line_prefix(&mut reader, &mut prefix, 64, &mut hash, true).unwrap();
        assert_eq!((bytes, nl), (9001, true));
        assert_eq!(prefix.len(), 64);
        assert_eq!(
            read_line_prefix(&mut reader, &mut prefix, 64, &mut hash, true).unwrap(),
            (5, true)
        );
        assert_eq!(prefix, b"next\n");
        assert_eq!(
            util::hex(hash.finish().as_ref()),
            util::sha256_hex(data.as_bytes())
        );
    }

    #[test]
    fn bounded_line_prefix_handles_empty_and_unterminated_lines() {
        let mut reader = BufReader::new(&b"\nlast"[..]);
        let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
        let mut prefix = Vec::new();
        assert_eq!(
            read_line_prefix(&mut reader, &mut prefix, 0, &mut hash, false).unwrap(),
            (1, true)
        );
        assert!(prefix.is_empty());
        assert_eq!(
            read_line_prefix(&mut reader, &mut prefix, 2, &mut hash, false).unwrap(),
            (4, false)
        );
        assert_eq!(prefix, b"la");
        assert_eq!(
            read_line_prefix(&mut reader, &mut prefix, 2, &mut hash, false).unwrap(),
            (0, false)
        );
    }

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

    fn item(old: &str, new: &str) -> EditItem {
        EditItem {
            old: old.into(),
            new: new.into(),
            all: false,
        }
    }

    #[test]
    fn edits_sequential_all_or_nothing() {
        let o = apply_edits("a\nb\nc\n", &[item("b", "B"), item("B\nc", "X")]).unwrap();
        assert_eq!(o.text, "a\nX\n");
        assert_eq!(o.replacements, 2);
        assert_eq!(o.first_line, 2);
        let e = apply_edits("a\nb\n", &[item("a", "A"), item("zzz", "y")])
            .err()
            .unwrap();
        assert_eq!(e.code, "conflict");
        assert!(e.message.starts_with("edit #2 of 2:"), "{}", e.message);
        assert_eq!(apply_edits("a", &[]).err().unwrap().code, "bad_request");
        // 兼容 Claude Code Edit 工具的字段名；未知字段报错
        let v: Vec<EditItem> =
            serde_json::from_str(r#"[{"old_string":"a","new_string":"b","replace_all":true}]"#)
                .unwrap();
        assert!(v[0].all);
        assert!(serde_json::from_str::<Vec<EditItem>>(r#"[{"old":"a","neww":"b"}]"#).is_err());
    }

    fn scan(acc: &mut GrepAcc, data: &str) -> Vec<String> {
        let re = regex::bytes::Regex::new("hit").unwrap();
        acc.scan("f.txt", data.as_bytes(), &re);
        acc.items
            .iter()
            .map(|v| {
                let sep = if v["ctx"] == true { '-' } else { ':' };
                format!("{}{sep}{}", v["line"], v["text"].as_str().unwrap())
            })
            .collect()
    }

    fn mk(mode: GrepMode, max: usize, offset: usize, before: usize, after: usize) -> GrepAcc {
        let req = GrepReq {
            pattern: "hit".into(),
            path: String::new(),
            glob: vec![],
            ignore_case: false,
            fixed: false,
            max,
            offset,
            full: false,
            mode,
            before,
            after,
        };
        GrepAcc::new(&req, 4)
    }

    #[test]
    fn grep_context_merges_overlap() {
        let data = "a\nhit1\nb\nhit2\nc\nd\ne\nhit3\n";
        let mut acc = mk(GrepMode::Lines, 200, 0, 1, 1);
        assert_eq!(
            scan(&mut acc, data),
            ["1-a", "2:hit1", "3-b", "4:hit2", "5-c", "7-e", "8:hit3"]
        );
        assert_eq!(acc.returned, 3);
        assert!(!acc.truncated);
    }

    #[test]
    fn grep_max_and_offset() {
        let data = "hit1\nx\nhit2\nhit3\n";
        let mut acc = mk(GrepMode::Lines, 2, 0, 0, 0);
        assert_eq!(scan(&mut acc, data), ["1:hit1", "3:hit2"]);
        assert!(acc.truncated);
        let mut acc = mk(GrepMode::Lines, 2, 2, 0, 0);
        assert_eq!(scan(&mut acc, data), ["4:hit3"]);
        assert!(!acc.truncated);
    }

    #[test]
    fn grep_files_and_count_modes() {
        let re = regex::bytes::Regex::new("hit").unwrap();
        let mut acc = mk(GrepMode::Count, 200, 0, 3, 3);
        assert_eq!(acc.before, 0);
        acc.scan("a", b"hit\nhit\nno\n", &re);
        acc.scan("b", b"no\n", &re);
        assert_eq!(acc.items, vec![json!({"path": "a", "count": 2})]);
        let mut acc = mk(GrepMode::Files, 1, 0, 0, 0);
        assert!(acc.scan("a", b"hit\nhit\n", &re));
        assert!(!acc.scan("b", b"hit\n", &re));
        assert_eq!(acc.items, vec![json!({"path": "a"})]);
        assert!(acc.truncated);
    }
}
