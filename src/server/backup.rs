//! 写操作前的备份与回滚：`<home>/backup/<op_id>/{meta.json, 0, 1, ...}`，默认保留 7 天。
//! 一次操作（write / edit / put / rm）可以涉及多个文件，rollback 一次全部撤销。

use super::workspace::Access;
use super::Ctx;
use crate::error::{AppError, Result};
use crate::util;
use serde::Deserialize;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

pub const RETAIN_DAYS: u64 = 7;

fn op_dir(home: &Path, op_id: &str) -> Result<PathBuf> {
    let ok = !op_id.is_empty()
        && op_id.len() <= 64
        && op_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    if !ok {
        return Err(AppError::bad_request(format!("op_id `{op_id}` 无效")));
    }
    Ok(home.join("backup").join(op_id))
}

pub struct Op {
    pub id: String,
    dir: PathBuf,
    entries: Vec<Value>,
}

impl Op {
    pub fn new(home: &Path) -> Result<Self> {
        let id = format!("{}-{}", util::compact_ts(), util::random_hex(3));
        let dir = op_dir(home, &id)?;
        fs::create_dir_all(&dir)?;
        Ok(Self {
            id,
            dir,
            entries: Vec::new(),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 在修改 `abs` 之前调用。文件不存在时记录 existed=false，回滚时删除新建的文件。
    pub fn add(&mut self, abs: &Path, display: &str) -> Result<()> {
        let path = abs.to_string_lossy().to_string();
        if self.entries.iter().any(|e| e["path"] == path) {
            return Ok(());
        }
        let idx = self.entries.len();
        let existed = abs.is_file();
        if existed {
            fs::copy(abs, self.dir.join(idx.to_string()))?;
        }
        self.entries
            .push(json!({"path": path, "display": display, "existed": existed, "idx": idx}));
        self.save()
    }

    /// 创建目录 `abs`（含缺失的上级，不越过工作区根 `root`），并记录本次新建的目录，
    /// 回滚时在删除新建文件之后删除这些目录（仅当已为空）。
    pub fn create_dirs(&mut self, root: &Path, abs: &Path, ws: &str) -> Result<()> {
        let mut missing = Vec::new();
        let mut cur = abs;
        while !cur.exists() && cur.starts_with(root) && cur != root {
            missing.push(cur.to_path_buf());
            match cur.parent() {
                Some(p) => cur = p,
                None => break,
            }
        }
        fs::create_dir_all(abs)?;
        // 外层先记录：回滚时逆序处理，内层目录先删
        for d in missing.into_iter().rev() {
            let path = d.to_string_lossy().to_string();
            if self.entries.iter().any(|e| e["path"] == path) {
                continue;
            }
            let rel = d
                .strip_prefix(root)
                .map(|r| r.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            let idx = self.entries.len();
            self.entries.push(json!({
                "path": path, "display": format!("{ws}:{rel}"), "existed": false, "dir": true, "idx": idx
            }));
        }
        self.save()
    }

    /// 删除目录前调用：记录目录本身，回滚时重新创建。
    pub fn add_removed_dir(&mut self, abs: &Path, display: &str) -> Result<()> {
        let idx = self.entries.len();
        self.entries.push(json!({
            "path": abs.to_string_lossy(), "display": display, "existed": true, "dir": true, "idx": idx
        }));
        self.save()
    }

    /// 已移入系统回收站后调用：记录原路径与删除时间，回滚时从回收站还原。
    pub fn add_trashed(&mut self, abs: &Path, display: &str, since: i64) -> Result<()> {
        let idx = self.entries.len();
        self.entries.push(json!({
            "path": abs.to_string_lossy(), "display": display, "existed": true, "idx": idx,
            "trash": true, "since": since
        }));
        self.save()
    }

    /// 删除符号链接前调用：记录链接目标，回滚时尽力重建（Windows 上可能需要权限）。
    pub fn add_removed_link(&mut self, abs: &Path, display: &str) -> Result<()> {
        let target = fs::read_link(abs)?;
        let target_is_dir = abs.is_dir();
        let idx = self.entries.len();
        self.entries.push(json!({
            "path": abs.to_string_lossy(), "display": display, "existed": true, "idx": idx,
            "link": target.to_string_lossy(), "link_dir": target_is_dir
        }));
        self.save()
    }

    fn save(&self) -> Result<()> {
        let meta = json!({"op_id": self.id, "ts": util::now_iso(), "entries": self.entries});
        fs::write(
            self.dir.join("meta.json"),
            serde_json::to_vec_pretty(&meta).unwrap_or_default(),
        )?;
        Ok(())
    }

    /// 没有记录任何文件时删除空备份目录。
    pub fn finish(self) {
        if self.entries.is_empty() {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}

#[derive(Deserialize)]
struct RollbackReq {
    op_id: String,
}

pub fn rollback(ctx: &Ctx, body: Value) -> Result<Value> {
    let req: RollbackReq =
        serde_json::from_value(body).map_err(|e| AppError::bad_request(e.to_string()))?;
    let dir = op_dir(&ctx.st.home, &req.op_id)?;
    let meta: Value = fs::read(dir.join("meta.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| {
            AppError::not_found(format!(
                "找不到操作 {}（可能已超过 {RETAIN_DAYS} 天被清理）",
                req.op_id
            ))
        })?;
    // 兼容 0.1.0 的单文件格式
    let entries: Vec<(Value, PathBuf)> = match meta["entries"].as_array() {
        Some(list) => list
            .iter()
            .map(|e| {
                (
                    e.clone(),
                    dir.join(e["idx"].as_u64().unwrap_or(0).to_string()),
                )
            })
            .collect(),
        None => vec![(meta.clone(), dir.join("content"))],
    };
    // 先整体校验，再动手，避免回滚到一半被拒。每条记录按回滚时的实际动作检查 CRUD 权限。
    let mut plan = Vec::new();
    for (e, backup_file) in entries.iter().rev() {
        let abs = PathBuf::from(e["path"].as_str().unwrap_or_default());
        let (ws, root) = ctx
            .st
            .ws
            .owner_of(&abs)
            .ok_or_else(|| AppError::denied("该操作的文件已不在任何工作区内"))?;
        let existed = e["existed"].as_bool().unwrap_or(false);
        let access = match (existed, abs.exists() || abs.is_symlink()) {
            // 原本不存在：回滚 = 删除
            (false, _) => Access::Delete,
            // 原本存在、现在还在：恢复内容 = 修改（目录无需动作，只要可读）
            (true, true) if e["dir"] == true => Access::Read,
            (true, true) => Access::Update,
            // 原本存在、现在没了（被 rm 删除）：重建 = 新建
            (true, false) => Access::Create,
        };
        let display = e["display"].as_str().unwrap_or_default();
        if e["trash"] == true && (abs.exists() || abs.is_symlink()) {
            return Err(AppError::conflict(format!(
                "{display} 处已存在同名文件或目录，无法从回收站还原；请先移走它"
            )));
        }
        ctx.st
            .ws
            .check_abs(&ws, &root, &abs, display, access)
            .map_err(|err| AppError::denied(format!("{}，无法回滚", err.message)))?;
        plan.push((e, abs, backup_file));
    }
    let mut files = Vec::new();
    for (e, abs, backup_file) in plan {
        let existed = e["existed"].as_bool().unwrap_or(false);
        let action = if e["trash"] == true {
            super::recycle::restore(&abs, e["since"].as_i64().unwrap_or(0))?;
            "untrashed"
        } else if e["dir"] == true && !existed {
            // 只删空目录：回滚后目录里若还有别的文件（非本次操作写入），保留
            if abs.is_dir() && fs::remove_dir(&abs).is_ok() {
                "rmdir"
            } else {
                "kept_dir"
            }
        } else if e["dir"] == true {
            if abs.is_dir() {
                "kept_dir"
            } else {
                fs::create_dir_all(&abs)?;
                "recreated_dir"
            }
        } else if let Some(target) = e["link"].as_str() {
            if let Some(p) = abs.parent() {
                fs::create_dir_all(p)?;
            }
            if abs.is_symlink() || abs.exists() {
                "kept_link"
            } else if make_symlink(Path::new(target), &abs, e["link_dir"] == true).is_ok() {
                "relinked"
            } else {
                "link_failed"
            }
        } else if existed {
            let data = fs::read(backup_file)?;
            if let Some(p) = abs.parent() {
                fs::create_dir_all(p)?;
            }
            util::atomic_write(&abs, &data)?;
            "restored"
        } else {
            if abs.is_file() {
                fs::remove_file(&abs)?;
            }
            "deleted"
        };
        files.push(json!({"path": e["display"], "action": action}));
    }
    ctx.st.audit.log(
        &ctx.ip,
        &ctx.caller,
        "rollback",
        true,
        json!({"op_id": req.op_id, "files": files.len()}),
    );
    Ok(json!({"op_id": req.op_id, "files": files}))
}

#[cfg(unix)]
fn make_symlink(target: &Path, link: &Path, _dir: bool) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn make_symlink(target: &Path, link: &Path, dir: bool) -> std::io::Result<()> {
    if dir {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    }
}
