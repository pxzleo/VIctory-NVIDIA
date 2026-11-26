//! Windows touch input and borderless fullscreen for the title window.

use std::cell::Cell;
use std::io;
use std::ptr::null_mut;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    GetLastError, HWND, LPARAM, LRESULT, POINT, RECT, SetLastError, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow, ScreenToClient,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::VK_F11;
use windows_sys::Win32::UI::Input::Pointer::{GetPointerInfo, POINTER_FLAG_CANCELED, POINTER_INFO};
use windows_sys::Win32::UI::WindowsAndMessaging::{self as wm, PT_TOUCH};

#[path = "window_controls/touch_overlay.rs"]
mod touch_overlay;

const LOOK_IDLE: Duration = Duration::from_millis(100);
const START_FULLSCREEN: u32 = wm::WM_APP + 0x4a3;
pub(super) fn hook_installed(hwnd: HWND) {
    touch_overlay::start_for_touchscreen(hwnd);
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Contact {
    window: usize,
    id: u32,
}

struct Drag {
    contact: Contact,
    anchor: POINT,
    point: POINT,
    pixels: f32,
    axes: [f32; 2],
    changed: Instant,
}

#[derive(Default)]
struct Touch {
    left: Option<Drag>,
    right: Option<Drag>,
}

impl Touch {
    fn start(&mut self, contact: Contact, point: POINT, size: RECT, now: Instant) {
        let side = size.right.min(size.bottom) as f32;
        if side <= 0.0 {
            return;
        }
        let (slot, pixels) = if point.x < size.right / 2 {
            (&mut self.left, (side * 0.10).clamp(32.0, 160.0))
        } else {
            (&mut self.right, side * 1.5)
        };
        if slot.is_none() {
            *slot = Some(Drag {
                contact,
                anchor: point,
                point,
                pixels,
                axes: [0.0; 2],
                changed: now,
            });
        }
    }

    fn move_to(&mut self, contact: Contact, point: POINT, now: Instant) {
        if let Some(left) = self.left.as_mut().filter(|drag| drag.contact == contact) {
            left.axes = stick(left.anchor, point, left.pixels, 0.12);
            left.point = point;
        }
        if let Some(right) = self.right.as_mut().filter(|drag| drag.contact == contact) {
            if point.x == right.point.x && point.y == right.point.y {
                return;
            }
            let seconds = now
                .duration_since(right.changed)
                .as_secs_f32()
                .clamp(1.0 / 120.0, 1.0 / 30.0);
            right.axes = stick(right.point, point, right.pixels * seconds, 0.0);
            right.point = point;
            right.changed = now;
        }
    }

    fn stop(&mut self, matches: impl Fn(Contact) -> bool) -> bool {
        let mut stopped = false;
        for slot in [&mut self.left, &mut self.right] {
            if slot.as_ref().is_some_and(|drag| matches(drag.contact)) {
                *slot = None;
                stopped = true;
            }
        }
        stopped
    }

    fn sticks(&self, now: Instant) -> [f32; 4] {
        let [lx, ly] = self.left.as_ref().map_or([0.0; 2], |drag| drag.axes);
        let [rx, ry] = self
            .right
            .as_ref()
            .filter(|drag| now.duration_since(drag.changed) < LOOK_IDLE)
            .map_or([0.0; 2], |drag| drag.axes);
        [lx, ly, rx, ry]
    }
}

fn stick(from: POINT, to: POINT, pixels: f32, deadzone: f32) -> [f32; 2] {
    let x = (to.x - from.x) as f32 / pixels;
    let y = (from.y - to.y) as f32 / pixels;
    let length = x.hypot(y);
    if length <= deadzone {
        return [0.0; 2];
    }
    let scale = (length.min(1.0) - deadzone) / (1.0 - deadzone) / length;
    [x * scale, y * scale]
}

static TOUCH: Mutex<Touch> = Mutex::new(Touch {
    left: None,
    right: None,
});

pub(super) fn sticks() -> [f32; 4] {
    let touch = TOUCH.lock().expect("touch state");
    touch.sticks(Instant::now())
}

fn refresh_pads(hwnd: HWND) {
    let touch = TOUCH.lock().expect("touch state");
    let drags = [&touch.left, &touch.right].map(|slot| {
        slot.as_ref()
            .filter(|drag| drag.contact.window == hwnd as usize)
            .map(|drag| (drag.anchor, drag.point))
    });
    drop(touch);
    touch_overlay::refresh(hwnd, drags);
}

/// Win32 implicitly captures a touch to the window that receives DOWN.
fn pointer(hwnd: HWND, message: u32, wparam: WPARAM) -> Option<LRESULT> {
    let contact = Contact {
        window: hwnd as usize,
        id: (wparam & 0xffff) as u32,
    };
    let mut info = POINTER_INFO::default();
    // SAFETY: the ID belongs to this pointer message and info is writable.
    if unsafe { GetPointerInfo(contact.id, &mut info) } == 0 {
        let stopped = TOUCH
            .lock()
            .expect("touch state")
            .stop(|active| active == contact);
        if stopped {
            refresh_pads(hwnd);
        }
        return stopped.then_some(0);
    }
    if info.pointerType != PT_TOUCH {
        return None;
    }
    let mut point = info.ptPixelLocation;
    let canceled = info.pointerFlags & POINTER_FLAG_CANCELED != 0;
    match message {
        wm::WM_POINTERUP | wm::WM_POINTERCAPTURECHANGED => {
            TOUCH
                .lock()
                .expect("touch state")
                .stop(|active| active == contact);
        }
        _ if canceled => {
            TOUCH
                .lock()
                .expect("touch state")
                .stop(|active| active == contact);
        }
        wm::WM_POINTERDOWN | wm::WM_POINTERUPDATE => {
            let mut size = RECT::default();
            // SAFETY: the live window owns this message. The coordinate and rectangle are writable.
            if unsafe { ScreenToClient(hwnd, &mut point) } != 0
                && (message == wm::WM_POINTERUPDATE
                    || unsafe { wm::GetClientRect(hwnd, &mut size) } != 0)
            {
                let mut touch = TOUCH.lock().expect("touch state");
                if message == wm::WM_POINTERDOWN {
                    touch.start(contact, point, size, Instant::now());
                } else {
                    touch.move_to(contact, point, Instant::now());
                }
            }
        }
        _ => {}
    }
    refresh_pads(hwnd);
    // Keep touch from also being promoted to gesture or mouse input.
    Some(0)
}

#[derive(Clone, Copy)]
struct Windowed {
    style: u32,
    placement: wm::WINDOWPLACEMENT,
}

thread_local! {
    // Only the title window's thread changes its mode. Never hold a borrow across
    // SetWindowPos, which synchronously sends more messages to this procedure.
    static WINDOWED: Cell<Option<(HWND, Windowed)>> = const { Cell::new(None) };
}

fn set_style(hwnd: HWND, style: u32) -> io::Result<()> {
    // SAFETY: hwnd belongs to the calling window procedure. A previous style of zero is valid.
    unsafe {
        SetLastError(0);
        if wm::SetWindowLongPtrW(hwnd, wm::GWL_STYLE, style as isize) == 0 && GetLastError() != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn window_pos(hwnd: HWND, area: Option<RECT>) -> io::Result<()> {
    let mut flags = wm::SWP_FRAMECHANGED | wm::SWP_NOACTIVATE | wm::SWP_NOZORDER;
    if area.is_none() {
        flags |= wm::SWP_NOMOVE | wm::SWP_NOSIZE;
    }
    let area = area.unwrap_or_default();
    // SAFETY: hwnd is the current window. A null insert-after is ignored by SWP_NOZORDER.
    let changed = unsafe {
        wm::SetWindowPos(
            hwnd,
            null_mut(),
            area.left,
            area.top,
            area.right - area.left,
            area.bottom - area.top,
            flags,
        )
    };
    if changed == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn restore(hwnd: HWND, windowed: Windowed) -> io::Result<()> {
    set_style(hwnd, windowed.style & !(wm::WS_MAXIMIZE | wm::WS_MINIMIZE))?;
    // SAFETY: placement came from GetWindowPlacement for this window.
    if unsafe { wm::SetWindowPlacement(hwnd, &windowed.placement) } == 0 {
        return Err(io::Error::last_os_error());
    }
    window_pos(hwnd, None)
}

fn fullscreen(hwnd: HWND, startup: bool) -> io::Result<()> {
    if let Some((window, state)) = WINDOWED.get() {
        if window == hwnd && !startup {
            restore(hwnd, state)?;
            WINDOWED.set(None);
        }
        return Ok(());
    }
    let mut placement = wm::WINDOWPLACEMENT {
        length: size_of::<wm::WINDOWPLACEMENT>() as u32,
        ..Default::default()
    };
    let mut monitor = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: hwnd is the title's live window and both outputs are initialized and writable.
    let style = unsafe {
        if wm::GetWindowPlacement(hwnd, &mut placement) == 0
            || GetMonitorInfoW(
                MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST),
                &mut monitor,
            ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        wm::GetWindowLongPtrW(hwnd, wm::GWL_STYLE) as u32
    };
    let saved = Windowed { style, placement };
    if style & (wm::WS_MAXIMIZE | wm::WS_MINIMIZE) != 0 {
        // SAFETY: normalizes a maximized window before changing its style; saved placement restores it.
        unsafe { wm::ShowWindow(hwnd, wm::SW_RESTORE) };
    }
    if let Err(error) = set_style(
        hwnd,
        (style & !(wm::WS_OVERLAPPEDWINDOW | wm::WS_MAXIMIZE | wm::WS_MINIMIZE)) | wm::WS_POPUP,
    )
    .and_then(|()| window_pos(hwnd, Some(monitor.rcMonitor)))
    {
        if let Err(restore_error) = restore(hwnd, saved) {
            tracing::warn!(%restore_error, "could not restore the title window after fullscreen failed");
        }
        return Err(error);
    }
    WINDOWED.set(Some((hwnd, saved)));
    Ok(())
}

pub(super) fn message(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    match message {
        wm::WM_POINTERDOWN
        | wm::WM_POINTERUPDATE
        | wm::WM_POINTERUP
        | wm::WM_POINTERENTER
        | wm::WM_POINTERLEAVE
        | wm::WM_POINTERCAPTURECHANGED => return pointer(hwnd, message, wparam),
        wm::WM_KILLFOCUS | wm::WM_CANCELMODE | wm::WM_SIZE | wm::WM_NCDESTROY => {
            TOUCH
                .lock()
                .expect("touch state")
                .stop(|contact| contact.window == hwnd as usize);
            if message == wm::WM_NCDESTROY {
                touch_overlay::forget(hwnd);
            } else if message == wm::WM_KILLFOCUS {
                touch_overlay::hide(hwnd);
            } else {
                refresh_pads(hwnd);
            }
            if message == wm::WM_NCDESTROY
                && WINDOWED.get().is_some_and(|(window, _)| window == hwnd)
            {
                WINDOWED.set(None);
            }
        }
        wm::WM_ACTIVATE if wparam & 0xffff == wm::WA_INACTIVE as usize => touch_overlay::hide(hwnd),
        wm::WM_ACTIVATE | wm::WM_ACTIVATEAPP | wm::WM_SETFOCUS | wm::WM_WINDOWPOSCHANGED => {
            refresh_pads(hwnd)
        }
        wm::WM_TIMER if wparam == touch_overlay::REFRESH_TIMER => {
            refresh_pads(hwnd);
            return Some(0);
        }
        wm::WM_KEYDOWN if wparam == VK_F11 as usize => {
            if (lparam & (1 << 30)) == 0 {
                if let Err(error) = fullscreen(hwnd, false) {
                    tracing::warn!(%error, "could not switch title fullscreen mode");
                }
            }
            return Some(0);
        }
        START_FULLSCREEN => {
            if let Err(error) = fullscreen(hwnd, true) {
                tracing::warn!(%error, "could not start the title fullscreen");
            }
            return Some(0);
        }
        wm::WM_SHOWWINDOW if wparam != 0 && lparam == 0 => {
            static REQUESTED: AtomicBool = AtomicBool::new(false);
            // SAFETY: the window is live. After ShowWindow finishes, the queued message resizes it.
            unsafe {
                if std::env::var_os("VICTORY_FULLSCREEN").is_some_and(|value| value == "1")
                    && wm::GetAncestor(hwnd, wm::GA_ROOTOWNER) == hwnd
                    && !REQUESTED.swap(true, Ordering::Relaxed)
                    && wm::PostMessageW(hwnd, START_FULLSCREEN, 0, 0) == 0
                {
                    REQUESTED.store(false, Ordering::Relaxed);
                    tracing::warn!(error = %io::Error::last_os_error(), "could not request title fullscreen");
                }
            }
        }
        _ => {}
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simultaneous_contacts_keep_their_roles_and_release_independently() {
        let mut touch = Touch::default();
        let now = Instant::now();
        let window = 1;
        let left = Contact { window, id: 1 };
        let right = Contact { window, id: 2 };
        let extra = Contact { window, id: 3 };
        let size = RECT {
            right: 1200,
            bottom: 800,
            ..Default::default()
        };
        touch.start(left, POINT { x: 590, y: 400 }, size, now);
        touch.start(right, POINT { x: 610, y: 400 }, size, now);
        touch.start(extra, POINT { x: 400, y: 400 }, size, now);
        let next = now + Duration::from_millis(16);
        touch.move_to(left, POINT { x: 700, y: 400 }, next);
        touch.move_to(right, POINT { x: 580, y: 400 }, next);
        touch.move_to(extra, POINT { x: 500, y: 300 }, next);
        assert_eq!(touch.sticks(next), [1.0, 0.0, -1.0, 0.0]);
        assert_eq!(touch.sticks(next + LOOK_IDLE), [1.0, 0.0, 0.0, 0.0]);
        assert!(touch.stop(|contact| contact == left));
        assert_eq!(touch.sticks(next), [0.0, 0.0, -1.0, 0.0]);
        assert!(touch.stop(|contact| contact.window == window));
        assert_eq!(touch.sticks(next), [0.0; 4]);
    }

    #[test]
    fn stick_has_a_radial_deadzone_and_preserves_diagonals() {
        let origin = POINT { x: 100, y: 100 };
        assert_eq!(
            stick(origin, POINT { x: 105, y: 102 }, 50.0, 0.12),
            [0.0; 2]
        );
        let [x, y] = stick(origin, POINT { x: 150, y: 50 }, 50.0, 0.12);
        assert_eq!(x, y);
        assert!((x.hypot(y) - 1.0).abs() < 1e-6);
    }
}
