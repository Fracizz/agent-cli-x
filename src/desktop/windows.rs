//! Native Win32 observation. Never switches desktops, activates windows, or injects input.

use super::*;
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::null_mut;
use std::sync::OnceLock;
use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM, RECT};
use windows_sys::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
use windows_sys::Win32::System::RemoteDesktop::*;
use windows_sys::Win32::System::StationsAndDesktops::*;
use windows_sys::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId};
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

type SetDpi = unsafe extern "system" fn(DPI_AWARENESS_CONTEXT) -> DPI_AWARENESS_CONTEXT;
type GetDpi = unsafe extern "system" fn(HWND) -> u32;
#[derive(Clone, Copy)]
struct DpiApi {
    set: SetDpi,
    get: GetDpi,
}

fn dpi_api() -> Result<DpiApi> {
    // Optional APIs must not stop unrelated commands from loading on older Windows.
    static API: OnceLock<Option<DpiApi>> = OnceLock::new();
    API.get_or_init(|| unsafe {
        let module = GetModuleHandleA(c"user32.dll".as_ptr().cast());
        if module.is_null() { return None; }
        let set = GetProcAddress(module,c"SetThreadDpiAwarenessContext".as_ptr().cast())?;
        let get = GetProcAddress(module,c"GetDpiForWindow".as_ptr().cast())?;
        Some(DpiApi {
            set: std::mem::transmute::<unsafe extern "system" fn() -> isize, SetDpi>(set),
            get: std::mem::transmute::<unsafe extern "system" fn() -> isize, GetDpi>(get),
        })
    }).as_ref().copied().ok_or_else(|| AppError::new("unsupported", "desktop capture requires Windows 10 (1703+) or Windows 11; select a newer Windows peer"))
}

fn window_dpi(hwnd: HWND) -> u32 {
    dpi_api().map_or(96, |api| unsafe { (api.get)(hwnd) }.max(96))
}

struct DpiGuard(DPI_AWARENESS_CONTEXT, SetDpi);
impl DpiGuard {
    fn new() -> Result<Self> {
        let api = dpi_api()?;
        let previous = unsafe { (api.set)(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        if previous.is_null() {
            return Err(AppError::new("unsupported", "per-monitor DPI v2 is unavailable; desktop capture requires Windows 10 (1703+) or Windows 11"));
        }
        Ok(Self(previous, api.set))
    }
}
impl Drop for DpiGuard {
    fn drop(&mut self) {
        unsafe {
            (self.1)(self.0);
        }
    }
}

struct InputDesktop(HDESK);
impl Drop for InputDesktop {
    fn drop(&mut self) {
        unsafe {
            CloseDesktop(self.0);
        }
    }
}

fn desktop_name(handle: HDESK) -> Result<String> {
    let mut name = [0u16; 512];
    let mut needed = 0;
    if unsafe {
        GetUserObjectInformationW(
            handle,
            UOI_NAME,
            name.as_mut_ptr().cast(),
            size_of_val(&name) as u32,
            &mut needed,
        )
    } == 0
    {
        return Err(unavailable("cannot inspect the interactive desktop"));
    }
    let end = name.iter().position(|n| *n == 0).unwrap_or(name.len());
    Ok(String::from_utf16_lossy(&name[..end]))
}

fn require_desktop() -> Result<InputDesktop> {
    let mut session = 0;
    if unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) } == 0 || session == 0 {
        return Err(unavailable("serve has no interactive user session"));
    }
    let mut state = null_mut();
    let mut bytes = 0;
    if unsafe {
        WTSQuerySessionInformationW(null_mut(), session, WTSConnectState, &mut state, &mut bytes)
    } == 0
    {
        return Err(unavailable("cannot check the Windows session state"));
    }
    let active = !state.is_null()
        && bytes >= size_of::<WTS_CONNECTSTATE_CLASS>() as u32
        && unsafe { std::ptr::read_unaligned(state.cast::<WTS_CONNECTSTATE_CLASS>()) } == WTSActive;
    unsafe {
        WTSFreeMemory(state.cast());
    }
    if !active {
        return Err(unavailable("the Windows user session is disconnected"));
    }
    let handle = unsafe { OpenInputDesktop(0, 0, DESKTOP_READOBJECTS) };
    if handle.is_null() {
        return Err(unavailable("the input desktop is locked or inaccessible"));
    }
    let input = InputDesktop(handle);
    let current = unsafe { GetThreadDesktop(GetCurrentThreadId()) };
    let name = desktop_name(handle)?;
    if !name.eq_ignore_ascii_case("Default") || desktop_name(current)? != name {
        return Err(unavailable(
            "serve is not on the unlocked Default input desktop",
        ));
    }
    Ok(input)
}

fn bounds(hwnd: HWND) -> Result<Rect> {
    let mut r: RECT = unsafe { zeroed() };
    if unsafe { GetWindowRect(hwnd, &mut r) } == 0 {
        return Err(AppError::not_found(
            "window no longer exists; run `desktop windows <peer>` again",
        ));
    }
    let mut frame = r;
    if unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS as u32,
            (&mut frame as *mut RECT).cast(),
            size_of::<RECT>() as u32,
        )
    } >= 0
    {
        r = frame;
    }
    let width = i64::from(r.right) - i64::from(r.left);
    let height = i64::from(r.bottom) - i64::from(r.top);
    if width <= 0 || height <= 0 || width > u32::MAX as i64 || height > u32::MAX as i64 {
        return Err(AppError::conflict(
            "window has no drawable area; select a visible window",
        ));
    }
    Ok(Rect {
        x: r.left,
        y: r.top,
        width: width as u32,
        height: height as u32,
    })
}

fn cloaked(hwnd: HWND) -> bool {
    let mut value = 0u32;
    unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED as u32,
            (&mut value as *mut u32).cast(),
            size_of::<u32>() as u32,
        ) >= 0
            && value != 0
    }
}

fn window_info(hwnd: HWND, foreground: HWND) -> Result<WindowInfo> {
    let mut pid = 0;
    unsafe {
        GetWindowThreadProcessId(hwnd, &mut pid);
    }
    let len = unsafe { GetWindowTextLengthW(hwnd) }.max(0) as usize;
    let mut title = vec![0u16; (len + 1).clamp(1, 32768)];
    let copied =
        unsafe { GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32) }.max(0) as usize;
    Ok(WindowInfo {
        id: format!("0x{:x}", hwnd as usize),
        pid,
        title: String::from_utf16_lossy(&title[..copied]),
        rect: bounds(hwnd)?,
        dpi: window_dpi(hwnd),
        foreground: hwnd == foreground,
        minimized: unsafe { IsIconic(hwnd) } != 0,
        title_omitted_chars: 0,
    })
}

pub fn list_windows(req: WindowsRequest) -> Result<WindowPage> {
    let _input = require_desktop()?;
    let _dpi = DpiGuard::new()?;
    struct Enumeration {
        page: PageBuilder,
        foreground: HWND,
    }
    unsafe extern "system" fn visit(hwnd: HWND, param: LPARAM) -> BOOL {
        let state = &mut *(param as *mut Enumeration);
        if IsWindowVisible(hwnd) != 0 && !cloaked(hwnd) {
            if let Ok(entry) = window_info(hwnd, state.foreground) {
                if !entry.title.is_empty() {
                    state.page.push(entry);
                }
            }
        }
        1
    }
    let mut state = Enumeration {
        page: PageBuilder::new(req)?,
        foreground: unsafe { GetForegroundWindow() },
    };
    if unsafe { EnumWindows(Some(visit), (&mut state as *mut Enumeration) as LPARAM) } == 0 {
        return Err(unavailable("cannot enumerate desktop windows"));
    }
    Ok(state.page.finish())
}

fn screen_rect() -> Result<Rect> {
    let (x, y, w, h) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    if w <= 0 || h <= 0 {
        return Err(unavailable("no connected display"));
    }
    Ok(Rect {
        x,
        y,
        width: w as u32,
        height: h as u32,
    })
}

struct Canvas {
    screen: HDC,
    memory: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
}
impl Drop for Canvas {
    fn drop(&mut self) {
        unsafe {
            if !self.previous.is_null() {
                SelectObject(self.memory, self.previous);
            }
            if !self.bitmap.is_null() {
                DeleteObject(self.bitmap);
            }
            if !self.memory.is_null() {
                DeleteDC(self.memory);
            }
            if !self.screen.is_null() {
                ReleaseDC(null_mut(), self.screen);
            }
        }
    }
}

fn pixels(rect: Rect, width: u32, height: u32) -> Result<Vec<u8>> {
    let mut canvas = Canvas {
        screen: unsafe { GetDC(null_mut()) },
        memory: null_mut(),
        bitmap: null_mut(),
        previous: null_mut(),
    };
    if canvas.screen.is_null() {
        return Err(unavailable("cannot open the display context"));
    }
    canvas.memory = unsafe { CreateCompatibleDC(canvas.screen) };
    if canvas.memory.is_null() {
        return Err(unavailable("cannot create the capture context"));
    }
    let mut info: BITMAPINFO = unsafe { zeroed() };
    info.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
    info.bmiHeader.biWidth = width as i32;
    info.bmiHeader.biHeight = -(height as i32);
    info.bmiHeader.biPlanes = 1;
    info.bmiHeader.biBitCount = 32;
    info.bmiHeader.biCompression = BI_RGB;
    let mut data: *mut c_void = null_mut();
    canvas.bitmap = unsafe {
        CreateDIBSection(
            canvas.screen,
            &info,
            DIB_RGB_COLORS,
            &mut data,
            null_mut(),
            0,
        )
    };
    if canvas.bitmap.is_null() || data.is_null() {
        return Err(AppError::io(
            "cannot allocate screenshot pixels; use a smaller --region",
        ));
    }
    let previous = unsafe { SelectObject(canvas.memory, canvas.bitmap) };
    if previous.is_null() || previous as isize == -1 {
        return Err(AppError::io("cannot select capture bitmap"));
    }
    canvas.previous = previous;
    unsafe {
        SetStretchBltMode(canvas.memory, HALFTONE);
        SetBrushOrgEx(canvas.memory, 0, 0, null_mut());
    }
    let copied = unsafe {
        StretchBlt(
            canvas.memory,
            0,
            0,
            width as i32,
            height as i32,
            canvas.screen,
            rect.x,
            rect.y,
            rect.width as i32,
            rect.height as i32,
            SRCCOPY | CAPTUREBLT,
        )
    };
    if copied == 0 {
        return Err(unavailable("display pixel copy failed"));
    }
    unsafe {
        GdiFlush();
    }
    let count = width as usize * height as usize;
    let bgra = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), count * 4) };
    let mut rgb = Vec::with_capacity(count * 3);
    for pixel in bgra.as_chunks::<4>().0 {
        rgb.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
    }
    Ok(rgb)
}

fn encode(rect: Rect, width: u32, height: u32) -> Result<Vec<u8>> {
    let rgb = pixels(rect, width, height)?;
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Fast);
        encoder.set_filter(png::FilterType::Sub);
        let mut writer = encoder
            .write_header()
            .map_err(|e| AppError::io(format!("PNG header: {e}")))?;
        writer
            .write_image_data(&rgb)
            .map_err(|e| AppError::io(format!("PNG image: {e}")))?;
    }
    Ok(out)
}

pub fn capture(req: CaptureRequest) -> Result<Screenshot> {
    req.validate()?;
    let _input = require_desktop()?;
    let _dpi = DpiGuard::new()?;
    let screen = screen_rect()?;
    let mut warning = None;
    let (rect, dpi, window) = if let Some(r) = req.region {
        if !screen.contains(r) {
            return Err(AppError::bad_request("--region must be inside the physical virtual desktop; use `desktop windows <peer>` for coordinates"));
        }
        (r, None, None)
    } else if req.screen {
        (screen, None, None)
    } else {
        let hwnd = match req.window.as_deref() {
            None | Some("foreground") => unsafe { GetForegroundWindow() },
            Some(value) => parse_handle(value)? as HWND,
        };
        if hwnd.is_null() || unsafe { IsWindow(hwnd) } == 0 {
            return Err(AppError::not_found(
                "window not found; run `desktop windows <peer>`",
            ));
        }
        if unsafe { IsWindowVisible(hwnd) } == 0 || unsafe { IsIconic(hwnd) } != 0 || cloaked(hwnd)
        {
            return Err(AppError::conflict(
                "window is hidden or minimized; restore it before capturing",
            ));
        }
        let original = bounds(hwnd)?;
        let rect = screen
            .intersect(original)
            .ok_or_else(|| AppError::conflict("window is off-screen; move it onto a display"))?;
        if unsafe { GetForegroundWindow() } != hwnd {
            warning = Some(
                "captures displayed pixels; a background window may be covered by other windows"
                    .to_string(),
            );
        } else if rect != original {
            warning = Some(
                "window is partly off-screen; only its visible desktop intersection was captured"
                    .to_string(),
            );
        }
        (
            rect,
            Some(window_dpi(hwnd)),
            Some(format!("0x{:x}", hwnd as usize)),
        )
    };
    rect.validate()?;
    let (mut width, mut height) = if req.full {
        (rect.width, rect.height)
    } else {
        preview_size(rect, req.max_edge)
    };
    let limit = if req.full {
        MAX_IMAGE_BYTES
    } else {
        PREVIEW_BYTES
    };
    let png = loop {
        let out = encode(rect, width, height)?;
        if out.len() <= limit {
            break out;
        }
        if req.full || (width == 1 && height == 1) {
            return Err(AppError::bad_request(
                "PNG exceeds the image byte limit; choose a smaller --region",
            ));
        }
        let edge = (width.max(height) * 3 / 4).max(1);
        (width, height) = preview_size(rect, edge);
    };
    Ok(Screenshot {
        meta: ScreenshotMeta {
            format: "png".into(),
            bytes: png.len(),
            width,
            height,
            rect,
            dpi,
            scale_x: f64::from(rect.width) / f64::from(width),
            scale_y: f64::from(rect.height) / f64::from(height),
            captured_at: crate::util::now_iso(),
            window,
            warning,
        },
        png,
    })
}
