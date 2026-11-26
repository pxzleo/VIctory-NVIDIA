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

const FACE_TAP: Duration = Duration::from_millis(85);
const START_FULLSCREEN: u32 = wm::WM_APP + 0x4a3;
pub(super) fn hook_installed(hwnd: HWND) {
    touch_overlay::start_for_touchscreen(hwnd);
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Contact {
    window: usize,
    id: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Action {
    Lt,
    Rt,
    A,
    B,
    X,
    Y,
    Menu,
    View,
    Lb,
    Rb,
    Ls,
    Rs,
    Up,
    Down,
    Left,
    Right,
    More,
    Fullscreen,
    Close,
}

impl Action {
    const ALL: [Self; 19] = [
        Self::Lt,
        Self::Rt,
        Self::A,
        Self::B,
        Self::X,
        Self::Y,
        Self::Menu,
        Self::View,
        Self::Lb,
        Self::Rb,
        Self::Ls,
        Self::Rs,
        Self::Up,
        Self::Down,
        Self::Left,
        Self::Right,
        Self::More,
        Self::Fullscreen,
        Self::Close,
    ];

    pub(super) fn in_mask(self, mask: u32) -> bool {
        mask & (1 << self as usize) != 0
    }

    fn command(self) -> bool {
        matches!(self, Self::More | Self::Fullscreen | Self::Close)
    }
}

struct Drag {
    contact: Contact,
    anchor: POINT,
    point: POINT,
    pixels: f32,
    axes: [f32; 2],
}

#[derive(Default)]
struct Touch {
    left: Option<Drag>,
    right: Option<Drag>,
    buttons: [Option<Contact>; Action::ALL.len()],
    taps: [Option<(Contact, Instant)>; Action::ALL.len()],
}

impl Touch {
    fn start(&mut self, contact: Contact, point: POINT, size: RECT, now: Instant) {
        let side = size.right.min(size.bottom) as f32;
        if side <= 0.0 {
            return;
        }
        if let Some(action) = touch_overlay::button_at(size, point) {
            let i = action as usize;
            if self.buttons[i].is_none() {
                self.buttons[i] = Some(contact);
                if !action.command() && !matches!(action, Action::Lt | Action::Rt) {
                    self.taps[i] = Some((contact, now + FACE_TAP));
                }
            }
            return;
        }
        if touch_overlay::menu_covers(size, point) {
            return;
        }
        let slot = if point.x < size.right / 2 {
            &mut self.left
        } else {
            &mut self.right
        };
        if slot.is_none() {
            *slot = Some(Drag {
                contact,
                anchor: point,
                point,
                pixels: stick_travel(size),
                axes: [0.0; 2],
            });
        }
    }

    fn move_to(&mut self, contact: Contact, point: POINT) {
        if let Some(drag) = [&mut self.left, &mut self.right]
            .into_iter()
            .flatten()
            .find(|drag| drag.contact == contact)
        {
            drag.axes = stick(drag.anchor, point, drag.pixels, 0.12);
            drag.point = point;
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
        for button in &mut self.buttons {
            if button.is_some_and(&matches) {
                *button = None;
                stopped = true;
            }
        }
        stopped
    }

    fn cancel(&mut self, matches: impl Fn(Contact) -> bool) {
        self.stop(&matches);
        for tap in &mut self.taps {
            if tap.is_some_and(|(contact, _)| matches(contact)) {
                *tap = None;
            }
        }
    }

    fn button_mask(
        &self,
        pressed: impl Fn(Option<Contact>, Option<(Contact, Instant)>) -> bool,
    ) -> u32 {
        self.buttons
            .iter()
            .zip(self.taps)
            .enumerate()
            .fold(0, |mask, (i, (&held, tap))| {
                mask | (u32::from(pressed(held, tap)) << i)
            })
    }

    fn sticks(&self) -> [f32; 4] {
        let [lx, ly] = self.left.as_ref().map_or([0.0; 2], |drag| drag.axes);
        let [rx, ry] = self.right.as_ref().map_or([0.0; 2], |drag| drag.axes);
        [lx, ly, rx, ry]
    }
}

fn stick_travel(size: RECT) -> f32 {
    (size.right.min(size.bottom) as f32 * 0.10).clamp(32.0, 160.0)
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
    buttons: [None; Action::ALL.len()],
    taps: [None; Action::ALL.len()],
});

pub(super) fn reading() -> ([f32; 4], u32) {
    let touch = TOUCH.lock().expect("touch state");
    let now = Instant::now();
    let buttons =
        touch.button_mask(|held, tap| held.is_some() || tap.is_some_and(|(_, until)| now < until));
    (touch.sticks(), buttons)
}

fn refresh_pads(hwnd: HWND) {
    let touch = TOUCH.lock().expect("touch state");
    let drags = [&touch.left, &touch.right].map(|slot| {
        slot.as_ref()
            .filter(|drag| drag.contact.window == hwnd as usize)
            .map(|drag| (drag.anchor, drag.point))
    });
    let buttons =
        touch.button_mask(|held, _| held.is_some_and(|contact| contact.window == hwnd as usize));
    drop(touch);
    touch_overlay::refresh(hwnd, drags, buttons);
}

fn release_pointer(hwnd: HWND, contact: Contact, mut point: POINT) {
    let command = {
        let mut touch = TOUCH.lock().expect("touch state");
        let command = Action::ALL
            .into_iter()
            .find(|&action| action.command() && touch.buttons[action as usize] == Some(contact));
        touch.stop(|active| active == contact);
        command
    };
    let Some(command) = command else { return };
    let mut size = RECT::default();
    // SAFETY: the window is processing this touch's UP message, and outputs are writable.
    if unsafe { ScreenToClient(hwnd, &mut point) == 0 || wm::GetClientRect(hwnd, &mut size) == 0 }
        || touch_overlay::button_at(size, point) != Some(command)
    {
        return;
    }
    match command {
        Action::More => touch_overlay::toggle_more(),
        Action::Fullscreen => {
            if let Err(error) = fullscreen(hwnd, false) {
                tracing::warn!(%error, "could not switch title fullscreen mode");
            }
        }
        Action::Close => {
            // Use the same graceful close path as Alt+F4.
            if unsafe { wm::PostMessageW(hwnd, wm::WM_SYSCOMMAND, wm::SC_CLOSE as usize, 0) } == 0 {
                tracing::warn!(error = %io::Error::last_os_error(), "could not close the title window");
            }
        }
        _ => {}
    }
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
        _ if canceled => {
            TOUCH
                .lock()
                .expect("touch state")
                .cancel(|active| active == contact);
        }
        wm::WM_POINTERUP => release_pointer(hwnd, contact, point),
        wm::WM_POINTERCAPTURECHANGED => {
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
                    touch.move_to(contact, point);
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
                .cancel(|contact| contact.window == hwnd as usize);
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
        touch.move_to(left, POINT { x: 700, y: 400 });
        touch.move_to(right, POINT { x: 500, y: 400 });
        touch.move_to(extra, POINT { x: 500, y: 300 });
        assert_eq!(touch.sticks(), [1.0, 0.0, -1.0, 0.0]);
        assert!(touch.stop(|contact| contact == left));
        assert_eq!(touch.sticks(), [0.0, 0.0, -1.0, 0.0]);
        assert!(touch.stop(|contact| contact.window == window));
        assert_eq!(touch.sticks(), [0.0; 4]);
    }

    #[test]
    fn throttle_is_independent_of_steering_and_releases_immediately() {
        let mut touch = Touch::default();
        let now = Instant::now();
        let left = Contact { window: 1, id: 1 };
        let rt = Contact { window: 1, id: 2 };
        let lt = Contact { window: 1, id: 3 };
        let size = RECT {
            right: 1600,
            bottom: 900,
            ..Default::default()
        };
        touch.start(left, POINT { x: 288, y: 657 }, size, now);
        touch.start(rt, POINT { x: 1024, y: 548 }, size, now);
        touch.move_to(left, POINT { x: 410, y: 657 });
        let mask = |state: &Touch| state.button_mask(|held, tap| held.is_some() || tap.is_some());
        assert!(Action::Rt.in_mask(mask(&touch)));
        assert_eq!(touch.sticks(), [1.0, 0.0, 0.0, 0.0]);
        assert!(touch.stop(|contact| contact == rt));
        touch.start(lt, POINT { x: 1024, y: 780 }, size, now);
        assert!(!Action::Rt.in_mask(mask(&touch)));
        assert!(Action::Lt.in_mask(mask(&touch)));
        touch.cancel(|contact| contact.window == 1);
        assert_eq!(mask(&touch), 0);
        assert_eq!(touch.sticks(), [0.0; 4]);
    }

    #[test]
    fn camera_stick_turns_at_a_held_offset_and_stops_at_center_or_release() {
        let mut touch = Touch::default();
        let contact = Contact { window: 1, id: 1 };
        let size = RECT {
            right: 1118,
            bottom: 540,
            ..Default::default()
        };
        let center = POINT { x: 916, y: 394 };
        touch.start(contact, center, size, Instant::now());
        for offset in 1..=40 {
            touch.move_to(
                contact,
                POINT {
                    x: center.x + offset,
                    y: center.y,
                },
            );
        }
        let held = touch.sticks();
        assert!(held[2] > 0.70 && held[2] < 0.71);
        assert_eq!(held[3], 0.0);
        touch.move_to(
            contact,
            POINT {
                x: center.x + 40,
                y: center.y,
            },
        );
        assert_eq!(touch.sticks(), held);
        touch.move_to(contact, center);
        assert_eq!(touch.sticks(), [0.0; 4]);
        touch.move_to(
            contact,
            POINT {
                x: center.x,
                y: center.y - 40,
            },
        );
        assert!(touch.sticks()[3] > 0.70);
        assert!(touch.stop(|active| active == contact));
        assert_eq!(touch.sticks(), [0.0; 4]);
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
