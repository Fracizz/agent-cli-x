//! 回收站：按系统条目标识恢复 / 清理，绝不通过相邻删除的时间窗口猜测。

use crate::error::{AppError, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

/// 移入后的确切条目。无法唯一识别时保留空 ID，后续恢复 / purge 会明确拒绝。
#[derive(Serialize, Deserialize)]
pub struct Receipt {
    id: Option<String>,
    deleted: Option<i64>,
    #[serde(default)]
    identity: Option<String>,
}

impl Receipt {
    pub fn identified(&self) -> bool {
        self.id.as_deref().is_some_and(|id| !id.is_empty())
            && self.deleted.is_some()
            && (!cfg!(all(unix, not(target_os = "macos"))) || self.identity.is_some())
    }
}

#[cfg(any(windows, all(unix, not(target_os = "macos"))))]
static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(any(windows, all(unix, not(target_os = "macos"))))]
fn same_path(a: &Path, b: &Path) -> bool {
    let n = |p: &Path| {
        let s = dunce::simplified(p).to_string_lossy().replace('\\', "/");
        let s = s.trim_end_matches('/').to_string();
        if cfg!(windows) {
            s.to_lowercase()
        } else {
            s
        }
    };
    n(a) == n(b)
}

#[cfg(any(windows, all(unix, not(target_os = "macos"))))]
fn item_id(item: &trash::TrashItem) -> String {
    crate::util::hex(item.id.as_encoded_bytes())
}

/// freedesktop 的条目 ID（.trashinfo 路径）可能被后续删除复用；同时绑定其 inode 与时间。
#[cfg(all(unix, not(target_os = "macos")))]
fn item_identity(item: &trash::TrashItem) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::symlink_metadata(Path::new(&item.id)).ok()?;
    Some(format!(
        "{}:{}:{}:{}:{}:{}",
        m.dev(),
        m.ino(),
        m.ctime(),
        m.ctime_nsec(),
        m.mtime(),
        m.mtime_nsec()
    ))
}

#[cfg(windows)]
fn item_identity(_item: &trash::TrashItem) -> Option<String> {
    None
}

#[cfg(any(windows, all(unix, not(target_os = "macos"))))]
fn newly_trashed<'a>(
    abs: &Path,
    before: &[trash::TrashItem],
    after: &'a [trash::TrashItem],
) -> Option<&'a trash::TrashItem> {
    let mut added = after.iter().filter(|item| {
        same_path(&item.original_path(), abs) && !before.iter().any(|old| old.id == item.id)
    });
    let only = added.next()?;
    added.next().is_none().then_some(only)
}

/// 删除前先列举已有条目，删除后只记录本次新增的唯一系统 ID。
/// 删除后无法列举 / 唯一识别时也返回 Ok：此时原文件已被移动，不能再尝试备份后删除。
#[cfg(any(windows, all(unix, not(target_os = "macos"))))]
pub fn move_to_trash(abs: &Path) -> std::result::Result<Receipt, String> {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let before = trash::os_limited::list().map_err(|e| e.to_string())?;
    trash::delete(abs).map_err(|e| e.to_string())?;
    let after = trash::os_limited::list().unwrap_or_default();
    let item = newly_trashed(abs, &before, &after);
    let receipt = Receipt {
        id: item.map(item_id),
        deleted: item.map(|i| i.time_deleted),
        identity: item.and_then(item_identity),
    };
    #[cfg(unix)]
    if receipt.identity.is_none() {
        return Ok(Receipt {
            id: None,
            deleted: None,
            identity: None,
        });
    }
    Ok(receipt)
}

#[cfg(target_os = "macos")]
pub fn move_to_trash(abs: &Path) -> std::result::Result<Receipt, String> {
    trash::delete(abs).map_err(|e| e.to_string())?;
    Ok(Receipt {
        id: None,
        deleted: None,
        identity: None,
    })
}

#[cfg(any(test, not(target_os = "macos")))]
fn parse_receipt(receipt: &Value) -> Result<Receipt> {
    serde_json::from_value::<Receipt>(receipt.clone()).ok().filter(Receipt::identified).ok_or_else(|| AppError::conflict(
        "recycle-bin entry has no verified identity (legacy or ambiguous operation); restore or delete it manually in the system recycle bin"
    ))
}

#[cfg(any(windows, all(unix, not(target_os = "macos"))))]
fn receipt_matches(item: &trash::TrashItem, receipt: &Receipt) -> bool {
    receipt.id.as_deref() == Some(item_id(item).as_str())
        && receipt.deleted == Some(item.time_deleted)
        && receipt.identity == item_identity(item)
}

#[cfg(any(windows, all(unix, not(target_os = "macos"))))]
fn find(abs: &Path, receipt: &Value) -> Result<trash::TrashItem> {
    let receipt = parse_receipt(receipt)?;
    trash::os_limited::list()
        .map_err(|e| AppError::internal(format!("failed to read recycle bin: {e}")))?
        .into_iter()
        .find(|i| same_path(&i.original_path(), abs) && receipt_matches(i, &receipt))
        .ok_or_else(|| AppError::not_found(format!(
            "verified recycle-bin entry for {} not found (may have been emptied or restored manually)", abs.display()
        )))
}

#[cfg(any(windows, all(unix, not(target_os = "macos"))))]
pub fn restore(abs: &Path, receipt: &Value) -> Result<()> {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let item = find(abs, receipt)?;
    trash::os_limited::restore_all([item])
        .map_err(|e| AppError::internal(format!("failed to restore from recycle bin: {e}")))
}

#[cfg(any(windows, all(unix, not(target_os = "macos"))))]
pub fn purge(abs: &Path, receipt: &Value) -> Result<bool> {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let item = match find(abs, receipt) {
        Ok(i) => i,
        Err(e) if e.code == "not_found" => return Ok(false),
        Err(e) => return Err(e),
    };
    trash::os_limited::purge_all([item])
        .map_err(|e| AppError::internal(format!("failed to purge from recycle bin: {e}")))?;
    Ok(true)
}

#[cfg(target_os = "macos")]
pub fn restore(abs: &Path, _receipt: &Value) -> Result<()> {
    Err(AppError::bad_request(format!(
        "macOS cannot restore from Trash automatically; restore {} manually in Trash",
        abs.display()
    )))
}

#[cfg(target_os = "macos")]
pub fn purge(abs: &Path, _receipt: &Value) -> Result<bool> {
    Err(AppError::bad_request(format!(
        "macOS cannot purge a single Trash entry automatically; delete {} manually in Trash",
        abs.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_and_unresolved_receipts_are_refused() {
        for value in [
            Value::Null,
            serde_json::json!({"id": null, "deleted": null}),
        ] {
            assert_eq!(parse_receipt(&value).err().unwrap().code, "conflict");
        }
    }

    #[cfg(any(windows, all(unix, not(target_os = "macos"))))]
    fn item(id: &str, deleted: i64) -> trash::TrashItem {
        trash::TrashItem {
            id: id.into(),
            original_parent: std::env::temp_dir(),
            name: "same.txt".into(),
            time_deleted: deleted,
        }
    }

    #[cfg(any(windows, all(unix, not(target_os = "macos"))))]
    #[test]
    fn new_id_distinguishes_same_path_and_same_second() {
        let first = item("first", 100);
        let second = item("second", 100);
        let abs = first.original_path();
        let before = vec![first.clone()];
        let after = vec![second.clone(), first];
        assert_eq!(newly_trashed(&abs, &before, &after).unwrap().id, second.id);
        let ambiguous = vec![second, item("third", 100)];
        assert!(newly_trashed(&abs, &before, &ambiguous).is_none());
        assert!(newly_trashed(&abs, &before, &before).is_none());
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn reused_info_path_does_not_match_old_receipt() {
        let dir = std::env::temp_dir().join(format!(
            "agent-cli-x-trash-id-{}",
            crate::util::random_hex(6)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("same.trashinfo");
        std::fs::write(&path, b"old").unwrap();
        let mut item = item("", 100);
        item.id = path.as_os_str().to_os_string();
        let receipt = Receipt {
            id: Some(item_id(&item)),
            deleted: Some(100),
            identity: item_identity(&item),
        };
        assert!(receipt_matches(&item, &receipt));
        // 保留旧 inode，使随后复用同一路径的新文件必然具有不同身份。
        std::fs::rename(&path, dir.join("previous")).unwrap();
        std::fs::write(&path, b"new").unwrap();
        assert!(!receipt_matches(&item, &receipt));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
