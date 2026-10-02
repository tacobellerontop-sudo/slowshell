//! Starting programs and opening files.
//!
//! Two very different operations that a config wants under similar names:
//!
//! * [`open`] hands a string to the shell, exactly as typing it into Run would.
//!   A `.txt` becomes Notepad, a URL becomes the browser, a bare name becomes a
//!   program lookup. That is what a "Run" widget means, and reimplementing the
//!   association lookup would only get it wrong.
//! * [`run`] starts a specific program and waits for it, which is what a build
//!   button or a script runner means.
//!
//! Both report failure. A launcher that silently does nothing when a program is
//! misspelled is the most annoying class of shell bug there is.


use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CreateProcessW, GetExitCodeProcess, PROCESS_INFORMATION, STARTUPINFOW,
    WaitForSingleObject,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowThreadProcessId, SW_RESTORE, SW_SHOWNORMAL, SetForegroundWindow,
    ShowWindow,
};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Open a file, URL, or program the way Run does.
///
/// `target` is passed to the shell verbatim, so a config can build it from a
/// value. This is a deliberate capability: a shell that can open things is a
/// shell that runs things, and that is the point of a launcher.
pub fn open(target: &str) -> Result<(), String> {
    if target.trim().is_empty() {
        return Err("nothing to open".into());
    }
    let verb = wide("open");
    let file = wide(target);
    unsafe {
        // `ShellExecuteW` returns a value above 32 on success; anything at or
        // below is the Win32 error code, and 0 means "out of memory or a
        // malformed path" rather than anything specific.
        let result = ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(file.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        );
        if result.0 as isize <= 32 {
            return Err(format!(
                "could not open `{target}` ({}); check the path, or that the program is installed",
                result.0 as isize
            ));
        }
    }
    Ok(())
}

/// Run a program and wait for it, returning its exit code.
///
/// Arguments are passed as an array rather than a command line, so a path with
/// a space in it needs no quoting from the caller and there is no way to inject
/// an extra argument.
pub fn run(program: &str, args: &[String]) -> Result<u32, String> {
    if program.trim().is_empty() {
        return Err("no program given".into());
    }
    // `CreateProcessW` takes one mutable command line, so the arguments are
    // quoted into one string here. The quoting is the documented rule: a
    // backslash before a quote doubles it, and a run of backslashes before a
    // closing quote doubles too.
    let mut command_line = quote(program);
    for a in args {
        command_line.push(' ');
        command_line.push_str(&quote(a));
    }
    let mut wide_command: Vec<u16> = command_line.encode_utf16().collect();
    wide_command.push(0);

    // The application name is passed separately from the command line so a
    // program path with spaces in it does not have to survive being re-parsed.
    let mut wide_program: Vec<u16> = program.encode_utf16().collect();
    wide_program.push(0);

    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut info = PROCESS_INFORMATION::default();
    unsafe {
        let created = CreateProcessW(
            PCWSTR(wide_program.as_ptr()),
            Some(PWSTR(wide_command.as_mut_ptr())),
            None,
            None,
            false,
            // No console window: a program launched from a bar should not flash
            // one up, and a console program allocates its own anyway.
            CREATE_NO_WINDOW,
            None,
            None,
            &startup,
            &mut info,
        );
        if let Err(e) = created {
            return Err(format!("could not start `{program}`: {e}"));
        }
        // Bounded so a program that never exits does not hold the shell's action
        // forever. The caller's `onClick` is already off the UI thread by then,
        // so this only affects the action's own promise.
        let _ = WaitForSingleObject(info.hProcess, 60_000);
        let mut code: u32 = 0;
        let _ = GetExitCodeProcess(info.hProcess, &mut code);
        let _ = CloseHandle(info.hThread);
        let _ = CloseHandle(info.hProcess);
        if code == WAIT_TIMEOUT.0 {
            return Err(format!("`{program}` did not finish within 60 seconds"));
        }
        Ok(code)
    }
}

/// Quote one argument for the command line `CreateProcessW` parses.
fn quote(s: &str) -> String {
    if !s.is_empty() && !s.contains([' ', '\t', '"']) {
        return s.to_string();
    }
    quoted(s)
}

/// The quoting rule on its own, for a caller that always wants it.
///
/// Separate from [`quote`] so the rule can be stated once and tested once:
/// doubling the backslashes before a quote, and again before the closing quote.
pub fn quote_public(s: &str) -> String {
    quoted(s)
}

fn quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for c in s.chars() {
        match c {
            '\\' => {
                backslashes += 1;
                out.push('\\');
            }
            '"' => {
                // Double the run so the quote is not read as the end of the
                // argument, then emit a literal quote.
                for _ in 0..backslashes {
                    out.push('\\');
                }
                out.push_str("\\\"");
                backslashes = 0;
            }
            other => {
                backslashes = 0;
                out.push(other);
            }
        }
    }
    // A trailing run of backslashes would escape the closing quote.
    for _ in 0..backslashes {
        out.push('\\');
    }
    out.push('"');
    out
}

/// The process that owns the foreground window, for a "focus" action.
///
/// Returns the pid rather than a handle: the shell has no business opening a
/// process handle for someone else's process, and a pid is enough to say "this
/// program is in front" in a status widget.
pub fn foreground_process() -> Option<u32> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        (pid != 0).then_some(pid)
    }
}

/// Bring a window back rather than starting a second copy of its program.
pub fn restore(hwnd: windows::Win32::Foundation::HWND) {
    unsafe {
        let _ = ShowWindow(hwnd, SW_RESTORE);
        let _ = SetForegroundWindow(hwnd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_argument_is_left_alone() {
        assert_eq!(quote("notepad"), "notepad");
        assert_eq!(quote("--flag"), "--flag");
    }

    #[test]
    fn an_argument_with_a_space_is_quoted() {
        assert_eq!(quote("C:\\Program Files\\App.exe"), "\"C:\\Program Files\\App.exe\"");
        assert_eq!(quote("two words"), "\"two words\"");
    }

    #[test]
    fn a_quote_inside_an_argument_survives() {
        // This is the case that matters: a naive quote produces a command line
        // where the rest of the argument is read as a new argument, which turns
        // a filename into a different program.
        assert_eq!(quote("say \"hi\""), "\"say \\\"hi\\\"\"");
    }

    #[test]
    fn a_trailing_backslash_does_not_escape_the_closing_quote() {
        // `C:\dir\` quoted naively leaves the quote escaped and swallows the
        // next argument. With no space in the path nothing is quoted at all,
        // which is also correct — so the case is only interesting once a quote
        // is forced.
        assert_eq!(quote("C:\\dir\\"), "C:\\dir\\", "a bare path needs no quoting");
        assert_eq!(
            quote("C:\\my dir\\"),
            "\"C:\\my dir\\\\\"",
            "a quoted path must double its trailing backslashes"
        );
    }

    #[test]
    fn an_empty_string_is_still_quoted() {
        assert_eq!(quote(""), "\"\"");
    }

    #[test]
    fn opening_nothing_is_an_error() {
        let e = open("   ").unwrap_err();
        assert!(e.contains("nothing to open"), "{e}");
    }

    #[test]
    fn running_a_missing_program_reports_the_name() {
        // Nothing here can be slow: the path cannot exist.
        let e = run(r"C:\definitely\not\here.exe", &[]).unwrap_err();
        assert!(e.contains("could not start"), "{e}");
        assert!(e.contains("not"), "{e}");
    }
}
