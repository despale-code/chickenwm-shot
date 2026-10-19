//! chickenwm-shot — ChickenWM's screenshot tool.
//!
//! On launch every monitor is captured and shown frozen in a full-screen overlay, so nothing
//! moves while you choose:
//! - drag to select a region,
//! - click to take the window under the cursor (or the whole monitor on the desktop),
//! - Space for the monitor under the cursor, Esc / right-click to cancel.
//!
//! The result is copied to the clipboard (bitmap + PNG), saved as a PNG in
//! `Pictures\Screenshots`, and announced with a ChickenWM notification.
//! `chickenwm-shot --full` captures the monitor under the cursor without the overlay.

#![windows_subsystem = "windows"]

use anyhow::{bail, Context, Result};
use chickenwm_core::css::load_css_sheet;
use chickenwm_core::ipc::{send_ipc_command_blocking, IpcCommand, Notification, PIPE_NAME_NOTIFY};
use std::mem::size_of;
use std::path::PathBuf;
use std::time::Instant;
use windows::core::w;
use windows::Win32::Foundation::{
    COLORREF, GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DwmSetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
    DWMWA_TRANSITIONS_FORCEDISABLED,
};
use windows::Win32::Graphics::Gdi::{
    AlphaBlend, BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW,
    CreatePen, DeleteDC, DeleteObject, DrawTextW, EndPaint, GetDC, GetDIBits, GetStockObject,
    InvalidateRect, ReleaseDC, SelectObject, SetBkMode, SetTextColor, AC_SRC_OVER, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS,
    DEFAULT_CHARSET, DEFAULT_PITCH, DIB_RGB_COLORS, DT_CENTER, DT_NOPREFIX, DT_SINGLELINE,
    DT_VCENTER, FW_NORMAL, FW_SEMIBOLD, HDC, HFONT, HBITMAP, HPEN, OUT_DEFAULT_PRECIS,
    PAINTSTRUCT, PS_SOLID, SRCCOPY, TRANSPARENT, NULL_BRUSH,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, VK_ESCAPE, VK_RETURN, VK_SPACE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, EnumWindows, GetClassNameW, GetCursorPos,
    GetMessageW, GetSystemMetrics, GetWindowLongPtrW, GetWindowTextW, IsIconic, IsWindowVisible,
    KillTimer, LoadCursorW, PostQuitMessage, RegisterClassExW, SetCursor, SetForegroundWindow,
    SetTimer, SetWindowLongPtrW, ShowWindow, TranslateMessage, CS_HREDRAW, CS_VREDRAW,
    GWLP_USERDATA, GWL_EXSTYLE, IDC_CROSS, MSG, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SW_HIDE, SW_SHOW, WM_DESTROY, WM_ERASEBKGND,
    WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_PAINT, WM_RBUTTONUP,
    WM_SETCURSOR, WM_TIMER, WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
    WS_POPUP,
};

/// `CF_DIB`.
const CF_DIB: u32 = 8;

/// `[x0, y0, x1, y1]` in capture pixels.
pub type PxRect = [i32; 4];

/// The whole virtual desktop, in physical pixels (compatible with tests and legacy consumers).
pub struct Capture {
    /// Top-left of the virtual screen (can be negative with monitors left of/above the primary).
    pub origin: (i32, i32),
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

impl Capture {
    pub fn crop(&self, r: PxRect) -> Option<image::RgbaImage> {
        let x0 = r[0].clamp(0, self.width as i32) as usize;
        let y0 = r[1].clamp(0, self.height as i32) as usize;
        let x1 = r[2].clamp(0, self.width as i32) as usize;
        let y1 = r[3].clamp(0, self.height as i32) as usize;
        if x1 <= x0 || y1 <= y0 {
            return None;
        }
        let mut out = Vec::with_capacity((x1 - x0) * (y1 - y0) * 4);
        for y in y0..y1 {
            let row = (y * self.width + x0) * 4;
            out.extend_from_slice(&self.rgba[row..row + (x1 - x0) * 4]);
        }
        image::RgbaImage::from_raw((x1 - x0) as u32, (y1 - y0) as u32, out)
    }

    pub fn to_local(&self, x: i32, y: i32) -> (i32, i32) {
        (x - self.origin.0, y - self.origin.1)
    }
}

/// Instant desktop capture directly into a hardware-compatible memory DC and bitmap.
/// Zero CPU pixel iterations, zero texture uploads.
fn capture_desktop_dc(x: i32, y: i32, w: i32, h: i32) -> Result<(HDC, HBITMAP)> {
    unsafe {
        let screen = GetDC(None);
        if screen.is_invalid() {
            bail!("could not get desktop DC");
        }
        let mem = CreateCompatibleDC(screen);
        let bmp = CreateCompatibleBitmap(screen, w, h);
        let old = SelectObject(mem, bmp);
        let copied = BitBlt(mem, 0, 0, w, h, screen, x, y, SRCCOPY).is_ok();
        ReleaseDC(None, screen);
        if !copied {
            let _ = SelectObject(mem, old);
            let _ = DeleteObject(bmp);
            let _ = DeleteDC(mem);
            bail!("BitBlt desktop capture failed");
        }
        Ok((mem, bmp))
    }
}

/// Helper for tests or legacy callers that need full RGBA memory.
#[cfg(test)]
fn capture_desktop() -> Result<Capture> {
    unsafe {
        let x = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let y = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let w = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let h = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        if w <= 0 || h <= 0 {
            bail!("no screen to capture");
        }
        let (mem, bmp) = capture_desktop_dc(x, y, w, h)?;
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        let lines = GetDIBits(mem, bmp, 0, h as u32, Some(rgba.as_mut_ptr() as *mut _), &mut info, DIB_RGB_COLORS);
        let _ = DeleteObject(bmp);
        let _ = DeleteDC(mem);
        if lines == 0 {
            bail!("couldn't read the screen");
        }
        for px in rgba.chunks_exact_mut(4) {
            px.swap(0, 2); // BGRA -> RGBA
            px[3] = 255;
        }
        Ok(Capture { origin: (x, y), width: w as usize, height: h as usize, rgba })
    }
}

/// Crop directly from a capture DC into an `image::RgbaImage`.
/// Reads only the cropped pixels from GDI, saving memory and CPU time.
fn crop_dc(hdc: HDC, origin_w: i32, origin_h: i32, r: PxRect) -> Option<image::RgbaImage> {
    let x0 = r[0].clamp(0, origin_w);
    let y0 = r[1].clamp(0, origin_h);
    let x1 = r[2].clamp(0, origin_w);
    let y1 = r[3].clamp(0, origin_h);
    let w = x1 - x0;
    let h = y1 - y0;
    if w <= 0 || h <= 0 {
        return None;
    }
    unsafe {
        let mem = CreateCompatibleDC(hdc);
        let bmp = CreateCompatibleBitmap(hdc, w, h);
        let old = SelectObject(mem, bmp);
        let copied = BitBlt(mem, 0, 0, w, h, hdc, x0, y0, SRCCOPY).is_ok();
        if !copied {
            SelectObject(mem, old);
            let _ = DeleteObject(bmp);
            let _ = DeleteDC(mem);
            return None;
        }

        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // Top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        let lines = GetDIBits(mem, bmp, 0, h as u32, Some(rgba.as_mut_ptr() as *mut _), &mut info, DIB_RGB_COLORS);
        SelectObject(mem, old);
        let _ = DeleteObject(bmp);
        let _ = DeleteDC(mem);
        if lines == 0 {
            return None;
        }
        for px in rgba.chunks_exact_mut(4) {
            px.swap(0, 2); // BGRA -> RGBA
            px[3] = 255;
        }
        image::RgbaImage::from_raw(w as u32, h as u32, rgba)
    }
}

/// Visible top-level windows, front to back, as capture-pixel rects (for click-to-capture).
fn window_rects(origin: (i32, i32)) -> Vec<PxRect> {
    struct Ctx {
        origin: (i32, i32),
        out: Vec<PxRect>,
    }
    unsafe extern "system" fn each(hwnd: HWND, lparam: LPARAM) -> windows::Win32::Foundation::BOOL {
        let ctx = unsafe { &mut *(lparam.0 as *mut Ctx) };
        unsafe {
            if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
                return true.into();
            }
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
            if ex & WS_EX_TRANSPARENT.0 != 0 {
                return true.into();
            }
            let mut cloaked = 0u32;
            if DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut u32 as *mut _, 4).is_ok() && cloaked != 0 {
                return true.into();
            }
            let mut class = [0u16; 64];
            let n = GetClassNameW(hwnd, &mut class);
            let class = String::from_utf16_lossy(&class[..n.max(0) as usize]);
            if matches!(class.as_str(), "Progman" | "WorkerW" | "ChickenWMPaperClass" | "ChickenWMShotOverlay") {
                return true.into();
            }
            let mut title = [0u16; 8];
            if GetWindowTextW(hwnd, &mut title) == 0 && ex & WS_EX_TOOLWINDOW.0 != 0 {
                return true.into();
            }
            let mut rc = RECT::default();
            if DwmGetWindowAttribute(hwnd, DWMWA_EXTENDED_FRAME_BOUNDS, &mut rc as *mut RECT as *mut _, size_of::<RECT>() as u32).is_err() {
                return true.into();
            }
            if rc.right - rc.left < 8 || rc.bottom - rc.top < 8 {
                return true.into();
            }
            let x0 = rc.left - ctx.origin.0;
            let y0 = rc.top - ctx.origin.1;
            let x1 = rc.right - ctx.origin.0;
            let y1 = rc.bottom - ctx.origin.1;
            ctx.out.push([x0, y0, x1, y1]);
        }
        true.into()
    }
    let mut ctx = Ctx { origin, out: Vec::new() };
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM(&mut ctx as *mut Ctx as isize));
    }
    ctx.out
}

fn monitor_rects(origin: (i32, i32)) -> Vec<PxRect> {
    chickenwm_core::monitor::enumerate_monitors()
        .iter()
        .map(|m| {
            let x = m.x - origin.0;
            let y = m.y - origin.1;
            [x, y, x + m.width, y + m.height]
        })
        .collect()
}

pub fn contains(r: &PxRect, p: (i32, i32)) -> bool {
    p.0 >= r[0] && p.0 < r[2] && p.1 >= r[1] && p.1 < r[3]
}

pub fn norm(a: (i32, i32), b: (i32, i32)) -> PxRect {
    [a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1)]
}

fn cursor_local(origin: (i32, i32)) -> (i32, i32) {
    let mut pt = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut pt);
    }
    (pt.x - origin.0, pt.y - origin.1)
}

// ───────────────────────────── output ─────────────────────────────

fn timestamp() -> String {
    let t = unsafe { GetLocalTime() };
    format!("{:04}-{:02}-{:02} {:02}{:02}{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}

fn save_png(img: &image::RgbaImage) -> Result<PathBuf> {
    let dir = dirs::picture_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join("Pictures")))
        .context("no Pictures folder")?
        .join("Screenshots");
    std::fs::create_dir_all(&dir)?;
    let mut path = dir.join(format!("Screenshot {}.png", timestamp()));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("Screenshot {} ({n}).png", timestamp()));
        n += 1;
    }
    img.save_with_format(&path, image::ImageFormat::Png)?;
    Ok(path)
}

fn global_bytes(bytes: &[u8]) -> Option<HGLOBAL> {
    unsafe {
        let mem = GlobalAlloc(GMEM_MOVEABLE, bytes.len()).ok()?;
        let ptr = GlobalLock(mem) as *mut u8;
        if ptr.is_null() {
            let _ = GlobalFree(mem);
            return None;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
        let _ = GlobalUnlock(mem);
        Some(mem)
    }
}

fn set_clipboard_bytes(format: u32, bytes: &[u8]) -> bool {
    let Some(mem) = global_bytes(bytes) else { return false };
    unsafe {
        if SetClipboardData(format, HANDLE(mem.0)).is_ok() {
            true
        } else {
            let _ = GlobalFree(mem);
            false
        }
    }
}

fn copy_to_clipboard(img: &image::RgbaImage, png: &[u8], owner: Option<HWND>) -> Result<()> {
    let (w, h) = img.dimensions();
    let header = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: w as i32,
        biHeight: h as i32,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        biSizeImage: w * h * 4,
        ..Default::default()
    };
    let mut dib = Vec::with_capacity(size_of::<BITMAPINFOHEADER>() + (w * h * 4) as usize);
    dib.extend_from_slice(unsafe {
        std::slice::from_raw_parts(&header as *const _ as *const u8, size_of::<BITMAPINFOHEADER>())
    });
    for row in img.rows().rev() {
        for px in row {
            dib.extend_from_slice(&[px[2], px[1], px[0], 255]);
        }
    }
    unsafe {
        OpenClipboard(owner.unwrap_or_default()).context("clipboard busy")?;
        let _ = EmptyClipboard();
        let dib_set = set_clipboard_bytes(CF_DIB, &dib);
        let png_format = RegisterClipboardFormatW(w!("PNG"));
        let png_set = png_format != 0 && set_clipboard_bytes(png_format, png);
        let _ = CloseClipboard();
        if !dib_set && !png_set {
            bail!("the clipboard refused the image");
        }
    }
    Ok(())
}

fn deliver(cap_dc: HDC, width: i32, height: i32, area: PxRect, owner: Option<HWND>) -> Result<()> {
    let img = crop_dc(cap_dc, width, height, area).context("empty selection")?;
    let mut png = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)?;
    let copied = copy_to_clipboard(&img, &png, owner).is_ok();
    let saved = save_png(&img);
    let body = match (&saved, copied) {
        (Ok(p), true) => format!("Copied to the clipboard and saved to {}", p.display()),
        (Ok(p), false) => format!("Saved to {}", p.display()),
        (Err(_), true) => "Copied to the clipboard (couldn't save the file)".to_string(),
        (Err(e), false) => format!("Couldn't save the screenshot: {e}"),
    };
    let _ = send_ipc_command_blocking(
        PIPE_NAME_NOTIFY,
        &IpcCommand::Notify(Notification {
            app: "Screenshot".into(),
            title: format!("Screenshot {}×{}", img.width(), img.height()),
            body,
            timeout_secs: Some(5),
            urgency: None,
        }),
    );
    saved.map(|_| ())
}

// ───────────────────────────── win32 gdi overlay ─────────────────────────────

struct OverlayState {
    cap_dc: HDC,
    cap_bmp: HBITMAP,
    back_dc: HDC,
    back_bmp: HBITMAP,
    black_dc: HDC,
    black_bmp: HBITMAP,
    _origin: (i32, i32),
    width: i32,
    height: i32,
    windows: Vec<PxRect>,
    monitors: Vec<PxRect>,
    cursor: (i32, i32),
    drag_from: Option<(i32, i32)>,
    dragging: bool,
    accent_pen: HPEN,
    font: HFONT,
    badge_font: HFONT,
    start_time: Instant,
}

impl OverlayState {
    fn new(origin: (i32, i32), width: i32, height: i32, cap_dc: HDC, cap_bmp: HBITMAP) -> Result<Self> {
        let accent_ref = load_css_sheet("roobar.css")
            .ok()
            .map(|s| {
                let c = s.var_color("--accent-color", chickenwm_core::config::RgbaColor { r: 217, g: 215, b: 212, a: 255 });
                COLORREF(c.r as u32 | ((c.g as u32) << 8) | ((c.b as u32) << 16))
            })
            .unwrap_or(COLORREF(0x00D4D7D9));

        unsafe {
            // Backbuffer for flicker-free painting
            let screen = GetDC(None);
            let back_dc = CreateCompatibleDC(screen);
            let back_bmp = CreateCompatibleBitmap(screen, width, height);
            let _ = SelectObject(back_dc, back_bmp);

            // 1x1 black bitmap for instant hardware AlphaBlend dimming
            let black_dc = CreateCompatibleDC(screen);
            let black_bmp = CreateCompatibleBitmap(screen, 1, 1);
            let _ = SelectObject(black_dc, black_bmp);
            ReleaseDC(None, screen);

            let accent_pen = CreatePen(PS_SOLID, 2, accent_ref);
            let font_name = w!("Segoe UI");
            let font = CreateFontW(
                17, 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0,
                DEFAULT_CHARSET.0 as u32, OUT_DEFAULT_PRECIS.0 as u32, CLIP_DEFAULT_PRECIS.0 as u32,
                CLEARTYPE_QUALITY.0 as u32, DEFAULT_PITCH.0 as u32, font_name,
            );
            let badge_font = CreateFontW(
                15, 0, 0, 0, FW_SEMIBOLD.0 as i32, 0, 0, 0,
                DEFAULT_CHARSET.0 as u32, OUT_DEFAULT_PRECIS.0 as u32, CLIP_DEFAULT_PRECIS.0 as u32,
                CLEARTYPE_QUALITY.0 as u32, DEFAULT_PITCH.0 as u32, font_name,
            );

            let windows = window_rects(origin);
            let monitors = monitor_rects(origin);
            let cursor = cursor_local(origin);

            Ok(Self {
                cap_dc,
                cap_bmp,
                back_dc,
                back_bmp,
                black_dc,
                black_bmp,
                _origin: origin,
                width,
                height,
                windows,
                monitors,
                cursor,
                drag_from: None,
                dragging: false,
                accent_pen,
                font,
                badge_font,
                start_time: Instant::now(),
            })
        }
    }
}

impl Drop for OverlayState {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.accent_pen);
            let _ = DeleteObject(self.font);
            let _ = DeleteObject(self.badge_font);
            let _ = DeleteObject(self.black_bmp);
            let _ = DeleteDC(self.black_dc);
            let _ = DeleteObject(self.back_bmp);
            let _ = DeleteDC(self.back_dc);
            let _ = DeleteObject(self.cap_bmp);
            let _ = DeleteDC(self.cap_dc);
        }
    }
}

unsafe extern "system" fn overlay_wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut OverlayState;
        if state_ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let state = &mut *state_ptr;

        match msg {
        WM_ERASEBKGND => LRESULT(1), // Prevent background erase flicker
        WM_SETCURSOR => {
            let _ = SetCursor(LoadCursorW(None, IDC_CROSS).unwrap_or_default());
            LRESULT(1)
        }
        WM_TIMER => {
            if state.start_time.elapsed().as_secs_f32() < 0.16 {
                let _ = InvalidateRect(hwnd, None, false);
            } else {
                let _ = KillTimer(hwnd, 1);
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let x = (lparam.0 & 0xffff) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xffff) as i16 as i32;
            state.cursor = (x, y);
            if let Some(from) = state.drag_from {
                if (x - from.0).abs() > 4 || (y - from.1).abs() > 4 {
                    state.dragging = true;
                }
            }
            let _ = InvalidateRect(hwnd, None, false);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            SetCapture(hwnd);
            state.drag_from = Some(state.cursor);
            state.dragging = false;
            let _ = InvalidateRect(hwnd, None, false);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let _ = ReleaseCapture();
            let cursor = state.cursor;
            let monitor = state.monitors.iter().find(|m| contains(m, cursor)).copied();
            let hovered_window = state.windows.iter().find(|r| contains(r, cursor)).copied();
            let selection = match (state.drag_from, state.dragging) {
                (Some(from), true) => Some(norm(from, cursor)),
                _ => hovered_window.or(monitor),
            };
            let _ = ShowWindow(hwnd, SW_HIDE);
            if let Some(sel) = selection {
                let _ = deliver(state.cap_dc, state.width, state.height, sel, Some(hwnd));
            }
            PostQuitMessage(0);
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            let _ = ShowWindow(hwnd, SW_HIDE);
            PostQuitMessage(0);
            LRESULT(0)
        }
        WM_KEYDOWN => {
            match wparam.0 as u16 {
                x if x == VK_ESCAPE.0 => {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                    PostQuitMessage(0);
                }
                x if x == VK_SPACE.0 => {
                    let cursor = state.cursor;
                    let monitor = state.monitors.iter().find(|m| contains(m, cursor)).copied();
                    let _ = ShowWindow(hwnd, SW_HIDE);
                    if let Some(m) = monitor {
                        let _ = deliver(state.cap_dc, state.width, state.height, m, Some(hwnd));
                    }
                    PostQuitMessage(0);
                }
                x if x == VK_RETURN.0 => {
                    let cursor = state.cursor;
                    let monitor = state.monitors.iter().find(|m| contains(m, cursor)).copied();
                    let hovered_window = state.windows.iter().find(|r| contains(r, cursor)).copied();
                    let selection = match (state.drag_from, state.dragging) {
                        (Some(from), true) => Some(norm(from, cursor)),
                        _ => hovered_window.or(monitor),
                    };
                    let _ = ShowWindow(hwnd, SW_HIDE);
                    if let Some(sel) = selection {
                        let _ = deliver(state.cap_dc, state.width, state.height, sel, Some(hwnd));
                    }
                    PostQuitMessage(0);
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let dc = BeginPaint(hwnd, &mut ps);

            // Copy frozen desktop to backbuffer
            let _ = BitBlt(state.back_dc, 0, 0, state.width, state.height, state.cap_dc, 0, 0, SRCCOPY);

            let cursor = state.cursor;
            let monitor = state.monitors.iter().find(|m| contains(m, cursor)).copied();
            let hovered_window = state.windows.iter().find(|r| contains(r, cursor)).copied();
            let selection = match (state.drag_from, state.dragging) {
                (Some(from), true) => Some(norm(from, cursor)),
                _ => hovered_window.or(monitor),
            };

            let fade = (state.start_time.elapsed().as_secs_f32() / 0.14).min(1.0);
            let alpha = (120.0 * fade) as u8;
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: alpha,
                AlphaFormat: 0,
            };

            if let Some(s) = selection {
                let s = [
                    s[0].clamp(0, state.width),
                    s[1].clamp(0, state.height),
                    s[2].clamp(0, state.width),
                    s[3].clamp(0, state.height),
                ];
                // Dim outer 4 rectangles
                if s[1] > 0 {
                    let _ = AlphaBlend(state.back_dc, 0, 0, state.width, s[1], state.black_dc, 0, 0, 1, 1, blend);
                }
                if s[3] < state.height {
                    let _ = AlphaBlend(state.back_dc, 0, s[3], state.width, state.height - s[3], state.black_dc, 0, 0, 1, 1, blend);
                }
                if s[0] > 0 && s[3] > s[1] {
                    let _ = AlphaBlend(state.back_dc, 0, s[1], s[0], s[3] - s[1], state.black_dc, 0, 0, 1, 1, blend);
                }
                if s[2] < state.width && s[3] > s[1] {
                    let _ = AlphaBlend(state.back_dc, s[2], s[1], state.width - s[2], s[3] - s[1], state.black_dc, 0, 0, 1, 1, blend);
                }

                // Accent rectangle outline
                let old_pen = SelectObject(state.back_dc, state.accent_pen);
                let old_brush = SelectObject(state.back_dc, GetStockObject(NULL_BRUSH));
                let _ = windows::Win32::Graphics::Gdi::Rectangle(state.back_dc, s[0], s[1], s[2], s[3]);
                SelectObject(state.back_dc, old_pen);
                SelectObject(state.back_dc, old_brush);

                // Dimension badge
                let sel_w = s[2] - s[0];
                let sel_h = s[3] - s[1];
                if sel_w > 0 && sel_h > 0 {
                    let text = format!("{sel_w} × {sel_h}");
                    let badge_w = 96;
                    let badge_h = 24;
                    let badge_x = s[0];
                    let mut badge_y = s[3] + 8;
                    if badge_y + badge_h > state.height {
                        badge_y = (s[3] - badge_h - 10).max(s[1] + 8);
                    }
                    let badge_blend = BLENDFUNCTION {
                        BlendOp: AC_SRC_OVER as u8,
                        BlendFlags: 0,
                        SourceConstantAlpha: (200.0 * fade) as u8,
                        AlphaFormat: 0,
                    };
                    let _ = AlphaBlend(state.back_dc, badge_x, badge_y, badge_w, badge_h, state.black_dc, 0, 0, 1, 1, badge_blend);
                    let old_font = SelectObject(state.back_dc, state.badge_font);
                    SetBkMode(state.back_dc, TRANSPARENT);
                    SetTextColor(state.back_dc, COLORREF(0x00FFFFFF));
                    let mut wide: Vec<u16> = text.encode_utf16().collect();
                    let mut rect = RECT { left: badge_x, top: badge_y, right: badge_x + badge_w, bottom: badge_y + badge_h };
                    let _ = DrawTextW(state.back_dc, &mut wide, &mut rect, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
                    SelectObject(state.back_dc, old_font);
                }
            } else {
                let _ = AlphaBlend(state.back_dc, 0, 0, state.width, state.height, state.black_dc, 0, 0, 1, 1, blend);
            }

            // Hint pill on active monitor
            if let Some(m) = monitor {
                let hint = "Drag to select  ·  Click a window  ·  Space: whole screen  ·  Esc: cancel";
                let pill_w = 480;
                let pill_h = 32;
                let pill_x = m[0] + (m[2] - m[0] - pill_w) / 2;
                let pill_y = m[1] + 56;
                let pill_blend = BLENDFUNCTION {
                    BlendOp: AC_SRC_OVER as u8,
                    BlendFlags: 0,
                    SourceConstantAlpha: (210.0 * fade) as u8,
                    AlphaFormat: 0,
                };
                let _ = AlphaBlend(state.back_dc, pill_x, pill_y, pill_w, pill_h, state.black_dc, 0, 0, 1, 1, pill_blend);

                let old_pen = SelectObject(state.back_dc, state.accent_pen);
                let old_brush = SelectObject(state.back_dc, GetStockObject(NULL_BRUSH));
                let _ = windows::Win32::Graphics::Gdi::Rectangle(state.back_dc, pill_x, pill_y, pill_x + pill_w, pill_y + pill_h);
                SelectObject(state.back_dc, old_pen);
                SelectObject(state.back_dc, old_brush);

                let old_font = SelectObject(state.back_dc, state.font);
                SetBkMode(state.back_dc, TRANSPARENT);
                SetTextColor(state.back_dc, COLORREF(0x00FFFFFF));
                let mut wide: Vec<u16> = hint.encode_utf16().collect();
                let mut rect = RECT { left: pill_x, top: pill_y, right: pill_x + pill_w, bottom: pill_y + pill_h };
                let _ = DrawTextW(state.back_dc, &mut wide, &mut rect, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
                SelectObject(state.back_dc, old_font);
            }

            // Hardware blit backbuffer to window DC
            let _ = BitBlt(dc, 0, 0, state.width, state.height, state.back_dc, 0, 0, SRCCOPY);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
    }
}

fn main() -> Result<()> {
    chickenwm_core::init_tracing("chickenwm_shot");
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let _instance = match chickenwm_core::instance::claim_instance(r"Local\ChickenWMShot", 0) {
        chickenwm_core::instance::Instance::Primary(guard) => guard,
        chickenwm_core::instance::Instance::Duplicate(_) => return Ok(()),
    };

    let x = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let y = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let w = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
    let h = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
    if w <= 0 || h <= 0 {
        bail!("no virtual screen found");
    }

    let (cap_dc, cap_bmp) = capture_desktop_dc(x, y, w, h)?;

    if std::env::args().any(|a| a == "--full") {
        let cursor = cursor_local((x, y));
        let mons = monitor_rects((x, y));
        let area = mons.into_iter().find(|m| contains(m, cursor));
        let res = deliver(cap_dc, w, h, area.unwrap_or([0, 0, w, h]), None);
        unsafe {
            let _ = DeleteObject(cap_bmp);
            let _ = DeleteDC(cap_dc);
        }
        return res;
    }

    let class_name = w!("ChickenWMShotOverlay");
    unsafe {
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(overlay_wnd_proc),
            hCursor: LoadCursorW(None, IDC_CROSS).unwrap_or_default(),
            lpszClassName: class_name,
            ..Default::default()
        };
        let _ = RegisterClassExW(&wc);

        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            class_name,
            w!("ChickenWM Screenshot"),
            WS_POPUP,
            x,
            y,
            w,
            h,
            None,
            None,
            None,
            None,
        )?;

        let off = windows::Win32::Foundation::BOOL(1);
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_TRANSITIONS_FORCEDISABLED, &off as *const _ as *const _, 4);

        let mut state = Box::new(OverlayState::new((x, y), w, h, cap_dc, cap_bmp)?);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state.as_mut() as *mut OverlayState as isize);

        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetTimer(hwnd, 1, 16, None);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).into() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture(w: usize, h: usize) -> Capture {
        let mut rgba = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            for x in 0..w {
                rgba.extend_from_slice(&[x as u8, y as u8, 0, 255]);
            }
        }
        Capture { origin: (-100, 0), width: w, height: h, rgba }
    }

    #[test]
    fn crops_the_selected_pixels_and_clamps_to_the_desktop() {
        let cap = capture(10, 8);
        let img = cap.crop([2, 3, 5, 6]).unwrap();
        assert_eq!(img.dimensions(), (3, 3));
        assert_eq!(img.get_pixel(0, 0).0, [2, 3, 0, 255]);
        assert_eq!(cap.crop([-5, -5, 50, 50]).unwrap().dimensions(), (10, 8));
        assert!(cap.crop([4, 4, 4, 9]).is_none());
    }

    #[test]
    #[ignore]
    fn captures_the_real_desktop() {
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        let cap = capture_desktop().expect("capture");
        let mons = monitor_rects(cap.origin);
        assert!(!mons.is_empty());
        for m in &mons {
            assert!(m[0] >= 0 && m[1] >= 0 && m[2] as usize <= cap.width && m[3] as usize <= cap.height, "{m:?} inside {}x{}", cap.width, cap.height);
        }
        assert!(cap.rgba.chunks_exact(4).any(|p| p[0] > 16 || p[1] > 16 || p[2] > 16), "not all black");
        if let Ok(out) = std::env::var("CHICKENWM_SHOT_TEST_OUT") {
            cap.crop(mons[0]).unwrap().save(out).unwrap();
        }
        println!("desktop {}x{} at {:?}, monitors {mons:?}, windows {}", cap.width, cap.height, cap.origin, window_rects(cap.origin).len());
    }

    #[test]
    fn selections_normalize_and_map_from_screen_coordinates() {
        assert_eq!(norm((9, 2), (3, 7)), [3, 2, 9, 7]);
        let cap = capture(10, 8);
        assert_eq!(cap.to_local(-100, 5), (0, 5));
    }
}
