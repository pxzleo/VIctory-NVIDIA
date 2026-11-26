//! Two small desktop-composited controls. They never render into the swap chain.

use std::cell::RefCell;
use std::io;
use std::ptr::null_mut;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{HWND, POINT, RECT, SIZE};
use windows_sys::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, ClientToScreen,
    CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, HBITMAP, HDC,
    HGDIOBJ, SelectObject,
};
use windows_sys::Win32::UI::WindowsAndMessaging as wm;
use windows_sys::core::w;

type Drags = [Option<(POINT, POINT)>; 2];
pub(super) const REFRESH_TIMER: usize = 0x64a4;

thread_local! {
    static OVERLAY: RefCell<Option<(HWND, [Pad; 2])>> = const { RefCell::new(None) };
}

fn enabled() -> Option<bool> {
    static ENABLED: OnceLock<Option<bool>> = OnceLock::new();
    *ENABLED.get_or_init(
        || match std::env::var_os("VICTORY_TOUCH_OVERLAY").as_deref() {
            Some(value) if value == "0" => Some(false),
            Some(value) if value == "1" => Some(true),
            // SAFETY: plain system metric query.
            _ if unsafe { wm::GetSystemMetrics(wm::SM_MAXIMUMTOUCHES) } > 0 => Some(true),
            _ => None,
        },
    )
}

pub(super) fn start_for_touchscreen(hwnd: HWND) {
    // SAFETY: this is a live window just hooked on the calling thread.
    if enabled() == Some(true) && unsafe { wm::GetAncestor(hwnd, wm::GA_ROOTOWNER) } == hwnd {
        watch_foreground(hwnd);
    }
}

fn watch_foreground(hwnd: HWND) {
    // A remote desktop can return the foreground without changing the title
    // thread's active window. Recheck visibility even when no focus message arrives.
    // SAFETY: the timer posts to the owner until that window is destroyed.
    if unsafe { wm::SetTimer(hwnd, REFRESH_TIMER, 250, None) } == 0 {
        tracing::warn!(error = %io::Error::last_os_error(), "could not watch touch control visibility");
    }
}

pub(super) fn refresh(owner: HWND, drags: Drags) {
    if enabled() == Some(false) {
        return;
    }
    OVERLAY.with(|state| {
        let Ok(mut state) = state.try_borrow_mut() else {
            return;
        };
        if let Err(error) = update(&mut state, owner, drags) {
            static LOGGED: std::sync::Once = std::sync::Once::new();
            LOGGED.call_once(|| tracing::warn!(%error, "could not display the touch controls"));
        }
    });
}

fn update(state: &mut Option<(HWND, [Pad; 2])>, owner: HWND, drags: Drags) -> io::Result<()> {
    let existing = state.as_ref().is_some_and(|(window, _)| *window == owner);
    // SAFETY: the caller owns this live title window.
    let active = unsafe {
        wm::GetForegroundWindow() == owner
            && wm::IsWindowVisible(owner) != 0
            && wm::IsIconic(owner) == 0
    };
    if !active {
        if existing {
            for pad in &mut state.as_mut().unwrap().1 {
                pad.hide();
            }
        }
        return Ok(());
    }
    if !existing && enabled() != Some(true) && drags.iter().all(Option::is_none) {
        return Ok(());
    }
    let mut rect = RECT::default();
    let mut origin = POINT::default();
    // SAFETY: the client rectangle and client origin have writable storage.
    if unsafe { wm::GetClientRect(owner, &mut rect) } == 0
        || unsafe { ClientToScreen(owner, &mut origin) } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if rect.right.min(rect.bottom) < 112 {
        if existing {
            for pad in &mut state.as_mut().unwrap().1 {
                pad.hide();
            }
        }
        return Ok(());
    }
    if !existing {
        *state = Some((owner, [Pad::new(owner)?, Pad::new(owner)?]));
        if enabled().is_none() {
            watch_foreground(owner);
        }
    }
    let pads = &mut state.as_mut().unwrap().1;
    for (index, (pad, drag)) in pads.iter_mut().zip(drags).enumerate() {
        pad.show(pose(rect, origin, index, drag))?;
    }
    Ok(())
}

pub(super) fn forget(hwnd: HWND) {
    OVERLAY.with(|state| {
        let mut state = state.borrow_mut();
        if state.as_ref().is_some_and(|(owner, _)| *owner == hwnd) {
            *state = None;
        }
    });
}

pub(super) fn hide(hwnd: HWND) {
    OVERLAY.with(|state| {
        if let Ok(mut state) = state.try_borrow_mut() {
            if let Some((_, pads)) = state.as_mut().filter(|(owner, _)| *owner == hwnd) {
                for pad in pads {
                    pad.hide();
                }
            }
        }
    });
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Pose {
    center: (i32, i32),
    thumb: (i32, i32),
    radius: i32,
    active: bool,
}

fn pose(rect: RECT, origin: POINT, index: usize, drag: Option<(POINT, POINT)>) -> Pose {
    let radius = (rect.right.min(rect.bottom) / 17).clamp(32, 76);
    let inset = radius + 2;
    let fallback = POINT {
        x: rect.right * [18, 82][index] / 100,
        y: rect.bottom * 73 / 100,
    };
    let (anchor, finger) = drag.unwrap_or((fallback, fallback));
    let feel = (rect.right.min(rect.bottom) as f32 * 0.10).clamp(32.0, 160.0);
    let [x, y] = super::stick(anchor, finger, feel, 0.0);
    let reach = radius as f32 * 0.64;
    Pose {
        center: (
            origin.x + anchor.x.clamp(inset, rect.right - inset),
            origin.y + anchor.y.clamp(inset, rect.bottom - inset),
        ),
        thumb: ((x * reach).round() as i32, (-y * reach).round() as i32),
        radius,
        active: drag.is_some(),
    }
}

struct Pad {
    window: HWND,
    dib: Option<Bitmap>,
    shown: Option<Pose>,
}

impl Pad {
    fn new(owner: HWND) -> io::Result<Self> {
        // SAFETY: a same-thread owned popup; the layered and transparent styles let input pass through.
        unsafe {
            let window = wm::CreateWindowExW(
                wm::WS_EX_LAYERED
                    | wm::WS_EX_TRANSPARENT
                    | wm::WS_EX_NOACTIVATE
                    | wm::WS_EX_TOOLWINDOW,
                w!("STATIC"),
                w!(""),
                wm::WS_POPUP,
                0,
                0,
                0,
                0,
                owner,
                null_mut(),
                null_mut(),
                null_mut(),
            );
            if window.is_null() {
                return Err(io::Error::last_os_error());
            }
            let pad = Self {
                window,
                dib: None,
                shown: None,
            };
            // The presenter's window search must skip the HUD's popups.
            if wm::SetPropW(window, w!("victory.touch-pad"), std::ptr::dangling_mut()) == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(pad)
        }
    }

    fn hide(&mut self) {
        if self.shown.take().is_some() {
            // SAFETY: this popup belongs to the current thread.
            unsafe { wm::ShowWindow(self.window, wm::SW_HIDE) };
        }
    }

    fn show(&mut self, pose: Pose) -> io::Result<()> {
        if self.shown == Some(pose) {
            return Ok(());
        }
        let (x, y) = pose.center;
        let edge = pose.radius + 1;
        let size = 2 * edge + 1;
        let at = POINT {
            x: x - edge,
            y: y - edge,
        };
        if self.shown.is_some_and(|last| {
            Pose {
                center: pose.center,
                ..last
            } == pose
        }) {
            // SAFETY: changing only the popup position preserves its composition bitmap.
            let moved = unsafe {
                wm::SetWindowPos(
                    self.window,
                    null_mut(),
                    at.x,
                    at.y,
                    0,
                    0,
                    wm::SWP_NOACTIVATE | wm::SWP_NOZORDER | wm::SWP_NOSIZE,
                )
            };
            if moved == 0 {
                return Err(io::Error::last_os_error());
            }
            self.shown = Some(pose);
            return Ok(());
        }
        if self.dib.as_ref().is_none_or(|dib| dib.size != size) {
            self.dib = Some(Bitmap::new(size)?);
        }
        let dib = self.dib.as_mut().unwrap();
        dib.paint(pose);
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        // SAFETY: the selected 32-bit bitmap matches the popup's size and uses premultiplied alpha.
        unsafe {
            if wm::UpdateLayeredWindow(
                self.window,
                null_mut(),
                &at,
                &SIZE { cx: size, cy: size },
                dib.dc,
                &POINT::default(),
                0,
                &blend,
                wm::ULW_ALPHA,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            if self.shown.is_none() {
                wm::ShowWindow(self.window, wm::SW_SHOWNOACTIVATE);
            }
        }
        self.shown = Some(pose);
        Ok(())
    }
}

impl Drop for Pad {
    fn drop(&mut self) {
        // SAFETY: same-thread popup. DestroyWindow also handles an already-destroyed owned window.
        unsafe {
            wm::RemovePropW(self.window, w!("victory.touch-pad"));
            wm::DestroyWindow(self.window);
        }
    }
}

struct Bitmap {
    dc: HDC,
    handle: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut u32,
    size: i32,
}

impl Bitmap {
    fn new(size: i32) -> io::Result<Self> {
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size,
                biHeight: -size,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..Default::default()
            },
            ..Default::default()
        };
        // SAFETY: this memory DC and its top-down DIB are owned by this object.
        unsafe {
            let dc = CreateCompatibleDC(null_mut());
            if dc.is_null() {
                return Err(io::Error::last_os_error());
            }
            let mut bitmap = Bitmap {
                dc,
                handle: null_mut(),
                previous: null_mut(),
                bits: null_mut(),
                size,
            };
            let mut bits = null_mut();
            bitmap.handle = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            if bitmap.handle.is_null() || bits.is_null() {
                return Err(io::Error::last_os_error());
            }
            bitmap.previous = SelectObject(dc, bitmap.handle);
            if bitmap.previous.is_null() {
                return Err(io::Error::last_os_error());
            }
            bitmap.bits = bits.cast();
            Ok(bitmap)
        }
    }

    fn paint(&mut self, pose: Pose) {
        // SAFETY: the private top-down 32-bit DIB has size * size pixels.
        let pixels =
            unsafe { std::slice::from_raw_parts_mut(self.bits, (self.size * self.size) as usize) };
        for (i, pixel) in pixels.iter_mut().enumerate() {
            let (x, y) = (
                i as i32 % self.size - self.size / 2,
                i as i32 / self.size - self.size / 2,
            );
            *pixel = paint_pixel(pose, x, y);
        }
    }
}

impl Drop for Bitmap {
    fn drop(&mut self) {
        // SAFETY: detach the DIB before deleting it and its private DC.
        unsafe {
            if !self.previous.is_null() {
                SelectObject(self.dc, self.previous);
            }
            if !self.handle.is_null() {
                DeleteObject(self.handle);
            }
            DeleteDC(self.dc);
        }
    }
}

fn coverage(distance: f32, radius: f32) -> f32 {
    (radius - distance + 0.5).clamp(0.0, 1.0)
}

fn paint_pixel(pose: Pose, x: i32, y: i32) -> u32 {
    let radius = pose.radius as f32;
    let distance = ((x * x + y * y) as f32).sqrt();
    let outer = coverage(distance, radius);
    let inner = coverage(distance, radius - 1.6);
    let (dx, dy) = (x - pose.thumb.0, y - pose.thumb.1);
    let thumb_distance = ((dx * dx + dy * dy) as f32).sqrt();
    let thumb = coverage(thumb_distance, radius * 0.28) * if pose.active { 0.76 } else { 0.38 };
    let ring = (outer - inner) * if pose.active { 0.69 } else { 0.47 };
    let shade = inner * if pose.active { 0.24 } else { 0.17 };
    let base = ring + shade;
    let alpha = thumb + base * (1.0 - thumb);
    let rgb = [235.0, 248.0, 255.0].map(|value| {
        (value * thumb + (value * ring + 18.0 * shade) * (1.0 - thumb)).round() as u32
    });
    ((alpha * 255.0).round() as u32) << 24 | rgb[0] << 16 | rgb[1] << 8 | rgb[2]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn centers_stay_in_the_client_and_active_thumb_tracks_the_finger() {
        let rect = RECT {
            right: 1280,
            bottom: 720,
            ..Default::default()
        };
        let origin = POINT { x: -1280, y: 0 };
        let idle = pose(rect, origin, 1, None);
        assert_eq!(idle.thumb, (0, 0));
        assert!(!idle.active);
        let active = pose(
            rect,
            origin,
            1,
            Some((POINT { x: 1279, y: 1 }, POINT { x: 1200, y: 1 })),
        );
        assert_eq!(active.center.0, -active.radius - 2);
        assert_eq!(active.center.1, active.radius + 2);
        assert!(active.active && active.thumb.0 < 0);
        let pixel = paint_pixel(active, active.thumb.0, active.thumb.1);
        let alpha = pixel >> 24;
        assert!((0..255).contains(&alpha) && alpha > 0);
        assert!((pixel & 255) <= alpha);
        assert_eq!(paint_pixel(active, active.radius + 1, active.radius + 1), 0);
    }
}
