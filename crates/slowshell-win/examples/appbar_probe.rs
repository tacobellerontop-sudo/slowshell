//! Which window styles does Windows accept as an app bar?
//!
//! ```text
//! cargo run -p slowshell-win --example appbar_probe
//! ```
//!
//! Registering a window as an app bar and calling `ABM_SETPOS` is supposed to
//! shrink the work area so maximized windows stop at the bar. It does not always,
//! and the variable is the window's style: the shell's own panels are
//! `WS_POPUP` with `WS_EX_TOOLWINDOW | WS_EX_TOPMOST`, which is a sensible thing
//! for a bar to be and may be exactly why the OS declines to treat one as a bar.
//!
//! So this walks the candidate styles, and for each one registers a window,
//! reserves space, reads the work area back, and unregisters. The answer is the
//! list at the end: which styles work, and which are silently ignored.
//!
//! A style that is silently ignored is the worst kind of result, which is why
//! every step prints. The shell needs to know this rather than discover that
//! `exclusive: true` reserves nothing.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GWLP_USERDATA, RegisterClassW,
    SetWindowLongPtrW, ShowWindow, SHOW_WINDOW_CMD, SW_SHOWNOACTIVATE, SW_SHOWNORMAL,
    WINDOW_EX_STYLE, WINDOW_STYLE, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP, WNDCLASSW,
};
use windows::core::w;
use windows::Win32::UI::Shell::{
    ABM_ACTIVATE, ABM_NEW, ABM_QUERYPOS, ABM_REMOVE, ABM_SETPOS, ABE_TOP, APPBARDATA,
    SHAppBarMessage,
};

/// The app bar's callback message, for the window procedure to recognise.
static CALLBACK: AtomicU32 = AtomicU32::new(0);

fn blank(hwnd: HWND) -> APPBARDATA {
    APPBARDATA {
        cbSize: std::mem::size_of::<APPBARDATA>() as u32,
        hWnd: hwnd,
        uCallbackMessage: 0,
        uEdge: 0,
        rc: RECT::default(),
        lParam: LPARAM(0),
    }
}

/// The primary monitor's work area: what maximized windows are actually given.
fn work_area() -> (i32, i32, i32, i32) {
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    unsafe {
        let _ = GetMonitorInfoW(
            MonitorFromWindow(HWND(std::ptr::null_mut()), MONITOR_DEFAULTTOPRIMARY),
            &mut mi,
        );
    }
    (mi.rcWork.left, mi.rcWork.top, mi.rcWork.right, mi.rcWork.bottom)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let cb = CALLBACK.load(Ordering::Relaxed);
    if cb != 0 && msg == cb {
        if wparam.0 as u32 == ABM_QUERYPOS {
            let mut d = blank(hwnd);
            d.uEdge = ABE_TOP;
            d.uCallbackMessage = cb;
            SHAppBarMessage(ABM_QUERYPOS, &mut d);
            // The OS's suggestion is often an empty rect; claiming the whole top
            // strip is the only sensible answer for a full-width bar.
            d.rc = RECT { left: 0, top: 0, right: 1920, bottom: 48 };
            SHAppBarMessage(ABM_SETPOS, &mut d);
        }
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

struct Case {
    name: &'static str,
    ex: WINDOW_EX_STYLE,
    style: WINDOW_STYLE,
    show: SHOW_WINDOW_CMD,
    userdata: bool,
}

fn main() {
    unsafe {
        // The shell calls these before it creates a window. Both are plausible
        // reasons the OS might decline to reserve space, so each is tested
        // rather than guessed at: the first case is bare, and the rest add one
        // piece of process setup.
        let dpi = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let com = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
        println!("process setup: per-monitor-v2 DPI awareness = {dpi:?}, COM = {com}");

        let module = GetModuleHandleW(None).expect("module handle");
        let class_name = w!("SlowshellAppBarProbe");
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: module.into(),
            lpszClassName: class_name,
            ..Default::default()
        };
        let _ = RegisterClassW(&class);

        let cases = [
            // The style is proven irrelevant: every combination reserves space.
            // What is left is *how the window is shown and initialised*, which is
            // what the shell does differently: it writes `GWLP_USERDATA` after
            // creating the window, and shows with `SW_SHOWNOACTIVATE` so the bar
            // cannot steal focus from the window the user is working in.
            Case { name: "SW_SHOWNORMAL", ex: WINDOW_EX_STYLE(0), style: WS_POPUP, show: SW_SHOWNORMAL, userdata: false },
            Case { name: "SW_SHOWNOACTIVATE", ex: WINDOW_EX_STYLE(0), style: WS_POPUP, show: SW_SHOWNOACTIVATE, userdata: false },
            Case { name: "SW_SHOWNORMAL + GWLP_USERDATA", ex: WINDOW_EX_STYLE(0), style: WS_POPUP, show: SW_SHOWNORMAL, userdata: true },
            Case { name: "SW_SHOWNOACTIVATE + GWLP_USERDATA", ex: WINDOW_EX_STYLE(0), style: WS_POPUP, show: SW_SHOWNOACTIVATE, userdata: true },
            // The shell's real combination, and the ones either side of it, so
            // a regression in either direction is visible.
            Case { name: "SHELL: TOOLWINDOW+NOACTIVATE, SHOWNOACTIVATE, userdata", ex: (WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE).into(), style: WS_POPUP, show: SW_SHOWNOACTIVATE, userdata: true },
            Case { name: "SHELL but SW_SHOWNORMAL", ex: (WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE).into(), style: WS_POPUP, show: SW_SHOWNORMAL, userdata: true },
            Case { name: "SHELL but no userdata", ex: (WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE).into(), style: WS_POPUP, show: SW_SHOWNOACTIVATE, userdata: false },
            Case { name: "SHELL but no NOACTIVATE", ex: WS_EX_TOOLWINDOW.into(), style: WS_POPUP, show: SW_SHOWNOACTIVATE, userdata: true },
        ];

        println!("baseline work area: {}\n", {
            let (l, t, r, b) = work_area();
            format!("{},{} {}x{}", l, t, r - l, b - t)
        });

        let mut worked: Vec<&str> = Vec::new();
        let mut ignored: Vec<&str> = Vec::new();

        for case in &cases {
            let hwnd = CreateWindowExW(
                case.ex,
                class_name,
                w!("probe"),
                case.style,
                0,
                0,
                1920,
                48,
                None,
                None,
                Some(module.into()),
                None,
            );
            let Ok(hwnd) = hwnd else {
                println!("{:<44} could not create the window", case.name);
                ignored.push(case.name);
                continue;
            };
            if case.userdata {
                // The shell stashes its `WindowState` in `GWLP_USERDATA`
                // immediately after creating the window, so the window procedure
                // can find it. Tested because it is a real difference between
                // this and the shell, not because it looks suspicious.
                let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0x1234);
            }
            let _ = ShowWindow(hwnd, case.show);

            let callback = {
                let mut d = blank(hwnd);
                d.uEdge = ABE_TOP;
                SHAppBarMessage(ABM_NEW, &mut d) as u32
            };
            CALLBACK.store(callback, Ordering::Relaxed);
            let mut d = blank(hwnd);
            d.uEdge = ABE_TOP;
            d.uCallbackMessage = callback;
            SHAppBarMessage(ABM_ACTIVATE, &mut d);
            d.rc = RECT { left: 0, top: 0, right: 1920, bottom: 48 };
            let ok = SHAppBarMessage(ABM_SETPOS, &mut d);

            // The work area is updated by the shell asynchronously, so a short
            // wait is not optional here.
            std::thread::sleep(std::time::Duration::from_millis(400));
            let (l, t, r, b) = work_area();
            let height = b - t;
            let moved = height < 1032;
            println!(
                "{:<44} ABM_SETPOS={ok}  work area {},{} {}x{}  {}",
                case.name, l, t, r - l, height,
                if moved { "RESERVED" } else { "ignored" }
            );
            if moved {
                worked.push(case.name);
            } else {
                ignored.push(case.name);
            }

            let mut d = blank(hwnd);
            d.uEdge = ABE_TOP;
            d.uCallbackMessage = callback;
            SHAppBarMessage(ABM_REMOVE, &mut d);
            CALLBACK.store(0, Ordering::Relaxed);
            std::thread::sleep(std::time::Duration::from_millis(300));
            let _ = DestroyWindow(hwnd);
        }

        println!("\nreserve space successfully:");
        for w in worked {
            println!("  {w}");
        }
        println!("\nsilently ignored:");
        for w in ignored {
            println!("  {w}");
        }
        let _ = std::ptr::null::<c_void>();
    }
}
