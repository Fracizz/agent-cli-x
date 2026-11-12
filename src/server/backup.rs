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
        return Err(AppError::bad_request(format!("invalid op_id `{op_id}`")));
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

    /// 已移入系统回收站后调用：保存本次的确切条目标识，禁止按删除时间猜测。
    pub fn add_trashed(
        &mut self,
        abs: &Path,
        display: &str,
        receipt: &super::recycle::Receipt,
    ) -> Result<()> {
        let idx = self.entries.len();
        let mut entry = json!({
            "path": abs.to_string_lossy(), "display": display, "existed": true, "idx": idx,
            "trash": true, "trash_ref": receipt
        });
        if !receipt.identified() {
            entry["trash_warning"] = json!("recycle-bin identity unavailable; restore or delete this item manually in the system recycle bin");
        }
        self.entries.push(entry);
        self.save().map_err(|e| AppError::io(format!(
            "{display} was already moved to the recycle bin, but saving operation {} failed: {}; restore it manually in the system recycle bin",
            self.id, e.message
        )))
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
        util::atomic_write(
            &self.dir.join("meta.json"),
            &serde_json::to_vec_pretty(&meta).unwrap_or_default(),
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

/// 读取一次操作的备份记录：返回 (备份目录, [(条目, 备份文件路径)])。
fn load_op(home: &Path, op_id: &str) -> Result<(PathBuf, Vec<(Value, PathBuf)>)> {
    let dir = op_dir(home, op_id)?;
    let meta: Value = fs::read(dir.join("meta.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| {
            AppError::not_found(format!(
                "op {op_id} not found (may be older than {RETAIN_DAYS} days and cleaned, or purged)"
            ))
        })?;
    // 兼容 0.1.0 的单文件格式
    let entries = match meta["entries"].as_array() {
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
    Ok((dir, entries))
}

/// 按回滚时的实际动作重新解析真实路径并校验 CRUD。
fn rollback_target(ws: &super::workspace::Workspaces, e: &Value) -> Result<PathBuf> {
    let recorded = PathBuf::from(e["path"].as_str().unwrap_or_default());
    let existed = e["existed"].as_bool().unwrap_or(false);
    let entry = e["link"].is_string() || (!existed && e["dir"] != true) || e["trash"] == true;
    let r = ws.resolve_recorded(&recorded, entry)?;
    let present = r.abs.exists() || r.abs.is_symlink();
    let access = match (existed, present) {
        (false, _) => Access::Delete,
        (true, true) if e["dir"] == true => Access::Read,
        (true, true) => Access::Update,
        (true, false) => Access::Create,
    };
    if e["trash"] == true && present {
        return Err(AppError::conflict(format!(
            "{} already exists; cannot restore from recycle bin; move it away first",
            r.display()
        )));
    }
    ws.check(&r, access)
        .map_err(|err| AppError::denied(format!("{}; cannot roll back", err.message)))?;
    Ok(r.abs)
}

pub fn rollback(ctx: &Ctx, body: Value) -> Result<Value> {
    let req: RollbackReq =
        serde_json::from_value(body).map_err(|e| AppError::bad_request(e.to_string()))?;
    let (_, entries) = load_op(&ctx.st.home, &req.op_id)?;
    // 先整体校验，再动手，避免回滚到一半被拒。每条记录按回滚时的实际动作检查 CRUD 权限。
    let mut plan = Vec::new();
    for (e, backup_file) in entries.iter().rev() {
        let abs = rollback_target(&ctx.st.ws, e)?;
        plan.push((e, abs, backup_file));
    }
    let mut files = Vec::new();
    for (e, _, backup_file) in plan {
        // 前面某个条目或其他调用可能已改变父目录，再解析一次后才执行。
        let abs = rollback_target(&ctx.st.ws, e)?;
        let existed = e["existed"].as_bool().unwrap_or(false);
        let action = if e["trash"] == true {
            super::recycle::restore(&abs, &e["trash_ref"])?;
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
            if abs.is_symlink() {
                remove_link(&abs)?;
            } else if abs.is_file() {
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

/// 彻底清除一次操作留下的东西：回收站里的条目被永久删除，备份副本被删除。
/// 之后该操作无法再 rollback。回收站条目要求原路径有删除权限（与 rm 相同）。
pub fn purge(ctx: &Ctx, body: Value) -> Result<Value> {
    let req: RollbackReq =
        serde_json::from_value(body).map_err(|e| AppError::bad_request(e.to_string()))?;
    let (dir, entries) = load_op(&ctx.st.home, &req.op_id)?;
    // 先整体校验权限，再动手
    for (e, _) in &entries {
        if e["trash"] != true {
            continue;
        }
        let abs = PathBuf::from(e["path"].as_str().unwrap_or_default());
        let r = ctx.st.ws.resolve_recorded(&abs, true)?;
        ctx.st
            .ws
            .check(&r, Access::Delete)
            .map_err(|err| AppError::denied(format!("{}; cannot purge", err.message)))?;
    }
    let mut files = Vec::new();
    let mut backups = 0u64;
    for (e, backup_file) in &entries {
        let display = e["display"].clone();
        if e["trash"] == true {
            let abs = PathBuf::from(e["path"].as_str().unwrap_or_default());
            let r = ctx.st.ws.resolve_recorded(&abs, true)?;
            ctx.st.ws.check(&r, Access::Delete)?;
            let gone = super::recycle::purge(&r.abs, &e["trash_ref"])?;
            files.push(json!({
                "path": display,
                "action": if gone { "purged" } else { "not_in_trash" },
            }));
        } else if backup_file.is_file() {
            backups += 1;
        }
    }
    let freed = util::dir_size(&dir);
    fs::remove_dir_all(&dir)?;
    ctx.st.audit.log(
        &ctx.ip,
        &ctx.caller,
        "purge",
        true,
        json!({"op_id": req.op_id, "files": files.len(), "backups": backups}),
    );
    Ok(json!({"op_id": req.op_id, "files": files, "backups": backups, "backup_bytes": freed}))
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

fn remove_link(abs: &Path) -> std::io::Result<()> {
    match fs::remove_file(abs) {
        Ok(()) => Ok(()),
        Err(e) if cfg!(windows) => fs::remove_dir(abs).map_err(|_| e),
        Err(e) => Err(e),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::workspace::Workspaces;
    use super::*;
    use crate::config::{Config, Policy};
    use std::collections::BTreeMap;

    struct Fixture {
        base: PathBuf,
        root: PathBuf,
        ws: Workspaces,
    }

    impl Fixture {
        fn new(deny: &[&str]) -> Self {
            let base =
                std::env::temp_dir().join(format!("agent-cli-x-rollback-{}", util::random_hex(6)));
            let root = base.join("proj");
            fs::create_dir_all(root.join("safe")).unwrap();
            fs::create_dir_all(root.join("secrets")).unwrap();
            fs::create_dir_all(base.join("outside")).unwrap();
            fs::create_dir_all(base.join("home")).unwrap();
            let cfg = Config {
                listen: String::new(),
                token: "x".repeat(20),
                node_name: None,
                node_id: None,
                ai_cmd: String::new(),
                default_workspace: None,
                workspaces: BTreeMap::from([(
                    "proj".to_string(),
                    root.to_string_lossy().to_string(),
                )]),
                policy: Policy {
                    deny_paths: deny.iter().map(|x| x.to_string()).collect(),
                    ..Policy::default()
                },
            };
            let ws = Workspaces::new(&cfg, &base.join("home")).unwrap();
            Self { base, root, ws }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    #[cfg(unix)]
    #[test]
    fn rollback_rechecks_existing_and_missing_ancestor_targets() {
        let f = Fixture::new(&[]);
        fs::write(f.base.join("outside/existing.txt"), "untouched").unwrap();
        fs::rename(f.root.join("safe"), f.root.join("safe-before")).unwrap();
        std::os::unix::fs::symlink(f.base.join("outside"), f.root.join("safe")).unwrap();
        for (name, existed) in [("existing.txt", true), ("missing.txt", false)] {
            let e = json!({"path": f.root.join("safe").join(name), "existed": existed});
            assert_eq!(
                rollback_target(&f.ws, &e).err().unwrap().code,
                "policy_denied"
            );
        }
        assert_eq!(
            fs::read_to_string(f.base.join("outside/existing.txt")).unwrap(),
            "untouched"
        );
        assert!(!f.base.join("outside/missing.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn rollback_rechecks_deny_rules_after_internal_redirect() {
        let f = Fixture::new(&["secrets"]);
        fs::rename(f.root.join("safe"), f.root.join("safe-before")).unwrap();
        std::os::unix::fs::symlink(f.root.join("secrets"), f.root.join("safe")).unwrap();
        let e = json!({"path": f.root.join("safe/new.txt"), "existed": false});
        assert_eq!(
            rollback_target(&f.ws, &e).err().unwrap().code,
            "policy_denied"
        );
    }

    #[cfg(unix)]
    #[test]
    fn deleting_created_leaf_link_checks_the_link_not_its_target() {
        let f = Fixture::new(&[]);
        let outside = f.base.join("outside/existing.txt");
        fs::write(&outside, "untouched").unwrap();
        let link = f.root.join("safe/link.txt");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let e = json!({"path": link, "existed": false});
        let target = rollback_target(&f.ws, &e).unwrap();
        assert_eq!(target, link);
        remove_link(&target).unwrap();
        assert!(!link.is_symlink());
        assert_eq!(fs::read_to_string(outside).unwrap(), "untouched");
    }
}
