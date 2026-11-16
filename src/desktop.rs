//! Read-only desktop protocol and bounds. Coordinates are physical desktop pixels.

#[cfg(any(windows, test))]
use crate::client::output::clip_line;
use crate::error::{AppError, Result};
use serde::{Deserialize, Serialize};

#[cfg(windows)]
mod windows;

pub const PREVIEW_EDGE: u32 = 1600;
pub const PREVIEW_BYTES: usize = 1024 * 1024;
pub const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_PIXELS: u64 = 64 * 1024 * 1024;
#[cfg(any(windows, test))]
pub const PAGE_BYTES: usize = 32 * 1024;
#[cfg(any(windows, test))]
pub const FULL_PAGE_BYTES: usize = 8 * 1024 * 1024;
#[cfg(any(windows, test))]
pub const TITLE_CHARS: usize = 200;
pub const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn validate(self) -> Result<Self> {
        if self.width == 0
            || self.height == 0
            || self.width > 32768
            || self.height > 32768
            || u64::from(self.width) * u64::from(self.height) > MAX_PIXELS
            || i64::from(self.x) + i64::from(self.width) > i64::from(i32::MAX)
            || i64::from(self.y) + i64::from(self.height) > i64::from(i32::MAX)
        {
            return Err(AppError::bad_request("invalid capture region (positive dimensions, <=32768 per edge / 64M pixels); use a smaller --region X,Y,W,H"));
        }
        Ok(self)
    }

    #[cfg(any(windows, test))]
    pub fn contains(self, other: Self) -> bool {
        i64::from(other.x) >= i64::from(self.x)
            && i64::from(other.y) >= i64::from(self.y)
            && i64::from(other.x) + i64::from(other.width)
                <= i64::from(self.x) + i64::from(self.width)
            && i64::from(other.y) + i64::from(other.height)
                <= i64::from(self.y) + i64::from(self.height)
    }

    #[cfg(any(windows, test))]
    pub fn intersect(self, other: Self) -> Option<Self> {
        let x = i64::from(self.x).max(i64::from(other.x));
        let y = i64::from(self.y).max(i64::from(other.y));
        let right = (i64::from(self.x) + i64::from(self.width))
            .min(i64::from(other.x) + i64::from(other.width));
        let bottom = (i64::from(self.y) + i64::from(self.height))
            .min(i64::from(other.y) + i64::from(other.height));
        (right > x && bottom > y).then_some(Self {
            x: x as i32,
            y: y as i32,
            width: (right - x).max(0) as u32,
            height: (bottom - y).max(0) as u32,
        })
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureRequest {
    #[serde(default)]
    pub window: Option<String>,
    #[serde(default)]
    pub screen: bool,
    #[serde(default)]
    pub region: Option<Rect>,
    #[serde(default = "default_edge")]
    pub max_edge: u32,
    #[serde(default)]
    pub full: bool,
}
fn default_edge() -> u32 {
    PREVIEW_EDGE
}

impl CaptureRequest {
    pub fn validate(&self) -> Result<()> {
        if usize::from(self.window.is_some())
            + usize::from(self.screen)
            + usize::from(self.region.is_some())
            > 1
        {
            return Err(AppError::bad_request(
                "use only one of --window, --screen, --region",
            ));
        }
        if self.max_edge == 0 || self.max_edge > 8192 {
            return Err(AppError::bad_request(
                "--max-edge must be 1..8192; use --full for original resolution",
            ));
        }
        if let Some(r) = self.region {
            r.validate()?;
        }
        if let Some(w) = &self.window {
            if w != "foreground" {
                parse_handle(w)?;
            }
        }
        Ok(())
    }
}

pub fn parse_handle(value: &str) -> Result<usize> {
    let parsed = if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        usize::from_str_radix(hex, 16)
    } else {
        value.parse()
    };
    parsed.ok().filter(|n| *n > 0).ok_or_else(|| {
        AppError::bad_request(
            "--window must be foreground or a window ID from `desktop windows <peer>`",
        )
    })
}

#[cfg(any(windows, test))]
pub fn preview_size(rect: Rect, edge: u32) -> (u32, u32) {
    let longest = rect.width.max(rect.height);
    if longest <= edge {
        return (rect.width, rect.height);
    }
    let width = (u64::from(rect.width) * u64::from(edge) / u64::from(longest)).max(1) as u32;
    let height = (u64::from(rect.height) * u64::from(edge) / u64::from(longest)).max(1) as u32;
    (width, height)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WindowInfo {
    pub id: String,
    pub pid: u32,
    pub title: String,
    pub rect: Rect,
    pub dpi: u32,
    pub foreground: bool,
    pub minimized: bool,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub title_omitted_chars: usize,
}
fn is_zero(n: &usize) -> bool {
    *n == 0
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WindowsRequest {
    #[serde(default = "default_max")]
    pub max: usize,
    #[serde(default)]
    pub offset: usize,
    #[serde(default)]
    pub full: bool,
    #[serde(default)]
    pub title: String,
}
fn default_max() -> usize {
    50
}

#[derive(Debug, Serialize)]
pub struct WindowPage {
    pub windows: Vec<WindowInfo>,
    pub total: usize,
    pub offset: usize,
    pub omitted: usize,
    pub next_offset: Option<usize>,
    pub truncated: bool,
}

impl WindowsRequest {
    pub fn validate(&self) -> Result<()> {
        if self.max == 0 || self.max > 10000 || self.offset > 100000 || self.title.len() > 2000 {
            return Err(AppError::bad_request(
                "windows requires --max 1..10000, --offset <=100000 and --title <=2000 bytes",
            ));
        }
        Ok(())
    }
}

#[cfg(any(windows, test))]
pub struct PageBuilder {
    req: WindowsRequest,
    entries: Vec<WindowInfo>,
    total: usize,
    bytes: usize,
    stopped: bool,
    clipped: bool,
}
#[cfg(any(windows, test))]
impl PageBuilder {
    pub fn new(req: WindowsRequest) -> Result<Self> {
        req.validate()?;
        Ok(Self {
            req,
            entries: Vec::new(),
            total: 0,
            bytes: 2,
            stopped: false,
            clipped: false,
        })
    }
    pub fn push(&mut self, mut entry: WindowInfo) {
        if !self.req.title.is_empty()
            && !entry
                .title
                .to_lowercase()
                .contains(&self.req.title.to_lowercase())
        {
            return;
        }
        self.total += 1;
        if self.total <= self.req.offset || self.stopped {
            return;
        }
        if !self.req.full {
            let title = entry.title.replace(['\r', '\n', '\t'], " ");
            let (text, omitted) = clip_line(&title, TITLE_CHARS);
            entry.title = text;
            entry.title_omitted_chars = omitted;
            self.clipped |= omitted > 0;
        }
        let size = serde_json::to_vec(&entry)
            .map_or(usize::MAX, |s| s.len())
            .saturating_add(usize::from(!self.entries.is_empty()));
        let budget = if self.req.full {
            FULL_PAGE_BYTES
        } else {
            PAGE_BYTES
        };
        if self.entries.len() >= self.req.max || size > budget.saturating_sub(self.bytes) {
            self.stopped = true;
            return;
        }
        self.bytes += size;
        self.entries.push(entry);
    }
    pub fn finish(self) -> WindowPage {
        let next = self.req.offset + self.entries.len();
        let omitted = self.total.saturating_sub(next);
        WindowPage {
            windows: self.entries,
            total: self.total,
            offset: self.req.offset,
            omitted,
            next_offset: (omitted > 0).then_some(next),
            truncated: omitted > 0 || self.clipped,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ScreenshotMeta {
    pub format: String,
    pub bytes: usize,
    pub width: u32,
    pub height: u32,
    pub rect: Rect,
    pub dpi: Option<u32>,
    pub scale_x: f64,
    pub scale_y: f64,
    pub captured_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}
pub struct Screenshot {
    pub meta: ScreenshotMeta,
    pub png: Vec<u8>,
}

pub fn supported() -> bool {
    cfg!(windows)
}
#[cfg(windows)]
pub fn unavailable(message: &str) -> AppError {
    AppError::new(
        "desktop_unavailable",
        format!("{message}; unlock the Windows user session and run serve there (not session 0)"),
    )
}

#[cfg(windows)]
pub use windows::{capture, list_windows};
#[cfg(not(windows))]
pub fn list_windows(_: WindowsRequest) -> Result<WindowPage> {
    Err(unsupported())
}
#[cfg(not(windows))]
pub fn capture(_: CaptureRequest) -> Result<Screenshot> {
    Err(unsupported())
}
#[cfg(not(windows))]
fn unsupported() -> AppError {
    AppError::new("unsupported", "native desktop capture currently requires a Windows peer; run `agent-cli-x peer list` to choose one")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn coordinates_and_preview_are_physical_and_bounded() {
        let r = Rect {
            x: -1920,
            y: 0,
            width: 3840,
            height: 2160,
        };
        assert!(r.validate().is_ok());
        assert_eq!(preview_size(r, 1600), (1600, 900));
        assert!(r.contains(Rect {
            x: -100,
            y: 50,
            width: 200,
            height: 100
        }));
        assert_eq!(
            r.intersect(Rect {
                x: 0,
                y: 0,
                width: 4000,
                height: 3000
            })
            .unwrap()
            .width,
            1920
        );
        assert!(Rect {
            x: i32::MAX,
            y: 0,
            width: 10,
            height: 10
        }
        .validate()
        .is_err());
        assert!(Rect {
            x: 0,
            y: 0,
            width: 32768,
            height: 32768
        }
        .validate()
        .is_err());
    }
    #[test]
    fn window_pages_bound_lines_bytes_and_titles() {
        let req = WindowsRequest {
            max: 10000,
            offset: 0,
            full: false,
            title: String::new(),
        };
        let mut page = PageBuilder::new(req).unwrap();
        for n in 0..1000 {
            page.push(WindowInfo {
                id: format!("0x{n:x}"),
                pid: n,
                title: "长标题".repeat(1000),
                rect: Rect {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                },
                dpi: 144,
                foreground: false,
                minimized: false,
                title_omitted_chars: 0,
            });
        }
        let result = page.finish();
        assert_eq!(result.total, 1000);
        assert!(result.windows.len() < 50);
        assert!(serde_json::to_vec(&result.windows).unwrap().len() <= PAGE_BYTES);
        assert!(result.windows[0].title_omitted_chars > 0);
        assert_eq!(result.next_offset, Some(result.windows.len()));
        assert_eq!(result.omitted, 1000 - result.windows.len());
    }
    #[test]
    fn invalid_targets_and_regions_fail_before_capture() {
        assert_eq!(parse_handle("0xABC").unwrap(), 2748);
        assert!(parse_handle("0").is_err());
        assert!(serde_json::from_str::<CaptureRequest>(
            r#"{"screen":true,"region":{"x":0,"y":0,"width":1,"height":1}}"#
        )
        .unwrap()
        .validate()
        .is_err());
    }
}
