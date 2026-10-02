//! The named-pipe transport for [`slowshell_core::ipc`].
//!
//! # Why a named pipe
//!
//! `shellctl` has to reach a running shell, which may be in a different session
//! but is always the same user. A named pipe is the only Windows IPC primitive
//! that is:
//!
//! * **per-user without a server.** A TCP socket would need a port, and a port is
//!   a thing anything on the machine can connect to. A pipe's name lives in the
//!   kernel object namespace, so the same user reaches it and nobody else does.
//! * **kernel-scheduled.** `ConnectNamedPipe` blocks in the kernel, so the
//!   server costs nothing while no client is connected. A shell polling a socket
//!   would spend the idle budget it just saved.
//! * **already there.** No extra runtime, no COM activation, no service.
//!
//! # Why the server runs on its own thread
//!
//! `ConnectNamedPipe` blocks until a client arrives. Calling it on the shell's UI
//! thread would freeze the bar for as long as nobody runs `shellctl`, which is
//! essentially always. So one thread accepts connections and hands requests over
//! a channel; the frame loop drains that channel once per turn, in the same place
//! it already drains window messages.
//!
//! # The pipe name carries the user and the process
//!
//! Two users on one machine, or two shells in one session, must not fight over
//! one name. Without a per-process suffix a second `Shell.exe` would silently
//! fail to bind and its `shellctl` would talk to the first one instead — a
//! failure with no symptom except a reload that reloads the wrong desktop.

use std::io::{BufRead, BufReader, Read, Write};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED,
    HANDLE, HWND, LPARAM, STILL_ACTIVE, WPARAM,
};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_SHARE_MODE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, SetNamedPipeHandleState,
    WaitNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};

use slowshell_core::ipc::{Request, Response, call};

const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const SHARE_BOTH: FILE_SHARE_MODE = FILE_SHARE_MODE(0x0000_0003);

/// The pipe this shell listens on.
///
/// The user name is sanitised because a pipe name is a flat namespace: a
/// backslash inside it would be read as a path separator by anything walking the
/// object namespace.
pub fn pipe_name() -> String {
    let raw = std::env::var("USERNAME").unwrap_or_else(|_| "user".into());
    let user: String = raw
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    format!(r"\\.\pipe\slowshell-{}-{}", user.to_ascii_lowercase(), std::process::id())
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Where a running shell can be reached.
///
/// # Why a file and not just a name
///
/// The pipe's name has to carry this shell's pid, so two shells on one desktop
/// do not end up sharing one pipe and stealing each other's `shellctl`
/// requests. But a `shellctl` does not know the shell's pid, and asking the user
/// to pass it would be worse than the problem. So the shell writes down where it
/// is, and the CLI reads it.
///
/// That also solves the crash case for free: a file left behind by a shell that
/// was killed names a pid that is no longer running, which is exactly the check
/// needed to say "no slowshell is running" rather than hanging on a pipe that
/// will never answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    /// The full pipe path, e.g. `\\.\pipe\slowshell-user-1234`.
    pub pipe: String,
    /// The process that owns it, used to detect a stale file.
    pub pid: u32,
}

impl Endpoint {
    fn path() -> std::path::PathBuf {
        slowshell_core::paths::state_dir().join("control.json")
    }

    /// Write down where this shell is listening.
    pub fn publish(&self) -> std::io::Result<()> {
        if let Some(dir) = Self::path().parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_string(self).map_err(std::io::Error::other)?;
        std::fs::write(Self::path(), text)
    }

    /// Find a running shell, or `None`.
    pub fn discover() -> Option<Endpoint> {
        let text = std::fs::read_to_string(Self::path()).ok()?;
        let endpoint: Endpoint = serde_json::from_str(&text).ok()?;
        // A file that outlives its shell is the normal case after a crash, so
        // liveness is checked rather than trusted.
        endpoint.alive().then_some(endpoint)
    }

    /// Whether the process that wrote this file is still running.
    fn alive(&self) -> bool {
        if self.pid == std::process::id() {
            return true;
        }
        unsafe {
            let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, self.pid) else {
                return false;
            };
            let mut code: u32 = 0;
            let ok = GetExitCodeProcess(handle, &mut code).is_ok();
            let _ = CloseHandle(handle);
            ok && code == STILL_ACTIVE.0 as u32
        }
    }

    /// Remove the file, if it is still ours.
    pub fn retract(&self) {
        if Self::discover().map(|e| e.pid) == Some(self.pid) {
            let _ = std::fs::remove_file(Self::path());
        }
    }
}

/// One end of a connected pipe.
///
/// `Copy` because a byte-mode pipe is a single duplex handle, and the protocol
/// needs a `BufReader` over one copy and a plain writer over the other.
#[derive(Clone, Copy)]
struct Pipe(HANDLE);

impl Pipe {
    fn is_valid(&self) -> bool {
        !self.0.is_invalid()
    }
}

impl Read for Pipe {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut read: u32 = 0;
        unsafe { ReadFile(self.0, Some(buf), Some(&mut read), None) }
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(read as usize)
    }
}

impl Write for Pipe {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut written: u32 = 0;
        unsafe { WriteFile(self.0, Some(buf), Some(&mut written), None) }
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(written as usize)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        // A pipe has no user-space buffer; the write above already reached the
        // kernel. Reporting success is the honest answer.
        Ok(())
    }
}

/// Send one request to a running shell and wait for its answer.
///
/// The pipe is found by reading the endpoint the shell published, and each
/// instance of the name is tried in turn, because a shell that was killed can
/// leave an instance whose `ConnectNamedPipe` nobody will ever answer.
pub fn request(req: &Request, timeout_ms: u32) -> std::result::Result<Response, String> {
    let Some(endpoint) = Endpoint::discover() else {
        return Err("no slowshell is running; start Shell.exe first".into());
    };
    let name = wide(&endpoint.pipe);
    let mut last: Option<String> = None;
    // A client connects in under a millisecond when a shell is listening. The
    // bound here is only reached when a stale instance is in the way, and a CLI
    // that hangs looks exactly like a shell that has frozen.
    for _ in 0..8 {
        let handle = unsafe {
            CreateFileW(
                windows::core::PCWSTR(name.as_ptr()),
                GENERIC_READ | GENERIC_WRITE,
                SHARE_BOTH,
                None,
                OPEN_EXISTING,
                Default::default(),
                None,
            )
        };
        let handle = match handle {
            Ok(h) => h,
            Err(e) if e.code() == ERROR_PIPE_BUSY.into() => {
                // Every instance is serving someone; wait for one to free up.
                let _ = unsafe { WaitNamedPipeW(windows::core::PCWSTR(name.as_ptr()), timeout_ms) };
                continue;
            }
            Err(e) if e.code() == ERROR_FILE_NOT_FOUND.into() => {
                return Err("the shell that published this pipe has gone".into());
            }
            Err(e) => {
                last = Some(e.to_string());
                continue;
            }
        };
        // The server created the pipe for duplex access, but a client must still
        // ask for byte mode explicitly or reads arrive as whole messages and a
        // short request never completes.
        let _ = unsafe { SetNamedPipeHandleState(handle, Some(&PIPE_READMODE_BYTE), None, None) };
        let pipe = Pipe(handle);
        let mut reader = BufReader::new(pipe);
        let mut writer = pipe;
        match call(&mut reader, &mut writer, req) {
            Ok(r) => {
                let _ = unsafe { CloseHandle(handle) };
                return Ok(r);
            }
            Err(e) => {
                last = Some(e);
                let _ = unsafe { CloseHandle(handle) };
                // A server that was torn down mid-exchange is a race, not a
                // failure: the next attempt finds the replacement instance.
                let _ = unsafe { WaitNamedPipeW(windows::core::PCWSTR(name.as_ptr()), timeout_ms) };
            }
        }
    }
    Err(match last {
        Some(e) => format!("could not reach the shell: {e}"),
        None => "could not reach the shell".into(),
    })
}

/// A request the shell still owes an answer to.
pub struct Pending {
    pub request: Option<Request>,
    reply: Sender<Response>,
}

impl Pending {
    /// Take the request, leaving the reply channel in place.
    ///
    /// `Option` because the frame loop has to keep the `Pending` to answer it,
    /// and a request is not `Copy`. Two steps is a small price for a type that
    /// cannot be half-consumed by mistake.
    pub fn take(&mut self) -> Request {
        self.request
            .take()
            .expect("a request is only taken once; the frame loop sends one answer")
    }

    /// Send the answer. Called exactly once, by the frame loop.
    pub fn answer(self, response: Response) {
        let _ = self.reply.send(response);
    }
}

/// Accepts connections and hands them to the frame loop.
pub struct Server {
    incoming: Receiver<Pending>,
    endpoint: Endpoint,
    /// Windows the shell has open, so a request can be announced.
    ///
    /// The frame loop blocks in the kernel until a message arrives, which is
    /// what makes idle cost zero rather than merely small. That only works if
    /// something *posts* a message when a client connects — otherwise a
    /// `shellctl` would wait for the next clock tick, and on a config with no
    /// `Clock` in it that is never.
    waker: Arc<std::sync::Mutex<Vec<isize>>>,
}

impl Server {
    /// Start listening and publish where.
    ///
    /// Publishing is what makes the pipe findable, and it happens before this
    /// returns, so a `shellctl` run immediately after the shell appears finds
    /// it. A failure to publish is *not* fatal: the shell is still perfectly
    /// usable, it just cannot be controlled, and saying so beats exiting.
    pub fn start() -> std::io::Result<Server> {
        let endpoint = Endpoint { pipe: pipe_name(), pid: std::process::id() };
        let (tx, rx) = channel::<Pending>();
        let waker = Arc::new(std::sync::Mutex::new(Vec::new()));
        let thread_endpoint = endpoint.clone();
        let thread_waker = waker.clone();
        std::thread::Builder::new()
            .name("slowshell-ipc".into())
            .spawn(move || accept_loop(&thread_endpoint.pipe, tx, thread_waker))
            .map_err(|e| std::io::Error::other(format!("could not start the IPC thread: {e}")))?;
        if let Err(e) = endpoint.publish() {
            slowshell_core::warn!("shellctl will not find this shell: {e}");
        }
        Ok(Server { incoming: rx, endpoint, waker })
    }

    /// Tell the accept thread which windows exist, so it can wake the frame loop.
    ///
    /// Called as surfaces come and go. An empty list means "no window to post
    /// to", and the request simply waits for the next loop turn — which is
    /// correct, because with no windows there is no loop to be prompt for.
    pub fn set_windows(&self, hwnds: &[isize]) {
        if let Ok(mut w) = self.waker.lock() {
            *w = hwnds.to_vec();
        }
    }

    /// One waiting request, or `None` if no client is waiting.
    ///
    /// Non-blocking on purpose: the frame loop calls this every turn, and
    /// blocking here would reintroduce exactly the freeze the thread exists to
    /// avoid.
    pub fn try_recv(&self) -> Option<Pending> {
        self.incoming.try_recv().ok()
    }

    /// Where this server is listening, for logs and `shellctl doctor`.
    pub fn name(&self) -> String {
        self.endpoint.pipe.clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // The file outliving the shell would make `shellctl` try to talk to a
        // pipe nobody is serving. Retracting it here is the difference between
        // "no slowshell is running" and a two-second hang on every command.
        self.endpoint.retract();
    }
}

fn accept_loop(name: &str, tx: Sender<Pending>, waker: Arc<std::sync::Mutex<Vec<isize>>>) {
    let wide = wide(name);
    // The next instance is bound *before* the current one is torn down, so the
    // name always has a live instance behind it. Creating it afterwards leaves a
    // window in which a client's `CreateFileW` succeeds and its read then fails
    // with `ERROR_BROKEN_PIPE`, which reads as "the shell crashed" when it did
    // not.
    let mut handle = match create_instance(&wide) {
        Some(h) => h,
        None => return,
    };
    loop {
        // A client that connected between `CreateNamedPipeW` and here is
        // already waiting, and `ConnectNamedPipe` reports that instead of
        // blocking. Skipping this case deadlocks against the first real client.
        if let Err(e) = unsafe { ConnectNamedPipe(handle, None) } {
            if e.code() != ERROR_PIPE_CONNECTED.into() {
                let _ = unsafe { CloseHandle(handle) };
                if e.code() != ERROR_BROKEN_PIPE.into() {
                    // A real failure: back off a little rather than spinning.
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                match create_instance(&wide) {
                    Some(h) => handle = h,
                    None => return,
                }
                continue;
            }
        }

        let pipe = Pipe(handle);
        if pipe.is_valid() {
            let mut reader = BufReader::new(pipe);
            let mut writer = pipe;
            let keep_going = serve_one(&mut reader, &mut writer, &tx, &waker);
            match create_instance(&wide) {
                Some(next) => {
                    let _ = unsafe { DisconnectNamedPipe(handle) };
                    let _ = unsafe { CloseHandle(handle) };
                    handle = next;
                }
                None => {
                    let _ = unsafe { CloseHandle(handle) };
                    if !keep_going {
                        return;
                    }
                    // Out of instances; without a replacement the shell loses
                    // its control channel, which it says so about below.
                    slowshell_core::error!("could not create another pipe instance; shellctl will stop working");
                    return;
                }
            }
            if !keep_going {
                // The receiver is gone: the shell is shutting down.
                return;
            }
        } else {
            match create_instance(&wide) {
                Some(next) => handle = next,
                None => return,
            }
        }
    }
}

fn create_instance(wide: &[u16]) -> Option<HANDLE> {
    let handle = unsafe {
        CreateNamedPipeW(
            windows::core::PCWSTR(wide.as_ptr()),
            PIPE_ACCESS_DUPLEX,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            // Big enough that a `graph` reply needs one write, small enough that
            // a pipe reserves nothing while it is idle.
            64 * 1024,
            16 * 1024,
            0,
            None,
        )
    };
    if handle.is_invalid() {
        // Out of handles, or the name is taken by something that is not us. The
        // shell keeps running either way; it just loses its control channel.
        slowshell_core::error!("could not create the control pipe; shellctl will not work");
        return None;
    }
    Some(handle)
}

/// Read one request, hand it over, and wait for the frame loop's answer.
///
/// Returns `false` when the shell is gone, so the thread can exit.
fn serve_one(
    reader: &mut BufReader<Pipe>,
    writer: &mut Pipe,
    tx: &Sender<Pending>,
    waker: &Arc<std::sync::Mutex<Vec<isize>>>,
) -> bool {
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => return true,
        Ok(_) => {}
        Err(_) => return true,
    }
    let line = line.trim();
    if line.is_empty() {
        return true;
    }
    let request = match Request::parse(line) {
        Ok(r) => r,
        Err(message) => {
            // A malformed request gets an answer rather than silence: a CLI that
            // hangs looks exactly like a shell that has frozen.
            let _ = slowshell_core::ipc::write_line(writer, &Response::err(message));
            return true;
        }
    };
    let (reply_tx, reply_rx) = channel();
    if tx.send(Pending { request: Some(request), reply: reply_tx }).is_err() {
        return false;
    }
    // Announce the request before waiting for the answer. The frame loop is
    // about to block in `MsgWaitForMultipleObjectsEx` with a timeout as long as
    // the next clock tick, and on a config with no `Clock` that is never — so
    // without this a `shellctl` would hang until something else happened.
    wake_frame_loop(&waker);
    // The frame loop answers within one turn, which is bounded by its own wait.
    // The timeout is a backstop against a leaked sender, not a latency budget.
    match reply_rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(response) => {
            let _ = slowshell_core::ipc::write_line(writer, &response);
            true
        }
        Err(_) => {
            let _ = slowshell_core::ipc::write_line(
                writer,
                &Response::err("the shell did not answer; it may be shutting down"),
            );
            true
        }
    }
}

/// Post a wake-up to every window the shell has open.
///
/// A no-op when no window exists yet, which is correct: the frame loop will
/// notice the request on its next turn regardless.
fn wake_frame_loop(waker: &Arc<std::sync::Mutex<Vec<isize>>>) {
    let Ok(windows) = waker.lock() else { return };
    for &hwnd in windows.iter() {
        unsafe {
            let _ = PostMessageW(
                Some(HWND(hwnd as *mut std::ffi::c_void)),
                crate::window::WM_SLOWSHELL_WAKE,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pipe_name_is_flat_and_per_process() {
        let name = pipe_name();
        // The shape is exactly `\\.\pipe\` plus a sanitised suffix: any other
        // backslash would be read as a path separator by anything walking the
        // object namespace.
        let prefix = r"\\.\pipe\";
        assert!(name.starts_with(prefix), "{name}");
        let suffix = &name[prefix.len()..];
        assert!(!suffix.contains('\\'), "the suffix must be flat: {suffix}");
        assert!(!suffix.contains(' '), "{suffix}");
        assert!(suffix.contains(&std::process::id().to_string()), "{suffix}");
    }

    #[test]
    fn a_nonsense_user_name_cannot_escape_the_namespace() {
        // The sanitiser is what stops a crafted USERNAME from naming a pipe
        // outside the object namespace.
        let raw = r"ev\il\..\pipe";
        let cleaned: String = raw
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
            .collect();
        assert!(!cleaned.contains('\\'));
    }

    #[test]
    fn wide_terminates_but_does_not_double_terminate() {
        assert_eq!(wide("ab"), vec![97, 98, 0]);
    }

    #[test]
    fn connecting_to_nothing_says_so_plainly() {
        // No server in this test process, so this must fail with something a user
        // can act on rather than a raw Win32 code.
        let e = request(&Request::Status, 50).unwrap_err();
        assert!(
            e.contains("no slowshell is running") || e.contains("could not reach"),
            "{e}"
        );
    }

    #[test]
    fn an_endpoint_is_alive_when_it_names_this_process() {
        let e = Endpoint { pipe: pipe_name(), pid: std::process::id() };
        assert!(e.alive(), "an endpoint for this process must count as alive");
    }

    #[test]
    fn a_stale_endpoint_is_not_alive() {
        // Pid 0 is the system idle process and never "exits" in a way a query can
        // see, so the check has to be a real one: a pid that cannot be opened is
        // how a crashed shell's file is recognised.
        let e = Endpoint { pipe: pipe_name(), pid: 0x7fff_fffe };
        assert!(!e.alive(), "an unopenable pid must not be reported as running");
    }

    #[test]
    fn a_discovered_endpoint_names_a_live_shell_or_nothing() {
        // Either the shell is running and its endpoint round-trips, or there is
        // no shell and this is `None`. Never a stale one.
        if let Some(e) = Endpoint::discover() {
            assert!(e.alive());
            assert!(!e.pipe.is_empty());
        }
    }
}
