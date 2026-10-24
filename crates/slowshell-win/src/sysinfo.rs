//! Readings of the machine: battery, processor, memory, network and volume.
//!
//! # Why these are read on the clock's tick
//!
//! Each reading here is one non-blocking call into the kernel or an already
//! activated COM object: `GetSystemPowerStatus`, `GetSystemTimes`,
//! `GlobalMemoryStatusEx`, `GetNetworkConnectivityHint` and two vtable calls on
//! `IAudioEndpointVolume`. Together they cost microseconds, and the shell
//! already wakes once a second for the clock. Reading them on that wake adds
//! no wake-ups of its own, which is what the idle budget actually counts, and
//! `Reactor::set` drops every value that did not change, so a still machine
//! costs a handful of comparisons and no frames.
//!
//! The push-based alternatives (power broadcast messages, connectivity
//! callbacks, an endpoint volume callback) would each need a registration, a
//! callback thread or a hidden window, and a test of their own, to save those
//! same microseconds. They are the better design once a provider needs
//! sub-second latency; nothing here does.

use std::cell::RefCell;

use windows::Win32::Foundation::FILETIME;
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{eConsole, eRender, IMMDeviceEnumerator, MMDeviceEnumerator};
use windows::Win32::Networking::WinSock::{
    NetworkConnectivityLevelHintConstrainedInternetAccess, NetworkConnectivityLevelHintInternetAccess,
    NetworkConnectivityLevelHintLocalAccess, NL_NETWORK_CONNECTIVITY_HINT,
};
use windows::core::{s, w};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::GetSystemTimes;

/// The battery, as Windows reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Battery {
    /// Whether the machine has a battery at all. A desktop reads `false`.
    pub present: bool,
    /// 0 to 100. Zero when there is no battery or Windows does not know.
    pub percentage: i64,
    /// Charging right now. Plugged in and full is not charging.
    pub charging: bool,
    /// On mains power, whether or not the battery is charging.
    pub plugged: bool,
}

/// Read the battery.
pub fn battery() -> Battery {
    let mut s = SYSTEM_POWER_STATUS::default();
    // SAFETY: fills a caller-owned struct.
    if unsafe { GetSystemPowerStatus(&mut s) }.is_err() {
        return Battery { present: false, percentage: 0, charging: false, plugged: false };
    }
    battery_from(s.ACLineStatus, s.BatteryFlag, s.BatteryLifePercent)
}

/// The decoding of `SYSTEM_POWER_STATUS`, apart so it can be tested anywhere.
///
/// `BatteryFlag` is a bit set: 8 is charging, 128 is "no system battery", and
/// 255 alone is "unknown". `BatteryLifePercent` is 255 when unknown.
fn battery_from(ac_line: u8, flag: u8, percent: u8) -> Battery {
    const CHARGING: u8 = 8;
    const NO_BATTERY: u8 = 128;
    const UNKNOWN: u8 = 255;
    let present = flag != UNKNOWN && flag & NO_BATTERY == 0;
    Battery {
        present,
        percentage: if present && percent != UNKNOWN { percent.min(100) as i64 } else { 0 },
        charging: present && flag & CHARGING != 0,
        plugged: ac_line == 1,
    }
}

/// Processor load across every core, from the difference between two readings.
///
/// The first reading has nothing to compare against and reports `None`, rather
/// than a number that would be the average since boot.
#[derive(Debug, Default)]
pub struct CpuMeter {
    last: Option<(u64, u64)>,
}

impl CpuMeter {
    pub fn new() -> CpuMeter {
        CpuMeter::default()
    }

    /// Percent busy since the previous call, 0 to 100.
    pub fn sample(&mut self) -> Option<i64> {
        let (mut idle, mut kernel, mut user) =
            (FILETIME::default(), FILETIME::default(), FILETIME::default());
        // SAFETY: fills three caller-owned structs.
        unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)) }.ok()?;
        // Kernel time includes idle time, so the total is kernel plus user.
        let now = (ticks(idle), ticks(kernel) + ticks(user));
        let usage = self.last.and_then(|then| cpu_percent(then, now));
        self.last = Some(now);
        usage
    }
}

fn ticks(t: FILETIME) -> u64 {
    (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)
}

/// Busy percentage between two `(idle, total)` readings.
fn cpu_percent(then: (u64, u64), now: (u64, u64)) -> Option<i64> {
    let idle = now.0.checked_sub(then.0)?;
    let total = now.1.checked_sub(then.1)?;
    if total == 0 {
        return None;
    }
    let busy = total.saturating_sub(idle);
    Some(((busy * 100 + total / 2) / total).min(100) as i64)
}

/// Physical memory in use, 0 to 100.
pub fn memory_usage() -> Option<i64> {
    let mut m = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: fills a caller-owned struct whose length field is set.
    unsafe { GlobalMemoryStatusEx(&mut m) }.ok()?;
    Some(i64::from(m.dwMemoryLoad.min(100)))
}

/// What the network can reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Network {
    /// The internet is reachable (possibly behind a captive portal).
    pub connected: bool,
    /// The connection is metered, e.g. a phone hotspot.
    pub metered: bool,
}

type ConnectivityHintFn = unsafe extern "system" fn(*mut NL_NETWORK_CONNECTIVITY_HINT) -> u32;

/// `GetNetworkConnectivityHint`, looked up at run time.
///
/// It is newer than the rest of `iphlpapi.dll` (Windows 10 2004), and a static
/// import of a missing export stops the whole executable from loading. Looked
/// up instead, an older system just reads as offline.
fn connectivity_hint_fn() -> Option<ConnectivityHintFn> {
    static FN: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    let addr = *FN.get_or_init(|| {
        // SAFETY: loading a system DLL by name and looking up an export by a
        // static name. The module is never freed, so the address stays valid.
        unsafe {
            let module = LoadLibraryW(w!("iphlpapi.dll")).ok()?;
            GetProcAddress(module, s!("GetNetworkConnectivityHint")).map(|f| f as usize)
        }
    });
    // SAFETY: the export has exactly this signature.
    addr.map(|a| unsafe { std::mem::transmute::<usize, ConnectivityHintFn>(a) })
}

/// Read connectivity from the network list service's cached hint.
///
/// `GetNetworkConnectivityHint` reads a value the system already maintains; it
/// sends nothing on the wire.
pub fn network() -> Network {
    let mut hint = NL_NETWORK_CONNECTIVITY_HINT::default();
    let Some(get_hint) = connectivity_hint_fn() else {
        return Network { connected: false, metered: false };
    };
    // SAFETY: fills a caller-owned struct; returns a Win32 error code.
    if unsafe { get_hint(&mut hint) } != 0 {
        return Network { connected: false, metered: false };
    }
    let level = hint.ConnectivityLevel;
    Network {
        connected: level == NetworkConnectivityLevelHintInternetAccess
            || level == NetworkConnectivityLevelHintConstrainedInternetAccess
            || level == NetworkConnectivityLevelHintLocalAccess,
        // 1 is "unrestricted"; fixed and variable cost both mean metered.
        metered: hint.ConnectivityCost.0 > 1,
    }
}

thread_local! {
    /// The default output device's volume control, activated once and kept.
    ///
    /// Dropped on any failure, which is what happens when the default device is
    /// unplugged, so the next call activates whatever the new default is.
    static ENDPOINT: RefCell<Option<IAudioEndpointVolume>> = const { RefCell::new(None) };
}

fn with_endpoint<R>(f: impl Fn(&IAudioEndpointVolume) -> windows::core::Result<R>) -> Option<R> {
    ENDPOINT.with(|cell| {
        // Two tries: the cached endpoint, then a freshly activated one.
        for _ in 0..2 {
            if cell.borrow().is_none() {
                *cell.borrow_mut() = activate_endpoint();
            }
            let result = cell.borrow().as_ref().map(&f)?;
            match result {
                Ok(v) => return Some(v),
                Err(_) => *cell.borrow_mut() = None,
            }
        }
        None
    })
}

fn activate_endpoint() -> Option<IAudioEndpointVolume> {
    // SAFETY: COM is initialised on the UI thread by `com::init_process`, which
    // is the only thread that calls into this module.
    unsafe {
        let devices: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let device = devices.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
        device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None).ok()
    }
}

/// Forget the cached output device, so the next reading picks up a new default.
///
/// Changing the default device does not invalidate the old one while it is
/// still plugged in, so a reading alone would keep reporting the old device.
pub fn refresh_audio_device() {
    ENDPOINT.with(|cell| *cell.borrow_mut() = None);
}

/// The default output device's volume, 0 to 100, and whether it is muted.
/// `None` when there is no output device.
pub fn volume() -> Option<(i64, bool)> {
    with_endpoint(|e| unsafe {
        let level = e.GetMasterVolumeLevelScalar()?;
        let muted = e.GetMute()?.as_bool();
        Ok(((level * 100.0).round().clamp(0.0, 100.0) as i64, muted))
    })
}

/// Set the default output device's volume, 0 to 100.
pub fn set_volume(level: f64) -> Result<(), String> {
    let scalar = (level / 100.0).clamp(0.0, 1.0) as f32;
    with_endpoint(|e| unsafe { e.SetMasterVolumeLevelScalar(scalar, std::ptr::null()) })
        .ok_or_else(|| "no audio output device".to_string())
}

/// Mute or unmute the default output device.
pub fn set_muted(muted: bool) -> Result<(), String> {
    with_endpoint(|e| unsafe { e.SetMute(muted, std::ptr::null()) })
        .ok_or_else(|| "no audio output device".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_desktop_has_no_battery() {
        let b = battery_from(1, 128, 255);
        assert!(!b.present);
        assert_eq!(b.percentage, 0);
        assert!(!b.charging);
        assert!(b.plugged);
    }

    #[test]
    fn a_charging_laptop_reads_as_charging() {
        let b = battery_from(1, 8 | 2, 42);
        assert_eq!(b, Battery { present: true, percentage: 42, charging: true, plugged: true });
    }

    #[test]
    fn an_unknown_status_is_not_a_battery() {
        assert!(!battery_from(255, 255, 255).present);
    }

    #[test]
    fn cpu_usage_is_busy_over_total() {
        assert_eq!(cpu_percent((0, 0), (75, 100)), Some(25));
        assert_eq!(cpu_percent((10, 10), (10, 10)), None);
        assert_eq!(cpu_percent((50, 50), (40, 60)), None, "a clock that ran backwards");
    }
}
