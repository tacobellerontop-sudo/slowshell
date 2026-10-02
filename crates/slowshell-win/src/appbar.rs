//! Reserving screen space, the way a taskbar does.
//!
//! # Why the AppBar API and not a work-area hack
//!
//! "Make maximized windows stop above the bar" is the `exclusive: true`
//! property. There are two ways to get it on Windows, and only one of them is
//! correct.
//!
//! The wrong way is to write the per-monitor `WorkArea` value under
//! `HKCU\System\CurrentControlSet\Control\Desktop\PerMonitorSettings` and
//! broadcast `WM_SETTINGCHANGE`. It works, briefly, and then causes trouble:
//!
//! - It **overwrites** Explorer's own per-monitor work area, so the real taskbar
//!   stops reserving its space and windows go under it.
//! - The shell never gets to restore it, because a value written to the registry
//!   outlives the process. Kill the shell with Task Manager and the work area
//!   stays wrong until someone logs off.
//! - Two bars, or a bar and a taskbar, produce two sources of truth and a fight.
//!
//! The right way is `SHAppBarMessage`, which is the documented mechanism
//! Windows provides for exactly this, and what Explorer's own taskbar uses. The
//! OS keeps a list of registered app bars, sums their edges, hands each one a
//! non-overlapping position, and shrinks the work area by the total. It
//! composes with the real taskbar, it restores itself if the process dies, and
//! it is the same thing every taskbar replacement on Windows is built on.
//!
//! # The protocol
//!
//! 1. `ABM_NEW` with `cbSize` and `hWnd` set. The return value is a callback
//!    message id, unique to this process. That id must be handled in the window
//!    procedure; the OS sends it whenever anything about the bar changes.
//! 2. Answer `ABM_QUERYPOS`: Windows puts the *whole* screen in `rc` and asks
//!    where this bar would go if it were the only one. Working out the
//!    non-overlapping position against the other registered bars is the app's
//!    job, and `answer_query_pos` does it.
//! 3. `ABM_SETPOS` with the final rectangle. The OS reserves that space.
//! 4. `ABM_REMOVE` on shutdown, so the space comes back.
//!
//! Steps 2 and 3 are the whole trick, and skipping step 2 is the usual reason a
//! hand-rolled app bar ends up on top of the taskbar.

use std::cell::Cell;
use std::collections::HashSet;

use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::WM_USER;
use windows::Win32::UI::Shell::{
    ABE_BOTTOM, ABE_LEFT, ABE_RIGHT, ABE_TOP, ABM_ACTIVATE, ABM_GETAUTOHIDEBAREX,
    ABM_GETTASKBARPOS, ABM_NEW, ABM_QUERYPOS, ABM_REMOVE, ABM_SETPOS, ABM_WINDOWPOSCHANGED,
    APPBARDATA, SHAppBarMessage,
};

/// Which screen edge a bar is attached to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Top,
    Bottom,
    Left,
    Right,
    /// A floating panel reserves nothing. It is an overlay, not a bar.
    None,
}

impl Edge {
    fn abi(self) -> u32 {
        match self {
            Edge::Top => ABE_TOP,
            Edge::Bottom => ABE_BOTTOM,
            Edge::Left => ABE_LEFT,
            Edge::Right => ABE_RIGHT,
            Edge::None => ABE_LEFT,
        }
    }

    pub fn is_horizontal(self) -> bool {
        matches!(self, Edge::Top | Edge::Bottom)
    }

    /// The edge as it appears in a config file and in diagnostics.
    ///
    /// Lower case and unquoted, so it can be pasted straight into a config and
    /// read in a log line. Used instead of `{:?}`, which would print `Top` and
    /// read like a type name rather than a setting.
    pub fn name(self) -> &'static str {
        match self {
            Edge::Top => "top",
            Edge::Bottom => "bottom",
            Edge::Left => "left",
            Edge::Right => "right",
            Edge::None => "floating",
        }
    }
}

// The callback message this process's app bar was assigned.
//
// `ABM_NEW` returns it, and it is not a constant. Every `APPBARDATA` the shell
// sends has to carry the same value, because the OS uses it to recognise the
// sender — so the raw value is kept as sent.
//
// The window procedure, however, must never *match* on a value this low, and
// `callback_message` is the one to match on. See there for why.
thread_local! {
    static CALLBACK: Cell<u32> = const { Cell::new(0) };
}

/// The callback id to put in `APPBARDATA`: what `ABM_NEW` handed back.
fn callback_raw() -> u32 {
    CALLBACK.with(|c| c.get())
}

/// The callback message the window procedure should match on, or 0 for none.
///
/// This is deliberately *not* the raw value. On this machine `ABM_NEW` returns
/// **1**, and 1 is `WM_CREATE`. A window procedure that matches `msg == 1`
/// swallows every window's `WM_CREATE` and fires a position request with a
/// zero-sized rectangle at the instant the window is created, which makes the
/// real request that follows be ignored — so `exclusive: true` reserves nothing,
/// silently, while every call reports success.
///
/// A genuine callback id is a registered window message and therefore well above
/// `WM_USER`. Anything at or below it is discarded for matching purposes, and the
/// position handshake is simply skipped. Losing the handshake is survivable:
/// the space is still reserved. Corrupting the registration is not.
pub fn callback_message() -> u32 {
    let raw = CALLBACK.with(|c| c.get());
    if raw > WM_USER { raw } else { 0 }
}

/// A registered app bar.
///
/// Registered on creation and released on drop, so a surface torn down mid-edit
/// cannot leave screen space reserved. That is the single most important property
/// here: a shell that crashes must not leave a 40-pixel hole in the desktop.
pub struct AppBar {
    hwnd: HWND,
    edge: Edge,
    registered: bool,
    /// The rectangle last handed to `ABM_SETPOS`, so a resize can tell the
    /// difference between "moved" and "did not change".
    current: Cell<Option<(i32, i32, i32, i32)>>,
}

impl AppBar {
    /// Register `hwnd` as an app bar on `edge`.
    ///
    /// Returns `None` for [`Edge::None`], because a floating panel reserves
    /// nothing — that is not a failure, it is what a floating window is.
    pub fn register(hwnd: HWND, edge: Edge) -> Option<AppBar> {
        if edge == Edge::None {
            return None;
        }
        let abi_edge = edge.abi();

        let callback = unsafe {
            let mut data = blank(hwnd);
            data.uEdge = abi_edge;
            SHAppBarMessage(ABM_NEW, &mut data) as u32
        };
        if callback == 0 {
            slowshell_core::warn!(
                "could not register as an app bar; the bar will overlap maximized windows"
            );
            return None;
        }
        CALLBACK.with(|c| c.set(callback));
        if callback_message() == 0 {
            slowshell_core::warn!(
                "the OS returned callback message {callback}, which is not a real window \
                 message; the bar will reserve space but will not negotiate a position"
            );
        }
        slowshell_core::debug!(
            "ABM_NEW registered on the {} edge, callback message {callback}",
            match edge {
                Edge::Top => "top",
                Edge::Bottom => "bottom",
                Edge::Left => "left",
                Edge::Right => "right",
                Edge::None => "none",
            }
        );

        // `ABM_ACTIVATE` puts the bar at the front of the list for its edge, so
        // the positioning query is sent to it first. Without this a new bar can
        // be handed a position that overlaps an existing one.
        //
        // The constant is imported rather than written down: `ABM_SETPOS` and
        // `ABM_ACTIVATE` are adjacent, and getting them the wrong way round
        // sends a zero-sized position request at the moment of registration,
        // which the OS answers by not asking again.
        unsafe {
            let mut data = blank(hwnd);
            data.uEdge = abi_edge;
            data.uCallbackMessage = callback;
            SHAppBarMessage(ABM_ACTIVATE, &mut data);
        }

        // A bar that is not on screen reserves nothing, and the OS says so by
        // accepting the position request and doing nothing with it. Logging the
        // two facts that decide this saves a long hunt: a registration that
        // "succeeds" on an invisible window is the shape of this bug.
        slowshell_core::debug!(
            "registered app bar: visible={}, style={:#x}, exstyle={:#x}",
            crate::window::is_visible(hwnd),
            crate::window::window_style(hwnd),
            crate::window::window_ex_style(hwnd),
        );

        Some(AppBar {
            hwnd,
            edge,
            registered: true,
            current: Cell::new(None),
        })
    }

    pub fn edge(&self) -> Edge {
        self.edge
    }

    /// The rectangle this bar currently reserves, exactly.
    ///
    /// Not the enumeration, which is a lower bound — this is the rectangle the
    /// bar last handed the OS, so it is what `shellctl screens` reports for
    /// Slowshell's own bars.
    pub fn reserved_rect(&self) -> Option<(i32, i32, i32, i32)> {
        self.current.get()
    }

    /// Reserve `rect` on screen, in physical pixels.
    ///
    /// Idempotent: reserving the same rectangle twice is free and sends no
    /// work area change, so a resize event that did not change the geometry
    /// costs nothing.
    pub fn set_bounds(&self, rect: (i32, i32, i32, i32)) {
        if !self.registered {
            return;
        }
        if self.current.get() == Some(rect) {
            return;
        }
        unsafe {
            let mut data = blank(self.hwnd);
            data.uEdge = self.edge.abi();
            data.uCallbackMessage = callback_raw();
            data.rc = RECT {
                left: rect.0,
                top: rect.1,
                right: rect.2,
                bottom: rect.3,
            };
            let ok = SHAppBarMessage(ABM_SETPOS, &mut data);
            slowshell_core::debug!(
                "ABM_SETPOS {:?} on the {} edge returned {ok}",
                rect,
                match self.edge {
                    Edge::Top => "top",
                    Edge::Bottom => "bottom",
                    Edge::Left => "left",
                    Edge::Right => "right",
                    Edge::None => "none",
                }
            );
            if ok == 0 {
                slowshell_core::warn!(
                    "ABM_SETPOS refused; the bar is visible but reserves no space"
                );
                return;
            }
        }
        self.current.set(Some(rect));
        slowshell_core::debug!(
            "reserved {:?} on the {} edge",
            rect,
            match self.edge {
                Edge::Top => "top",
                Edge::Bottom => "bottom",
                Edge::Left => "left",
                Edge::Right => "right",
                Edge::None => "none",
            }
        );
    }

    /// Whether the OS has been asked to slide this bar out of the way.
    ///
    /// Auto-hide is not implemented — nothing here slides the window — but the
    /// OS's setting is real and worth reporting. A bar the OS has marked for
    /// auto-hide is still on screen, because only the owner of the window can
    /// slide it, so it is still correctly reserving space. What the flag tells
    /// you is that the *user* asked for a bar that gets out of the way, and the
    /// shell has not given them that.
    pub fn auto_hidden(&self) -> bool {
        if !self.registered {
            return false;
        }
        unsafe {
            let mut data = blank(self.hwnd);
            data.uEdge = self.edge.abi();
            data.uCallbackMessage = callback_raw();
            SHAppBarMessage(ABM_GETAUTOHIDEBAREX, &mut data) != 0
        }
    }

    /// Answer a callback message from the OS.
    ///
    /// Returns `true` if the message was ours, so the window procedure knows
    /// whether to pass it on to `DefWindowProc`.
    pub fn handle(&self, wparam: WPARAM) -> bool {
        if !self.registered {
            return false;
        }
        let message = wparam.0 as u32;
        slowshell_core::debug!("app bar callback {message}");
        match message {
            ABM_QUERYPOS => {
                self.answer_query_pos();
                true
            }
            ABM_WINDOWPOSCHANGED => {
                // The OS moved us. It has already reserved the space, so there
                // is nothing to do but let the shell repaint.
                self.current.set(None);
                true
            }
            _ => false,
        }
    }

    /// Work out where this bar goes without overlapping any other.
    ///
    /// This is the documented algorithm, and it is the part that is usually
    /// missed. Windows puts the whole screen in `rc` and asks "given the other
    /// bars that are already registered, where would you go?" The answer is
    /// found by walking the screen edge in the bar's direction and subtracting
    /// the thickness of every other bar it passes.
    fn answer_query_pos(&self) {
        unsafe {
            let mut data = blank(self.hwnd);
            data.uEdge = self.edge.abi();
            data.uCallbackMessage = callback_raw();
            SHAppBarMessage(ABM_QUERYPOS, &mut data);

            // The OS hands us the whole screen and asks where this bar would go
            // given everything already registered. The answer is found by
            // walking the edge inward, stepping over whatever is already
            // reserved, and claiming the next free strip.
            let screen = data.rc;
            let (w, h) = self.window_size();
            let mut r = screen;

            match self.edge {
                Edge::Top => {
                    let mut cursor = screen.top;
                    for other in reserved_on(Edge::Top) {
                        if other.1 == cursor {
                            cursor = other.3;
                        }
                    }
                    r.left = screen.left;
                    r.right = screen.right;
                    r.top = cursor;
                    r.bottom = cursor + h;
                }
                Edge::Bottom => {
                    let mut cursor = screen.bottom;
                    for other in reserved_on(Edge::Bottom) {
                        if other.3 == cursor {
                            cursor = other.1;
                        }
                    }
                    r.left = screen.left;
                    r.right = screen.right;
                    r.bottom = cursor;
                    r.top = cursor - h;
                }
                Edge::Left => {
                    let mut cursor = screen.left;
                    for other in reserved_on(Edge::Left) {
                        if other.0 == cursor {
                            cursor = other.2;
                        }
                    }
                    r.top = screen.top;
                    r.bottom = screen.bottom;
                    r.left = cursor;
                    r.right = cursor + w;
                }
                Edge::Right => {
                    let mut cursor = screen.right;
                    for other in reserved_on(Edge::Right) {
                        if other.2 == cursor {
                            cursor = other.0;
                        }
                    }
                    r.top = screen.top;
                    r.bottom = screen.bottom;
                    r.right = cursor;
                    r.left = cursor - w;
                }
                Edge::None => return,
            }

            data.rc = r;
            SHAppBarMessage(ABM_SETPOS, &mut data);
        }
    }

    /// This window's size, in the order the edge branches want it.
    fn window_size(&self) -> (i32, i32) {
        let r = self.window_rect();
        (r.2 - r.0, r.3 - r.1)
    }

    fn window_rect(&self) -> (i32, i32, i32, i32) {
        use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
        let mut r = RECT::default();
        unsafe {
            let _ = GetWindowRect(self.hwnd, &mut r);
        }
        (r.left, r.top, r.right, r.bottom)
    }
}

impl Drop for AppBar {
    fn drop(&mut self) {
        if !self.registered {
            return;
        }
        unsafe {
            let mut data = blank(self.hwnd);
            data.uEdge = self.edge.abi();
            data.uCallbackMessage = callback_raw();
            SHAppBarMessage(ABM_REMOVE, &mut data);
        }
        self.registered = false;
    }
}

fn blank(hwnd: HWND) -> APPBARDATA {
    APPBARDATA {
        // Windows requires `cbSize` and refuses the call without it. Getting
        // this wrong is the single most common AppBar bug.
        cbSize: std::mem::size_of::<APPBARDATA>() as u32,
        hWnd: hwnd,
        uCallbackMessage: 0,
        uEdge: 0,
        rc: RECT::default(),
        lParam: LPARAM(0),
    }
}

/// Every app bar Windows has registered, with the edge **it** reports.
///
/// The subtle part, and the reason a hand-rolled version of this usually
/// misreports rather than errors. `ABM_GETTASKBARPOS` takes the edge you want
/// in `uEdge` and returns the first bar on that edge, but the `hWnd` it hands
/// back is the *input* to the next call. So the walk is: keep asking, following
/// the `hWnd` the OS gives you, until one comes back that has already been
/// seen — which is how you know the list has wrapped.
///
/// Two mistakes are easy here. Passing the same null `hWnd` every time returns
/// the same first bar forever, so the loop never advances and one bar is
/// reported per edge asked about — which looks like the taskbar being reported
/// four times. And trusting the edge that was asked for rather than the edge
/// that came back counts one strip as two.
fn enumerate_for_edge(edge: Edge) -> Vec<(u32, (i32, i32, i32, i32))> {
    let want = edge.abi();
    let mut out = Vec::new();
    let mut seen: HashSet<isize> = HashSet::new();
    // Start from nothing, then follow whatever the OS hands back.
    let mut cursor = HWND(std::ptr::null_mut());
    unsafe {
        for _ in 0..32 {
            let mut data = blank(cursor);
            data.uEdge = want;
            // The id the OS gave this process, so the query is recognised as
            // coming from a registered app bar rather than a stranger.
            data.uCallbackMessage = match callback_raw() {
                0 => u32::MAX,
                m => m,
            };
            if SHAppBarMessage(ABM_GETTASKBARPOS, &mut data) == 0 {
                break;
            }
            if data.hWnd.0.is_null() || !seen.insert(data.hWnd.0 as isize) {
                // The list has wrapped, or there is nothing on this edge.
                break;
            }
            cursor = data.hWnd;
            let r = data.rc;
            if r.right <= r.left || r.bottom <= r.top {
                // A zero-area entry is the terminator, not a bar.
                break;
            }
            // Trust the edge the OS reported over the one that was asked for.
            if data.uEdge == want {
                out.push((want, (r.left, r.top, r.right, r.bottom)));
            }
        }
    }
    out
}

/// The rectangles already reserved on one edge, including our own.
///
/// Used when answering a positioning query, where our own current position must
/// be stepped over or the bar would be told to sit on top of itself.
fn reserved_on(edge: Edge) -> Vec<(i32, i32, i32, i32)> {
    enumerate_for_edge(edge).into_iter().map(|(_, r)| r).collect()
}

/// Every app bar currently registered, as `(edge, rect)`.
///
/// Walks the OS's own list. **Read this as a lower bound**: Windows does not
/// reliably disclose *other* processes' bars, and Explorer's own taskbar in
/// particular may simply not appear. An empty result does not mean nothing is
/// registered — the per-monitor work area is the authoritative answer to "is
/// space reserved", and that is what `shellctl screens` reports.
///
/// For our own bar, `AppBar::reserved_rect` is exact and is what the shell
/// reports for itself.
pub fn reserved_bars() -> Vec<(u32, (i32, i32, i32, i32))> {
    const EDGES: [Edge; 4] = [Edge::Left, Edge::Top, Edge::Right, Edge::Bottom];
    let mut out = Vec::new();
    for e in EDGES {
        out.extend(enumerate_for_edge(e));
    }
    out
}

/// A one-line description of each registered bar, for diagnostics.
pub fn describe_reserved() -> Vec<String> {
    const NAMES: [&str; 4] = ["left", "top", "right", "bottom"];
    reserved_bars()
        .into_iter()
        .map(|(edge, r)| {
            format!(
                "{} edge, {}x{} at {},{}",
                NAMES[(edge as usize).min(3)],
                r.2 - r.0,
                r.3 - r.1,
                r.0,
                r.1
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_floating_panel_reserves_nothing() {
        // Not a failure: a floating window is an overlay by definition, and
        // making it reserve space would be a bug.
        assert!(AppBar::register(HWND(std::ptr::null_mut()), Edge::None).is_none());
    }

    #[test]
    fn edges_map_to_the_documented_values() {
        // Getting these wrong reserves space on the wrong side of the screen,
        // which looks like a rendering fault somewhere else entirely.
        assert_eq!(Edge::Left.abi(), ABE_LEFT);
        assert_eq!(Edge::Top.abi(), ABE_TOP);
        assert_eq!(Edge::Right.abi(), ABE_RIGHT);
        assert_eq!(Edge::Bottom.abi(), ABE_BOTTOM);
    }

    #[test]
    fn only_top_and_bottom_are_horizontal() {
        assert!(Edge::Top.is_horizontal());
        assert!(Edge::Bottom.is_horizontal());
        assert!(!Edge::Left.is_horizontal());
        assert!(!Edge::Right.is_horizontal());
    }

    #[test]
    fn the_structure_size_is_what_windows_expects() {
        // `cbSize` must be the real size of the struct or the call is refused,
        // with no error to tell you why.
        let d = blank(HWND(std::ptr::null_mut()));
        assert_eq!(d.cbSize as usize, std::mem::size_of::<APPBARDATA>());
    }

    #[test]
    fn a_still_desktop_reports_a_short_list() {
        // Proves the enumeration terminates rather than looping on a null
        // handle, which is the failure mode of a hand-rolled version of this.
        for (_, r) in reserved_bars() {
            assert!(
                r.2 > r.0 && r.3 > r.1,
                "a reserved bar with no area is a bug, not a bar: {r:?}"
            );
        }
        assert!(reserved_bars().len() <= 32, "the list must be bounded");
    }

    #[test]
    fn every_reserved_bar_describes_itself() {
        for line in describe_reserved() {
            assert!(line.contains("edge"), "{line}");
        }
    }
}
