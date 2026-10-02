//! Display enumeration, work areas and reserved screen space.
//!
//! A shell must never assume a single monitor, so every position the runtime
//! computes flows through [`Monitor`]. The three things that make multi-monitor
//! work are handled here rather than in the UI layer:
//!
//! * **Physical pixels everywhere.** The process is per-monitor DPI aware, so
//!   Win32 coordinates are physical and must not be scaled again.
//! * **Mixed DPI.** Each monitor carries its own scale, and a panel on a 150%
//!   display is sized in that display's pixels.
//! * **Negative origins.** A monitor left of or above the primary has negative
//!   coordinates. `EnumDisplayMonitors` ordering is not the primary, and neither
//!   is device name ordering, so the primary flag comes from the API.

use std::collections::HashMap;

use windows::core::{BOOL, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, MonitorFromWindow, HDC, HMONITOR, MONITORINFO,
    MONITORINFOEXW, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, GetDpiForWindow, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN,
};

/// `MONITORINFOF_PRIMARY`, which the `windows` crate does not surface.
const MONITORINFOF_PRIMARY: u32 = 1;

/// One display, in physical pixels.
#[derive(Debug, Clone, PartialEq)]
pub struct Monitor {
    /// Win32 device name, e.g. `\\.\DISPLAY1`.
    pub name: String,
    /// Stable identifier used in config: `screen: primary` or `screen: 1`.
    pub id: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// Work area: the desktop minus the taskbar and other shell furniture.
    pub work_x: i32,
    pub work_y: i32,
    pub work_width: u32,
    pub work_height: u32,
    /// DPI scale, 1.0 at 96 DPI.
    pub scale: f32,
    pub refresh_hz: u32,
    pub primary: bool,
    /// Windows display rotation, in degrees.
    pub rotation: u32,
}

impl Monitor {
    pub fn rect(&self) -> (i32, i32, u32, u32) {
        (self.x, self.y, self.width, self.height)
    }

    pub fn work_rect(&self) -> (i32, i32, u32, u32) {
        (self.work_x, self.work_y, self.work_width, self.work_height)
    }

    pub fn center(&self) -> (i32, i32) {
        (self.x + self.width as i32 / 2, self.y + self.height as i32 / 2)
    }

    /// Whether a physical pixel point lies on this monitor.
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.x + self.width as i32 && y >= self.y && y < self.y + self.height as i32
    }

    /// Apply margins and convert a logical size into physical pixels on this
    /// display. Panels are sized in logical units in config, and this is the one
    /// place that knows the display's scale.
    pub fn physical(&self, logical: f32) -> i32 {
        (logical * self.scale).round() as i32
    }

    /// The inverse of [`Monitor::physical`], for a size that came back from
    /// Windows in device pixels.
    ///
    /// A layout pass runs in logical units, so a window's client size has to be
    /// divided by the scale — *not* by the DPI. The DPI is `96 * scale`, and
    /// dividing by it instead of by `scale` shrinks every layout to a twentieth
    /// of its size on a 100% display.
    pub fn logical(&self, physical: f32) -> f32 {
        if self.scale > 0.0 {
            physical / self.scale
        } else {
            physical
        }
    }

    /// The display width in logical pixels.
    pub fn logical_width(&self) -> f32 {
        self.logical(self.width as f32)
    }

    /// The display height in logical pixels.
    pub fn logical_height(&self) -> f32 {
        self.logical(self.height as f32)
    }
}

struct EnumCtx {
    out: Vec<Monitor>,
    primary_found: bool,
    index: usize,
}

unsafe extern "system" fn enum_callback(
    hmonitor: HMONITOR,
    _dc: HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> BOOL {
    let ctx = &mut *(data.0 as *mut EnumCtx);
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !GetMonitorInfoW(hmonitor, &mut mi).as_bool() {
        return BOOL(1); // Keep enumerating; one bad monitor must not hide the rest.
    }
    let primary = mi.dwFlags & MONITORINFOF_PRIMARY != 0;    let name = device_name(hmonitor);
    let scale = monitor_scale(hmonitor);
    let refresh = refresh_rate(hmonitor);
    let rotation = rotation_degrees(&mi.rcMonitor, &mi.rcWork);

    let r = mi.rcMonitor;
    let w = mi.rcWork;
    let m = Monitor {
        id: String::new(), // filled in below, once ordering is known
        name: name.clone(),
        x: r.left,
        y: r.top,
        width: (r.right - r.left).max(0) as u32,
        height: (r.bottom - r.top).max(0) as u32,
        work_x: w.left,
        work_y: w.top,
        work_width: (w.right - w.left).max(0) as u32,
        work_height: (w.bottom - w.top).max(0) as u32,
        scale,
        refresh_hz: refresh,
        primary,
        rotation,
    };
    ctx.out.push(m);
    if primary {
        ctx.primary_found = true;
    }
    ctx.index += 1;
    BOOL(1)
}

unsafe fn device_name(hmonitor: HMONITOR) -> String {
    let mut info = MONITORINFOEXW {
        monitorInfo: MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        },
        ..Default::default()
    };
    if !GetMonitorInfoW(hmonitor, &mut info as *mut _ as *mut MONITORINFO).as_bool() {
        return "unknown".into();
    }
    let len = info.szDevice.iter().position(|c| *c == 0).unwrap_or(0);
    String::from_utf16_lossy(&info.szDevice[..len])
}

unsafe fn monitor_scale(hmonitor: HMONITOR) -> f32 {
    let mut x = 96u32;
    let mut y = 96u32;
    // `GetDpiForMonitor` predates per-monitor v2; it still reports the right
    // effective DPI when the process is per-monitor aware.
    if GetDpiForMonitor(hmonitor, MDT_EFFECTIVE_DPI, &mut x, &mut y).is_ok() && x > 0 {
        x as f32 / 96.0
    } else {
        1.0
    }
}

unsafe fn refresh_rate(hmonitor: HMONITOR) -> u32 {
    use windows::Win32::Graphics::Gdi::EnumDisplaySettingsW;
    use windows::Win32::Graphics::Gdi::{DEVMODEW, ENUM_CURRENT_SETTINGS};
    let name = device_name(hmonitor);
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let mut dm = DEVMODEW::default();
    dm.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
    if EnumDisplaySettingsW(PCWSTR(wide.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm).as_bool() {
        dm.dmDisplayFrequency as u32
    } else {
        60
    }
}

unsafe fn rotation_degrees(monitor: &RECT, work: &RECT) -> u32 {
    let w = (monitor.right - monitor.left) as u32;
    let h = (monitor.bottom - monitor.top) as u32;
    // Rotation is inferred from how the work area sits inside the monitor, which
    // is the only reliable signal without opening display settings.
    let dx = (work.left - monitor.left).abs();
    let dy = (work.top - monitor.top).abs();
    if w > h {
        if dy > dx {
            90
        } else {
            0
        }
    } else if dx > dy {
        270
    } else {
        0
    }
}

/// The set of connected displays.
#[derive(Debug, Clone, Default)]
pub struct Monitors {
    pub list: Vec<Monitor>,
}

impl Monitors {
    /// Enumerate the current displays.
    pub fn enumerate() -> Monitors {
        unsafe {
            let mut ctx = EnumCtx { out: Vec::new(), primary_found: false, index: 0 };
            let _ = EnumDisplayMonitors(
                None,
                None,
                Some(enum_callback),
                LPARAM(&mut ctx as *mut EnumCtx as isize),
            );
            let mut list = ctx.out;
            if list.is_empty() {
                // Enumeration should never fail, but a shell with no monitor is
                // worse than one with a synthesised primary.
                list.push(Monitor {
                    name: "\\\\.\\DISPLAY1".into(),
                    id: "primary".into(),
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                    work_x: 0,
                    work_y: 0,
                    work_width: 1920,
                    work_height: 1040,
                    scale: 1.0,
                    refresh_hz: 60,
                    primary: true,
                    rotation: 0,
                });
            }
            // Guarantee exactly one primary: the flag can be missing when a
            // session is being restored, and the runtime must not have to cope.
            if !list.iter().any(|m| m.primary) {
                if let Some(first) = list.first_mut() {
                    first.primary = true;
                }
            }
            // Ids follow enumeration order with the primary pinned first, so
            // `screen: primary` and `screen: 1` are both stable and meaningful.
            list.sort_by_key(|m| !m.primary);
            let mut seen: HashMap<String, u32> = HashMap::new();
            for (i, m) in list.iter_mut().enumerate() {
                let n = seen.entry(m.name.clone()).or_insert(0);
                *n += 1;
                // Duplicate device names can appear in a cloned display layout.
                let suffix = if *n == 1 { String::new() } else { format!("#{n}") };
                m.id = if m.primary {
                    "primary".to_string()
                } else {
                    format!("{i}{suffix}")
                };
            }
            Monitors { list }
        }
    }

    pub fn primary(&self) -> &Monitor {
        self.list
            .iter()
            .find(|m| m.primary)
            .or_else(|| self.list.first())
            .expect("Monitors::enumerate always yields at least one display")
    }

    /// Resolve `primary`, an index, or a device name.
    pub fn get(&self, key: &str) -> Option<&Monitor> {
        if key.eq_ignore_ascii_case("primary") {
            return Some(self.primary());
        }
        if let Ok(i) = key.parse::<usize>() {
            return self.list.get(i);
        }
        self.list
            .iter()
            .find(|m| m.id.eq_ignore_ascii_case(key) || m.name.eq_ignore_ascii_case(key))
    }

    /// The monitor containing a physical point, falling back to the closest by
    /// centre distance so a window dragged off-screen still lands somewhere sane.
    pub fn monitor_at(&self, x: i32, y: i32) -> &Monitor {
        if let Some(m) = self.list.iter().find(|m| m.contains(x, y)) {
            return m;
        }
        self.list
            .iter()
            .min_by_key(|m| {
                let (cx, cy) = m.center();
                let dx = (cx - x) as i64;
                let dy = (cy - y) as i64;
                dx * dx + dy * dy
            })
            .unwrap_or_else(|| self.primary())
    }

    /// The monitor a window currently sits on.
    pub fn monitor_for_window(&self, hwnd: HWND) -> Option<&Monitor> {
        let hmonitor = unsafe { monitor_from_window(hwnd) }?;
        self.by_device(&unsafe { device_name(hmonitor) })
    }

    fn by_device(&self, device: &str) -> Option<&Monitor> {
        self.list.iter().find(|m| m.name.eq_ignore_ascii_case(device))
    }

    /// Bounding box of every display, used to size a full-desktop overlay.
    ///
    /// Computed from this snapshot rather than re-querying the system, so a caller
    /// that just handled a display change gets a consistent answer and a caller
    /// that built a synthetic set gets a deterministic one.
    pub fn virtual_bounds(&self) -> (i32, i32, i32, i32) {
        let mut min_x = i32::MAX;
        let mut min_y = i32::MAX;
        let mut max_x = i32::MIN;
        let mut max_y = i32::MIN;
        for m in &self.list {
            min_x = min_x.min(m.x);
            min_y = min_y.min(m.y);
            max_x = max_x.max(m.x + m.width as i32);
            max_y = max_y.max(m.y + m.height as i32);
        }
        if min_x > max_x {
            return (0, 0, 1920, 1080);
        }
        (min_x, min_y, max_x - min_x, max_y - min_y)
    }

    /// The virtual-screen metrics as Windows reports them, for callers that need
    /// the compositor's own view rather than this snapshot.
    pub fn system_virtual_bounds(&self) -> (i32, i32, i32, i32) {
        unsafe {
            let x = GetSystemMetrics(SM_XVIRTUALSCREEN);
            let y = GetSystemMetrics(SM_YVIRTUALSCREEN);
            let w = GetSystemMetrics(SM_CXVIRTUALSCREEN);
            let h = GetSystemMetrics(SM_CYVIRTUALSCREEN);
            if w > 0 && h > 0 {
                return (x, y, w, h);
            }
        }
        self.virtual_bounds()
    }
}

unsafe fn monitor_from_window(hwnd: HWND) -> Option<HMONITOR> {
    let hm = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
    (!hm.0.is_null()).then_some(hm)
}

/// Effective DPI scale for a window, used when a window moves between displays.
pub fn dpi_scale_of(hwnd: HWND) -> f32 {
    unsafe {
        let dpi = GetDpiForWindow(hwnd);
        if dpi == 0 {
            1.0
        } else {
            dpi as f32 / 96.0
        }
    }
}

/// Ask the compositor for the display refresh rate, so animations can target the
/// real frame budget rather than assuming 60.
pub fn primary_refresh_hz() -> u32 {
    Monitors::enumerate().primary().refresh_hz.max(30)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(x: i32, y: i32, w: u32, h: u32, primary: bool) -> Monitor {
        Monitor {
            name: format!("\\\\.\\DISPLAY{}", x + y),
            id: if primary { "primary".into() } else { "1".into() },
            x,
            y,
            width: w,
            height: h,
            work_x: x,
            work_y: y,
            work_width: w,
            work_height: h,
            scale: 1.0,
            refresh_hz: 60,
            primary,
            rotation: 0,
        }
    }

    #[test]
    fn enumerates_at_least_one_display() {
        let mons = Monitors::enumerate();
        assert!(!mons.list.is_empty());
        let p = mons.primary();
        assert!(p.primary, "there must always be a primary");
        assert!(p.width > 0 && p.height > 0);
    }

    #[test]
    fn exactly_one_display_is_primary() {
        let mons = Monitors::enumerate();
        assert_eq!(mons.list.iter().filter(|m| m.primary).count(), 1);
    }

    #[test]
    fn every_display_has_a_usable_scale() {
        for m in Monitors::enumerate().list {
            assert!(m.scale > 0.5 && m.scale <= 4.0, "{} has scale {}", m.id, m.scale);
        }
    }

    #[test]
    fn resolves_by_id_index_and_name() {
        let mons = Monitors::enumerate();
        let p = mons.primary();
        assert_eq!(mons.get("primary").map(|m| &m.name), Some(&p.name));
        assert_eq!(mons.get(&p.name).map(|m| &m.name), Some(&p.name));
        assert!(mons.get("0").is_some());
        assert!(mons.get("nope").is_none());
    }

    #[test]
    fn finds_the_display_under_a_point() {
        let mons = Monitors { list: vec![monitor(0, 0, 1920, 1080, true), monitor(1920, 0, 2560, 1440, false)] };
        assert_eq!(mons.monitor_at(100, 100).primary, true);
        assert_eq!(mons.monitor_at(2000, 100).primary, false);
    }

    #[test]
    fn a_point_off_every_display_uses_the_nearest() {
        let mons = Monitors { list: vec![monitor(0, 0, 1920, 1080, true), monitor(1920, 0, 2560, 1440, false)] };
        // Far to the left of both; the primary is nearer.
        assert_eq!(mons.monitor_at(-5000, 0).primary, true);
    }

    #[test]
    fn negative_origins_are_supported() {
        // A monitor left of the primary has a negative x.
        let mons = Monitors { list: vec![monitor(0, 0, 1920, 1080, true), monitor(-1280, 0, 1280, 1024, false)] };
        let left = mons.monitor_at(-100, 100);
        assert!(!left.primary);
        assert_eq!(left.x, -1280);
        let (vx, _, vw, _) = mons.virtual_bounds();
        assert_eq!(vx, -1280);
        assert_eq!(vw, 3200);
    }

    #[test]
    fn the_work_area_is_never_larger_than_the_display() {
        // A monitor whose work area exceeds its bounds would let a maximized
        // window be positioned partly off screen, and every panel placed against
        // the work area would be misplaced.
        for m in &Monitors::enumerate().list {
            assert!(m.work_width <= m.width, "{}: work area wider than the display", m.id);
            assert!(m.work_height <= m.height, "{}: work area taller than the display", m.id);
            assert!(m.work_x >= m.x - 1, "{}: work area starts left of the display", m.id);
            assert!(m.work_y >= m.y - 1, "{}: work area starts above the display", m.id);
        }
    }

    #[test]
    fn physical_conversion_uses_the_monitor_scale() {
        let mut m = monitor(0, 0, 1920, 1080, true);
        assert_eq!(m.physical(36.0), 36);
        m.scale = 1.5;
        assert_eq!(m.physical(36.0), 54);
    }

    #[test]
    fn contains_uses_a_half_open_rectangle() {
        let m = monitor(0, 0, 100, 100, true);
        assert!(m.contains(0, 0));
        assert!(m.contains(99, 99));
        assert!(!m.contains(100, 50));
    }
}
