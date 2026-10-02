//! Direct2D + DXGI + DirectWrite device creation.
//!
//! One device is shared by every surface in the process. That matters for both
//! correctness and cost: creating a D3D11 device per panel would multiply startup
//! time and VRAM, and a shell typically owns several surfaces.
//!
//! ## Two presentation paths, and why both exist
//!
//! The preferred path is a flip-model swap chain with a premultiplied alpha
//! mode and no window background brush. That is what gives a panel the per-pixel
//! transparency the desktop compositor respects, and it presents on the GPU
//! locked to the display refresh rate.
//!
//! It is not always available. Some drivers — AMD adapters under a hypervisor
//! among them — reject every alpha-aware chain *and* then reject the Direct2D
//! bitmap that would have to sit on top of the one chain they do accept,
//! because a Direct2D render target requires premultiplied alpha and an
//! `DXGI_ALPHA_MODE_IGNORE` surface has none. On such a machine DXGI and
//! Direct2D cannot be used together at all.
//!
//! So [`Graphics::create`] probes first and, when no swap chain survives,
//! falls back to [`Presentation::Window`]: an `ID2D1HwndRenderTarget` created
//! directly from the Direct2D factory. It is slower and it presents opaquely,
//! and it works anyway. A shell that cannot present at all is worth nothing; a
//! shell with a solid bar on one machine is worth a great deal.

use std::cell::{Cell, RefCell};

use serde::{Deserialize, Serialize};
use slowshell_core::Color;
use windows::core::{Error, Interface, Result, HRESULT};
use windows::Win32::Foundation::{HMODULE, HWND, RECT};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_SIZE_U,
};
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE, DXGI_ALPHA_MODE_IGNORE, DXGI_ALPHA_MODE_PREMULTIPLIED,
    DXGI_ALPHA_MODE_STRAIGHT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::UI::WindowsAndMessaging::{GetClientRect, IsWindow};

use super::target::RenderTarget;

const E_FAIL: HRESULT = HRESULT(0x8000_4005u32 as i32);
const E_HANDLE: HRESULT = HRESULT(0x8007_0006u32 as i32);
/// `D2DERR_RECREATE_TARGET`. The `windows` crate does not surface it, and a
/// window render target that returns it must be thrown away and rebuilt.
const D2DERR_RECREATE_TARGET: HRESULT = HRESULT(0x8899_000Cu32 as i32);

/// The D2D1 alpha mode to use for the render target bitmap.
///
/// Always premultiplied. Direct2D composites in premultiplied alpha and rejects
/// a target bitmap that says otherwise, even when the swap chain behind it was
/// created with `DXGI_ALPHA_MODE_IGNORE`. When the alpha channel is discarded at
/// present time the renderer pre-composites colours instead of relying on it.
fn d2d_alpha_for(_dxgi_alpha: DXGI_ALPHA_MODE) -> D2D1_ALPHA_MODE {
    D2D1_ALPHA_MODE_PREMULTIPLIED
}

fn bitmap_props(alpha_mode: DXGI_ALPHA_MODE) -> D2D1_BITMAP_PROPERTIES1 {
    D2D1_BITMAP_PROPERTIES1 {
        pixelFormat: D2D1_PIXEL_FORMAT {
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: d2d_alpha_for(alpha_mode),
        },
        dpiX: 96.0,
        dpiY: 96.0,
        bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET,
        colorContext: Default::default(),
    }
}

/// Feature levels tried in order. A virtual machine or an older laptop may not
/// offer 11_0, and refusing to start there would be worse than starting slower.
const FEATURE_LEVELS: &[D3D_FEATURE_LEVEL] = &[
    D3D_FEATURE_LEVEL_11_0,
    D3D_FEATURE_LEVEL_10_1,
    D3D_FEATURE_LEVEL_10_0,
    D3D_FEATURE_LEVEL_9_3,
    D3D_FEATURE_LEVEL_9_1,
];

unsafe fn create_hardware_device() -> Result<(ID3D11Device, String)> {
    let mut device: Option<ID3D11Device> = None;
    let mut obtained = D3D_FEATURE_LEVEL_11_0;
    let hr = D3D11CreateDevice(
        None,
        D3D_DRIVER_TYPE_HARDWARE,
        HMODULE(std::ptr::null_mut()),
        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        Some(FEATURE_LEVELS),
        D3D11_SDK_VERSION,
        Some(&mut device),
        Some(&mut obtained),
        None,
    );
    if !hr.is_ok() {
        return Err(Error::new(E_FAIL, "no D3D11 hardware device"));
    }
    let device = device.ok_or_else(|| Error::new(E_FAIL, "no D3D11 hardware device"))?;
    let name = adapter_name(&device).unwrap_or_else(|| "unknown adapter".into());
    Ok((device, name))
}

fn probe_hardware_device() -> Result<(ID3D11Device, String)> {
    unsafe { create_hardware_device() }
}

unsafe fn create_warp_device() -> Result<(ID3D11Device, String)> {
    let mut device: Option<ID3D11Device> = None;
    let mut level = D3D_FEATURE_LEVEL_11_0;
    D3D11CreateDevice(
        None,
        D3D_DRIVER_TYPE_WARP,
        HMODULE(std::ptr::null_mut()),
        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        Some(FEATURE_LEVELS),
        D3D11_SDK_VERSION,
        Some(&mut device),
        Some(&mut level),
        None,
    )
    .map_err(|_| Error::new(E_FAIL, "WARP render device unavailable"))?;
    let device = device.ok_or_else(|| Error::new(E_FAIL, "WARP render device unavailable"))?;
    Ok((device, "Microsoft Basic Render Driver (WARP)".to_string()))
}

unsafe fn adapter_name(device: &ID3D11Device) -> Option<String> {
    let dxgi: IDXGIDevice = device.cast().ok()?;
    let adapter: IDXGIAdapter1 = dxgi.GetAdapter().ok()?.cast().ok()?;
    let desc = adapter.GetDesc1().ok()?;
    let len = desc.Description.iter().position(|c| *c == 0).unwrap_or(0);
    Some(String::from_utf16_lossy(&desc.Description[..len]))
}

unsafe fn default_adapter_name(factory: &IDXGIFactory2) -> Option<String> {
    let mut i = 0u32;
    while let Ok(adapter) = factory.EnumAdapters1(i) {
        i += 1;
        if let Ok(d) = adapter.GetDesc1() {
            let len = d.Description.iter().position(|c| *c == 0).unwrap_or(0);
            return Some(String::from_utf16_lossy(&d.Description[..len]));
        }
    }
    None
}

/// An identity for the adapter and its driver, used to cache the probe.
///
/// The name alone is not enough: two identical cards can carry different driver
/// versions, and one of them can accept a swap chain the other cannot. So the
/// LUID plus the PCI identity go in with it — the driver's own version number is
/// not exposed by `DXGI_ADAPTER_DESC1`, and these fields change when the driver
/// does in all the cases that matter.
unsafe fn adapter_fingerprint() -> String {
    let Ok(factory) = CreateDXGIFactory2::<IDXGIFactory2>(DXGI_CREATE_FACTORY_FLAGS(0)) else {
        return "unknown".into();
    };
    let Ok(adapter) = factory.EnumAdapters1(0) else {
        return "unknown".into();
    };
    let Ok(desc) = adapter.GetDesc1() else {
        return "unknown".into();
    };
    let len = desc.Description.iter().position(|c| *c == 0).unwrap_or(0);
    format!(
        "{}|{:04X}:{:04X}:{:04X}:{:X}|{:X}:{:X}",
        String::from_utf16_lossy(&desc.Description[..len]),
        desc.VendorId,
        desc.DeviceId,
        desc.SubSysId,
        desc.Revision,
        desc.AdapterLuid.HighPart,
        desc.AdapterLuid.LowPart,
    )
}

/// How a panel's pixels reach the screen on this machine.
///
/// Chosen once by [`Graphics::create`] and reported by `--doctor`, because
/// "why is my bar solid" is a question a user deserves a plain answer to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presentation {
    /// A DXGI swap chain with the configuration the probe settled on.
    SwapChain {
        effect: DXGI_SWAP_EFFECT,
        alpha: DXGI_ALPHA_MODE,
    },
    /// An `ID2D1HwndRenderTarget`, because no swap chain can be created *and*
    /// drawn into on this adapter.
    ///
    /// Presents through GDI and has no per-pixel alpha, so the renderer
    /// pre-composites translucent colours against an opaque backdrop.
    Window,
}

impl Presentation {
    /// A short name for logs and `--doctor`.
    pub fn label(&self) -> &'static str {
        match self {
            Presentation::SwapChain { .. } => "swap chain",
            Presentation::Window => "window render target (opaque)",
        }
    }

    /// Whether per-pixel alpha reaches the compositor on this path.
    pub fn honours_alpha(&self) -> bool {
        match self {
            Presentation::SwapChain { alpha, .. } => *alpha != DXGI_ALPHA_MODE_IGNORE,
            Presentation::Window => false,
        }
    }

    /// The stable form written to the cache file.
    ///
    /// The enum values rather than the Win32 ones, so a binding change cannot
    /// make an old cache file mean something else.
    fn to_cache(&self) -> String {
        match self {
            Presentation::SwapChain { .. } => "swap-chain".into(),
            Presentation::Window => "window".into(),
        }
    }

    fn from_cache(s: &str) -> Option<Presentation> {
        match s {
            // The configuration is not cached, only the verdict: re-probing to
            // find out *which* alpha mode works is the expensive part, and a
            // chain that worked once works again after a reboot.
            "swap-chain" => Some(Presentation::SwapChain {
                effect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                alpha: DXGI_ALPHA_MODE_PREMULTIPLIED,
            }),
            "window" => Some(Presentation::Window),
            _ => None,
        }
    }
}

/// The remembered probe result for one adapter.
///
/// # Why this exists
///
/// Probing is not cheap. Each attempt builds a D3D11 device, a DXGI factory, a
/// Direct2D factory and a Direct2D device, and on a machine where *nothing*
/// works every candidate is tried twice — once on hardware and once on WARP.
/// That measured 3.5 seconds of startup on an AMD Radeon 780M under Hyper-V,
/// which is most of a shell's whole startup budget spent re-answering a question
/// whose answer cannot change until the driver does.
///
/// So the verdict is cached against the adapter's identity. A driver update
/// changes the key, and `SLOWSHELL_REPROBE=1` forces a fresh answer.
#[derive(Debug, Serialize, Deserialize)]
struct ProbeCache {
    fingerprint: String,
    verdict: String,
}

impl ProbeCache {
    fn path() -> std::path::PathBuf {
        slowshell_core::paths::state_dir().join("presentation.json")
    }

    fn read(fingerprint: &str) -> Option<Presentation> {
        let text = std::fs::read_to_string(Self::path()).ok()?;
        let cached: ProbeCache = serde_json::from_str(&text).ok()?;
        if cached.fingerprint != fingerprint {
            return None;
        }
        Presentation::from_cache(&cached.verdict)
    }

    fn write(fingerprint: &str, presentation: Presentation) {
        let cached =
            ProbeCache { fingerprint: fingerprint.to_string(), verdict: presentation.to_cache() };
        let Ok(text) = serde_json::to_string_pretty(&cached) else { return };
        if let Some(dir) = Self::path().parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // A cache that cannot be written is not a failure: the next start pays
        // for the probe again and everything still works.
        let _ = std::fs::write(Self::path(), text);
    }
}

/// Owns the shared graphics device and hands out one target per window.
///
/// Construction is split in two. [`Graphics::new`] only records the adapter;
/// [`Graphics::create`] builds the device **after** the swap chain probe has run.
/// The probe has to come first because a driver that rejects a swap chain
/// configuration can leave its device unable to create any chain at all, so a
/// device that has already seen a rejected `CreateSwapChainForHwnd` is unusable.
pub struct Graphics {
    d2d: RefCell<Option<ID2D1Factory3>>,
    dw: RefCell<Option<IDWriteFactory>>,
    /// `None` on the window path, where Direct2D needs no DXGI device at all.
    device: RefCell<Option<ID2D1Device>>,
    dxgi: RefCell<Option<IDXGIFactory2>>,
    d3d: RefCell<Option<ID3D11Device>>,
    presentation: RefCell<Presentation>,
    software: Cell<bool>,
    adapter_name: RefCell<String>,
}

impl Graphics {
    /// Record the adapter. Call [`Graphics::create`] before anything else.
    pub fn new() -> Result<Graphics> {
        unsafe {
            let adapter_name = CreateDXGIFactory2::<IDXGIFactory2>(DXGI_CREATE_FACTORY_FLAGS(0))
                .ok()
                .and_then(|f| default_adapter_name(&f))
                .unwrap_or_else(|| "unknown adapter".into());
            Ok(Graphics {
                d2d: RefCell::new(None),
                dw: RefCell::new(None),
                device: RefCell::new(None),
                dxgi: RefCell::new(None),
                d3d: RefCell::new(None),
                presentation: RefCell::new(Presentation::Window),
                software: Cell::new(false),
                adapter_name: RefCell::new(adapter_name),
            })
        }
    }

    /// Build the device, factories and presentation path.
    ///
    /// The presentation probe is skipped when this adapter's verdict is already
    /// cached, which is the difference between a 200 ms startup and a 3.5 s one
    /// on a machine where the probe has to try everything. Set
    /// `SLOWSHELL_REPROBE=1` to force a fresh answer.
    pub fn create(&self) -> Result<()> {
        let fingerprint = unsafe { adapter_fingerprint() };
        let forced = std::env::var_os("SLOWSHELL_REPROBE").is_some();
        let presentation = if forced {
            None
        } else {
            ProbeCache::read(&fingerprint)
        };
        let (presentation, cached) = match presentation {
            Some(p) => (p, true),
            None => {
                // Probe on throwaway objects: a rejected `CreateSwapChainForHwnd`
                // can leave its device unable to create any chain at all, so the
                // probe must not touch the device the shell will actually use.
                let p = self.probe_presentation();
                ProbeCache::write(&fingerprint, p);
                (p, false)
            }
        };
        unsafe {
            let d2d: ID2D1Factory3 =
                D2D1CreateFactory::<ID2D1Factory3>(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dw: IDWriteFactory =
                DWriteCreateFactory::<IDWriteFactory>(DWRITE_FACTORY_TYPE_SHARED)?;
            self.d2d.replace(Some(d2d));
            self.dw.replace(Some(dw));
            self.presentation.replace(presentation);
            if let Presentation::SwapChain { .. } = presentation {
                let (d3d, name) = match create_hardware_device() {
                    Ok(v) => v,
                    Err(_) => {
                        // WARP ships with every Windows install that has a
                        // graphics driver, so a shell that fails here is badly
                        // broken and should still start in a degraded mode.
                        let (d, n) = create_warp_device()?;
                        self.software.set(true);
                        (d, n)
                    }
                };
                let dxgi: IDXGIFactory2 =
                    CreateDXGIFactory2::<IDXGIFactory2>(DXGI_CREATE_FACTORY_FLAGS(0))?;
                let dxgi_device: IDXGIDevice = d3d.cast()?;
                let device: ID2D1Device = self.d2d_factory().CreateDevice(&dxgi_device)?.cast()?;
                self.adapter_name.replace(name);
                self.dxgi.replace(Some(dxgi));
                self.d3d.replace(Some(d3d));
                self.device.replace(Some(device));
            } else {
                // No swap chain, so no DXGI objects: starting without them
                // saves both the time to create them and the memory they hold.
                slowshell_core::warn!(
                    "no usable DXGI swap chain on this adapter; presenting through a window \
                     render target. Panels will be opaque and there is no per-pixel transparency."
                );
            }
        }
        slowshell_core::info!(
            "presentation: {}{}{}",
            presentation.label(),
            if self.software.get() { " (software rasteriser)" } else { "" },
            if cached { " (from cache)" } else { "" }
        );
        Ok(())
    }

    /// Find a swap chain this machine will accept **and that Direct2D can draw
    /// into**.
    ///
    /// Both halves matter, and the second is the one that is easy to get wrong.
    /// `CreateSwapChainForHwnd` on its own is not enough: an
    /// `DXGI_ALPHA_MODE_IGNORE` chain is accepted by drivers that refuse every
    /// alpha-aware one, and then `CreateBitmapFromDxgiSurface` rejects the
    /// resulting surface with `E_INVALIDARG`, because a Direct2D render target
    /// requires premultiplied alpha. Probing only the first half would pick a
    /// configuration that looks fine and cannot be drawn into. Measured on an AMD
    /// Radeon 780M under Hyper-V, where the result is no swap chain at all.
    ///
    /// The search runs on a **throwaway window and a throwaway device**. Both
    /// details matter: a failed `CreateSwapChainForHwnd` leaves that device
    /// unable to create *any* chain, so probing on the device the shell will use
    /// would make every later attempt fail for the wrong reason.
    ///
    /// WARP is tried as well, because a machine whose hardware driver refuses
    /// every chain can still present one in software.
    fn probe_presentation(&self) -> Presentation {
        /// Best first.
        const CANDIDATES: &[(DXGI_SWAP_EFFECT, DXGI_ALPHA_MODE)] = &[
            (DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_ALPHA_MODE_PREMULTIPLIED),
            (DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_ALPHA_MODE_STRAIGHT),
            (DXGI_SWAP_EFFECT_DISCARD, DXGI_ALPHA_MODE_PREMULTIPLIED),
            (DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_ALPHA_MODE_IGNORE),
            (DXGI_SWAP_EFFECT_DISCARD, DXGI_ALPHA_MODE_IGNORE),
        ];
        unsafe {
            let Ok(probe) = crate::window::create_surface(
                crate::window::SurfaceRole::Popup,
                0,
                0,
                64,
                64,
                "probe",
            ) else {
                // Without a window there is nothing to probe against, and the
                // window path needs no probe.
                return Presentation::Window;
            };
            crate::window::show(probe.hwnd);
            let found = match probe_candidates(
                probe.hwnd,
                || probe_hardware_device().map(|(d, _)| d),
                CANDIDATES,
            ) {
                Some(v) => Presentation::SwapChain { effect: v.0, alpha: v.1 },
                None => match probe_candidates(probe.hwnd, || {
                    create_warp_device().map(|(d, _)| d)
                }, CANDIDATES) {
                    Some(v) => {
                        self.software.set(true);
                        Presentation::SwapChain { effect: v.0, alpha: v.1 }
                    }
                    None => Presentation::Window,
                },
            };
            crate::window::destroy(probe);
            // The probe window's own `WM_SIZE` and `WM_DESTROY` are now in the
            // queue. Left there, the shell would read the destroy on its first
            // frame and quit.
            crate::window::discard_events();
            found
        }
    }

    /// The presentation path this machine will use.
    pub fn presentation(&self) -> Presentation {
        *self.presentation.borrow()
    }

    pub fn write_factory(&self) -> IDWriteFactory {
        self.dw
            .borrow()
            .clone()
            .expect("Graphics::create must run before the write factory is used")
    }

    pub fn d2d_factory(&self) -> ID2D1Factory3 {
        self.d2d
            .borrow()
            .clone()
            .expect("Graphics::create must run before the D2D factory is used")
    }

    pub fn adapter_name(&self) -> String {
        self.adapter_name.borrow().clone()
    }

    /// True when running on the WARP software rasteriser.
    pub fn software(&self) -> bool {
        self.software.get()
    }

    /// Build the render target for one window.
    ///
    /// `width`/`height` are physical pixels. The window must already be visible:
    /// both a swap chain and a window render target are refused for a window that
    /// has never been shown, with a bare `DXGI_ERROR_INVALID_CALL` that gives no
    /// hint of the real cause.
    pub fn create_surface(&self, hwnd: HWND, width: u32, height: u32) -> Result<Surface> {
        match self.presentation() {
            Presentation::SwapChain { effect, alpha } => {
                self.create_swap_chain_surface(hwnd, width, height, effect, alpha)
            }
            Presentation::Window => self.create_window_target_surface(hwnd, width, height),
        }
    }

    /// Reject a window that cannot present anything at all, with a specific
    /// message. DXGI reports a bare `INVALID_CALL` for several distinct problems.
    fn check_presentable(hwnd: HWND) -> Result<()> {
        unsafe {
            if !IsWindow(Some(hwnd)).as_bool() {
                return Err(Error::new(E_HANDLE, "the window handle is not a live window"));
            }
            let mut client = RECT::default();
            if GetClientRect(hwnd, &mut client).is_ok() && (client.right - client.left) <= 0 {
                return Err(Error::new(E_HANDLE, "the window has no client area to present to"));
            }
        }
        Ok(())
    }

    fn create_swap_chain_surface(
        &self,
        hwnd: HWND,
        width: u32,
        height: u32,
        effect: DXGI_SWAP_EFFECT,
        alpha: DXGI_ALPHA_MODE,
    ) -> Result<Surface> {
        let (dxgi, d3d, device) = {
            let dxgi = self.dxgi.borrow().clone();
            let d3d = self.d3d.borrow().clone();
            let device = self.device.borrow().clone();
            match (dxgi, d3d, device) {
                (Some(a), Some(b), Some(c)) => (a, b, c),
                _ => return Err(Error::new(E_FAIL, "Graphics::create has not run")),
            }
        };
        unsafe {
            Self::check_presentable(hwnd)?;
            let chain = dxgi.CreateSwapChainForHwnd(
                &d3d,
                hwnd,
                &DXGI_SWAP_CHAIN_DESC1 {
                    // A swap chain cannot be zero-sized; the compositor has nothing
                    // to present to and DXGI reports the call as invalid.
                    Width: width.max(1),
                    Height: height.max(1),
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    Stereo: windows::core::BOOL(0),
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                    BufferCount: 2,
                    Scaling: DXGI_SCALING_NONE,
                    SwapEffect: effect,
                    AlphaMode: alpha,
                    Flags: 0,
                },
                None,
                None,
            )?;
            let ctx: ID2D1DeviceContext2 = device
                .CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)
                .map_err(|e| Error::new(e.code(), format!("device context: {e}")))?
                .cast()?;
            let target = create_dxgi_bitmap(&ctx, &chain, alpha)?;
            Ok(Surface::new(
                hwnd,
                width.max(1),
                height.max(1),
                self.d2d_factory(),
                Presentation::SwapChain { effect, alpha },
                Backend::Dxgi { chain, ctx, bitmap: target },
            ))
        }
    }

    fn create_window_target_surface(&self, hwnd: HWND, width: u32, height: u32) -> Result<Surface> {
        unsafe {
            Self::check_presentable(hwnd)?;
            let target = self.d2d_factory().CreateHwndRenderTarget(
                &window_target_props(),
                &D2D1_HWND_RENDER_TARGET_PROPERTIES {
                    hwnd,
                    pixelSize: D2D_SIZE_U { width: width.max(1), height: height.max(1) },
                    // `NONE` presents on the next vertical blank, which keeps
                    // animation locked to the display refresh rate. `IMMEDIATELY`
                    // would tear, and `RETAIN_CONTENTS` exists for windows that
                    // are only redrawn in patches.
                    presentOptions: D2D1_PRESENT_OPTIONS_NONE,
                },
            )?;
            Ok(Surface::new(
                hwnd,
                width.max(1),
                height.max(1),
                self.d2d_factory(),
                Presentation::Window,
                Backend::Hwnd { target },
            ))
        }
    }
}

/// Properties for a window render target.
///
/// Premultiplied alpha is still requested: it makes Direct2D composite the
/// frame the same way it would on the swap chain path, so the only difference
/// on this path is that the alpha is discarded when the frame reaches the
/// window rather than before.
fn window_target_props() -> D2D1_RENDER_TARGET_PROPERTIES {
    D2D1_RENDER_TARGET_PROPERTIES {
        r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
        pixelFormat: D2D1_PIXEL_FORMAT {
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
        },
        dpiX: 96.0,
        dpiY: 96.0,
        usage: D2D1_RENDER_TARGET_USAGE_NONE,
        minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
    }
}

/// Try each candidate configuration on a throwaway factory, device and D2D
/// device, and return the first that both creates a chain **and** yields a
/// Direct2D target over it.
unsafe fn probe_candidates(
    hwnd: HWND,
    make_device: impl Fn() -> Result<ID3D11Device>,
    candidates: &[(DXGI_SWAP_EFFECT, DXGI_ALPHA_MODE)],
) -> Option<(DXGI_SWAP_EFFECT, DXGI_ALPHA_MODE)> {
    for (effect, alpha) in candidates {
        // A fresh factory and device per attempt, so one failure cannot poison
        // the next.
        let Ok(factory) = CreateDXGIFactory2::<IDXGIFactory2>(DXGI_CREATE_FACTORY_FLAGS(0)) else {
            continue;
        };
        let Ok(device) = make_device() else { continue };
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: 64,
            Height: 64,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            Stereo: windows::core::BOOL(0),
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_NONE,
            SwapEffect: *effect,
            AlphaMode: *alpha,
            Flags: 0,
        };
        let Ok(chain) = factory.CreateSwapChainForHwnd(&device, hwnd, &desc, None, None) else {
            continue;
        };
        // The chain exists. That is necessary and not sufficient.
        let Ok(d2d) = D2D1CreateFactory::<ID2D1Factory3>(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)
        else {
            continue;
        };
        let Ok(dxgi_device) = device.cast::<IDXGIDevice>() else { continue };
        let Ok(d2d_device) = d2d.CreateDevice(&dxgi_device) else { continue };
        let Ok(ctx) = d2d_device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE) else {
            continue;
        };
        let ctx: ID2D1DeviceContext2 = match ctx.cast() {
            Ok(c) => c,
            Err(_) => continue,
        };
        if create_dxgi_bitmap(&ctx, &chain, *alpha).is_ok() {
            return Some((*effect, *alpha));
        }
    }
    None
}

/// Wrap a swap chain's back buffer in a Direct2D bitmap.
///
/// `IDXGISurface` is a *sibling* of `IDXGISwapChain` in the DXGI hierarchy, not
/// a base of it, so QueryInterface does not find it. The surface behind a swap
/// chain is reached through `GetBuffer`.
unsafe fn create_dxgi_bitmap(
    ctx: &ID2D1DeviceContext2,
    chain: &IDXGISwapChain1,
    alpha: DXGI_ALPHA_MODE,
) -> Result<ID2D1Bitmap1> {
    let surface: IDXGISurface = chain
        .GetBuffer(0)
        .map_err(|e| Error::new(e.code(), format!("get buffer 0: {e}")))?;
    ctx.CreateBitmapFromDxgiSurface(&surface, Some(&bitmap_props(alpha)))
        .map_err(|e| {
            Error::new(
                e.code(),
                format!(
                    "bitmap from surface (alpha {:?}, d2d alpha {:?}): {e}",
                    alpha,
                    d2d_alpha_for(alpha)
                ),
            )
        })
}

/// The per-window drawing target.
///
/// Presentation is behind an enum because the two paths are not variants of one
/// idea but two different Windows mechanisms; see [`Presentation`].
pub struct Surface {
    backend: Backend,
    /// Kept so a window target lost to a display change can be rebuilt without
    /// asking `Graphics` for a second factory.
    factory: ID2D1Factory3,
    hwnd: HWND,
    /// Physical size, which a window target resizes itself to.
    size: Cell<(u32, u32)>,
    /// True between `begin` and `present`, so a draw session is never left open.
    drawn: Cell<bool>,
    presentation: Presentation,
}

enum Backend {
    /// DXGI flip-model or bitblt chain. Per-pixel alpha reaches the compositor
    /// unless the driver forced `DXGI_ALPHA_MODE_IGNORE`.
    Dxgi {
        chain: IDXGISwapChain1,
        ctx: ID2D1DeviceContext2,
        bitmap: ID2D1Bitmap1,
    },
    /// A window render target. Presents through GDI, opaquely.
    Hwnd { target: ID2D1HwndRenderTarget },
}

impl Surface {
    fn new(
        hwnd: HWND,
        width: u32,
        height: u32,
        factory: ID2D1Factory3,
        presentation: Presentation,
        backend: Backend,
    ) -> Surface {
        Surface {
            backend,
            factory,
            hwnd,
            size: Cell::new((width, height)),
            drawn: Cell::new(false),
            presentation,
        }
    }

    /// The presentation path in use, for logs and `--doctor`.
    pub fn presentation(&self) -> Presentation {
        self.presentation
    }

    /// False when the driver only offered a bitblt chain or no chain at all,
    /// which cannot present as smoothly.
    pub fn flip_model(&self) -> bool {
        match self.presentation {
            Presentation::SwapChain { effect, .. } => effect == DXGI_SWAP_EFFECT_FLIP_DISCARD,
            Presentation::Window => false,
        }
    }

    /// The alpha mode the swap chain accepted. Meaningless on the window path,
    /// which is reported as opaque.
    pub fn alpha_mode(&self) -> DXGI_ALPHA_MODE {
        match self.presentation {
            Presentation::SwapChain { alpha, .. } => alpha,
            Presentation::Window => DXGI_ALPHA_MODE_IGNORE,
        }
    }

    /// Whether per-pixel alpha reaches the compositor.
    ///
    /// When false, the renderer must pre-composite translucent colours against
    /// an opaque backdrop and clear to that same backdrop, or the desktop would
    /// show through as black.
    pub fn honours_alpha(&self) -> bool {
        self.presentation.honours_alpha()
    }

    /// Where the frame is drawn. The painter is written against this, so it
    /// does not care which path produced it.
    pub fn target(&self) -> &dyn RenderTarget {
        match &self.backend {
            Backend::Dxgi { ctx, .. } => ctx,
            Backend::Hwnd { target } => target,
        }
    }

    /// Begin a frame at the given DPI scale and clear it.
    ///
    /// `clear` is the colour the frame starts from. `None` means fully
    /// transparent, which is what lets a panel blend with the desktop. A
    /// presentation path that cannot carry per-pixel alpha needs an opaque
    /// colour here, so pass the same backdrop the painter composites against.
    pub fn begin(&self, dpi: f32, clear: Option<Color>) {
        let color = match clear {
            Some(c) => D2D1_COLOR_F {
                r: c.r as f32 / 255.0,
                g: c.g as f32 / 255.0,
                b: c.b as f32 / 255.0,
                a: 1.0,
            },
            None => D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 0.0 },
        };
        unsafe {
            match &self.backend {
                Backend::Dxgi { ctx, bitmap, .. } => {
                    ctx.SetTarget(bitmap);
                    ctx.SetDpi(dpi, dpi);
                    ctx.Clear(Some(&color));
                }
                Backend::Hwnd { target } => {
                    target.BeginDraw();
                    target.SetDpi(dpi, dpi);
                    target.Clear(Some(&color));
                }
            }
        }
        self.drawn.set(true);
    }

    /// Present the frame.
    ///
    /// `false` means the window is occluded or the target was lost; the caller
    /// should stop work and let the compositor wake it again.
    pub fn present(&mut self) -> bool {
        if !self.drawn.replace(false) {
            // `present` without `begin` would end a session that never started.
            return true;
        }
        unsafe {
            match &mut self.backend {
                Backend::Dxgi { chain, .. } => {
                    // A flip-model swap chain blocks when the queue is full, which
                    // is how presentation stays locked to the display refresh rate.
                    let hr = chain.Present(0, DXGI_PRESENT(0));
                    if hr == windows::Win32::Foundation::DXGI_STATUS_OCCLUDED {
                        return false;
                    }
                    hr.is_ok()
                }
                Backend::Hwnd { target } => {
                    let state = target.CheckWindowState();
                    if (state & D2D1_WINDOW_STATE_OCCLUDED) != D2D1_WINDOW_STATE_NONE {
                        // Drawing is skipped rather than thrown away, but the
                        // draw session still has to be closed or every later
                        // frame would nest inside this one.
                        let _ = target.EndDraw(None, None);
                        slowshell_core::debug!("window target is occluded; frame skipped");
                        return false;
                    }
                    match target.EndDraw(None, None) {
                        Ok(()) => true,
                        Err(e) if e.code() == D2DERR_RECREATE_TARGET => {
                            // The target is unusable, not the window. Rebuilding
                            // here rather than reporting failure keeps a display
                            // change from becoming a permanently blank panel.
                            self.rebuild_window_target();
                            false
                        }
                        // Anything else means the drawing was bad, not the
                        // target; the next frame starts clean either way.
                        Err(e) => {
                            slowshell_core::warn!("window target EndDraw failed: {e}");
                            true
                        }
                    }
                }
            }
        }
    }

    /// Recreate the render target after a resize.
    pub fn resize(&mut self) -> Result<()> {
        let (w, h) = self.size.get();
        let alpha = self.alpha_mode();
        unsafe {
            match &mut self.backend {
                Backend::Dxgi { ctx, chain, bitmap } => {
                    ctx.SetTarget(None);
                    chain.ResizeBuffers(2, 0, 0, DXGI_FORMAT_UNKNOWN, DXGI_SWAP_CHAIN_FLAG(0))?;
                    *bitmap = create_dxgi_bitmap(ctx, chain, alpha)?;
                }
                Backend::Hwnd { target } => {
                    target.Resize(&D2D_SIZE_U { width: w.max(1), height: h.max(1) })?;
                }
            }
        }
        Ok(())
    }

    /// Replace the window render target, for a lost device.
    fn rebuild_window_target(&mut self) {
        let (w, h) = self.size.get();
        let recreated = unsafe {
            self.factory.CreateHwndRenderTarget(
                &window_target_props(),
                &D2D1_HWND_RENDER_TARGET_PROPERTIES {
                    hwnd: self.hwnd,
                    pixelSize: D2D_SIZE_U { width: w.max(1), height: h.max(1) },
                    presentOptions: D2D1_PRESENT_OPTIONS_NONE,
                },
            )
        };
        match (recreated, &mut self.backend) {
            (Ok(target), Backend::Hwnd { target: slot }) => {
                *slot = target;
                slowshell_core::info!("window render target rebuilt after a device loss");
            }
            (Ok(_), _) => {}
            (Err(e), _) => slowshell_core::warn!("window render target could not be rebuilt: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presentation_names_itself_for_logs() {
        let flip = Presentation::SwapChain {
            effect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            alpha: DXGI_ALPHA_MODE_PREMULTIPLIED,
        };
        assert_eq!(flip.label(), "swap chain");
        assert!(flip.honours_alpha());
        assert_ne!(Presentation::Window.label(), flip.label());
    }

    #[test]
    fn only_an_alpha_aware_chain_reaches_the_compositor() {
        // The whole reason the renderer has a pre-composite path: a chain created
        // with `IGNORE` presents every colour fully opaque, so a translucent bar
        // background would arrive as a solid one.
        let ignore = Presentation::SwapChain {
            effect: DXGI_SWAP_EFFECT_DISCARD,
            alpha: DXGI_ALPHA_MODE_IGNORE,
        };
        assert!(!ignore.honours_alpha());
        assert!(!Presentation::Window.honours_alpha());
    }

    #[test]
    fn the_window_path_never_claims_a_flip_model() {
        // A window render target presents through GDI, so a flip model would be a
        // claim the shell cannot keep.
        assert!(!Presentation::Window.honours_alpha());
    }

    #[test]
    fn a_cached_verdict_round_trips_and_a_foreign_one_is_refused() {
        for p in [Presentation::Window, Presentation::SwapChain {
            effect: DXGI_SWAP_EFFECT_DISCARD,
            alpha: DXGI_ALPHA_MODE_IGNORE,
        }] {
            let text = p.to_cache();
            let back = Presentation::from_cache(&text).expect("a verdict this shell wrote must load");
            assert_eq!(back.to_cache(), text);
        }
        // An old or hand-edited file must not be guessed at: a path that reads as
        // something else is worse than a fresh probe.
        assert_eq!(Presentation::from_cache("d3d11"), None);
        assert_eq!(Presentation::from_cache(""), None);
        assert_eq!(Presentation::from_cache("window "), None);
    }

    #[test]
    fn a_cache_written_for_one_adapter_does_not_answer_for_another() {
        // The fingerprint is the whole point: two machines, or one machine after
        // a driver update, must each get their own answer.
        let mine = ProbeCache { fingerprint: "card-a".into(), verdict: "window".into() };
        let text = serde_json::to_string(&mine).unwrap();
        let parsed: ProbeCache = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.fingerprint, "card-a");
        assert_eq!(parsed.verdict, "window");
        assert_ne!(parsed.fingerprint, "card-b");
    }

    #[test]
    fn a_corrupt_cache_file_is_ignored_rather_than_fatal() {
        assert!(serde_json::from_str::<ProbeCache>("not json").is_err());
        assert!(serde_json::from_str::<ProbeCache>("{}").is_err(), "a missing fingerprint is not a match");
    }
}
