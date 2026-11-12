//! 回收站：rm 默认把文件移入系统回收站（Windows 回收站 / Linux freedesktop Trash / macOS 废纸篓），
//! rollback 时再从回收站还原，purge 时从回收站彻底删除（Windows、Linux 支持；macOS 需在废纸篓中手动处理）。

use crate::error::{AppError, Result};
use std::path::Path;

/// 移入回收站。失败时返回原因，调用方可退回“备份后删除”。
pub fn move_to_trash(abs: &Path) -> std::result::Result<(), String> {
    trash::delete(abs).map_err(|e| e.to_string())
}

/// 当前 Unix 时间（秒），记录在备份元数据中，用于在回收站里找到本次删除的条目。
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

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

/// 系统记录的删除时间与本机记录的 since / until 之间的容差（秒）。
#[cfg(any(windows, all(unix, not(target_os = "macos")), test))]
const SLACK: i64 = 5;
/// 旧版本的记录没有 until（移入完成的时间）时，假定移入不超过这么久。
#[cfg(any(windows, all(unix, not(target_os = "macos")), test))]
const MAX_AFTER: i64 = 600;

/// 本次删除对应的条目：[since, until] 窗口内最早的一条。同一路径之后再被删除（另一个 op）时，
/// 取最新的会误选到那一次的条目，purge 时就会永久删错。
#[cfg(any(windows, all(unix, not(target_os = "macos")), test))]
fn pick(times: impl Iterator<Item = i64>, since: i64, until: Option<i64>) -> Option<usize> {
    let hi = until.unwrap_or(since + MAX_AFTER) + SLACK;
    times
        .enumerate()
        .filter(|(_, t)| (since - SLACK..=hi).contains(t))
        .min_by_key(|(_, t)| *t)
        .map(|(i, _)| i)
}

/// 在回收站中找到本次删除的 `abs`。
#[cfg(any(windows, all(unix, not(target_os = "macos"))))]
fn find(abs: &Path, since: i64, until: Option<i64>) -> Result<trash::TrashItem> {
    let mut items: Vec<_> = trash::os_limited::list()
        .map_err(|e| AppError::internal(format!("failed to read recycle bin: {e}")))?
        .into_iter()
        .filter(|i| same_path(&i.original_path(), abs))
        .collect();
    pick(items.iter().map(|i| i.time_deleted), since, until)
        .map(|idx| items.swap_remove(idx))
        .ok_or_else(|| {
            AppError::not_found(format!(
                "{} not found in recycle bin (may have been emptied or restored manually)",
                abs.display()
            ))
        })
}

/// 从回收站还原 `abs`。
#[cfg(any(windows, all(unix, not(target_os = "macos"))))]
pub fn restore(abs: &Path, since: i64, until: Option<i64>) -> Result<()> {
    let item = find(abs, since, until)?;
    trash::os_limited::restore_all([item])
        .map_err(|e| AppError::internal(format!("failed to restore from recycle bin: {e}")))
}

/// 从回收站彻底删除 `abs`。返回 false 表示回收站里已经没有它（被清空或手动还原过）。
#[cfg(any(windows, all(unix, not(target_os = "macos"))))]
pub fn purge(abs: &Path, since: i64, until: Option<i64>) -> Result<bool> {
    let item = match find(abs, since, until) {
        Ok(i) => i,
        Err(e) if e.code == "not_found" => return Ok(false),
        Err(e) => return Err(e),
    };
    trash::os_limited::purge_all([item])
        .map_err(|e| AppError::internal(format!("failed to purge from recycle bin: {e}")))?;
    Ok(true)
}

#[cfg(target_os = "macos")]
pub fn restore(abs: &Path, _since: i64, _until: Option<i64>) -> Result<()> {
    Err(AppError::bad_request(format!(
        "macOS cannot restore from Trash automatically; restore {} manually in Trash",
        abs.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::pick;

    #[test]
    fn pick_earliest_in_window() {
        // 本次删除（100 附近）之后同一路径又被删了一次（130）：必须选 100 那条
        assert_eq!(pick([130, 101, 99].into_iter(), 100, Some(101)), Some(2));
        // 系统时间略早于记录时间：在容差内仍能找到
        assert_eq!(pick([96].into_iter(), 100, Some(100)), Some(0));
        // 本次条目已被还原，之后又删了一次（30 秒后）：有 until 时不会误选
        assert_eq!(pick([130].into_iter(), 100, Some(101)), None);
        // 旧记录没有 until：按 600 秒上限
        assert_eq!(pick([130].into_iter(), 100, None), Some(0));
        assert_eq!(pick([100 + 606].into_iter(), 100, None), None);
        // 更早的删除不属于本次
        assert_eq!(pick([50].into_iter(), 100, Some(100)), None);
    }
}

#[cfg(target_os = "macos")]
pub fn purge(abs: &Path, _since: i64, _until: Option<i64>) -> Result<bool> {
    Err(AppError::bad_request(format!(
        "macOS cannot purge a single Trash entry automatically; delete {} manually in Trash",
        abs.display()
    )))
}
