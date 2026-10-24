//! Kerys, the hotkey daemon, as seen from the shell.
//!
//! Kerys owns the keyboard (a `WH_KEYBOARD_LL` hook, binding modes like
//! Hyprland submaps) and drives rustWM over its pipe. The shell owns pixels. The
//! two meet in exactly two places, and neither needs the other to be running:
//!
//! * **Kerys talks to the shell.** A binding such as
//!   `"win+space" = "shell toggle launcher"` sends one line of JSON to the
//!   shell's control pipe, and every mode switch sends
//!   `{"cmd":"set","key":"kerys.mode","value":"resize"}`. So `kerys.mode` in a
//!   config is pushed, not polled.
//! * **The shell looks for Kerys.** `kerys.running` is whether Kerys's
//!   message-only control window exists, which is the same check `kerys stop`
//!   makes. It is one `FindWindowExW` on the clock's one-second tick: no handle
//!   is held and nothing is polled faster than the clock already is.
//!
//! The actions start, stop and configure Kerys the way its own command line
//! does, so a bar can carry a "keys off" button without knowing how Kerys works.

use std::os::windows::process::CommandExt;

use slowshell_core::{Registry, Value};
use windows::core::w;
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowExW, PostMessageW, HWND_MESSAGE, WM_CLOSE};

/// The executable, found on `PATH` or next to `rustwm.exe` the way rustWM
/// itself finds it.
const PROGRAM: &str = "kerys.exe";

/// Kerys's control window, if Kerys is running.
fn control_window() -> Option<windows::Win32::Foundation::HWND> {
    // SAFETY: a lookup of a message-only window by a static class name.
    unsafe { FindWindowExW(Some(HWND_MESSAGE), None, w!("kerys"), None) }
        .ok()
        .filter(|h| !h.is_invalid())
}

/// Whether a Kerys daemon is running in this session.
pub fn is_running() -> bool {
    control_window().is_some()
}

/// Ask a running Kerys to quit, as `kerys stop` does. False if none was running.
pub fn stop() -> bool {
    // SAFETY: posting WM_CLOSE to the window found by class name; Kerys's
    // window procedure answers it by leaving its message loop.
    control_window().is_some_and(|hwnd| unsafe {
        PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)).is_ok()
    })
}

/// Run `kerys <verb>` without waiting for it and without a console window.
fn spawn(verb: &str) -> Result<(), String> {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new(PROGRAM)
        .arg(verb)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(|_| ())
        .map_err(|e| {
            format!("could not start `{PROGRAM} {verb}`: {e}. Is kerys.exe on PATH?")
        })
}

/// Add the `kerys.*` actions to `registry`.
pub fn register(registry: &mut Registry) {
    registry.register(
        "kerys.start",
        "Start the Kerys hotkey daemon in the background.",
        &[],
        |_| {
            if is_running() {
                return Ok(Value::str("already running"));
            }
            spawn("start").map(|_| Value::str("starting"))
        },
    );
    registry.register("kerys.stop", "Stop the Kerys hotkey daemon.", &[], |_| {
        Ok(Value::str(if stop() { "stopping" } else { "not running" }))
    });
    registry.register(
        "kerys.toggle",
        "Start Kerys if it is stopped, stop it if it is running.",
        &[],
        |_| {
            if stop() {
                Ok(Value::str("stopping"))
            } else {
                spawn("start").map(|_| Value::str("starting"))
            }
        },
    );
    registry.register(
        "kerys.settings",
        "Open the Kerys keybinding editor.",
        &[],
        |_| spawn("gui").map(|_| Value::str("opened")),
    );
}
