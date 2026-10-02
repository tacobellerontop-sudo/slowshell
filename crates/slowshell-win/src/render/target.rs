//! One drawing API over Direct2D's two families of render target.
//!
//! Direct2D has two unrelated ways to get pixels onto a window, and they share
//! almost nothing but the `ID2D1RenderTarget` interface at the root of the
//! hierarchy:
//!
//! * **`ID2D1DeviceContext2`** — the modern path. Backed by DXGI, targets a
//!   bitmap over a swap chain buffer, and can present with per-pixel alpha.
//! * **`ID2D1HwndRenderTarget`** (and its `ID2D1DCRenderTarget` sibling) — the
//!   older path. Created straight from the factory, targets the window's own
//!   back buffer, and presents through GDI without per-pixel alpha.
//!
//! A shell cannot assume it will get the first one. Some drivers refuse every
//! alpha-aware swap chain configuration *and* then refuse a Direct2D bitmap over
//! the one chain they do accept, which leaves no swap-chain presentation at
//! all. See [`crate::render::device::Graphics`]. The window path is slower and
//! opaque, but it works on hardware where the modern path does not exist, and a
//! shell that cannot present at all is worthless.
//!
//! So [`Painter`](super::Painter) is written against this trait, and every
//! operation is implemented once, in terms of `ID2D1RenderTarget`, which is
//! reachable from all three interfaces by deref coercion. That keeps one copy
//! of the drawing code rather than three, and means a fourth path would only
//! need one new `impl AsRenderTarget`.

use windows::core::Result;
use windows::Win32::Graphics::Direct2D::Common::{D2D1_COLOR_F, D2D_RECT_F};
use windows::Win32::Graphics::Direct2D::{
    D2D1_ANTIALIAS_MODE, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_DRAW_TEXT_OPTIONS,
    D2D1_ELLIPSE, D2D1_LAYER_PARAMETERS, D2D1_ROUNDED_RECT, ID2D1Brush, ID2D1DCRenderTarget,
    ID2D1DeviceContext2, ID2D1HwndRenderTarget, ID2D1RenderTarget, ID2D1SolidColorBrush,
};
use windows::Win32::Graphics::DirectWrite::{DWRITE_MEASURING_MODE_NATURAL, IDWriteTextFormat};

/// Every drawing operation the widget layer performs.
///
/// Object safe on purpose: a [`crate::render::Surface`] hands out a
/// `&dyn RenderTarget` so the painter does not care which presentation path
/// produced it.
pub trait RenderTarget {
    /// The interface every method below is defined in terms of.
    fn base(&self) -> &ID2D1RenderTarget;

    /// A brush for one flat colour.
    ///
    /// Called once per distinct colour per frame by the painter's brush cache.
    /// With `None` properties the brush adopts the target's current DPI, which
    /// the shell sets before the first draw of a frame.
    fn create_brush(&self, color: &D2D1_COLOR_F) -> Result<ID2D1SolidColorBrush> {
        // SAFETY: the target is alive for the call, and Direct2D copies both the
        // colour and the properties.
        unsafe { self.base().CreateSolidColorBrush(color, None) }
    }

    fn set_dpi(&self, dpi_x: f32, dpi_y: f32) {
        unsafe { self.base().SetDpi(dpi_x, dpi_y) }
    }

    /// Clear the whole target. `None` is black.
    fn clear(&self, color: Option<&D2D1_COLOR_F>) {
        unsafe { self.base().Clear(color.map(|c| c as *const D2D1_COLOR_F)) }
    }

    fn fill_rect(&self, rect: D2D_RECT_F, brush: &ID2D1Brush) {
        unsafe { self.base().FillRectangle(&rect, brush) };
    }

    fn draw_rect(&self, rect: D2D_RECT_F, brush: &ID2D1Brush, width: f32) {
        unsafe { self.base().DrawRectangle(&rect, brush, width, None) };
    }

    fn fill_round_rect(&self, rect: &D2D1_ROUNDED_RECT, brush: &ID2D1Brush) {
        unsafe { self.base().FillRoundedRectangle(rect, brush) };
    }

    fn draw_round_rect(&self, rect: &D2D1_ROUNDED_RECT, brush: &ID2D1Brush, width: f32) {
        unsafe { self.base().DrawRoundedRectangle(rect, brush, width, None) };
    }

    fn fill_ellipse(&self, ellipse: &D2D1_ELLIPSE, brush: &ID2D1Brush) {
        unsafe { self.base().FillEllipse(ellipse, brush) };
    }

    fn draw_ellipse(&self, ellipse: &D2D1_ELLIPSE, brush: &ID2D1Brush, width: f32) {
        unsafe { self.base().DrawEllipse(ellipse, brush, width, None) };
    }

    fn draw_line(
        &self,
        from: windows_numerics::Vector2,
        to: windows_numerics::Vector2,
        brush: &ID2D1Brush,
        width: f32,
    ) {
        unsafe { self.base().DrawLine(from, to, brush, width, None) };
    }

    /// Draw a string with a DirectWrite format.
    ///
    /// This is the context's `DrawText` rather than `IDWriteTextLayout::Draw`,
    /// because the latter is the custom-renderer path and takes no brush.
    fn draw_text(
        &self,
        text: &[u16],
        format: &IDWriteTextFormat,
        rect: &D2D_RECT_F,
        brush: &ID2D1Brush,
        options: D2D1_DRAW_TEXT_OPTIONS,
    ) {
        unsafe {
            self.base().DrawText(
                text,
                format,
                rect,
                brush,
                options,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }
    }

    /// Begin a group of drawing commands at reduced opacity.
    ///
    /// Uses `D2D1_LAYER_PARAMETERS` rather than the `…1` form the device context
    /// prefers, because the window render target has no `1` form and the
    /// difference at bar scale is a layer size hint.
    fn push_layer(&self, opacity: f32) {
        let mut params = D2D1_LAYER_PARAMETERS::default();
        params.opacity = opacity.clamp(0.0, 1.0);
        params.maskAntialiasMode = D2D1_ANTIALIAS_MODE_PER_PRIMITIVE;
        unsafe { self.base().PushLayer(&params, None::<&windows::Win32::Graphics::Direct2D::ID2D1Layer>) };
    }

    fn pop_layer(&self) {
        unsafe { self.base().PopLayer() };
    }

    fn push_clip(&self, rect: D2D_RECT_F, antialias: D2D1_ANTIALIAS_MODE) {
        unsafe { self.base().PushAxisAlignedClip(&rect, antialias) };
    }

    fn pop_clip(&self) {
        unsafe { self.base().PopAxisAlignedClip() };
    }
}

/// Sealed: the Direct2D interfaces that sit on an `ID2D1RenderTarget`.
///
/// Implemented for the interfaces, not for the trait objects, so that the
/// [`RenderTarget`] blanket impl below can dispatch once.
trait AsRenderTarget {
    fn render_target(&self) -> &ID2D1RenderTarget;
}

macro_rules! as_render_target {
    ($($ty:ty),* $(,)?) => {
        $(
            impl AsRenderTarget for $ty {
                fn render_target(&self) -> &ID2D1RenderTarget {
                    // A deref coercion: each of these interfaces derives from
                    // `ID2D1RenderTarget`, however many generations lie between.
                    self
                }
            }
        )*
    };
}

as_render_target!(ID2D1DeviceContext2, ID2D1HwndRenderTarget, ID2D1DCRenderTarget);

/// Every Direct2D interface reachable here draws through one implementation.
///
/// A blanket impl is what makes this file short: the three interfaces differ
/// only in which vtable slot `RenderTarget` ends up calling, and the code above
/// is that code.
impl<T: AsRenderTarget> RenderTarget for T {
    fn base(&self) -> &ID2D1RenderTarget {
        AsRenderTarget::render_target(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A compile-time check that all three presentation paths satisfy the
    /// painter's contract. If a new path is ever added, it must be listed here
    /// too — the compiler is the test.
    fn assert_render_target<T: RenderTarget>() {}

    #[test]
    fn every_presentation_path_is_a_render_target() {
        assert_render_target::<ID2D1DeviceContext2>();
        assert_render_target::<ID2D1HwndRenderTarget>();
        assert_render_target::<ID2D1DCRenderTarget>();
    }

    #[test]
    fn device_contexts_deref_down_to_a_render_target() {
        // The blanket impl depends on this coercion, so assert it directly
        // rather than trusting it to keep compiling.
        fn takes_render_target(_: &ID2D1RenderTarget) {}
        fn probe(t: &ID2D1DeviceContext2, h: &ID2D1HwndRenderTarget, d: &ID2D1DCRenderTarget) {
            takes_render_target(t);
            takes_render_target(h);
            takes_render_target(d);
        }
        let _ = probe;
    }
}
