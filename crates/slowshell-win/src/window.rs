//! Native window creation and the message loop.
//!
//! The window styles here are the crux of what makes a shell feel like a shell
//! rather than an app floating on the desktop:
//!
//! * `WS_EX_TOOLWINDOW` keeps the panel out of the Alt+Tab list and the taskbar.
//! * `WS_EX_NOACTIVATE` means clicking a widget does not steal focus from the
//!   user's actual work, which is the single most important property of a bar.
//! * `WS_EX_LAYERED` is deliberately *not* set. A flip-model swap chain with a
//!   premultiplied alpha mode is composited by DWM directly, which is both faster
//!   and smoother than `UpdateLayeredWindow`.
//! * A null class background brush avoids the flicker of a background erase.

use std::cell::RefCell;
use std::rc::Rc;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WAIT_TIMEOUT, WPARAM};
use windows::Win32::Graphics::Gdi::{BeginPaint, EndPaint, HBRUSH, PAINTSTRUCT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, VK_LBUTTON};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::backdrop::Backdrop;
use crate::input::{InputEvent, KeyModifiers};

/// The class every shell window is registered under.
pub const CLASS_NAME: PCWSTR = w!("SlowshellWindow");

/// What a surface should ask of the window manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceRole {
    /// A bar or panel anchored to a screen edge.
    Panel,
    /// A transient window such as a launcher or menu.
    Popup,
    /// A full-screen overlay.
    Overlay,
}

impl SurfaceRole {
    fn ex_style(self) -> WINDOW_EX_STYLE {
        let base = WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
        match self {
            // A panel must never take focus from the window the user is working in.
            SurfaceRole::Panel => base,
            // A popup needs focus so it can accept keyboard input.
            SurfaceRole::Popup => base | WS_EX_TOPMOST,
            SurfaceRole::Overlay => base | WS_EX_TOPMOST,
        }
    }

    fn style(self) -> WINDOW_STYLE {
        match self {
            // `WS_EX_NOACTIVATE` on a frameless popup stops it activating on show,
            // so the first keystroke is not swallowed.
            SurfaceRole::Panel => WS_POPUP,
            SurfaceRole::Popup => WS_POPUP,
            SurfaceRole::Overlay => WS_POPUP | WS_CLIPCHILDREN | WS_CLIPSIBLINGS,
        }
    }
}

/// Events a window forwards to the shell.
#[derive(Debug)]
pub enum WindowEvent {
    /// A frame was requested: the reactive graph changed or an animation is live.
    Tick,
    /// The window was resized. `scale` is the monitor's DPI scale.
    Resized { width: u32, height: u32, scale: f32 },
    /// The DPI of the monitor this window is on changed.
    DpiChanged { scale: f32 },
    /// A pointer event in *logical* (DPI-scaled) coordinates.
    Pointer(InputEvent),
    /// The window was asked to close.
    CloseRequested,
    /// The window was shown or hidden.
    Visibility(bool),
    /// A hotkey fired that was registered against this window.
    Hotkey(u32),
    /// Focus entered the window.
    Focused(bool),
}

impl WindowEvent {
    pub fn render(&self) -> String {
        match self {
            WindowEvent::Tick => "Tick".into(),
            WindowEvent::Resized { width, height, scale } => {
                format!("Resized {width}x{height} @{scale}")
            }
            WindowEvent::DpiChanged { scale } => format!("DpiChanged {scale}"),
            WindowEvent::Pointer(p) => format!("Pointer {p:?}"),
            WindowEvent::CloseRequested => "CloseRequested".into(),
            WindowEvent::Visibility(v) => format!("Visibility {v}"),
            WindowEvent::Hotkey(id) => format!("Hotkey {id}"),
            WindowEvent::Focused(v) => format!("Focused {v}"),
        }
    }
}

/// State owned by a window, reachable from the window procedure.
pub struct WindowState {
    pub role: SurfaceRole,
    /// Where the compositor should place the surface.
    pub backdrop: Backdrop,
    /// Clicks outside any interactive widget pass through to the desktop.
    /// Take pointer events even where nothing is clickable, so a panel that
    /// opens on hover can be found when it is collapsed and empty.
    pub hover_capture: bool,
    pub click_through: bool,
    /// Logical size, updated on resize.
    pub size: (u32, u32),
    pub scale: f32,
    /// Hotkey ids registered against this window, mapped to shell hotkey ids.
    pub hotkeys: Vec<(u32, u32)>,
    pub pointer_pos: (f32, f32),
    pub pointer_inside: bool,
    /// Whether a `WM_MOUSELEAVE` request is currently outstanding.
    mouse_tracked: bool,
    buttons: KeyModifiers,
    /// Set by the shell to request a repaint.
    pub dirty: bool,
    /// Set by the shell to close the window on the next loop turn.
    pub close: bool,
    /// The app bar this window registers as, if it reserves screen space.
    ///
    /// Held here rather than in the shell so it lives exactly as long as the
    /// window: `AppBar::drop` sends `ABM_REMOVE`, so a surface destroyed on a
    /// config reload gives its space back. A shell that crashed with the space
    /// reserved would leave a permanent hole in the desktop.
    pub appbar: Option<crate::appbar::AppBar>,
}

impl WindowState {
    fn new(role: SurfaceRole) -> WindowState {
        WindowState {
            role,
            backdrop: Backdrop::None,
            click_through: false,
            hover_capture: false,
            size: (0, 0),
            scale: 1.0,
            hotkeys: Vec::new(),
            pointer_pos: (0.0, 0.0),
            pointer_inside: false,
            mouse_tracked: false,
            buttons: KeyModifiers::NONE,
            dirty: true,
            close: false,
            appbar: None,
        }
    }

    /// A default state, for tests that only need the hit-test fields.
    pub fn for_tests() -> WindowState {
        WindowState::new(SurfaceRole::Panel)
    }
}

thread_local! {
    /// Events queued by the window procedure, drained once per loop turn.
    static PENDING: RefCell<Vec<(isize, WindowEvent)>> = const { RefCell::new(Vec::new()) };
}

fn queue(hwnd: isize, ev: WindowEvent) {
    PENDING.with(|p| p.borrow_mut().push((hwnd, ev)));
}

/// Take everything queued since the last call.
pub fn take_events() -> Vec<(isize, WindowEvent)> {
    PENDING.with(|p| std::mem::take(&mut *p.borrow_mut()))
}

/// Throw away every queued event.
///
/// Diagnostics that create and destroy a throwaway window leave that window's
/// `WM_SIZE` and `WM_DESTROY` behind, and the shell reads the queue on its first
/// frame. Without this a probe would close the very shell that ran it, so a
/// diagnostic has to leave no trace.
pub fn discard_events() {
    let _ = take_events();
}

/// Returns a raw pointer for `GWLP_USERDATA`. `HWND` is a thin wrapper over a
/// pointer, so this is free, but the indirection keeps the cast in one place.
fn userdata(hwnd: HWND) -> *const WindowState {
    unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WindowState }
}

/// A live window handle.
#[derive(Clone, Copy)]
pub struct SurfaceHandle {
    pub hwnd: HWND,
}

impl SurfaceHandle {
    pub fn id(&self) -> isize {
        self.hwnd.0 as isize
    }

    pub fn is_valid(&self) -> bool {
        unsafe { IsWindow(Some(self.hwnd)).as_bool() }
    }
}

/// Register the window class. Safe to call more than once.
pub fn register_class() -> std::io::Result<()> {
    unsafe {
        let instance: HINSTANCE = GetModuleHandleW(None)?.into();
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: HICON::default(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            // A null background brush: every pixel is painted by Direct2D, and an
            // erase would only produce flicker.
            hbrBackground: HBRUSH::default(),
            lpszMenuName: PCWSTR::null(),
            lpszClassName: CLASS_NAME,
            hIconSm: HICON::default(),
        };
        // A second call fails with ERROR_CLASS_ALREADY_EXISTS, which is success
        // for our purposes: the class is registered and identical.
        match RegisterClassExW(&wc) {
            0 => {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(1410) {
                    Ok(())
                } else {
                    Err(err)
                }
            }
            _ => Ok(()),
        }
    }
}

/// Create a shell window.
///
/// `x`/`y`/`width`/`height` are physical pixels. Windows positioned with
/// `SetWindowPos` are not DPI virtualised once the process is per-monitor aware,
/// which is why the caller passes physical values throughout.
pub fn create_surface(
    role: SurfaceRole,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    title: &str,
) -> std::io::Result<SurfaceHandle> {
    register_class()?;
    unsafe {
        let state = Box::new(WindowState::new(role));
        let boxed = Box::into_raw(state);
        let instance: HINSTANCE = GetModuleHandleW(None)?.into();

        let hwnd = CreateWindowExW(
            role.ex_style(),
            CLASS_NAME,
            w!("{}"),
            role.style(),
            x,
            y,
            width as i32,
            height as i32,
            None,
            None,
            Some(instance),
            Some(boxed as *const _),
        )
        .map_err(|_| std::io::Error::last_os_error())?;

        let handle = SurfaceHandle { hwnd };
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, boxed as isize);
        let _ = title;
        Ok(handle)
    }
}

/// Destroy a window and release its state.
///
/// The user data is cleared *before* `DestroyWindow`, which is what tells the
/// window procedure this teardown was the shell's own doing. Without that
/// ordering, `WM_DESTROY` looks exactly like the user closing the window, and
/// the shell quits the first time a config reload removes a panel — which is the
/// single most ordinary thing a shell does.
pub fn destroy(handle: SurfaceHandle) {
    unsafe {
        let ptr = userdata(handle.hwnd);
        if !ptr.is_null() {
            // The box was created in `create_surface` and is freed exactly once.
            drop(Box::from_raw(ptr as *mut WindowState));
            SetWindowLongPtrW(handle.hwnd, GWLP_USERDATA, 0);
        }
        let _ = DestroyWindow(handle.hwnd);
    }
}

/// Register this window as an app bar, so it reserves screen space.
///
/// Returns whether a registration happened. `Edge::None` — a floating panel —
/// reserves nothing and reports `false`, which is correct rather than a
/// failure.
///
/// The window takes ownership, so the space is released by [`destroy`]. A shell
/// that reserved space it did not own would leave a permanent hole in the
/// desktop when it died.
pub fn register_appbar(hwnd: HWND, edge: crate::appbar::Edge) -> bool {
    let Some(bar) = crate::appbar::AppBar::register(hwnd, edge) else {
        return false;
    };
    with_state(hwnd, |s| {
        // Replacing releases any previous registration, so a config reload that
        // toggles `exclusive` does not leak a reservation.
        s.appbar = Some(bar);
    })
    .is_some()
}

/// Reserve the window's current rectangle.
///
/// Called after a move or a resize, and idempotent: the same rectangle twice
/// costs nothing and broadcasts no work-area change.
pub fn reserve_appbar(hwnd: HWND) {
    with_state(hwnd, |s| {
        if let Some(bar) = &s.appbar {
            let mut r = RECT::default();
            unsafe {
                let _ = GetWindowRect(hwnd, &mut r);
            }
            bar.set_bounds((r.left, r.top, r.right, r.bottom));
        }
    });
}

/// Reserve the window's current rectangle, addressed by the id the shell holds.
///
/// The shell stores window ids as `isize` in its event queue, so this saves it
/// from reconstructing an `HWND` for the sake of a call it makes on every tick.
pub fn reserve_appbar_by_id(id: isize) {
    reserve_appbar(HWND(id as *mut std::ffi::c_void));
}

/// Stop reserving screen space for this window.
pub fn release_appbar(hwnd: HWND) {
    with_state(hwnd, |s| {
        s.appbar = None;
    });
}

/// This window's app bar, if it reserves screen space.
///
/// The exact rectangle it reserved, which is more reliable than enumerating the
/// OS's bar list — Windows does not always disclose other processes' bars.
pub fn appbar_rect(hwnd: HWND) -> Option<crate::appbar::Edge> {
    with_state(hwnd, |s| s.appbar.as_ref().and_then(|b| {
        b.reserved_rect()?;
        Some(b.edge())
    }))
    .flatten()
}

/// The rectangle this window's app bar reserved, if any.
pub fn appbar_bounds(hwnd: HWND) -> Option<(i32, i32, i32, i32)> {
    with_state(hwnd, |s| s.appbar.as_ref().and_then(|b| b.reserved_rect())).flatten()
}

/// The rectangle this window's app bar reserved, addressed by id.
pub fn appbar_bounds_by_id(id: isize) -> Option<(i32, i32, i32, i32)> {
    appbar_bounds(HWND(id as *mut std::ffi::c_void))
}

/// This window's style bits, for diagnostics.
pub fn window_style(hwnd: HWND) -> u32 {
    unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) as u32 }
}

/// This window's extended style bits, for diagnostics.
pub fn window_ex_style(hwnd: HWND) -> u32 {
    unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 }
}

/// This window's role, or `None` if it is being destroyed.
///
/// Exposed because it is the difference between "the bar did not appear" and
/// "the *popup* did not appear", which are different bugs with different fixes
/// and no other way to tell apart from outside.
pub fn role_of(hwnd: HWND) -> Option<SurfaceRole> {
    with_state(hwnd, |s| s.role)
}

/// Whether the OS has been asked to auto-hide this window's app bar.
///
/// Not a failure, and not auto-hide: nothing slides the window, because only the
/// owner of a window can. It is reported so `shellctl screens` can say that the
/// user asked for auto-hide and the shell has not implemented it, rather than
/// leaving them to wonder why their bar does not slide away.
pub fn appbar_auto_hidden(hwnd: HWND) -> bool {
    with_state(hwnd, |s| s.appbar.as_ref().is_some_and(|b| b.auto_hidden()))
        .unwrap_or(false)
}

/// Read a window's state, or `None` if it is being destroyed.
pub fn with_state<R>(hwnd: HWND, f: impl FnOnce(&mut WindowState) -> R) -> Option<R> {
    let ptr = userdata(hwnd);
    if ptr.is_null() {
        return None;
    }
    // SAFETY: the pointer is set at creation and cleared at destruction, and the
    // window procedure and the shell both run on the UI thread.
    Some(f(unsafe { &mut *(ptr as *mut WindowState) }))
}

/// Make this window take pointer events even where nothing is clickable.
///
/// Needed by a panel that opens on hover: while collapsed it is a thin strip with
/// its content squeezed out, so it has no hit regions, so `hit_test` says no, so
/// `WM_NCHITTEST` reports it transparent and the pointer passes straight through to
/// the desktop. The panel then can never be hovered open, and nothing says why.
///
/// This is the narrow version of that problem. It does not make the window
/// interactive — clicks are still routed by region and still fall through empty
/// areas — it only means the window is *told* where the pointer is.
pub fn set_hover_capture(hwnd: HWND, on: bool) {
    with_state(hwnd, |s| s.hover_capture = on);
}

pub fn set_click_through(hwnd: HWND, on: bool) {
    with_state(hwnd, |s| s.click_through = on);
    unsafe {
        let mut ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        if on {
            // `WS_EX_TRANSPARENT` makes the whole window click-through, which is
            // what a hover-revealed auto-hidden panel needs.
            ex |= WS_EX_TRANSPARENT.0 as isize;
        } else {
            ex &= !(WS_EX_TRANSPARENT.0 as isize);
        }
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex);
    }
}

pub fn set_always_on_top(hwnd: HWND, on: bool) {
    unsafe {
        let mut ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        if on {
            ex |= WS_EX_TOPMOST.0 as isize;
        } else {
            ex &= !(WS_EX_TOPMOST.0 as isize);
        }
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex);
    }
}

/// Move and resize in physical pixels without activating the window.
pub fn set_bounds(hwnd: HWND, x: i32, y: i32, width: u32, height: u32) {
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            None,
            x,
            y,
            width as i32,
            height as i32,
            SWP_NOACTIVATE | SWP_NOZORDER,
        );
    }
}

pub fn show(hwnd: HWND) {
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
}

pub fn hide(hwnd: HWND) {
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
}

pub fn is_visible(hwnd: HWND) -> bool {
    unsafe { IsWindowVisible(hwnd).as_bool() }
}

/// Force a redraw on the next loop turn.
pub fn request_frame(hwnd: HWND) {
    with_state(hwnd, |s| s.dirty = true);
}

pub fn scale_of(hwnd: HWND) -> f32 {
    with_state(hwnd, |s| s.scale).unwrap_or(1.0)
}

/// Ask the compositor to draw a frame soon. Used to coalesce a burst of
/// reactive updates into a single present.
pub fn post_frame(hwnd: HWND) {
    unsafe {
        let _ = PostMessageW(Some(hwnd), WM_SLOWSHELL_TICK, WPARAM(0), LPARAM(0));
    }
}

/// A message the shell posts to itself to request an animation frame.
pub const WM_SLOWSHELL_TICK: u32 = WM_APP + 1;

/// A `shellctl` request arrived and the frame loop should answer it.
///
/// Posted by the IPC thread, because the frame loop blocks in the kernel until
/// a message appears — so without a message, a `shellctl` on a config with no
/// `Clock` in it would wait for ever.
pub const WM_SLOWSHELL_WAKE: u32 = WM_APP + 2;

/// `WM_MOUSELEAVE`, which the `windows` crate does not surface. Tracking it is
/// what lets a widget drop its hover state when the pointer leaves the window.
pub const WM_MOUSELEAVE: u32 = 0x02A3;

/// Ask Windows to send [`WM_MOUSELEAVE`] the next time the pointer leaves.
pub fn track_mouse_leave(hwnd: HWND) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        TrackMouseEvent, TRACKMOUSEEVENT, TRACKMOUSEEVENT_FLAGS,
    };
    unsafe {
        let mut e = TRACKMOUSEEVENT {
            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TRACKMOUSEEVENT_FLAGS(0x0000_0002), // TME_LEAVE
            dwHoverTime: 0,
            hwndTrack: hwnd,
        };
        let _ = TrackMouseEvent(&mut e);
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_NCCREATE => {
            // The creation payload is our `Box<WindowState>`.
            let create = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
            let state_ptr = create.lpCreateParams as *mut WindowState;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize);
            LRESULT(1) // TRUE: proceed with creation.
        }
        WM_NCDESTROY => {
            let _ = DefWindowProcW(hwnd, msg, wparam, lparam);
            LRESULT(0)
        }
        WM_SIZE => {
            let width = (lparam.0 & 0xffff) as u32;
            let height = ((lparam.0 >> 16) & 0xffff) as u32;
            let scale = scale_of(hwnd);
            with_state(hwnd, |s| {
                s.size = (width, height);
                s.dirty = true;
            });
            queue(hwnd.0 as isize, WindowEvent::Resized { width, height, scale });
            LRESULT(0)
        }
        WM_DPICHANGED => {
            // wparam packs x and y DPI in the two 16-bit halves.
            let dpi = (wparam.0 & 0xffff) as u32;
            let scale = dpi as f32 / 96.0;
            with_state(hwnd, |s| {
                s.scale = scale;
                s.dirty = true;
            });
            queue(hwnd.0 as isize, WindowEvent::DpiChanged { scale });
            LRESULT(0)
        }
        WM_DPICHANGED_BEFOREPARENT | WM_DPICHANGED_AFTERPARENT => LRESULT(0),
        WM_PAINT => {
            // Direct2D presents explicitly; there is nothing to do in WM_PAINT.
            // Validating avoids a permanent "not painted" state that would make
            // Windows spin the message queue.
            unsafe {
                let mut ps = PAINTSTRUCT::default();
                let _ = BeginPaint(hwnd, &mut ps);
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }        WM_ERASEBKGND => LRESULT(1), // Fully handled by the renderer.
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_NCHITTEST => {
            let x = (lparam.0 & 0xffff) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xffff) as i16 as i32;
            if with_state(hwnd, |s| s.click_through).unwrap_or(false) {
                return LRESULT(HTTRANSPARENT as isize);
            }
            // A window that wants to know where the pointer is, even over nothing
            // clickable. Set on a panel that opens on hover.
            //
            // Without it, a collapsed reveal panel is 8px wide with its content
            // squeezed out, so it has no hit regions, so `hit_test` says no, so
            // the window reports itself transparent and the pointer passes through
            // to the desktop — and the panel can never be hovered open. The
            // feature fails completely and silently, which is why it is worth a
            // flag rather than a workaround in the hit test.
            if with_state(hwnd, |s| s.hover_capture).unwrap_or(false) {
                return LRESULT(HTCLIENT as isize);
            }
            // The shell decides per pixel whether the point is interactive. A
            // default of "transparent" means empty regions of a bar fall through
            // to the desktop without the user configuring anything.
            let interactive = with_state(hwnd, |s| crate::hit_test::hit_test(s, x as f32, y as f32))
                .unwrap_or(false);
            LRESULT(if interactive { HTCLIENT as isize } else { HTTRANSPARENT as isize })
        }
        WM_MOUSEMOVE => {
            let x = (lparam.0 & 0xffff) as i16 as f32;
            let y = ((lparam.0 >> 16) & 0xffff) as i16 as f32;
            // Windows only sends WM_MOUSELEAVE after being asked, and the request
            // is one-shot per entry, so it is re-armed on every leave.
            let needs_tracking =
                with_state(hwnd, |s| {
                    s.pointer_pos = (x, y);
                    s.pointer_inside = true;
                    std::mem::replace(&mut s.mouse_tracked, true)
                })
                .unwrap_or(false);
            if needs_tracking {
                track_mouse_leave(hwnd);
            }
            queue(
                hwnd.0 as isize,
                WindowEvent::Pointer(InputEvent::Moved { x, y, modifiers: current_modifiers() }),
            );
            LRESULT(0)
        }
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN => {
            let button = match msg {
                WM_LBUTTONDOWN => 0,
                WM_RBUTTONDOWN => 2,
                _ => 1,
            };
            with_state(hwnd, |s| s.buttons = current_modifiers());
            unsafe {
                SetCapture(hwnd);
            }
            let x = (lparam.0 & 0xffff) as i16 as f32;
            let y = ((lparam.0 >> 16) & 0xffff) as i16 as f32;
            queue(
                hwnd.0 as isize,
                WindowEvent::Pointer(InputEvent::Pressed {
                    x,
                    y,
                    button,
                    modifiers: current_modifiers(),
                }),
            );
            LRESULT(0)
        }
        WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP => {
            let button = match msg {
                WM_LBUTTONUP => 0,
                WM_RBUTTONUP => 2,
                _ => 1,
            };
            let _ = unsafe { ReleaseCapture() };
            let x = (lparam.0 & 0xffff) as i16 as f32;
            let y = ((lparam.0 >> 16) & 0xffff) as i16 as f32;
            queue(
                hwnd.0 as isize,
                WindowEvent::Pointer(InputEvent::Released {
                    x,
                    y,
                    button,
                    modifiers: current_modifiers(),
                }),
            );
            with_state(hwnd, |s| s.buttons = KeyModifiers::NONE);
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = ((wparam.0 >> 16) & 0xffff) as i16;
            queue(
                hwnd.0 as isize,
                WindowEvent::Pointer(InputEvent::Wheel {
                    delta: delta as f32 / 120.0,
                    modifiers: current_modifiers(),
                }),
            );
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            with_state(hwnd, |s| {
                s.pointer_inside = false;
                // The one-shot request is spent; the next move re-arms it.
                s.mouse_tracked = false;
            });
            queue(hwnd.0 as isize, WindowEvent::Pointer(InputEvent::Left));
            LRESULT(0)
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            queue(
                hwnd.0 as isize,
                WindowEvent::Pointer(InputEvent::Key {
                    key: crate::input::key_from_vk(wparam.0 as u16),
                    pressed: true,
                    repeat: lparam.0 & 0x1fff_0000 != 0,
                    modifiers: current_modifiers(),
                }),
            );
            LRESULT(0)
        }
        WM_KEYUP | WM_SYSKEYUP => {
            queue(
                hwnd.0 as isize,
                WindowEvent::Pointer(InputEvent::Key {
                    key: crate::input::key_from_vk(wparam.0 as u16),
                    pressed: false,
                    repeat: false,
                    modifiers: current_modifiers(),
                }),
            );
            LRESULT(0)
        }
        WM_CHAR => LRESULT(0), // Let the shell handle text input itself.
        WM_SETFOCUS => {
            queue(hwnd.0 as isize, WindowEvent::Focused(true));
            LRESULT(0)
        }
        WM_KILLFOCUS => {
            queue(hwnd.0 as isize, WindowEvent::Focused(false));
            LRESULT(0)
        }
        WM_CLOSE | WM_QUERYENDSESSION => {
            queue(hwnd.0 as isize, WindowEvent::CloseRequested);
            LRESULT(0)
        }
        WM_DISPLAYCHANGE | WM_DEVICECHANGE => {
            // A resolution or GPU change invalidates monitor geometry, and with
            // it the space this bar reserved. The shell re-reads the monitors
            // and calls `set_bounds` again, which re-reserves against whatever
            // the new layout is.
            queue(hwnd.0 as isize, WindowEvent::Tick);
            LRESULT(0)
        }
        // A work-area or taskbar change, most often because the user toggled the
        // real taskbar. Our reserved space has to be re-negotiated, or the bar
        // and the taskbar end up overlapping.
        WM_SETTINGCHANGE => {
            with_state(hwnd, |s| s.dirty = true);
            queue(hwnd.0 as isize, WindowEvent::Tick);
            LRESULT(0)
        }
        WM_DESTROY => {
            // No user data means `window::destroy` is already unwinding this
            // window, so this is not the user closing it. Reporting
            // `CloseRequested` here would end the shell on the first config
            // reload that drops a panel.
            if userdata(hwnd).is_null() {
                LRESULT(0)
            } else {
                queue(hwnd.0 as isize, WindowEvent::CloseRequested);
                LRESULT(0)
            }
        }
        WM_HOTKEY => {
            queue(hwnd.0 as isize, WindowEvent::Hotkey(wparam.0 as u32));
            LRESULT(0)
        }
        WM_SLOWSHELL_TICK => {
            with_state(hwnd, |s| s.dirty = true);
            queue(hwnd.0 as isize, WindowEvent::Tick);
            LRESULT(0)
        }
        // A wake-up only needs to interrupt the wait; the request itself is
        // picked up from the channel by the frame loop, not from the message.
        // Deliberately not a `Tick`: there may be nothing to redraw.
        WM_SLOWSHELL_WAKE => LRESULT(0),
        _ => {
            // The app bar callback id is assigned at registration and is not a
            // constant, so it has to be matched dynamically.
            //
            // It is only ever compared once validated: a real one is a registered
            // message well above `WM_USER`, and anything lower would collide with
            // a standard message. `ABM_NEW` can return a small value — it returns
            // 1 on some systems, which is `WM_CREATE` — and matching on that
            // makes the procedure swallow every window's `WM_CREATE`.
            let callback = crate::appbar::callback_message();
            if callback > WM_USER && msg == callback {
                let handled = with_state(hwnd, |s| {
                    s.appbar.as_ref().is_some_and(|bar| bar.handle(wparam))
                })
                .unwrap_or(false);
                if handled {
                    return LRESULT(0);
                }
                // Ours, but not a message we act on. Acknowledged so the OS does
                // not treat the bar as unresponsive.
                return LRESULT(0);
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
    }
}

fn current_modifiers() -> KeyModifiers {
    use crate::input::vk;
    let mut m = KeyModifiers::NONE;
    if crate::input::key_is_down(vk::SHIFT) {
        m |= KeyModifiers::SHIFT;
    }
    if crate::input::key_is_down(vk::CONTROL) {
        m |= KeyModifiers::CTRL;
    }
    if crate::input::key_is_down(vk::ALT) {
        m |= KeyModifiers::ALT;
    }
    if crate::input::key_is_down(vk::LWIN) || crate::input::key_is_down(vk::RWIN) {
        m |= KeyModifiers::SUPER;
    }
    m
}

/// The message pump.
///
/// Returns when `should_quit` says so or when the last window closes, which keeps
/// the loop simple: a shell with no windows left has nothing to do.
pub fn run(should_quit: impl Fn() -> bool) {
    unsafe {
        let mut msg = MSG::default();
        while !should_quit() {
            let r = GetMessageW(&mut msg, None, 0, 0);
            if r.0 <= 0 {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Drain every pending Win32 message without blocking, then run `on_event` for
/// each window event. This is the shell's per-frame tick.
/// Drain queued events, without blocking.
///
/// For the *start* of a frame: the caller wants whatever is already waiting and
/// nothing more. Use [`wait`] when the shell has nothing else to do.
pub fn pump<F: FnMut(isize, WindowEvent)>(mut on_event: F) {
    unsafe {
        let mut msg = MSG::default();
        // `PM_REMOVE` with a zero window handle drains every message; a bounded
        // loop keeps a message storm from starving rendering.
        for _ in 0..512 {
            if !PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    for (hwnd, ev) in take_events() {
        on_event(hwnd, ev);
    }
}

/// Block until there is a message, or `timeout_ms` elapses. Then drain.
///
/// # Why this exists
///
/// The frame loop used to `sleep` between turns, and the sleep had a 50 ms
/// ceiling so that a `shellctl` request would be answered within a frame or two.
/// That ceiling is what the idle CPU was spent on: twenty wake-ups a second,
/// each one a message pump, a few `stat` calls and a handful of reactor reads,
/// on a desktop where nothing is happening.
///
/// `MsgWaitForMultipleObjectsEx` blocks *in the kernel* until a message actually
/// arrives. So the timeout can be as long as the shell genuinely has nothing to
/// do — up to the next clock tick — and a posted message still wakes it at once.
/// Idle cost becomes zero rather than small, and `shellctl` stays instant
/// because the IPC thread posts a message when a request lands.
///
/// Returns whether it waited, so a caller can tell "nothing happened" from
/// "I was woken", though the shell's loop does not currently need the
/// difference.
pub fn wait<F: FnMut(isize, WindowEvent)>(timeout_ms: u32, mut on_event: F) -> bool {
    unsafe {
        // No handles: wait on the message queue alone, which is all a shell
        // has. `None` rather than a null handle, because the count is derived
        // from the slice and a null handle with a non-zero count is a fault.
        let r = MsgWaitForMultipleObjectsEx(
            None,
            timeout_ms,
            QS_ALLINPUT,
            MWMO_INPUTAVAILABLE,
        );
        // Anything other than the timeout means there is work: a message, or
        // the wait was interrupted.
        let woke = r.0 as u32 != WAIT_TIMEOUT.0 as u32;
        pump(&mut on_event);
        woke
    }
}

/// Post a quit to every window's loop.
pub fn post_quit() {
    unsafe {
        PostQuitMessage(0);
    }
}

/// Create a shared `Rc` handle, the form the shell stores in its surface table.
pub fn shared(handle: SurfaceHandle) -> Rc<SurfaceHandle> {
    Rc::new(handle)
}

/// Whether a button is physically down, for widget press states.
pub fn mouse_down() -> bool {
    crate::input::key_is_down(VK_LBUTTON.0 as u16)
}

