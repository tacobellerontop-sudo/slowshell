//! rustWM, the window manager, as seen from the shell.
//!
//! rustWM takes one line of JSON per command on `\\.\pipe\rustwm`, the same
//! `{"cmd": "workspace 3"}` that `rwm workspace 3` and Kerys send. Writing to
//! the pipe directly means a bar's workspace buttons work without `rwm.exe`
//! on `PATH` and without a console window flashing up for every click.

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};

/// rustWM's control pipe.
pub const PIPE: &str = r"\\.\pipe\rustwm";

/// Whether rustWM is running, i.e. its pipe exists.
///
/// `GetFileAttributes` on a pipe name answers from the pipe namespace without
/// connecting, so this takes none of rustWM's pipe instances.
pub fn is_running() -> bool {
    std::path::Path::new(PIPE).exists()
}

/// Send one command to rustWM, such as `workspace 3` or `focus left`.
///
/// The write happens here, so "rustWM is not running" comes back as an error to
/// the caller. The reply is read on a thread of its own: it only goes to the
/// log, and a window manager that is slow to answer must not stall the frame.
pub fn send(cmd: &str) -> Result<(), String> {
    let mut pipe = OpenOptions::new()
        .read(true)
        .write(true)
        .open(PIPE)
        .map_err(|e| format!("rustWM is not reachable on {PIPE}: {e}. Is rustWM running?"))?;
    let mut line = serde_json::to_vec(&serde_json::json!({ "cmd": cmd }))
        .map_err(|e| e.to_string())?;
    line.push(b'\n');
    pipe.write_all(&line).and_then(|_| pipe.flush()).map_err(|e| e.to_string())?;
    let cmd = cmd.to_string();
    std::thread::spawn(move || {
        let mut reply = String::new();
        if BufReader::new(pipe).read_line(&mut reply).is_ok() && !reply.trim().is_empty() {
            slowshell_core::debug!("rustWM `{cmd}` -> {}", reply.trim());
        }
    });
    Ok(())
}

/// The workspace a `workspace N` command switches to, if that is what it is.
pub fn workspace_of(cmd: &str) -> Option<&str> {
    let rest = cmd.trim().strip_prefix("workspace ")?.trim();
    (!rest.is_empty() && !rest.contains(' ')).then_some(rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_workspace_command_names_its_workspace() {
        assert_eq!(workspace_of("workspace 3"), Some("3"));
        assert_eq!(workspace_of("  workspace web "), Some("web"));
        assert_eq!(workspace_of("move-to-workspace 3"), None);
        assert_eq!(workspace_of("focus left"), None);
        assert_eq!(workspace_of("workspace"), None);
    }
}
