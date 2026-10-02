//! Can a plain desktop process actually read these two things?
//!
//! ```text
//! cargo run -p slowshell-win --example capability_probe
//! ```
//!
//! Two features were asked for that the config language has no data source for:
//! virtual desktop ("workspace") numbers, and the currently-playing track. Both
//! have Windows APIs. Neither is obviously reachable from an unpackaged desktop
//! process, and the difference matters:
//!
//! - Virtual desktops: `IVirtualDesktopManager` is documented and can only *move*
//!   a window. Enumerating and switching desktops live on
//!   `IVirtualDesktopManagerInternal`, which is undocumented but has been stable
//!   since Windows 10 1607.
//! - Now playing: SMTC's **global** session manager. The per-app
//!   `SystemMediaTransportControls.GetForCurrentView` definitely needs a package
//!   identity; the global one may not. That is the question.
//!
//! Rather than reason about it, this asks the OS and prints what came back. A
//! shell that faked either of these would show a permanent placeholder or an
//! invented track name, which is worse than an honest gap.
//!
//! The async calls are *polled* rather than awaited, deliberately. A shell has no
//! async runtime, and polling `Status()` until it reads `Completed` is all a
//! background thread ever needs.

use std::time::{Duration, Instant};

use windows::core::{Interface, GUID};

/// Poll a WinRT async operation to completion, or give up.
///
/// A macro rather than a function: `IAsyncOperation<T>::GetResults` needs
/// `T: RuntimeType`, and that trait lives in a private module of `windows-core`,
/// so a generic helper cannot name its own bound. Expanding inline sidesteps it.
macro_rules! block_on {
    ($op:expr) => {{
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            // AsyncStatus::Completed is 1. Comparing the raw value rather than a
            // named variant, because the enum is an i32 newtype with no derives.
            if $op.Status()?.0 == 1 {
                break $op.GetResults()?;
            }
            if Instant::now() > deadline {
                return Err(windows::core::Error::from_hresult(
                    windows::Win32::Foundation::E_FAIL,
                ));
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }};
}

// ---------------------------------------------------------------------------
// Virtual desktops
// ---------------------------------------------------------------------------

/// `IVirtualDesktopManagerInternal`, declared by hand because the `windows` crate
/// ships only the documented three-method version.
///
/// The vtable is the documented interface's three methods followed by
/// `GetDesktopIds`, `GetCurrentDesktopId` and `SwitchToDesktop`. The order is
/// load-bearing: a COM vtable is positional, so putting these anywhere else makes
/// the first call do something else entirely, with no error to tell you.
#[repr(C)]
#[allow(non_snake_case)]
struct VdmInternalVtbl {
    base__: windows::core::IUnknown_Vtbl,
    IsWindowOnCurrentVirtualDesktop: unsafe extern "system" fn(
        *mut std::ffi::c_void,
        windows::Win32::Foundation::HWND,
        *mut windows::core::BOOL,
    ) -> windows::core::HRESULT,
    GetWindowDesktopId: unsafe extern "system" fn(
        *mut std::ffi::c_void,
        windows::Win32::Foundation::HWND,
        *mut GUID,
    ) -> windows::core::HRESULT,
    MoveWindowToDesktop: unsafe extern "system" fn(
        *mut std::ffi::c_void,
        windows::Win32::Foundation::HWND,
        *const GUID,
    ) -> windows::core::HRESULT,
    GetDesktopIds: unsafe extern "system" fn(*mut std::ffi::c_void) -> *mut GUID,
    GetCurrentDesktopId: unsafe extern "system" fn(*mut std::ffi::c_void) -> GUID,
    SwitchToDesktop: unsafe extern "system" fn(*mut std::ffi::c_void, *const GUID) -> windows::core::HRESULT,
}

#[repr(transparent)]
#[derive(Clone)]
struct VdmInternal(windows::core::IUnknown);
unsafe impl Interface for VdmInternal {
    type Vtable = VdmInternalVtbl;
    const IID: GUID = GUID::from_u128(0x3e08568a_4b24_4f2b_9f57_2b64a4b2ef29);
}

impl VdmInternal {
    /// The interface pointer. `repr(transparent)` over a single `IUnknown`, so
    /// the reference *is* the COM pointer — no need to reach into `IUnknown`,
    /// whose field is private.
    unsafe fn this(&self) -> *mut std::ffi::c_void {
        windows::core::Interface::as_raw(&self.0)
    }

    unsafe fn vtable(&self) -> &'static VdmInternalVtbl {
        let p = self.this();
        let vtbl = unsafe { *(p as *const *const VdmInternalVtbl) };
        unsafe { &*vtbl }
    }

    unsafe fn current_desktop_id(&self) -> GUID {
        unsafe { (self.vtable().GetCurrentDesktopId)(self.this()) }
    }

    /// The desktop ids, as the SAFEARRAY `GetDesktopIds` returns.
    ///
    /// SAFEARRAY of GUID lays out as one GUID per element here; the pointer is
    /// freed with `CoTaskMemFree`, which is what the shell runtime allocates it
    /// with. Not a reference, and not a copy of the array — the values are read
    /// out and the memory handed straight back.
    unsafe fn desktop_ids(&self) -> windows::core::Result<Vec<GUID>> {
        unsafe {
            let p = (self.vtable().GetDesktopIds)(self.this());
            if p.is_null() {
                return Err(windows::core::Error::from_hresult(
                    windows::Win32::Foundation::E_FAIL,
                ));
            }
            let each = std::mem::size_of::<GUID>();
            let mut out = Vec::new();
            let mut q = p;
            for _ in 0..64 {
                let id = *q.cast::<GUID>();
                if id.to_u128() == 0 {
                    break;
                }
                out.push(id);
                q = q.add(each);
            }
            windows::Win32::System::Com::CoTaskMemFree(Some(p.cast()));
            Ok(out)
        }
    }
}

fn probe_desktops() {
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::{IVirtualDesktopManager, VirtualDesktopManager as Clsid};
    use windows::Win32::UI::WindowsAndMessaging::GetDesktopWindow;

    // The SDK's own constant. Hardcoding the CLSID is how you get
    // REGDB_E_CLASSNOTREG and conclude the feature is missing on Windows.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        match CoCreateInstance::<_, IVirtualDesktopManager>(&Clsid, None, CLSCTX_ALL) {
            Ok(mgr) => {
                println!("  IVirtualDesktopManager         documented, resolved");
                // The desktop window lives on the current desktop, so its id is
                // the current desktop's - the one thing the documented interface
                // can tell us, given a window.
                match mgr.GetWindowDesktopId(GetDesktopWindow()) {
                    Ok(id) => println!("    current desktop id   {id:?}"),
                    Err(e) => println!("    GetWindowDesktopId   FAILED: {e}"),
                }
            }
            Err(e) => println!("  IVirtualDesktopManager         FAILED: {e}"),
        }

        match CoCreateInstance::<_, VdmInternal>(&Clsid, None, CLSCTX_ALL) {
            Ok(mgr) => {
                println!("  IVirtualDesktopManagerInternal undocumented, resolved");
                println!("    GetCurrentDesktopId  {:?}", mgr.current_desktop_id());
                match mgr.desktop_ids() {
                    Ok(ids) if !ids.is_empty() => {
                        println!("    GetDesktopIds        {} desktop(s)", ids.len());
                        for (i, id) in ids.iter().enumerate().take(8) {
                            println!("      [{i}] {id:?}");
                        }
                    }
                    Ok(_) => println!("    GetDesktopIds        returned none"),
                    Err(e) => println!("    GetDesktopIds        FAILED: {e}"),
                }
            }
            Err(e) => println!("  IVirtualDesktopManagerInternal FAILED: {e}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Now playing
// ---------------------------------------------------------------------------

fn probe_media() -> windows::core::Result<()> {
    use windows::Media::Control::GlobalSystemMediaTransportControlsSessionManager;

    // `block_on!` unwraps with `?`, so it yields the value directly and lets an
    // error out of here. A failure is information, though, so each call that can
    // fail is matched rather than propagated.
    let op = match GlobalSystemMediaTransportControlsSessionManager::RequestAsync() {
        Ok(op) => op,
        Err(e) => {
            println!("  SMTC global manager              RequestAsync FAILED: {e}");
            return Ok(());
        }
    };
    let mgr = block_on!(op);
    println!("  SMTC global manager              RequestAsync OK");

    match mgr.GetCurrentSession() {
        Ok(session) => {
            println!("    current session        present");
            match session.TryGetMediaPropertiesAsync() {
                Ok(props_op) => {
                    let props = block_on!(props_op);
                    let title = props.Title().map(|t| t.to_string()).unwrap_or_default();
                    let artist = props.Artist().map(|a| a.to_string()).unwrap_or_default();
                    println!("    title                  {title:?}");
                    println!("    artist                 {artist:?}");
                }
                Err(e) => println!("    TryGetMediaProperties  FAILED: {e}"),
            }
            match session.GetPlaybackInfo() {
                Ok(info) => println!("    status                 {:?}", info.PlaybackStatus()),
                Err(e) => println!("    GetPlaybackInfo        FAILED: {e}"),
            }
            if let Ok(app) = session.SourceAppUserModelId() {
                println!("    source app             {app}");
            }
        }
        Err(e) => println!("    GetCurrentSession      no current session ({e})"),
    }

    match mgr.GetSessions() {
        Ok(sessions) => {
            let n = sessions.Size().unwrap_or(0);
            println!("    sessions               {n}");
            for i in 0..n.min(5) {
                if let Ok(s) = sessions.GetAt(i) {
                    let app = s
                        .SourceAppUserModelId()
                        .map(|x| x.to_string())
                        .unwrap_or_else(|_| "?".into());
                    println!("      [{i}] {app}");
                }
            }
        }
        Err(e) => println!("    GetSessions            FAILED: {e}"),
    }
    Ok(())
}

fn main() {
    println!("virtual desktops");
    probe_desktops();
    println!();
    println!("now playing");
    if let Err(e) = probe_media() {
        println!("  probe aborted: {e}");
    }
}
