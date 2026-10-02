//! The clipboard.
//!
//! A shell without clipboard access cannot copy a value out of a widget, which
//! makes every read-only display useless the moment you want its contents. This
//! is a thin, honest wrapper over the Win32 clipboard: open, put or get, close,
//! and treat every failure as a plain error rather than a silent no-op.

use std::ffi::c_void;

use windows::Win32::Foundation::{HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::{CF_UNICODETEXT, OleInitialize, OleUninitialize};

/// Unicode text, the only format a shell needs.
const CF_TEXT: u32 = CF_UNICODETEXT.0 as u32;

thread_local! {
    /// `OleInitialize` must run on the thread that touches the clipboard, and
    /// the shell's UI thread is the only one that does. Calling it once per
    /// thread keeps the rule obvious without a global.
    static OLE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn with_ole<T>(f: impl FnOnce() -> T) -> T {
    OLE.with(|ready| {
        if !ready.get() {
            // S_FALSE and S_OK both mean "already initialised", which is fine.
            let _ = unsafe { OleInitialize(None) };
            ready.set(true);
        }
        f()
    })
}

/// Copy text to the clipboard, replacing what was there.
pub fn set(text: &str) -> Result<(), String> {
    with_ole(|| {
        open_clipboard().map_err(|_| who_else())?;
        // Emptied *after* the open and *before* the put, because `EmptyClipboard`
        // destroys whatever the previous owner had and fails if the clipboard is
        // not open. Doing it in the other order races with the owner.
        unsafe {
            let _ = EmptyClipboard();
        }
        let result = unsafe { put(text) };
        unsafe {
            let _ = CloseClipboard();
        }
        result
    })
}

/// Read text from the clipboard, or `None` if it holds something else.
///
/// A clipboard holding a bitmap is not an error: a config that asked for text
/// gets nothing, and the widget shows its placeholder.
pub fn get() -> Option<String> {
    with_ole(|| {
        open_clipboard().ok()?;
        let handle = unsafe { GetClipboardData(CF_TEXT) }.ok();
        let text = handle.and_then(|h| unsafe { read_global(h) });
        unsafe {
            let _ = CloseClipboard();
        }
        text
    })
}

/// Whether the clipboard currently holds text.
///
/// Cheap enough to call from a reactive expression, which is the point: a widget
/// can bind to it and repaint when the user copies something.
pub fn has_text() -> bool {
    get().is_some()
}

/// Open the clipboard, waiting briefly if something else has it.
///
/// Windows allows one process to hold the clipboard open, and plenty of things
/// do — a file manager copying a file, another thread in this shell. Giving up on
/// the first refusal turns a momentary overlap into a lost copy, so a short
/// bounded wait is the difference between "works almost always" and "works".
fn open_clipboard() -> Result<(), ()> {
    const TRIES: u32 = 20;
    for attempt in 0..TRIES {
        if unsafe { OpenClipboard(None) }.is_ok() {
            return Ok(());
        }
        if attempt + 1 < TRIES {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
    Err(())
}

unsafe fn put(text: &str) -> Result<(), String> {
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    wide.push(0);
    let bytes = wide.len() * 2;
    let handle = GlobalAlloc(GMEM_MOVEABLE, bytes).map_err(|e| format!("{e}"))?;
    let ptr = GlobalLock(handle) as *mut c_void;
    if ptr.is_null() {
        return Err("could not lock clipboard memory".into());
    }
    std::ptr::copy_nonoverlapping(wide.as_ptr() as *const u8, ptr as *mut u8, bytes);
    let _ = GlobalUnlock(handle);
    // On success the system owns the memory and frees it when the clipboard is
    // emptied. On failure the caller has to free it, so the error path does
    // rather than returning early and leaking.
    match SetClipboardData(CF_TEXT, Some(HANDLE(handle.0))) {
        Ok(_) => Ok(()),
        Err(e) => Err(format!("could not set the clipboard: {e}")),
    }
}

unsafe fn read_global(handle: HANDLE) -> Option<String> {
    let global = HGLOBAL(handle.0);
    let ptr = GlobalLock(global) as *const u16;
    if ptr.is_null() {
        return None;
    }
    // The clipboard holds a null-terminated string that may not be valid UTF-16
    // in the middle, so it is read to the terminator and lossy-decoded rather
    // than trusted as a slice.
    let mut len = 0usize;
    while *ptr.add(len) != 0 {
        len += 1;
        // A clipboard written by another process can be truncated mid-string.
        // Stopping here beats walking off the end of the allocation.
        if len > 1 << 20 {
            break;
        }
    }
    let slice = std::slice::from_raw_parts(ptr, len);
    let _ = GlobalUnlock(global);
    let text = String::from_utf16_lossy(slice);
    (!text.is_empty()).then_some(text)
}

fn who_else() -> String {
    // The common cause is another process holding the clipboard open, which
    // Windows allows for a short time. Saying so is more use than a code.
    "the clipboard is in use by another program; try again".into()
}

/// Release the thread's OLE state. Called once, at shutdown.
///
/// OLE keeps a clipboard-format viewer alive; without this a text editor that
/// stays open after the shell closes keeps polling a clipboard nobody will write
/// to again.
pub fn shutdown() {
    OLE.with(|ready| {
        if ready.replace(false) {
            unsafe {
                OleUninitialize();
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The clipboard is one machine-wide resource, and the test harness runs
    /// tests on many threads. Without this, two of these tests race and the
    /// loser reports a failure that has nothing to do with the clipboard.
    static CLIPBOARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Hold the clipboard for the duration of a test, skipping rather than
    /// failing if another program happens to be holding it.
    fn guard() -> Option<std::sync::MutexGuard<'static, ()>> {
        let g = CLIPBOARD.lock().ok()?;
        if set("slowshell clipboard test").is_err() {
            return None;
        }
        Some(g)
    }

    #[test]
    fn text_round_trips_including_awkward_characters() {
        let Some(_g) = guard() else { return };
        for text in ["hello", "line\nbreak", "\u{6f22}\u{5b57}", "emoji \u{1F980}"] {
            if set(text).is_ok() {
                // The empty string is indistinguishable from "no text", so it is
                // the one case deliberately not in the list.
                assert_eq!(get().as_deref(), Some(text), "round trip failed for {text:?}");
            }
        }
    }

    #[test]
    fn replacing_the_clipboard_replaces_the_text() {
        let Some(_g) = guard() else { return };
        set("first").unwrap();
        set("second").unwrap();
        assert_eq!(get().as_deref(), Some("second"), "the old text must be gone");
    }

    #[test]
    fn has_text_agrees_with_get() {
        let Some(_g) = guard() else { return };
        set("x").unwrap();
        assert_eq!(has_text(), get().is_some());
    }
}
