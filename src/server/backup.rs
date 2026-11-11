//! 写操作前的备份与回滚：`<home>/backup/<op_id>/{meta.json, content}`，默认保留 7 天。

use super::Ctx;
use crate::error::{AppError, Result};
use crate::util;
use serde::Deserialize;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

pub const RETAIN_DAYS: u64 = 7;

pub fn new_op_id() -> String {
    format!("{}-{}", util::compact_ts(), util::random_hex(3))
}

fn op_dir(home: &Path, op_id: &str) -> Result<PathBuf> {
    let ok = !op_id.is_empty()
        && op_id.len() <= 64
        && op_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    if !ok {
        return Err(AppError::bad_request(format!("op_id `{op_id}` 无效")));
    }
    Ok(home.join("backup").join(op_id))
}

/// 在修改 `abs` 之前调用。文件不存在时记录 existed=false，回滚时删除新建的文件。
pub fn backup(home: &Path, op_id: &str, abs: &Path, display: &str) -> Result<()> {
    let dir = op_dir(home, op_id)?;
    fs::create_dir_all(&dir)?;
    let existed = abs.is_file();
    if existed {
        fs::copy(abs, dir.join("content"))?;
    }
    let meta = json!({
        "op_id": op_id,
        "path": abs.to_string_lossy(),
        "display": display,
        "existed": existed,
        "ts": util::now_iso(),
    });
    fs::write(
        dir.join("meta.json"),
        serde_json::to_vec_pretty(&meta).unwrap_or_default(),
    )?;
    Ok(())
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
    let abs = PathBuf::from(meta["path"].as_str().unwrap_or_default());
    let display = meta["display"].as_str().unwrap_or_default().to_string();
    // 回滚同样受工作区边界与黑名单约束
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
    let action = if meta["existed"].as_bool().unwrap_or(false) {
        let data = fs::read(dir.join("content"))?;
        util::atomic_write(&abs, &data)?;
        "restored"
    } else {
        if abs.is_file() {
            fs::remove_file(&abs)?;
        }
        "deleted"
    };
    ctx.st.audit.log(
        &ctx.ip,
        "rollback",
        true,
        json!({"op_id": req.op_id, "path": display, "action": action}),
    );
    Ok(json!({"op_id": req.op_id, "path": display, "action": action}))
}
