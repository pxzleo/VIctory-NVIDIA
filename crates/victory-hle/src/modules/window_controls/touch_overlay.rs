//! Two small desktop-composited controls. They never render into the swap chain.

use std::cell::{Cell, RefCell};
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

use super::Action;

type Drags = [Option<(POINT, POINT)>; 2];
pub(super) const REFRESH_TIMER: usize = 0x64a4;

thread_local! {
    static OVERLAY: RefCell<Option<(HWND, Vec<Pad>)>> = const { RefCell::new(None) };
    static MORE: Cell<bool> = const { Cell::new(false) };
}

pub(super) fn toggle_more() {
    MORE.set(!MORE.get());
}

const PRIMARY: [Action; 7] = [
    Action::Lt,
    Action::Rt,
    Action::A,
    Action::B,
    Action::Lb,
    Action::Rs,
    Action::More,
];
const MENU_ROWS: [[Option<Action>; 4]; 4] = [
    [
        Some(Action::X),
        Some(Action::Y),
        Some(Action::Menu),
        Some(Action::View),
    ],
    [None, Some(Action::Rb), Some(Action::Ls), None],
    [
        Some(Action::Left),
        Some(Action::Up),
        Some(Action::Down),
        Some(Action::Right),
    ],
    [None, Some(Action::Fullscreen), Some(Action::Close), None],
];

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

pub(super) fn refresh(owner: HWND, drags: Drags, pressed: u32) {
    if enabled() == Some(false) {
        return;
    }
    OVERLAY.with(|state| {
        let Ok(mut state) = state.try_borrow_mut() else {
            return;
        };
        if let Err(error) = update(&mut state, owner, drags, pressed) {
            static LOGGED: std::sync::Once = std::sync::Once::new();
            LOGGED.call_once(|| tracing::warn!(%error, "could not display the touch controls"));
        }
    });
}

fn update(
    state: &mut Option<(HWND, Vec<Pad>)>,
    owner: HWND,
    drags: Drags,
    pressed: u32,
) -> io::Result<()> {
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
    if !existing && enabled() != Some(true) && drags.iter().all(Option::is_none) && pressed == 0 {
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
        let pads = (0..3 + PRIMARY.len())
            .map(|_| Pad::new(owner))
            .collect::<io::Result<_>>()?;
        *state = Some((owner, pads));
        if enabled().is_none() {
            watch_foreground(owner);
        }
    }
    let (sticks, controls) = state.as_mut().unwrap().1.split_at_mut(2);
    let (menu, buttons) = controls.split_last_mut().unwrap();
    for (index, (pad, drag)) in sticks.iter_mut().zip(drags).enumerate() {
        let drag = drag.map(|(anchor, pointer)| {
            let to_client = |p: POINT| POINT {
                x: p.x - origin.x,
                y: p.y - origin.y,
            };
            (to_client(anchor), to_client(pointer))
        });
        pad.show(pose(rect, origin, index, drag))?;
    }
    for (pad, action) in buttons.iter_mut().zip(PRIMARY) {
        let mut pose = primary_pose(rect, action);
        pose.center.0 += origin.x;
        pose.center.1 += origin.y;
        pose.active = action.in_mask(pressed);
        pad.show(pose)?;
    }
    if MORE.get() {
        let primary_mask = PRIMARY
            .into_iter()
            .fold(0, |mask, action| mask | (1 << action as usize));
        let mut pose = menu_pose(rect, pressed & !primary_mask);
        pose.center.0 += origin.x;
        pose.center.1 += origin.y;
        menu.show(pose)?;
    } else {
        menu.hide();
    }
    Ok(())
}

pub(super) fn forget(hwnd: HWND) {
    OVERLAY.with(|state| {
        let mut state = state.borrow_mut();
        if state.as_ref().is_some_and(|(owner, _)| *owner == hwnd) {
            *state = None;
            MORE.set(false);
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
    button: Option<Action>,
    menu: Option<MenuArt>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct MenuArt {
    cell: i32,
    pressed: u32,
}

impl MenuArt {
    fn item(self, x: i32, y: i32) -> Option<(Action, i32, i32)> {
        let (col, row) = (x + 2 * self.cell, y + 2 * self.cell);
        if col < 0 || row < 0 {
            return None;
        }
        let action = MENU_ROWS
            .get((row / self.cell) as usize)?
            .get((col / self.cell) as usize)?
            .as_ref()?;
        Some((
            *action,
            col % self.cell - self.cell / 2,
            row % self.cell - self.cell / 2,
        ))
    }
}

fn original_button_radius(rect: RECT) -> i32 {
    (rect.right.min(rect.bottom) / 16).clamp(10, 64)
}

fn action_radius(rect: RECT) -> i32 {
    let previous = (original_button_radius(rect) * 6 + 2) / 5;
    (previous * 85 + 50) / 100
}

fn action_gap(rect: RECT) -> i32 {
    6 + (rect.right.min(rect.bottom) / 54).clamp(4, 16)
}

fn action_diagonal(reach: i32) -> i32 {
    (reach as f32 * std::f32::consts::FRAC_1_SQRT_2).ceil() as i32
}

fn pose(rect: RECT, origin: POINT, index: usize, drag: Option<(POINT, POINT)>) -> Pose {
    let previous = (rect.right.min(rect.bottom) / 17).clamp(32, 76) * 3 / 2;
    let button = action_radius(rect);
    let gap = action_gap(rect);
    let arc_room = rect.right.min(rect.bottom) - 2 * button - 5;
    let max_reach = (arc_room as f32 / (1.0 + std::f32::consts::FRAC_1_SQRT_2)) as i32;
    let radius = ((previous * 3 + 1) / 2)
        .min((rect.right - 4 * button - 16) / 3)
        .min((rect.right - 2 * button - gap - 8) / 4)
        .min(max_reach - button - gap);
    let inset = radius + 2;
    let mut fallback = POINT {
        x: rect.right * [18, 82][index] / 100,
        y: rect.bottom * 73 / 100,
    };
    if index == 1 {
        let reach = radius + button + gap;
        let far_edge = button + reach + 2;
        let bottom = rect.bottom - button - action_diagonal(reach) - 2;
        fallback.x = fallback
            .x
            .clamp(far_edge, rect.right - button - action_diagonal(reach) - 2);
        fallback.y = fallback.y.clamp(far_edge, bottom);
    }
    let (anchor, finger) = drag.unwrap_or((fallback, fallback));
    let feel = super::stick_travel(rect);
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
        button: None,
        menu: None,
    }
}

fn menu_pose(rect: RECT, pressed: u32) -> Pose {
    let cell = (rect.right.min(rect.bottom) / 7).clamp(18, 112);
    let radius = 2 * cell + 8;
    Pose {
        center: (rect.right / 2, radius + (rect.bottom / 16).clamp(8, 48)),
        thumb: (0, 0),
        radius,
        active: false,
        button: None,
        menu: Some(MenuArt { cell, pressed }),
    }
}

fn primary_pose(rect: RECT, action: Action) -> Pose {
    let (center, radius) = if action == Action::More {
        let radius = original_button_radius(rect);
        let center = (
            (rect.right * 92 / 100).clamp(radius, rect.right - radius),
            (rect.bottom * 15 / 100).clamp(radius, rect.bottom - radius),
        );
        (center, radius)
    } else {
        let stick = pose(
            rect,
            POINT::default(),
            usize::from(action != Action::Lb),
            None,
        );
        let radius = action_radius(rect);
        let reach = stick.radius + radius + action_gap(rect);
        let offset = action_diagonal(reach);
        let (x, y) = match action {
            Action::Lt => (-offset, offset),
            Action::Rt => (-reach, 0),
            Action::A => (-offset, -offset),
            Action::B => (0, -reach),
            Action::Lb => (0, -reach),
            Action::Rs => (offset, -offset),
            _ => unreachable!("secondary button has a menu position"),
        };
        ((stick.center.0 + x, stick.center.1 + y), radius)
    };
    Pose {
        center,
        thumb: (0, 0),
        radius,
        active: false,
        button: Some(action),
        menu: None,
    }
}

pub(super) fn menu_covers(rect: RECT, point: POINT) -> bool {
    if !MORE.get() || enabled() == Some(false) || rect.right.min(rect.bottom) < 112 {
        return false;
    }
    let pose = menu_pose(rect, 0);
    (point.x - pose.center.0).abs() <= pose.radius && (point.y - pose.center.1).abs() <= pose.radius
}

pub(super) fn button_at(rect: RECT, point: POINT) -> Option<Action> {
    if enabled() == Some(false) || rect.right.min(rect.bottom) < 112 {
        return None;
    }
    if menu_covers(rect, point) {
        let pose = menu_pose(rect, 0);
        return pose
            .menu
            .unwrap()
            .item(point.x - pose.center.0, point.y - pose.center.1)
            .map(|(action, _, _)| action);
    }
    PRIMARY
        .into_iter()
        .find(|&action| primary_pose(rect, action).contains(point))
}

pub(super) fn mouse_hits_control(hwnd: HWND, rect: RECT, point: POINT) -> bool {
    if rect.right.min(rect.bottom) < 112
        || enabled() == Some(false)
        || !OVERLAY.with(|state| {
            state.try_borrow().is_ok_and(|state| {
                state.as_ref().is_some_and(|(owner, pads)| {
                    *owner == hwnd && pads.iter().any(|pad| pad.shown.is_some())
                })
            })
        })
    {
        return false;
    }
    button_at(rect, point).is_some()
        || menu_covers(rect, point)
        || (0..2).any(|index| pose(rect, POINT::default(), index, None).contains(point))
}

impl Pose {
    fn contains(self, point: POINT) -> bool {
        let (dx, dy) = (point.x - self.center.0, point.y - self.center.1);
        dx.abs() <= self.radius
            && dy.abs() <= self.radius
            && dx * dx + dy * dy <= self.radius * self.radius
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

fn label(action: Action, radius: i32, x: i32, y: i32) -> bool {
    let text: &[u8] = match action {
        Action::Lt => b"LT",
        Action::Rt => b"RT",
        Action::A => b"A",
        Action::B => b"B",
        Action::X => b"X",
        Action::Y => b"Y",
        Action::Menu => b"MENU",
        Action::View => b"VIEW",
        Action::Lb => b"LB",
        Action::Rb => b"RB",
        Action::Ls => b"LS",
        Action::Rs => b"R",
        Action::Up => b"^",
        Action::Down => b"v",
        Action::Left => b"<",
        Action::Right => b">",
        Action::More if MORE.get() => b"-",
        Action::More => b"...",
        Action::Fullscreen => b"FS",
        Action::Close => b"OFF",
    };
    let width = 4 * text.len() as i32 - 1;
    let scale = (radius * 3 / (2 * width.max(10))).max(1);
    let x = x + width * scale / 2;
    let y = y + 5 * scale / 2;
    if x < 0 || !(0..5 * scale).contains(&y) {
        return false;
    }
    let column = (x / scale) % 4;
    if column == 3 {
        return false;
    }
    let Some(&letter) = text.get((x / scale / 4) as usize) else {
        return false;
    };
    let glyph = match letter {
        b'A' => [2, 5, 7, 5, 5],
        b'B' => [6, 5, 6, 5, 6],
        b'E' => [7, 4, 6, 4, 7],
        b'F' => [7, 4, 6, 4, 4],
        b'I' => [7, 2, 2, 2, 7],
        b'L' => [4, 4, 4, 4, 7],
        b'M' => [5, 7, 7, 5, 5],
        b'N' => [5, 7, 7, 7, 5],
        b'O' => [2, 5, 5, 5, 2],
        b'R' => [6, 5, 6, 5, 5],
        b'S' => [7, 4, 7, 1, 7],
        b'T' => [7, 2, 2, 2, 2],
        b'U' => [5, 5, 5, 5, 7],
        b'V' => [5, 5, 5, 5, 2],
        b'W' => [5, 5, 7, 7, 5],
        b'X' => [5, 5, 2, 5, 5],
        b'Y' => [5, 5, 2, 2, 2],
        b'^' => [2, 7, 2, 2, 2],
        b'v' => [2, 2, 2, 7, 2],
        b'<' => [1, 2, 7, 2, 1],
        b'>' => [4, 2, 7, 2, 4],
        b'.' => [0, 0, 0, 0, 2],
        b'-' => [0, 0, 7, 0, 0],
        _ => [7, 0, 7, 0, 7],
    };
    glyph[(y / scale) as usize] & (4 >> column) != 0
}

fn paint_pixel(pose: Pose, x: i32, y: i32) -> u32 {
    if let Some(menu) = pose.menu {
        let corner = 12;
        let edge = pose.radius - corner;
        let dx = (x.abs() - edge).max(0);
        let dy = (y.abs() - edge).max(0);
        let distance = ((dx * dx + dy * dy) as f32).sqrt();
        let outer = coverage(distance, corner as f32);
        let border = outer - coverage(distance, corner as f32 - 1.0);
        let pixel = (0.84 * outer * 255.0).round() as u32;
        let channel = |base: f32| (base * outer + 65.0 * border).round() as u32;
        let background = pixel << 24 | channel(14.0) << 16 | channel(19.0) << 8 | channel(27.0);
        let Some((action, dx, dy)) = menu.item(x, y) else {
            return background;
        };
        let button = paint_pixel(
            Pose {
                center: (0, 0),
                thumb: (0, 0),
                radius: menu.cell / 2 - 5,
                active: action.in_mask(menu.pressed),
                button: Some(action),
                menu: None,
            },
            dx,
            dy,
        );
        let rest = 255 - (button >> 24);
        return [0, 8, 16, 24].into_iter().fold(0, |pixel, shift| {
            let front = (button >> shift) & 255;
            let back = (background >> shift) & 255;
            pixel | ((front + (back * rest + 127) / 255) << shift)
        });
    }
    let radius = pose.radius as f32;
    let distance = ((x * x + y * y) as f32).sqrt();
    let outer = coverage(distance, radius);
    if outer == 0.0 {
        return 0;
    }
    let inner = coverage(distance, radius - 1.6);
    let thumb = if let Some(action) = pose.button {
        if label(action, pose.radius, x, y) {
            0.92
        } else {
            0.0
        }
    } else {
        let (dx, dy) = (x - pose.thumb.0, y - pose.thumb.1);
        let thumb_distance = ((dx * dx + dy * dy) as f32).sqrt();
        coverage(thumb_distance, radius * 0.28) * if pose.active { 0.76 } else { 0.38 }
    };
    let ring = (outer - inner) * if pose.active { 0.69 } else { 0.47 };
    let shade = inner
        * match (pose.button.is_some(), pose.active) {
            (true, true) => 0.48,
            (true, false) => 0.34,
            (false, true) => 0.24,
            (false, false) => 0.17,
        };
    let base = ring + shade;
    let alpha = thumb + base * (1.0 - thumb);
    let tint = if pose.button.is_some() && pose.active {
        [146.0, 232.0, 255.0]
    } else {
        [235.0, 248.0, 255.0]
    };
    let rgb = tint.map(|value| {
        (value * thumb + (value * ring + 18.0 * shade) * (1.0 - thumb)).round() as u32
    });
    ((alpha * 255.0).round() as u32) << 24 | rgb[0] << 16 | rgb[1] << 8 | rgb[2]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secondary_controls_share_one_menu_and_primary_buttons_get_larger() {
        let rect = RECT {
            right: 1118,
            bottom: 540,
            ..Default::default()
        };
        let pose = menu_pose(rect, 0);
        let cell = pose.menu.unwrap().cell;
        let x = POINT {
            x: pose.center.0 - 3 * cell / 2,
            y: pose.center.1 - 3 * cell / 2,
        };
        let blank = POINT {
            x: x.x,
            y: pose.center.1 + 3 * cell / 2,
        };
        assert!(!menu_covers(rect, x));
        assert!(button_at(rect, x).is_none());
        toggle_more();
        assert!(button_at(rect, x) == Some(Action::X));
        assert!(menu_covers(rect, blank) && button_at(rect, blank).is_none());
        toggle_more();
        assert_eq!(primary_pose(rect, Action::Rt).radius, 34);
        assert_eq!(action_gap(rect), 16);
        assert!((175..255).contains(&(paint_pixel(pose, 0, 0) >> 24)));
    }

    #[test]
    fn centers_stay_in_the_client_and_active_thumb_tracks_the_finger() {
        let rect = RECT {
            right: 1280,
            bottom: 720,
            ..Default::default()
        };
        let origin = POINT { x: -1280, y: 0 };
        let idle = pose(rect, origin, 1, None);
        assert_eq!(idle.radius, 95);
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

    #[test]
    fn larger_idle_sticks_fit_in_landscape_and_portrait() {
        for (width, height, radius) in [
            (1118, 540, 72),
            (1646, 926, 122),
            (540, 1118, 72),
            (390, 844, 72),
            (540, 540, 72),
        ] {
            let rect = RECT {
                right: width,
                bottom: height,
                ..Default::default()
            };
            let stick = pose(rect, POINT::default(), 1, None);
            assert_eq!(stick.radius, radius);
            for (i, &action) in PRIMARY.iter().enumerate() {
                let pad = primary_pose(rect, action);
                let old = (width.min(height) / 16).clamp(10, 64);
                let previous = (old * 6 + 2) / 5;
                assert_eq!(
                    pad.radius,
                    if action == Action::More {
                        old
                    } else {
                        (previous * 85 + 50) / 100
                    }
                );
                assert!((pad.radius..=width - pad.radius).contains(&pad.center.0));
                assert!((pad.radius..=height - pad.radius).contains(&pad.center.1));
                let (dx, dy) = (pad.center.0 - stick.center.0, pad.center.1 - stick.center.1);
                assert!(dx * dx + dy * dy >= (stick.radius + pad.radius).pow(2));
                if action != Action::More && action != Action::Lb {
                    assert!(
                        dx * dx + dy * dy >= (stick.radius + pad.radius + action_gap(rect)).pow(2)
                    );
                    let direction = match action {
                        Action::Lt => (-1, 1),
                        Action::Rt => (-1, 0),
                        Action::A => (-1, -1),
                        Action::B => (0, -1),
                        Action::Rs => (1, -1),
                        _ => unreachable!(),
                    };
                    assert_eq!((dx.signum(), dy.signum()), direction);
                    assert!(
                        button_at(
                            rect,
                            POINT {
                                x: pad.center.0,
                                y: pad.center.1
                            }
                        ) == Some(action)
                    );
                }
                if action == Action::Lb {
                    let left = pose(rect, POINT::default(), 0, None);
                    assert_eq!(pad.center.0, left.center.0);
                    assert!(left.center.1 - pad.center.1 >= left.radius + pad.radius);
                    assert!(
                        button_at(
                            rect,
                            POINT {
                                x: pad.center.0,
                                y: pad.center.1
                            }
                        ) == Some(Action::Lb)
                    );
                }
                for &other in &PRIMARY[..i] {
                    let other = primary_pose(rect, other);
                    let (dx, dy) = (pad.center.0 - other.center.0, pad.center.1 - other.center.1);
                    assert!(dx * dx + dy * dy >= (pad.radius + other.radius).pow(2));
                }
            }
        }

        let rect = RECT {
            right: 112,
            bottom: 112,
            ..Default::default()
        };
        let left = pose(rect, POINT::default(), 0, None);
        let right = pose(rect, POINT::default(), 1, None);
        assert!(right.center.0 - left.center.0 >= left.radius + right.radius);
    }
}
