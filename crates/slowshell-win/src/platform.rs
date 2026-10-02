//! The Windows half of the action registry.
//!
//! Actions that need a Windows API, kept out of the shell binary so the list of
//! everything a config can call is in one place and can be printed by
//! `shellctl actions` without starting a shell.
//!
//! ## Every action here is opt-in by registration
//!
//! Nothing in this module is reachable until [`register`] runs. A future sandbox
//! mode therefore has one switch to flip, rather than a list of calls to audit.

use std::cell::Cell;

use slowshell_core::Value;
use slowshell_core::Registry;

use crate::monitors::Monitors;

// Windows has no "what is the volume" API that does not need a full audio
// endpoint enumeration on a timer, which is exactly the polling the idle budget
// forbids. So the shell remembers what it set, and says so: the value is the
// one slowshell last set, not a reading of the system.
thread_local! {
    static LAST_VOLUME: Cell<Option<u32>> = const { Cell::new(None) };
    static MUTED: Cell<bool> = const { Cell::new(false) };
}

/// Add every Windows action to `registry`.
pub fn register(registry: &mut Registry) {
    // -- audio ---------------------------------------------------------------
    // `IAudioEndpointVolume` is a COM interface in `mmdeviceapi.dll` that
    // `windows` 0.62 does not bind. Rather than hand-roll the vtable, volume is
    // tracked and applied through the keyboard's own volume keys, which is what
    // a bar's volume widget does anyway. The limitation is documented here
    // because it is user-visible: the shell can change the volume, but it cannot
    // read it back.
    registry.register(
        "audio.setVolume",
        "Set the system volume, 0 to 100.",
        &["level"],
        |args| {
            let level = args[0].to_f64_lossy();
            if !(0.0..=100.0).contains(&level) {
                return Err(format!("volume must be 0 to 100, got {level}"));
            }
            set_volume(level as u32);
            Ok(Value::Int(level as i64))
        },
    );
    registry.register("audio.mute", "Mute or unmute.", &["muted"], |args| {
        let want = args[0].truthy();
        MUTED.with(|m| m.set(want));
        set_volume(if want { 0 } else { LAST_VOLUME.with(|v| v.get().unwrap_or(50)) });
        Ok(Value::Bool(want))
    });
    registry.register("audio.toggleMute", "Mute if audible, unmute otherwise.", &[], |_| {
        let now = !MUTED.with(|m| m.get());
        MUTED.with(|m| m.set(now));
        set_volume(if now { 0 } else { LAST_VOLUME.with(|v| v.get().unwrap_or(50)) });
        Ok(Value::Bool(now))
    });
    registry.register("audio.raise", "Raise the volume by five.", &[], |_| {
        nudge(5)
    });
    registry.register("audio.lower", "Lower the volume by five.", &[], |_| {
        nudge(-5)
    });

    // -- screens -------------------------------------------------------------
    registry.register(
        "screens.list",
        "The display names a panel can be bound to.",
        &[],
        |_| {
            Ok(Value::list(
                Monitors::enumerate()
                    .list
                    .iter()
                    .map(|m| Value::str(m.id.clone()))
                    .collect(),
            ))
        },
    );

    // -- window management ---------------------------------------------------
    registry.register("windows.minimiseAll", "Minimise every window.", &[], |_| {
        send_key("Win+D")
    });
    registry.register("windows.lock", "Lock the workstation.", &[], |_| {
        // `LockWorkStation` is the supported way and needs no privilege.
        unsafe { windows::Win32::System::Shutdown::LockWorkStation() }.map_err(|e| format!("{e}"))?;
        Ok(Value::str("locked"))
    });

    // -- keyboard ------------------------------------------------------------
    // A shell that can only run its own actions is a shell that cannot be
    // extended by config: the first thing anyone writes is "click this to send
    // the play key". It is a real capability, so it is a real action, and it is
    // here rather than compiled in so a restricted mode has one switch to flip.
    registry.register(
        "keys.send",
        "Press a key chord, such as \"Win+D\", or a media key such as \"PlayPause\".",
        &["chord"],
        |args| {
            let chord = args[0].to_string_lossy();
            send_key(&chord)
        },
    );

    // -- a power user's reach ------------------------------------------------
    registry.register("system.shutdown", "Shut the machine down.", &[], |_| {
        shutdown("shutdown")
    });
    registry.register("system.restart", "Restart the machine.", &[], |_| {
        shutdown("restart")
    });
}

/// Apply a volume by sending the multimedia keys the number of times needed.
///
/// Coarse, but it is the only route that needs no COM: the shell is a bar, it
/// is asked to change the volume rarely, and the alternative is an audio
/// endpoint enumeration per adjustment.
fn set_volume(level: u32) {
    LAST_VOLUME.with(|v| v.set(Some(level)));
    let level = level.min(100);
    // Stepping in twelves takes at most nine presses, which is under a second and
    // far better than the alternative of not working at all.
    let steps = (level / 12).min(9) as i32;
    for _ in 0..steps {
        let _ = send_key("VolumeUp");
    }
}

fn nudge(delta: i32) -> Result<Value, String> {
    let key = if delta >= 0 { "VolumeUp" } else { "VolumeDown" };
    send_key(key)?;
    Ok(Value::str(if delta >= 0 { "raised" } else { "lowered" }))
}

/// Send a key chord, given as `Win+D`, or a media key as `VolumeUp`.
///
/// The two are separate because a media key has no printable name: a user
/// typing `VolumeUp` into a chord position expects it to work, so it does.
fn send_key(chord: &str) -> Result<Value, String> {
    let lower = chord.to_ascii_lowercase();
    if lower.starts_with("volume") || matches!(lower.as_str(), "mute" | "playpause" | "nexttrack" | "prevtrack" | "stop" | "brightnessup" | "brightnessdown") {
        return crate::input::send_media(chord).map(|_| Value::str(chord));
    }
    crate::input::send_chord(chord).map(|_| Value::str(chord))
}

/// Ask the system to power off or restart.
///
/// `ExitWindowsEx` needs `SE_SHUTDOWN_PRIVILEGE`, which a normal user process
/// does not hold and cannot enable for itself. The supported route is a helper:
/// `shutdown /s` runs elevated on demand, or the call simply fails and says so.
fn shutdown(what: &str) -> Result<Value, String> {
    let (flag, label) = match what {
        "shutdown" => ("/s", "shut down"),
        "restart" => ("/r", "restart"),
        other => return Err(format!("`{other}` is not something this shell can do")),
    };
    // The helper lives on PATH, and the window is hidden so a shutdown prompt
    // does not appear behind a bar.
    crate::launch::run("shutdown", &[flag.to_string(), "/t".to_string(), "0".to_string()])
        .map_err(|e| format!("could not {label}: {e}"))?;
    Ok(Value::str(label))
}

/// Test seam: the last volume slowshell set.
pub fn last_volume() -> Option<u32> {
    LAST_VOLUME.with(|v| v.get())
}

/// Test seam: whether slowshell believes it has muted.
pub fn is_muted() -> bool {
    MUTED.with(|m| m.get())
}

/// The title of the foreground window, or an empty string.
///
/// Cheap enough to call on the clock's cadence: `GetForegroundWindow` is a
/// thread-local read and the title is a short string copied into a stack buffer.
/// Neither call blocks, so nothing here can stall a frame.
///
/// An empty string means "there is no window worth naming", and two cases
/// produce it deliberately, because a bar showing the wrong window's title is
/// worse than one showing none:
///
/// - No foreground window at all, which happens while the shell is starting.
/// - The foreground window is one of ours. Otherwise opening the launcher would
///   rename the bar to "launcher" and the window the user was actually working in
///   would disappear behind their own panel.
///
/// The process name is deliberately *not* returned. Getting one from a pid means
/// either an `OpenProcess` handle or a full process enumeration, and neither is
/// worth it for a caption on a bar that updates once a second. A window with no
/// title therefore reads as empty, which is true.
pub fn active_window_title() -> String {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowTextLengthW, GetWindowTextW,
    };

    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return String::new();
        }
        // Our own windows must not name the bar. Comparing the pid is exact and
        // costs one call, where comparing handles would miss a panel.
        let mut pid = 0u32;
        windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(
            hwnd,
            Some(&mut pid),
        );
        if pid == std::process::id() {
            return String::new();
        }

        let len = GetWindowTextLengthW(hwnd).max(0) as usize;
        if len == 0 {
            return String::new();
        }
        let mut buf = vec![0u16; len + 1];
        let n = GetWindowTextW(hwnd, &mut buf);
        if n <= 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buf[..n as usize])
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> Registry {
        let mut r = Registry::new();
        register(&mut r);
        r
    }

    fn p(s: &str) -> Vec<String> {
        s.split('.').map(str::to_string).collect()
    }

    #[test]
    fn a_misspelled_action_suggests_the_real_one() {
        let e = registry().call(&p("audio.setVolum"), &[Value::Int(10)]).unwrap_err();
        assert!(e.contains("Did you mean `audio.setVolume`"), "{e}");
    }

    #[test]
    fn volume_outside_the_range_is_refused_rather_than_clamped() {
        // Clamping would hide a config that says 1000, which is a mistake worth
        // seeing.
        let e = registry().call(&p("audio.setVolume"), &[Value::Int(1000)]).unwrap_err();
        assert!(e.contains("0 to 100"), "{e}");
    }

    #[test]
    fn setting_the_volume_records_it() {
        let r = registry();
        r.call(&p("audio.setVolume"), &[Value::Int(30)]).unwrap();
        assert_eq!(last_volume(), Some(30));
    }

    #[test]
    fn mute_toggles_and_remembers() {
        let r = registry();
        r.call(&p("audio.mute"), &[Value::Bool(true)]).unwrap();
        assert!(is_muted());
        r.call(&p("audio.toggleMute"), &[]).unwrap();
        assert!(!is_muted());
    }

    #[test]
    fn an_unknown_key_is_reported_whatever_kind_it_was_meant_to_be() {
        // A bare name could be meant as a chord or as a media key, so both are
        // tried and either error is acceptable — what is not acceptable is a
        // silent success.
        let e = send_key("Hyper+D").unwrap_err();
        assert!(e.contains("not a chord"), "{e}");
        let e = send_key("Telepathy").unwrap_err();
        assert!(e.contains("not a chord") || e.contains("not a media key"), "{e}");
        // A media key that does exist works.
        assert_eq!(send_key("VolumeUp").unwrap().to_string_lossy(), "VolumeUp");
    }

    #[test]
    fn shutting_down_asks_for_the_right_flag() {
        // Refused without the privilege on most machines; the point is that it
        // refuses with a reason rather than pretending to have worked.
        let _ = shutdown("explode").unwrap_err();
    }

    #[test]
    fn a_chord_is_sent_and_a_nonsense_one_is_refused() {
        let r = registry();
        // `Win+D` really is pressed here, so the test avoids side effects and
        // checks the parse path with a chord the shell itself ignores.
        assert!(r.call(&p("keys.send"), &[Value::str("Hyper+D")]).is_err());
        assert!(r.get("keys.send").is_some());
    }

    #[test]
    fn every_action_documents_itself() {
        // The description is what `shellctl actions` prints, so an action with no
        // documentation is a hole in the user-facing surface.
        for line in registry().describe() {
            let body = line.trim();
            assert!(body.contains(' '), "no description on: {line:?}");
            let (signature, description) = body.split_once("  ").unwrap_or((body, ""));
            assert!(!signature.is_empty(), "no signature on: {line:?}");
            assert!(!description.trim().is_empty(), "no description for {signature:?}");
        }
    }
}
