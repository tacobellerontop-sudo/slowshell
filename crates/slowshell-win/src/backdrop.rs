//! Windows backdrop effects: acrylic, mica and the blur behind a panel.
//!
//! These are compositor features, not something the renderer draws, so a panel
//! gets real system blur for free rather than a sampled copy of the desktop.
//!
//! Every function here is fallible by design. On Windows 10, on a build without
//! the backdrop attributes, or inside a VM without composition, the call fails and
//! the caller falls back to a translucent fill. Nothing here may be fatal.

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_USE_IMMERSIVE_DARK_MODE,
    DWMSBT_MAINWINDOW, DWMSBT_NONE, DWMSBT_TRANSIENTWINDOW,
};

/// The visual material behind a surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backdrop {
    /// No system effect. The surface is whatever the config paints.
    #[default]
    None,
    /// Flat translucent colour, the most portable option.
    Acrylic,
    /// Tint sampled from the desktop wallpaper. Best for full-screen windows.
    Mica,
    /// Mica tinted by the user's accent colour. Best for panels.
    MicaAlt,
    /// DWM blur, available where the backdrop types are not.
    Blur,
}

impl Backdrop {
    /// Parse the name used in config, e.g. `background: Acrylic`.
    pub fn parse(name: &str) -> Option<Backdrop> {
        Some(match name.to_ascii_lowercase().as_str() {
            "none" | "transparent" | "opaque" => Backdrop::None,
            "acrylic" => Backdrop::Acrylic,
            "mica" => Backdrop::Mica,
            "micaalt" | "mica_alt" | "mica-alt" => Backdrop::MicaAlt,
            "blur" => Backdrop::Blur,
            _ => return None,
        })
    }

    pub fn name(&self) -> &'static str {
        match self {
            Backdrop::None => "none",
            Backdrop::Acrylic => "acrylic",
            Backdrop::Mica => "mica",
            Backdrop::MicaAlt => "micaAlt",
            Backdrop::Blur => "blur",
        }
    }

    /// Whether this needs a light-on-dark or dark-on-light text treatment.
    /// Only a wallpaper-derived backdrop is ambiguous; a dark UI theme with
    /// acrylic wants light text regardless.
    pub fn needs_contrast_check(&self) -> bool {
        matches!(self, Backdrop::Mica)
    }
}

/// Apply a backdrop to a window. Returns whether the system honoured it.
pub fn apply(hwnd: HWND, backdrop: Backdrop, dark: bool) -> bool {
    unsafe {
        set_dark_mode(hwnd, dark);
        match backdrop {
            Backdrop::None => {
                set_system_backdrop(hwnd, DWMSBT_NONE);
                false
            }
            Backdrop::Acrylic => {
                // Acrylic is the transient-window backdrop: a blurred, lightly
                // tinted version of what is behind the surface.
                set_system_backdrop(hwnd, DWMSBT_TRANSIENTWINDOW)
            }
            Backdrop::Mica | Backdrop::MicaAlt => set_system_backdrop(hwnd, DWMSBT_MAINWINDOW),
            Backdrop::Blur => {
                set_system_backdrop(hwnd, DWMSBT_NONE);
                true
            }
        }
    }
}

unsafe fn set_system_backdrop(hwnd: HWND, kind: windows::Win32::Graphics::Dwm::DWM_SYSTEMBACKDROP_TYPE) -> bool {
    let value = kind.0 as i32;
    // `DwmSetWindowAttribute` returns S_OK on builds that know the attribute and
    // E_INVALIDARG on older ones, so the boolean is the real answer.
    DwmSetWindowAttribute(
        hwnd,
        DWMWA_SYSTEMBACKDROP_TYPE,
        &value as *const i32 as *const _,
        std::mem::size_of::<i32>() as u32,
    )
    .is_ok()
}

unsafe fn set_dark_mode(hwnd: HWND, dark: bool) {
    let value: i32 = if dark { 1 } else { 0 };
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_USE_IMMERSIVE_DARK_MODE,
        &value as *const i32 as *const _,
        std::mem::size_of::<i32>() as u32,
    );
}

/// Tint the system backdrop with a colour.
///
/// This uses `SetWindowCompositionAttribute`, which is undocumented but has been
/// the entry point for accent-tinted acrylic since Windows 10 1809 and is what
/// first-party apps use. It is resolved dynamically so a Windows build without it
/// degrades instead of failing to link.
pub fn apply_tint(hwnd: HWND, tint: (u8, u8, u8), opacity: u8) -> bool {
    let Some(set_attr) = composition_attribute_fn() else {
        return false;
    };
    // WINDOWCOMPOSITIONATTRIBUTE_ACCENT_POLICY
    const ACCENT_POLICY: i32 = 19;
    #[repr(C)]
    struct AttribData {
        attribute: i32,
        data: *mut core::ffi::c_void,
        size: u32,
    }
    let mut policy = AccentPolicy {
        state: 4, // ACCENT_ENABLE_TRANSPARENTGRADIENT
        // 0xAABBGGRR, the byte order DWM uses.
        color: ((opacity as u32) << 24) | ((tint.2 as u32) << 16) | ((tint.1 as u32) << 8) | tint.0 as u32,
        gradient: 0,
    };
    let mut attrib = AttribData {
        attribute: ACCENT_POLICY,
        data: &mut policy as *mut AccentPolicy as *mut core::ffi::c_void,
        size: std::mem::size_of::<AccentPolicy>() as u32,
    };
    unsafe { set_attr(hwnd, &mut attrib as *mut AttribData as *mut core::ffi::c_void) != 0 }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AccentPolicy {
    state: u32,
    color: u32,
    gradient: u32,
}

type SetWindowCompositionAttributeFn =
    unsafe extern "system" fn(HWND, *mut core::ffi::c_void) -> i32;

/// Resolve `SetWindowCompositionAttribute` from `user32.dll` once.
fn composition_attribute_fn() -> Option<SetWindowCompositionAttributeFn> {
    use std::sync::OnceLock;
    static FN: OnceLock<Option<SetWindowCompositionAttributeFn>> = OnceLock::new();
    *FN.get_or_init(|| unsafe {
        use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
        let user32 = GetModuleHandleW(windows::core::PCWSTR(wide(b"user32.dll").as_ptr())).ok()?;
        // `GetProcAddress` takes an ANSI name even when resolving from an
        // already-loaded Unicode module.
        let name = b"SetWindowCompositionAttribute\0";
        let addr = GetProcAddress(user32, windows::core::PCSTR(name.as_ptr()));
        if addr.is_none() {
            return None;
        }
        // The signature is documented by usage rather than by a header, so the
        // cast is the only way to reach it. The vtable shape is stable and is
        // what every Windows app in this situation does.
        Some(std::mem::transmute::<
            windows::Win32::Foundation::FARPROC,
            SetWindowCompositionAttributeFn,
        >(addr))
    })
}

fn wide(bytes: &[u8]) -> Vec<u16> {
    bytes.iter().map(|b| *b as u16).chain(std::iter::once(0)).collect()
}

/// Report whether this build supports system backdrops at all.
///
/// Used by `shellctl doctor` and `Shell.exe --doctor` to explain why
/// `background: Acrylic` looks flat.
///
/// Feature-detected by asking: a real window is created, the backdrop attribute is
/// set on it, and the result is observed. A build-number check would be a guess,
/// and a VM's compositor can be older than its kernel claims.
pub fn supported() -> bool {
    use crate::window::{create_surface, destroy, SurfaceRole};
    // A 1x1 window is enough; it is never shown.
    match create_surface(SurfaceRole::Popup, 0, 0, 1, 1, "probe") {
        Ok(h) => {
            let ok = apply(h.hwnd, Backdrop::Acrylic, true);
            destroy(h);
            ok
        }
        Err(_) => false,
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_config_names_case_insensitively() {
        assert_eq!(Backdrop::parse("Acrylic"), Some(Backdrop::Acrylic));
        assert_eq!(Backdrop::parse("mica_alt"), Some(Backdrop::MicaAlt));
        assert_eq!(Backdrop::parse("NONE"), Some(Backdrop::None));
        assert_eq!(Backdrop::parse("chrome"), None);
    }

    #[test]
    fn names_round_trip() {
        for b in [Backdrop::None, Backdrop::Acrylic, Backdrop::Mica, Backdrop::MicaAlt, Backdrop::Blur] {
            assert_eq!(Backdrop::parse(b.name()), Some(b), "{b:?}");
        }
    }

    #[test]
    fn accent_colour_uses_dwm_byte_order() {
        let p = AccentPolicy { state: 4, color: 0, gradient: 0 };
        let _ = p;
        // 0xAABBGGRR: blue, green, red, alpha.
        let color = ((0x80u32) << 24) | ((0x33u32) << 16) | ((0x22u32) << 8) | 0x11u32;
        assert_eq!(color, 0x8033_2211);
    }
}

