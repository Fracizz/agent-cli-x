//! 写操作前的备份与回滚：`<home>/backup/<op_id>/{meta.json, 0, 1, ...}`，默认保留 7 天。
//! 一次操作（write / edit / put）可以涉及多个文件，rollback 一次全部撤销。

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
    // 先整体校验，再动手，避免回滚到一半被拒
    let mut plan = Vec::new();
    for (e, backup_file) in entries.iter().rev() {
        let abs = PathBuf::from(e["path"].as_str().unwrap_or_default());
        let (ws, root) = ctx
            .st
            .ws
            .owner_of(&abs)
            .ok_or_else(|| AppError::denied("该操作的文件已不在任何工作区内"))?;
        if let Some(rule) = ctx.st.ws.denied_by(&ws, &root, &abs) {
            return Err(AppError::denied(format!(
                "路径命中黑名单 `{rule}`，禁止回滚"
            )));
        }
        plan.push((e, abs, backup_file));
    }
    let mut files = Vec::new();
    for (e, abs, backup_file) in plan {
        let action = if e["existed"].as_bool().unwrap_or(false) {
            let data = fs::read(backup_file)?;
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
        "rollback",
        true,
        json!({"op_id": req.op_id, "files": files.len()}),
    );
    Ok(json!({"op_id": req.op_id, "files": files}))
}
