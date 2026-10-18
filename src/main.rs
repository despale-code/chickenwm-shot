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
use eframe::egui::{self, Color32, Pos2, Rect, Stroke, Vec2};
use std::mem::size_of;
use std::path::PathBuf;
use std::time::Instant;
use windows::core::w;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DwmSetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
    DWMWA_TRANSITIONS_FORCEDISABLED,
};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
    ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, SRCCOPY,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetCursorPos, GetSystemMetrics, GetWindowLongPtrW, GetWindowTextW,
    IsIconic, IsWindowVisible, SetForegroundWindow, SetWindowPos, ShowWindow, GWL_EXSTYLE,
    HWND_TOPMOST, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    SWP_NOACTIVATE, SW_HIDE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT,
};

/// `CF_DIB`.
const CF_DIB: u32 = 8;
/// Largest texture tile uploaded at once (GPUs cap texture sizes; a multi-monitor desktop can
/// exceed them).
const TILE: usize = 4096;

/// The whole virtual desktop, in physical pixels.
struct Capture {
    /// Top-left of the virtual screen (can be negative with monitors left of/above the primary).
    origin: (i32, i32),
    width: usize,
    height: usize,
    rgba: Vec<u8>,
}

/// `[x0, y0, x1, y1]` in capture pixels.
type PxRect = [i32; 4];

fn capture_desktop() -> Result<Capture> {
    unsafe {
        let x = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let y = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let w = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let h = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        if w <= 0 || h <= 0 {
            bail!("no screen to capture");
        }
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(screen);
        let bmp = CreateCompatibleBitmap(screen, w, h);
        let old = SelectObject(mem, bmp);
        let copied = BitBlt(mem, 0, 0, w, h, screen, x, y, SRCCOPY).is_ok();
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
        SelectObject(mem, old);
        let _ = DeleteObject(bmp);
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        if !copied || lines == 0 {
            bail!("couldn't read the screen");
        }
        for px in rgba.chunks_exact_mut(4) {
            px.swap(0, 2); // BGRA -> RGBA
            px[3] = 255;
        }
        Ok(Capture { origin: (x, y), width: w as usize, height: h as usize, rgba })
    }
}

impl Capture {
    fn crop(&self, r: PxRect) -> Option<image::RgbaImage> {
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

    fn to_local(&self, x: i32, y: i32) -> (i32, i32) {
        (x - self.origin.0, y - self.origin.1)
    }
}

/// Visible top-level windows, front to back, as capture-pixel rects (for click-to-capture).
fn window_rects(cap: &Capture) -> Vec<PxRect> {
    struct Ctx<'a> {
        cap: &'a Capture,
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
                return true.into(); // click-through overlays (ChickenWM's borders)
            }
            let mut cloaked = 0u32;
            if DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut u32 as *mut _, 4).is_ok() && cloaked != 0 {
                return true.into();
            }
            let mut class = [0u16; 64];
            let n = GetClassNameW(hwnd, &mut class);
            let class = String::from_utf16_lossy(&class[..n.max(0) as usize]);
            // The desktop and wallpaper: clicking there takes the whole monitor instead.
            if matches!(class.as_str(), "Progman" | "WorkerW" | "ChickenWMPaperClass") {
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
            let (x0, y0) = ctx.cap.to_local(rc.left, rc.top);
            let (x1, y1) = ctx.cap.to_local(rc.right, rc.bottom);
            ctx.out.push([x0, y0, x1, y1]);
        }
        true.into()
    }
    let mut ctx = Ctx { cap, out: Vec::new() };
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM(&mut ctx as *mut Ctx as isize));
    }
    ctx.out
}

fn monitor_rects(cap: &Capture) -> Vec<PxRect> {
    chickenwm_core::monitor::enumerate_monitors()
        .iter()
        .map(|m| {
            let (x, y) = cap.to_local(m.x, m.y);
            [x, y, x + m.width, y + m.height]
        })
        .collect()
}

fn contains(r: &PxRect, p: (i32, i32)) -> bool {
    p.0 >= r[0] && p.0 < r[2] && p.1 >= r[1] && p.1 < r[3]
}

fn cursor_px(cap: &Capture) -> (i32, i32) {
    let mut pt = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut pt);
    }
    cap.to_local(pt.x, pt.y)
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

/// Hands `bytes` to the clipboard as `format`; the memory is freed if the clipboard refuses it
/// (on success the system owns it).
/// The clipboard must be open.
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

/// Puts the image on the clipboard as a bitmap (every app) and as PNG (apps that prefer it).
/// `owner` is the overlay window when there is one (a NULL-owned clipboard may refuse data).
fn copy_to_clipboard(img: &image::RgbaImage, png: &[u8], owner: Option<HWND>) -> Result<()> {
    let (w, h) = img.dimensions();
    let header = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: w as i32,
        biHeight: h as i32, // bottom-up: the most widely understood layout
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

/// Clipboard + file + notification for the chosen area.
fn deliver(cap: &Capture, area: PxRect, owner: Option<HWND>) -> Result<()> {
    let img = cap.crop(area).context("empty selection")?;
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

// ───────────────────────────── overlay ─────────────────────────────

struct ShotApp {
    cap: Capture,
    tiles: Vec<(egui::TextureHandle, PxRect)>,
    windows: Vec<PxRect>,
    monitors: Vec<PxRect>,
    accent: Color32,
    font: chickenwm_core::fonts::EguiFont,
    hwnd: Option<HWND>,
    placed: bool,
    opened: Instant,
    drag_from: Option<(i32, i32)>,
    dragging: bool,
}

impl ShotApp {
    fn new(cap: Capture) -> Self {
        let accent = load_css_sheet("roobar.css")
            .ok()
            .map(|s| {
                let c = s.var_color("--accent-color", chickenwm_core::config::RgbaColor { r: 217, g: 215, b: 212, a: 255 });
                Color32::from_rgb(c.r, c.g, c.b)
            })
            .unwrap_or(Color32::from_rgb(217, 215, 212));
        let windows = window_rects(&cap);
        let monitors = monitor_rects(&cap);
        Self {
            cap,
            tiles: Vec::new(),
            windows,
            monitors,
            accent,
            font: Default::default(),
            hwnd: None,
            placed: false,
            opened: Instant::now(),
            drag_from: None,
            dragging: false,
        }
    }

    fn upload_tiles(&mut self, ctx: &egui::Context) {
        let side = ctx.input(|i| i.max_texture_side).min(TILE).max(256);
        let (w, h) = (self.cap.width, self.cap.height);
        for ty in (0..h).step_by(side) {
            for tx in (0..w).step_by(side) {
                let tw = side.min(w - tx);
                let th = side.min(h - ty);
                let mut px = Vec::with_capacity(tw * th * 4);
                for y in ty..ty + th {
                    let row = (y * w + tx) * 4;
                    px.extend_from_slice(&self.cap.rgba[row..row + tw * 4]);
                }
                let img = egui::ColorImage::from_rgba_unmultiplied([tw, th], &px);
                let tex = ctx.load_texture(format!("shot-{tx}-{ty}"), img, egui::TextureOptions::NEAREST);
                self.tiles.push((tex, [tx as i32, ty as i32, (tx + tw) as i32, (ty + th) as i32]));
            }
        }
    }

    /// Covers the whole virtual desktop, above everything, without Windows' open animation.
    fn place_window(&mut self, frame: &eframe::Frame) {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        if self.placed {
            return;
        }
        let Ok(handle) = frame.window_handle() else { return };
        let RawWindowHandle::Win32(h) = handle.as_raw() else { return };
        let hwnd = HWND(h.hwnd.get() as *mut core::ffi::c_void);
        unsafe {
            let off = windows::Win32::Foundation::BOOL(1);
            let _ = DwmSetWindowAttribute(hwnd, DWMWA_TRANSITIONS_FORCEDISABLED, &off as *const _ as *const _, 4);
            let _ = SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                self.cap.origin.0,
                self.cap.origin.1,
                self.cap.width as i32,
                self.cap.height as i32,
                SWP_NOACTIVATE,
            );
            let _ = SetForegroundWindow(hwnd);
        }
        self.hwnd = Some(hwnd);
        self.placed = true;
    }

    fn finish(&self, area: Option<PxRect>) -> ! {
        if let Some(hwnd) = self.hwnd {
            unsafe {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
        }
        if let Some(area) = area {
            if let Err(e) = deliver(&self.cap, area, self.hwnd) {
                tracing::warn!("screenshot failed: {e:#}");
            }
        }
        std::process::exit(0)
    }
}

fn norm(a: (i32, i32), b: (i32, i32)) -> PxRect {
    [a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1)]
}

impl eframe::App for ShotApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 1.0]
    }

    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        if self.tiles.is_empty() {
            self.upload_tiles(ctx);
            if let Ok(sheet) = load_css_sheet("roobar.css") {
                self.font.sync(ctx, &sheet, true);
            }
        }
        self.place_window(frame);

        let ppp = ctx.pixels_per_point();
        let to_pt = |r: PxRect| Rect::from_min_max(Pos2::new(r[0] as f32 / ppp, r[1] as f32 / ppp), Pos2::new(r[2] as f32 / ppp, r[3] as f32 / ppp));
        // The real cursor position (pointer events only start once the window has focus).
        let cursor = cursor_px(&self.cap);

        // ── input ──
        let (pressed, released, secondary, esc, space, enter) = ctx.input(|i| {
            (
                i.pointer.primary_pressed(),
                i.pointer.primary_released(),
                i.pointer.secondary_clicked(),
                i.key_pressed(egui::Key::Escape),
                i.key_pressed(egui::Key::Space),
                i.key_pressed(egui::Key::Enter),
            )
        });
        if esc || secondary {
            self.finish(None);
        }
        let monitor = self.monitors.iter().find(|m| contains(m, cursor)).copied();
        if space {
            self.finish(monitor);
        }
        if pressed {
            self.drag_from = Some(cursor);
            self.dragging = false;
        }
        if let Some(from) = self.drag_from {
            if (cursor.0 - from.0).abs() > 4 || (cursor.1 - from.1).abs() > 4 {
                self.dragging = true;
            }
        }
        let hovered_window = self.windows.iter().find(|r| contains(r, cursor)).copied();
        let selection = match (self.drag_from, self.dragging) {
            (Some(from), true) => Some(norm(from, cursor)),
            _ => hovered_window.or(monitor),
        };
        if released && self.drag_from.is_some() {
            self.finish(selection);
        }
        if enter {
            self.finish(selection);
        }

        // ── drawing ──
        let fade = (self.opened.elapsed().as_secs_f32() / 0.14).min(1.0);
        egui::CentralPanel::default().frame(egui::Frame::NONE).show(ctx, |ui| {
            let painter = ui.painter();
            let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
            for (tex, r) in &self.tiles {
                painter.image(tex.id(), to_pt(*r), uv, Color32::WHITE);
            }
            let full = to_pt([0, 0, self.cap.width as i32, self.cap.height as i32]);
            let dim = Color32::from_black_alpha((120.0 * fade) as u8);
            match selection {
                Some(sel) => {
                    let s = to_pt(sel).intersect(full);
                    // Darken everything but the selection.
                    for r in [
                        Rect::from_min_max(full.min, Pos2::new(full.max.x, s.min.y)),
                        Rect::from_min_max(Pos2::new(full.min.x, s.max.y), full.max),
                        Rect::from_min_max(Pos2::new(full.min.x, s.min.y), Pos2::new(s.min.x, s.max.y)),
                        Rect::from_min_max(Pos2::new(s.max.x, s.min.y), Pos2::new(full.max.x, s.max.y)),
                    ] {
                        painter.rect_filled(r, 0.0, dim);
                    }
                    let stroke = Stroke::new(2.0_f32, self.accent.gamma_multiply(fade));
                    painter.rect_stroke(s, 0.0, stroke, egui::StrokeKind::Outside);
                    // Size badge under (or inside, near the bottom of the screen) the selection.
                    let text = format!("{} × {}", sel[2] - sel[0], sel[3] - sel[1]);
                    let font = egui::FontId::proportional(13.0);
                    let galley = painter.layout_no_wrap(text, font, Color32::WHITE);
                    let mut pos = Pos2::new(s.min.x, s.max.y + 8.0);
                    if pos.y + galley.size().y + 12.0 > full.max.y {
                        pos.y = s.max.y - galley.size().y - 16.0;
                    }
                    let badge = Rect::from_min_size(pos, galley.size() + Vec2::new(14.0, 8.0));
                    painter.rect_filled(badge, 6.0, Color32::from_black_alpha((190.0 * fade) as u8));
                    painter.galley(badge.min + Vec2::new(7.0, 4.0), galley, Color32::WHITE);
                }
                None => {
                    painter.rect_filled(full, 0.0, dim);
                }
            }

            // Hint on the monitor under the cursor.
            if let Some(m) = monitor {
                let m = to_pt(m);
                let hint = "Drag to select  ·  Click a window  ·  Space: whole screen  ·  Esc: cancel";
                let galley = painter.layout_no_wrap(hint.to_owned(), egui::FontId::proportional(14.0), Color32::WHITE);
                let size = galley.size() + Vec2::new(28.0, 16.0);
                let pill = Rect::from_center_size(Pos2::new(m.center().x, m.min.y + 56.0 + size.y / 2.0), size);
                painter.rect_filled(pill, size.y / 2.0, Color32::from_black_alpha((200.0 * fade) as u8));
                painter.rect_stroke(pill, size.y / 2.0, Stroke::new(1.0_f32, self.accent.gamma_multiply(0.6 * fade)), egui::StrokeKind::Inside);
                painter.galley(pill.min + Vec2::new(14.0, 8.0), galley, Color32::WHITE.gamma_multiply(fade));
            }
        });
        // Follow the cursor even before the window has focus.
        ctx.request_repaint();
    }
}

fn main() -> Result<()> {
    chickenwm_core::init_tracing("chickenwm_shot");
    // Physical pixels everywhere, before anything reads screen metrics.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    // One at a time: a held / repeated Print key must not stack overlays.
    let _instance = match chickenwm_core::instance::claim_instance(r"Local\ChickenWMShot", 0) {
        chickenwm_core::instance::Instance::Primary(guard) => guard,
        chickenwm_core::instance::Instance::Duplicate(_) => return Ok(()),
    };

    let cap = capture_desktop()?;
    if std::env::args().any(|a| a == "--full") {
        let cursor = cursor_px(&cap);
        let area = monitor_rects(&cap).into_iter().find(|m| contains(m, cursor));
        return deliver(&cap, area.unwrap_or([0, 0, cap.width as i32, cap.height as i32]), None);
    }

    let options = eframe::NativeOptions {
        // glow, not wgpu: egui_kittest (headless feature) also enables eframe's wgpu backend,
        // which eframe would then prefer, and wgpu draws transparent windows black on Windows.
        renderer: eframe::Renderer::Glow,
        viewport: egui::ViewportBuilder::default()
            .with_title("ChickenWM Screenshot")
            .with_decorations(false)
            .with_always_on_top()
            .with_taskbar(false) // a tool window: the WM never tiles it
            .with_resizable(false)
            .with_active(true)
            // Moved over the whole desktop on the first frame (in physical pixels).
            .with_inner_size([1.0, 1.0]),
        ..Default::default()
    };
    eframe::run_native("ChickenWM Screenshot", options, Box::new(move |_cc| Ok(Box::new(ShotApp::new(cap)))))
        .map_err(|e| anyhow::anyhow!("eframe error: {e}"))?;
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

    /// Real desktop capture, written where `CHICKENWM_SHOT_TEST_OUT` points (no clipboard, no
    /// notification). Run with `cargo test -p chickenwm-shot -- --ignored`.
    #[test]
    #[ignore]
    fn captures_the_real_desktop() {
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        let cap = capture_desktop().expect("capture");
        let mons = monitor_rects(&cap);
        assert!(!mons.is_empty());
        for m in &mons {
            assert!(m[0] >= 0 && m[1] >= 0 && m[2] as usize <= cap.width && m[3] as usize <= cap.height, "{m:?} inside {}x{}", cap.width, cap.height);
        }
        assert!(cap.rgba.chunks_exact(4).any(|p| p[0] > 16 || p[1] > 16 || p[2] > 16), "not all black");
        if let Ok(out) = std::env::var("CHICKENWM_SHOT_TEST_OUT") {
            cap.crop(mons[0]).unwrap().save(out).unwrap();
        }
        println!("desktop {}x{} at {:?}, monitors {mons:?}, windows {}", cap.width, cap.height, cap.origin, window_rects(&cap).len());
    }

    #[test]
    fn selections_normalize_and_map_from_screen_coordinates() {
        assert_eq!(norm((9, 2), (3, 7)), [3, 2, 9, 7]);
        let cap = capture(10, 8);
        assert_eq!(cap.to_local(-100, 5), (0, 5));
    }
}
