//! 回收站：rm 默认把文件移入系统回收站（Windows 回收站 / Linux freedesktop Trash / macOS 废纸篓），
//! rollback 时再从回收站还原（Windows、Linux 支持；macOS 需手动从废纸篓恢复）。

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

/// 从回收站还原 `abs`：取原路径相同、删除时间不早于 `since` 的最近一条。
#[cfg(any(windows, all(unix, not(target_os = "macos"))))]
pub fn restore(abs: &Path, since: i64) -> Result<()> {
    use trash::os_limited;
    let items =
        os_limited::list().map_err(|e| AppError::internal(format!("读取回收站失败: {e}")))?;
    // 留几秒余量：删除时间由系统记录，可能与本机记录的时间有细微差异
    let item = items
        .into_iter()
        .filter(|i| i.time_deleted >= since - 5 && same_path(&i.original_path(), abs))
        .max_by_key(|i| i.time_deleted)
        .ok_or_else(|| {
            AppError::not_found(format!(
                "回收站中找不到 {}（可能已被清空或手动还原）",
                abs.display()
            ))
        })?;
    os_limited::restore_all([item])
        .map_err(|e| AppError::internal(format!("从回收站还原失败: {e}")))
}

#[cfg(target_os = "macos")]
pub fn restore(abs: &Path, _since: i64) -> Result<()> {
    Err(AppError::bad_request(format!(
        "macOS 不支持自动从废纸篓还原，请在废纸篓中手动恢复 {}",
        abs.display()
    )))
}
