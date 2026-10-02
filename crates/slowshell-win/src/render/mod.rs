//! The renderer: Direct2D device, text engine and the painting API.
//!
//! The UI layer never touches a Direct2D type. It drives [`Painter`], whose
//! signatures are in logical pixels and [`slowshell_core::Color`], which is what
//! lets `slowshell-ui` be compiled and unit tested without a GPU.

pub mod device;
pub mod painter;
pub mod target;
pub mod text;

pub use device::{Graphics, Presentation, Surface};
pub use painter::{measure, point_on_circle, ring_point, Painter, Rect, Shadow};
pub use target::RenderTarget;
pub use text::{Align, Ellipsis, FontMetrics, TextEngine, TextStyle};
