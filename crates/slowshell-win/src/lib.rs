//! Windows platform layer: windowing, monitors, input, system data and the
//! Direct2D renderer.
//!
//! Every Windows API the shell uses is reached through this crate. Nothing above
//! it links against `windows` directly, which keeps the boundary honest and makes
//! a future platform implementable behind the same surface.
//!
//! ## Module map
//!
//! | Module | Responsibility |
//! |---|---|
//! | [`com`] | COM and per-monitor DPI initialisation |
//! | [`capture`] | Reading a window's pixels back, for diagnostics |
//! | [`window`] | HWND creation, styles, message pump, input routing |
//! | [`backdrop`] | Acrylic / Mica / blur via DWM |
//! | [`hit_test`] | Per-pixel click-through regions |
//! | [`input`] | Logical keys, hotkey parsing |
//! | [`ipc_pipe`] | The named-pipe transport `shellctl` talks over |
//! | [`monitors`] | Display enumeration, work areas, reserved zones |
//! | [`render`] | Direct2D device, brushes, text, draw commands |

pub mod appbar;
pub mod backdrop;
pub mod capture;
pub mod clipboard;
pub mod com;
pub mod hit_test;
pub mod input;
pub mod ipc_pipe;
pub mod launch;
pub mod media;
pub mod monitors;
pub mod notify;
pub mod platform;
pub mod render;
pub mod window;

pub use backdrop::Backdrop;
pub use com::{init_process, init_thread, init_thread_dpi, local_civil, utc_offset_seconds};
pub use hit_test::{HitRegion, HitTest};
pub use input::{InputEvent, Key, KeyModifiers};
pub use appbar::{AppBar, Edge};
pub use media::{MediaSnapshot, MediaState};
pub use monitors::{Monitor, Monitors};
pub use render::{
    Align, Graphics, Painter, Presentation, Rect, RenderTarget, Shadow, Surface, TextEngine,
    TextStyle,
};
pub use window::{
    create_surface, destroy, hide, is_visible, post_frame, pump, request_frame, scale_of,
    set_always_on_top, set_bounds, set_click_through, show, SurfaceHandle, SurfaceRole, WindowEvent,
    WindowState,
};
