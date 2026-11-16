use super::Client;
use crate::desktop::{self, ScreenshotMeta};
use crate::error::{AppError, Result};
use crate::{config, frame, util};
use serde_json::Value;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub fn upgrade_hint(mut error: AppError) -> AppError {
    if error.code == "not_found" && error.message.contains("unknown endpoint") {
        error.message.push_str(
            "; this peer does not support desktop capture; upgrade its agent-cli-x binary first",
        );
    }
    error
}

fn save<R: BufRead>(reader: &mut R, target: &Path, full: bool) -> Result<Value> {
    let header = frame::read_header(reader)?;
    let meta: ScreenshotMeta = serde_json::from_value(header.clone())
        .map_err(|_| AppError::bad_request("invalid screenshot metadata from peer"))?;
    let byte_limit = if full {
        desktop::MAX_IMAGE_BYTES
    } else {
        desktop::PREVIEW_BYTES
    };
    if meta.format != "png"
        || meta.bytes < 33
        || meta.bytes > byte_limit
        || meta.width == 0
        || meta.height == 0
        || u64::from(meta.width) * u64::from(meta.height) > desktop::MAX_PIXELS
        || !meta.scale_x.is_finite()
        || !meta.scale_y.is_finite()
        || meta.scale_x <= 0.0
        || meta.scale_y <= 0.0
        || meta.captured_at.len() > 64
        || meta.window.as_ref().is_some_and(|s| s.len() > 32)
        || meta.warning.as_ref().is_some_and(|s| s.len() > 500)
    {
        return Err(AppError::bad_request(
            "screenshot metadata exceeds image bounds",
        ));
    }
    meta.rect.validate()?;
    let mut prefix = [0u8; 24];
    reader.read_exact(&mut prefix)?;
    if &prefix[..8] != desktop::PNG_SIGNATURE
        || prefix[8..12] != 13u32.to_be_bytes()
        || &prefix[12..16] != b"IHDR"
        || prefix[16..20] != meta.width.to_be_bytes()
        || prefix[20..24] != meta.height.to_be_bytes()
    {
        return Err(AppError::bad_request(
            "PNG header does not match screenshot metadata",
        ));
    }
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    // create_new prevents accidental overwrites, including a file created during the request.
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(target).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            AppError::conflict(format!(
                "{} already exists; choose another --out path",
                target.display()
            ))
        } else {
            e.into()
        }
    })?;
    let saved = (|| -> Result<()> {
        file.write_all(&prefix)?;
        frame::copy_exact(reader, &mut file, (meta.bytes - prefix.len()) as u64)?;
        file.sync_all()?;
        Ok(())
    })();
    drop(file);
    if let Err(error) = saved {
        let _ = fs::remove_file(target);
        return Err(error);
    }
    util::restrict_permissions(target);
    let mut result = serde_json::to_value(meta).map_err(|e| AppError::internal(e.to_string()))?;
    result["path"] = Value::String(target.to_string_lossy().into_owned());
    Ok(result)
}

pub fn screenshot(client: &Client, body: &Value, out: Option<&Path>, full: bool) -> Result<Value> {
    let target: PathBuf = match out {
        Some(path) => std::path::absolute(path)?,
        None => std::path::absolute(config::home_dir().join("screenshots").join(format!(
            "{}-{}.png",
            util::compact_ts(),
            util::random_hex(4)
        )))?,
    };
    if target.exists() {
        return Err(AppError::conflict(format!(
            "{} already exists; choose another --out path",
            target.display()
        )));
    }
    let mut reader = client
        .download("/v1/desktop/screenshot", body, Duration::from_secs(30))
        .map_err(upgrade_hint)?;
    save(&mut reader, &target, full)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;

    fn packet(size: usize, actual: &[u8]) -> Cursor<Vec<u8>> {
        let mut data = frame::header_bytes(
            &json!({"format":"png","bytes":size,"width":1,"height":1,
            "rect":{"x":-100,"y":0,"width":1,"height":1},"dpi":144,"scale_x":1.0,"scale_y":1.0,"captured_at":"test"}),
        );
        data.extend_from_slice(actual);
        Cursor::new(data)
    }

    #[test]
    fn incomplete_or_oversized_images_never_leave_files_or_overwrite() {
        let dir = std::env::temp_dir().join(format!("agent-cli-x-shot-{}", util::random_hex(8)));
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("shot.png");
        let mut prefix = desktop::PNG_SIGNATURE.to_vec();
        prefix.extend_from_slice(&13u32.to_be_bytes());
        prefix.extend_from_slice(b"IHDR");
        prefix.extend_from_slice(&1u32.to_be_bytes());
        prefix.extend_from_slice(&1u32.to_be_bytes());
        assert!(save(&mut packet(64, &prefix), &target, false).is_err());
        assert!(!target.exists());
        assert!(save(
            &mut packet(desktop::PREVIEW_BYTES + 1, desktop::PNG_SIGNATURE),
            &target,
            false
        )
        .is_err());
        assert!(!target.exists());
        fs::write(&target, b"keep").unwrap();
        assert!(save(&mut packet(33, &prefix), &target, false).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"keep");
        fs::remove_dir_all(dir).unwrap();
    }
}
