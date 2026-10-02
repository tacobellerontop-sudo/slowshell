//! Transient notifications.
//!
//! Windows has no single notification API, and the two that exist need setup a
//! desktop shell does not get for free:
//!
//! * `Shell_NotifyIcon` balloon tips need a tray icon, and the icon must stay
//!   alive for the balloon to be shown — which for a shell that has no tray is
//!   not an option.
//! * `ToastNotificationManager` needs a registered AppUserModelID and a
//!   shortcut on disk, which only works for a packaged app or one the user has
//!   installed.
//!
//! Rather than fake either, this draws a small always-on-top window in the
//! bottom corner, styled like a notification, for a few seconds. It is the shell's
//! own surface type, so it needs no registration, works on every Windows 10 and
//! 11 machine, and is the same code path a themed notification would use. The
//! limitation is stated plainly: it belongs to the shell process, so it
//! disappears when the shell does.

use slowshell_core::Color;

use crate::render::{Graphics, Painter, Rect, TextEngine, TextStyle};
use crate::window::{self, SurfaceRole};

/// How long a notification stays up, and where it sits.
const WIDTH: u32 = 340;
const MARGIN: i32 = 16;
const PAD: f32 = 12.0;

/// Show a notification.
///
/// Fire and forget: the surface is destroyed by its own timer, so a caller that
/// spams `notify` gets a stack of cards rather than a frozen one. Returns as soon
/// as the window exists.
pub fn toast(title: &str, body: &str) -> Result<(), String> {
    if title.trim().is_empty() && body.trim().is_empty() {
        return Err("a notification needs a title or a body".into());
    }
    let graphics = Graphics::new().map_err(|e| format!("{e}"))?;
    graphics.create().map_err(|e| format!("{e}"))?;

    // Bottom corner of the primary display, above the taskbar.
    let monitors = crate::monitors::Monitors::enumerate();
    let m = monitors.primary().clone();
    let height = 96u32;
    let x = m.x + m.width as i32 - WIDTH as i32 - MARGIN;
    let y = m.y + m.work_height as i32 - height as i32 - MARGIN;

    let handle = window::create_surface(SurfaceRole::Popup, x, y, WIDTH, height, "notification")
        .map_err(|e| format!("{e}"))?;
    window::show(handle.hwnd);
    window::set_always_on_top(handle.hwnd, true);

    let surface = match graphics.create_surface(handle.hwnd, WIDTH, height) {
        Ok(s) => s,
        Err(e) => {
            window::destroy(handle);
            return Err(format!("{e}"));
        }
    };
    let mut text = TextEngine::new(graphics.write_factory());
    let mut surface = surface;
    // The window render target presents the previous frame, so a couple of
    // frames are drawn before anything is on screen.
    for _ in 0..3 {
        surface.begin(96.0, Some(Color::rgba(0x1a, 0x1b, 0x26, 0xf2)));
        {
            let mut p = Painter::new(surface.target(), &mut text, 96.0);
            p.fill_rect(Rect::new(0.0, 0.0, WIDTH as f32, height as f32), Color::rgba(0x24, 0x25, 0x32, 0xf8), 10.0);
            p.fill_rect(Rect::new(0.0, 0.0, 3.0, height as f32), Color::rgb(0x7c, 0x9c, 0xff), 10.0);
            let title_style = TextStyle { size: 14.0, weight: 600, ..TextStyle::default() };
            let body_style = TextStyle { size: 13.0, ..TextStyle::default() };
            p.text(
                title,
                &title_style,
                Rect::new(PAD, PAD, WIDTH as f32 - PAD * 2.0, 20.0),
                Color::rgb(0xe6, 0xe7, 0xef),
            );
            p.text(
                body,
                &body_style,
                Rect::new(PAD, PAD + 22.0, WIDTH as f32 - PAD * 2.0, height as f32 - PAD * 2.0 - 22.0),
                Color::rgba(0x9a, 0x9d, 0xb0, 0xff),
            );
        }
        surface.present();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    // Takes the window id over and destroys the window when the sleep ends. The
    // graphics device and text engine go with it, which is the right time: they
    // are only needed for the frames above. The id travels as a `usize` because
    // an `HWND` is a raw pointer and would not be `Send`.
    let id = handle.hwnd.0 as usize;
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(4));
        window::destroy(window::SurfaceHandle {
            hwnd: windows::Win32::Foundation::HWND(id as *mut std::ffi::c_void),
        });
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_notification_is_refused() {
        // Showing an empty card is worse than showing none: the user cannot tell
        // it apart from a broken one.
        let e = toast("  ", "").unwrap_err();
        assert!(e.contains("title or a body"), "{e}");
    }
}
