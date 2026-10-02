//! COM and DPI process initialisation.
//!
//! Must run before any window or Direct2D object is created, and before the
//! first window exists for DPI awareness to be honoured.

use windows::core::Result;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, SetThreadDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::SetProcessDPIAware;

/// Initialise the calling thread for COM and opt the process into per-monitor
/// DPI v2.
///
/// Returns whether COM initialised. A missing COM registration is not fatal: the
/// shell falls back to the WARP renderer and reports it through `shellctl doctor`
/// rather than refusing to start.
pub fn init_process() -> bool {
    let _dpi = init_dpi();
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok() }
}

fn init_dpi() -> bool {
    unsafe {
        // Per-monitor v2 gives correct scaling on mixed-DPI setups, which is the
        // common case for a desktop shell. It can be refused by a compatibility
        // shim, so fall back to the system DPI aware behaviour rather than
        // crashing.
        if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2).is_ok() {
            return true;
        }
        SetProcessDPIAware().as_bool()
    }
}

/// Opt a thread into per-monitor awareness. Threads created after the
/// process-wide setting may need this.
pub fn init_thread_dpi() {
    unsafe {
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// Initialise COM on a dedicated background thread used for out-of-process
/// system calls.
pub fn init_thread() -> Result<()> {
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.ok()
}

/// The current **local** wall-clock time, as `(year, month, day, hour, minute, second)`.
///
/// `GetLocalTime` is the authoritative local clock and already accounts for the
/// time zone and daylight saving, which is why the shell reads it directly rather
/// than applying a UTC offset to a UTC timestamp.
pub fn local_civil() -> (i64, u32, u32, u32, u32, u32) {
    unsafe {
        use windows::Win32::System::SystemInformation::GetLocalTime;
        let st = GetLocalTime();
        (
            st.wYear as i64,
            st.wMonth as u32,
            st.wDay as u32,
            st.wHour as u32,
            st.wMinute as u32,
            st.wSecond as u32,
        )
    }
}

/// The local UTC offset in seconds, positive east of Greenwich.
///
/// Only used for diagnostics; the shell itself formats from [`local_civil`].
pub fn utc_offset_seconds() -> i64 {
    unsafe {
        use windows::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
        let mut tz = TIME_ZONE_INFORMATION::default();
        // `TIME_ZONE_ID_INVALID` means the call failed, in which case UTC is the
        // correct fallback.
        if GetTimeZoneInformation(&mut tz) == u32::MAX {
            return 0;
        }
        // `Bias` is UTC - local in minutes, and daylight saving is folded in.
        -(tz.Bias as i64) * 60 - (tz.DaylightBias as i64) * 60
    }
}
