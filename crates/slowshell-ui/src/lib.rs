//! # Slowshell UI
//!
//! The retained widget tree, the layout engine and the paint pass.
//!
//! This crate knows about rectangles, colours and element kinds. It does not know
//! about HWNDs, Direct2D or the reactive graph beyond reading a `PropId` — which
//! is what lets the whole of it be unit tested without a GPU or a display.
//!
//! ## The frame
//!
//! ```text
//!   reactor  ──read──▶  resolve_values   (which elements changed)
//!                             │
//!                             ▼
//!   layout(element tree, bounds)  ──▶  element.rect
//!                             │
//!                             ▼
//!   paint(tree, painter)      ──▶  pixels + hit regions
//! ```

pub mod element;
pub mod layout;
pub mod paint;
pub mod style;

pub use element::{civil_from_unix, civil_to_unix,
    ClockFormat, Dyn, Element, ElementId, ElementKind, Handler,
};
pub use layout::{layout, panel_intrinsic, resolve_values, ElementIdKey, Intrinsic, Measurer, Resolved};
pub use paint::{collect_regions, paint, take_output, PaintOutput};
pub use style::{
    ColorRef, CrossAlign, Edges, MainAlign, Position, ShadowStyle, Size, Style, TextAlign, Theme,
};

pub use slowshell_win::render::Rect;
pub use slowshell_win::render::text::{TextEngine, TextStyle};

/// Seconds since the Unix epoch, UTC.
///
/// The UI layer formats civil time itself so the clock is deterministic and
/// testable; the local-time offset is applied by the platform layer when it
/// publishes the clock value.
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
