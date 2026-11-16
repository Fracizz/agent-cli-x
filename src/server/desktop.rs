//! Desktop permissions are independent of workspace CRUD permissions.

use super::Ctx;
use crate::desktop::{self, CaptureRequest, WindowsRequest};
use crate::error::{AppError, Result};
use crate::{frame, http};
use serde_json::{json, Value};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

static CAPTURE_ACTIVE: AtomicBool = AtomicBool::new(false);
struct CapturePermit;
impl CapturePermit {
    fn acquire() -> Result<Self> {
        CAPTURE_ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| {
                AppError::new(
                    "busy",
                    "another desktop capture is in progress; retry after it finishes",
                )
            })?;
        Ok(Self)
    }
}
impl Drop for CapturePermit {
    fn drop(&mut self) {
        CAPTURE_ACTIVE.store(false, Ordering::Release);
    }
}

fn check(ctx: &Ctx) -> Result<()> {
    if !desktop::supported() {
        return Err(AppError::new("unsupported", "native desktop capture currently requires a Windows peer; run `agent-cli-x peer list` to choose one"));
    }
    if !ctx.st.cfg.policy.desktop_capture {
        return Err(AppError::denied("desktop observation is disabled; the peer owner must set [policy] desktop_capture = true in config.toml, then restart serve"));
    }
    Ok(())
}

pub fn windows(ctx: &Ctx, body: Value) -> Result<Value> {
    check(ctx)?;
    let req: WindowsRequest = serde_json::from_value(body)
        .map_err(|e| AppError::bad_request(format!("bad desktop parameters: {e}")))?;
    let page = desktop::list_windows(req)?;
    ctx.st.audit.log(
        &ctx.ip,
        &ctx.caller,
        "desktop_windows",
        true,
        json!({"returned":page.windows.len(),"total":page.total}),
    );
    serde_json::to_value(page).map_err(|e| AppError::internal(e.to_string()))
}

pub fn screenshot<W: Write>(ctx: &Ctx, body: Value, writer: &mut W) {
    let capture = (|| {
        check(ctx)?;
        let req: CaptureRequest = serde_json::from_value(body)
            .map_err(|e| AppError::bad_request(format!("bad desktop parameters: {e}")))?;
        let permit = CapturePermit::acquire()?;
        let result = desktop::capture(req)?;
        if result.png.len() as u64
            > ctx
                .st
                .cfg
                .policy
                .max_transfer_mb
                .saturating_mul(1024 * 1024)
        {
            return Err(AppError::denied(
                "screenshot exceeds policy.max_transfer_mb; use a smaller --region or --max-edge",
            ));
        }
        Ok((result, permit))
    })();
    match capture {
        Ok((image, _permit)) => {
            let meta = serde_json::to_value(&image.meta).unwrap_or_default();
            let sent = (|| -> std::io::Result<()> {
                http::write_head(writer, 200, "application/octet-stream")?;
                frame::write_header(writer, &meta)?;
                writer.write_all(&image.png)?;
                writer.flush()
            })();
            ctx.st.audit.log(
                &ctx.ip,
                &ctx.caller,
                "desktop_screenshot",
                sent.is_ok(),
                meta,
            );
        }
        Err(error) => {
            ctx.st.audit.log(
                &ctx.ip,
                &ctx.caller,
                "desktop_screenshot",
                false,
                json!({"code":error.code}),
            );
            let _ = http::write_error(writer, &error);
        }
    }
}
