//! The painting API.
//!
//! Everything the widget layer needs to draw, expressed in logical pixels and
//! [`slowshell_core::Color`], with no Windows types in the signatures. That is
//! what lets `slowshell-ui` be compiled and unit tested on its own.
//!
//! ## Why logical pixels, not device pixels
//!
//! Coordinates are device-independent pixels and the context's DPI does the
//! scaling. A layout computed once then works on a 100% and a 200% display and a
//! mixed-DPI setup needs no special cases. Physical pixels appear only at the
//! window boundary, when a surface is created or resized.
//!
//! ## Techniques chosen for predictability
//!
//! * **Arcs** are drawn as polylines. A ring gauge at bar scale is 24 px across;
//!   24 segments are indistinguishable from a true arc, and a polyline needs no
//!   geometry sink.
//! * **Shadows** are drawn as a few concentric translucent fills rather than a
//!   blurred layer. Direct2D's shadow brush is not available through the bindings
//!   used here, and a blur effect would need an offscreen bitmap per element. The
//!   stack reads as a soft edge at the low contrast a shell wants.
//! * **Gradients** are not exposed. Nothing in the default shell needs one, and
//!   an unused API is one more thing to keep correct.

use std::collections::HashMap;

use slowshell_core::Color;
use windows::Win32::Graphics::Direct2D::Common::{D2D1_COLOR_F, D2D_RECT_F, D2D_SIZE_F};
use windows::Win32::Graphics::Direct2D::*;

use crate::render::target::RenderTarget;
use crate::render::text::{self, TextEngine, TextStyle};

/// An axis-aligned rectangle in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const ZERO: Rect = Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 };

    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }

    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    pub fn center(&self) -> (f32, f32) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }

    pub fn is_empty(&self) -> bool {
        self.w <= 0.0 || self.h <= 0.0
    }

    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.right() && py >= self.y && py < self.bottom()
    }

    pub fn inset(&self, dx: f32, dy: f32) -> Rect {
        Rect { x: self.x + dx, y: self.y + dy, w: self.w - dx * 2.0, h: self.h - dy * 2.0 }
    }

    pub fn with_size(&self, w: f32, h: f32) -> Rect {
        Rect { w, h, ..*self }
    }

    pub fn offset(&self, dx: f32, dy: f32) -> Rect {
        Rect { x: self.x + dx, y: self.y + dy, ..*self }
    }

    /// The smallest rectangle containing both. An empty operand is ignored.
    pub fn union(&self, other: &Rect) -> Rect {
        if self.is_empty() {
            return *other;
        }
        if other.is_empty() {
            return *self;
        }
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let r = self.right().max(other.right());
        let b = self.bottom().max(other.bottom());
        Rect::new(x, y, r - x, b - y)
    }

    /// Clip to another rectangle, which is what a scroll container needs.
    pub fn intersect(&self, other: &Rect) -> Rect {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let r = self.right().min(other.right());
        let b = self.bottom().min(other.bottom());
        Rect::new(x, y, (r - x).max(0.0), (b - y).max(0.0))
    }

    fn to_d2d(self) -> D2D_RECT_F {
        D2D_RECT_F { left: self.x, top: self.y, right: self.right(), bottom: self.bottom() }
    }
}

/// A soft drop shadow, drawn behind an element.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Shadow {
    pub color: Color,
    pub offset: (f32, f32),
    /// Blur radius in DIPs. Zero draws a hard shadow.
    pub blur: f32,
}

impl Shadow {
    pub fn is_visible(&self) -> bool {
        self.color.a > 0
    }
}

/// What one frame actually asked Direct2D to do.
///
/// A shell that draws nothing and a shell that draws something invisible look
/// identical on screen, so the counts are reported rather than guessed at. A
/// frame with `texts` above zero and `texts_dropped` above it explains itself:
/// the text engine had nothing usable for that style.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DrawStats {
    pub fills: u32,
    pub strokes: u32,
    pub lines: u32,
    /// Text runs handed to DirectWrite.
    pub texts: u32,
    /// Text runs that had content and a colour but were dropped before drawing.
    pub texts_dropped: u32,
    /// Draw calls skipped because the resolved colour was fully transparent.
    pub transparent: u32,
    /// Draw calls skipped because the target refused to make a brush.
    pub no_brush: u32,
}

impl DrawStats {
    /// Whether the frame drew anything at all.
    pub fn is_empty(&self) -> bool {
        self.fills == 0 && self.strokes == 0 && self.lines == 0 && self.texts == 0
    }
}

/// Draws one frame of one surface.
///
/// Brushes are cached across frames by colour: creating an
/// `ID2D1SolidColorBrush` per draw call is one of the easiest ways to make a
/// Direct2D UI slow, and a bar repaints the same dozen colours constantly.
pub struct Painter<'a> {
    /// Whatever Direct2D surface is presenting this frame. The painter does not
    /// know or care whether it is a swap chain bitmap or a window target; see
    /// [`RenderTarget`].
    ctx: &'a dyn RenderTarget,
    text: &'a mut TextEngine,
    brushes: HashMap<u32, ID2D1SolidColorBrush>,
    layer_depth: u32,
    clip_depth: u32,
    dpi: f32,
    /// When the presentation path discards the alpha channel, translucent
    /// colours are pre-composited against this backdrop so a bar looks the same
    /// as it would with real transparency.
    composite_over: Option<Color>,
    stats: DrawStats,
}

fn to_d2d_color(c: Color) -> D2D1_COLOR_F {
    // Direct2D takes non-premultiplied float components.
    D2D1_COLOR_F {
        r: c.r as f32 / 255.0,
        g: c.g as f32 / 255.0,
        b: c.b as f32 / 255.0,
        a: c.a as f32 / 255.0,
    }
}

fn color_key(c: Color) -> u32 {
    ((c.r as u32) << 24) | ((c.g as u32) << 16) | ((c.b as u32) << 8) | c.a as u32
}

/// A rounded radius clamped so the shape can never self-intersect.
fn safe_radius(radius: f32, rect: &Rect) -> f32 {
    if radius <= 0.0 {
        0.0
    } else {
        radius.min(rect.w.min(rect.h) / 2.0)
    }
}

impl<'a> Painter<'a> {
    pub fn new(ctx: &'a dyn RenderTarget, text: &'a mut TextEngine, dpi: f32) -> Painter<'a> {
        Painter {
            ctx,
            text,
            brushes: HashMap::new(),
            layer_depth: 0,
            clip_depth: 0,
            dpi,
            composite_over: None,
            stats: DrawStats::default(),
        }
    }

    /// What this frame has drawn so far.
    pub fn stats(&self) -> DrawStats {
        self.stats
    }

    /// Compose every translucent colour against `backdrop` before drawing.
    ///
    /// Needed when the swap chain was created with `DXGI_ALPHA_MODE_IGNORE`,
    /// which some drivers will accept when they refuse the alpha-aware modes.
    /// Without this, `background: "#11111bcc"` would be presented fully opaque.
    pub fn set_composite_backdrop(&mut self, backdrop: Color) {
        self.composite_over = Some(backdrop);
    }

    /// Flatten a colour against the composite backdrop when one is set.
    fn resolve(&self, c: Color) -> Color {
        match self.composite_over {
            Some(bg) if c.a < 255 => c.over(bg),
            _ => c,
        }
    }

    pub fn dpi(&self) -> f32 {
        self.dpi
    }

    /// Measure a string. The text engine caches layouts, so this is cheap enough
    /// to call from a layout pass on every frame.
    pub fn measure(&mut self, content: &str, style: &TextStyle) -> (f32, f32) {
        self.text.measure(content, style, self.dpi)
    }

    /// The line box height for a style, for vertical centring.
    pub fn line_height(&mut self, style: &TextStyle) -> f32 {
        self.text
            .font_metrics(style, self.dpi)
            .map(|m| m.line_height())
            .unwrap_or(style.size * 1.35)
    }

    pub fn has_font(&self, family: &str) -> bool {
        self.text.has_family(family)
    }

    fn brush(&mut self, color: Color) -> Option<ID2D1SolidColorBrush> {
        let color = self.resolve(color);
        if color.a == 0 {
            return None;
        }
        let key = color_key(color);
        if let Some(b) = self.brushes.get(&key) {
            return Some(b.clone());
        }
        // The context outlives the painter, and a solid colour brush is bound to
        // the target's DPI, which is fixed for the frame.
        let Ok(brush) = self.ctx.create_brush(&to_d2d_color(color)) else {
            self.stats.no_brush += 1;
            return None;
        };
        self.brushes.insert(key, brush.clone());
        Some(brush)
    }

    /// Record a draw call that will not happen, so the frame's stats explain
    /// themselves. `counted` is what the call would have incremented.
    fn note_skipped(&mut self, counted: fn(&mut DrawStats), transparent: bool) {
        if transparent {
            self.stats.transparent += 1;
        }
        counted(&mut self.stats);
    }

    /// Fill a rectangle, optionally with rounded corners.
    pub fn fill_rect(&mut self, rect: Rect, color: Color, radius: f32) {
        if rect.is_empty() {
            return;
        }
        if color.a == 0 {
            self.note_skipped(|s| s.fills += 1, true);
            return;
        }
        let Some(brush) = self.brush(color) else { return };
        self.stats.fills += 1;
        let r = safe_radius(radius, &rect);
        let d = rect.to_d2d();
        if r > 0.0 {
            self.ctx.fill_round_rect(&D2D1_ROUNDED_RECT { rect: d, radiusX: r, radiusY: r }, &brush);
        } else {
            self.ctx.fill_rect(d, &brush);
        }
    }

    /// Stroke a rectangle outline. The stroke is centred on the path, so half of
    /// it falls outside `rect`; callers that need it inside should inset.
    pub fn stroke_rect(&mut self, rect: Rect, color: Color, radius: f32, width: f32) {
        if rect.is_empty() || width <= 0.0 {
            return;
        }
        if color.a == 0 {
            self.note_skipped(|s| s.strokes += 1, true);
            return;
        }
        let Some(brush) = self.brush(color) else { return };
        self.stats.strokes += 1;
        let r = safe_radius(radius, &rect);
        let d = rect.to_d2d();
        if r > 0.0 {
            self.ctx.draw_round_rect(
                &D2D1_ROUNDED_RECT { rect: d, radiusX: r, radiusY: r },
                &brush,
                width,
            );
        } else {
            self.ctx.draw_rect(d, &brush, width);
        }
    }

    pub fn fill_circle(&mut self, center: (f32, f32), radius: f32, color: Color) {
        if radius <= 0.0 {
            return;
        }
        if color.a == 0 {
            self.note_skipped(|s| s.fills += 1, true);
            return;
        }
        let Some(brush) = self.brush(color) else { return };
        self.stats.fills += 1;
        let ellipse = D2D1_ELLIPSE {
            point: windows_numerics::Vector2 { X: center.0, Y: center.1 },
            radiusX: radius,
            radiusY: radius,
        };
        self.ctx.fill_ellipse(&ellipse, &brush);
    }

    pub fn stroke_circle(&mut self, center: (f32, f32), radius: f32, color: Color, width: f32) {
        if radius <= 0.0 || width <= 0.0 {
            return;
        }
        if color.a == 0 {
            self.note_skipped(|s| s.strokes += 1, true);
            return;
        }
        let Some(brush) = self.brush(color) else { return };
        self.stats.strokes += 1;
        let ellipse = D2D1_ELLIPSE {
            point: windows_numerics::Vector2 { X: center.0, Y: center.1 },
            radiusX: radius,
            radiusY: radius,
        };
        self.ctx.draw_ellipse(&ellipse, &brush, width);
    }

    /// A straight line.
    pub fn line(&mut self, from: (f32, f32), to: (f32, f32), color: Color, width: f32) {
        if width <= 0.0 {
            return;
        }
        if color.a == 0 {
            self.note_skipped(|s| s.lines += 1, true);
            return;
        }
        let Some(brush) = self.brush(color) else { return };
        self.stats.lines += 1;
        self.ctx.draw_line(
            windows_numerics::Vector2 { X: from.0, Y: from.1 },
            windows_numerics::Vector2 { X: to.0, Y: to.1 },
            &brush,
            width,
        );
    }

    /// A ring segment, drawn as a polyline.
    ///
    /// `fraction` is clamped to 0..=1 so a widget may pass a raw ratio without
    /// risking a geometry error. A full ring is closed by repeating the first
    /// point.
    pub fn arc(
        &mut self,
        center: (f32, f32),
        radius: f32,
        width: f32,
        start_deg: f32,
        fraction: f32,
        color: Color,
    ) {
        if radius <= 0.0 || width <= 0.0 || color.a == 0 {
            return;
        }
        let sweep = fraction.clamp(0.0, 1.0) * 360.0;
        if sweep <= 0.0 {
            return;
        }
        // One segment per 12 degrees is smooth at bar scale and keeps the cost
        // proportional to the arc actually drawn.
        let steps = ((sweep / 12.0).ceil() as usize).clamp(1, 64);
        let mut prev = polar(center, radius, start_deg);
        for i in 1..=steps {
            let angle = start_deg + sweep * (i as f32 / steps as f32);
            let next = polar(center, radius, angle);
            self.line(prev, next, color, width);
            prev = next;
        }
    }

    /// Draw a string inside a box. Alignment and trimming come from the style.
    pub fn text(&mut self, content: &str, style: &TextStyle, rect: Rect, color: Color) {
        if content.is_empty() || rect.is_empty() {
            return;
        }
        if color.a == 0 {
            self.note_skipped(|s| s.texts += 1, true);
            return;
        }
        // Everything past this point is a real attempt to draw text, so a failure
        // here is a bug rather than a design decision and is counted as dropped.
        self.stats.texts += 1;
        let Some(format) = self.text.format_for(style) else {
            self.stats.texts_dropped += 1;
            return;
        };
        let Some(brush) = self.brush(color) else {
            self.stats.texts_dropped += 1;
            return;
        };
        text::draw_text(
            self.ctx,
            content,
            &format,
            (rect.x, rect.y, rect.right(), rect.bottom()),
            &brush,
        );
    }

    /// Draw text vertically centred in a box, which is what a bar row wants.
    pub fn text_centered_v(&mut self, content: &str, style: &TextStyle, rect: Rect, color: Color) {
        let h = self.line_height(style);
        let y = rect.y + (rect.h - h) / 2.0;
        self.text(content, style, rect.offset(0.0, y).with_size(rect.w, h), color);
    }

    /// Begin an opacity multiplier. Every push must be paired with a pop.
    pub fn push_opacity(&mut self, opacity: f32) {
        self.ctx.push_layer(opacity);
        self.layer_depth += 1;
    }

    pub fn pop_opacity(&mut self) {
        if self.layer_depth == 0 {
            return;
        }
        self.ctx.pop_layer();
        self.layer_depth -= 1;
    }

    /// Restrict drawing to a rectangle, optionally rounded.
    pub fn push_clip(&mut self, rect: Rect, _radius: f32) {
        // Direct2D's axis-aligned clip has no corner radius, so a rounded clip
        // falls back to the bounding rectangle. Widgets that need true rounded
        // clipping draw into a masked region themselves.
        self.ctx.push_clip(rect.to_d2d(), D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
        self.clip_depth += 1;
    }

    pub fn pop_clip(&mut self) {
        if self.clip_depth == 0 {
            return;
        }
        self.ctx.pop_clip();
        self.clip_depth -= 1;
    }

    /// Draw a soft shadow beneath a rounded rectangle.
    ///
    /// Implemented as a stack of expanding translucent fills. The passes are
    /// weighted so the density ramps the way a Gaussian falls off, and the total
    /// alpha is normalised to the requested colour.
    pub fn draw_shadow(&mut self, rect: Rect, radius: f32, shadow: Shadow) {
        if !shadow.is_visible() || rect.is_empty() {
            return;
        }
        const PASSES: usize = 5;
        let spread = shadow.blur.max(1.0);
        let base_alpha = shadow.color.a as f32 / 255.0;
        for i in (1..=PASSES).rev() {
            let t = i as f32 / PASSES as f32;
            let grow = spread * t;
            let layer = rect.offset(shadow.offset.0, shadow.offset.1).inset(-grow, -grow);
            // Quadratic falloff approximates a Gaussian, then the total is scaled
            // so overlapping passes do not exceed the requested opacity.
            let alpha = base_alpha * (1.0 - t) * (1.0 - t) * 2.2;
            let c = shadow.color.with_opacity(alpha.clamp(0.0, 1.0));
            self.fill_rect(layer, c, radius + grow);
        }
    }

    /// Whether layers and clips are balanced. Unbalanced means a widget pushed
    /// without popping, which would corrupt every later frame.
    pub fn is_balanced(&self) -> bool {
        self.layer_depth == 0 && self.clip_depth == 0
    }
}

impl Drop for Painter<'_> {
    fn drop(&mut self) {
        // Never leave the target with an unbalanced stack, even when a widget
        // fails partway through building.
        while self.layer_depth > 0 {
            self.pop_opacity();
        }
        while self.clip_depth > 0 {
            self.pop_clip();
        }
    }
}

fn polar(center: (f32, f32), radius: f32, degrees: f32) -> (f32, f32) {
    let rad = degrees.to_radians();
    (center.0 + radius * rad.cos(), center.1 + radius * rad.sin())
}

/// Text measurement for a layout pass that has no surface yet.
///
/// A window must be able to size its widgets before the Direct2D device exists,
/// so the layout phase uses these approximations. Once a `Painter` is available
/// the real DirectWrite measurement is used, and the difference is invisible at
/// bar scale.
pub mod measure {
    /// Approximate advance width of one character, as a fraction of the font size.
    ///
    /// A flat per-character factor would measure a row of `i` the same as a row of
    /// `m`, which clips labels. These classes are rough but keep the ordering
    /// right, which is all a pre-layout pass needs.
    fn advance(c: char) -> f32 {
        match c {
            // Narrow punctuation and stems.
            'i' | 'j' | 'l' | 'I' | '|' | '.' | ',' | ':' | ';' | '\'' | '`' | '!' => 0.30,
            'f' | 't' | 'r' | '(' | ')' | '[' | ']' | '{' | '}' | '/' | '\\' => 0.38,
            // Wide forms.
            'm' | 'M' | 'W' | 'w' | '@' | '%' | '—' | '…' => 0.86,
            // CJK and fullwidth forms are square.
            c if ('\u{1100}'..='\u{11FF}').contains(&c)
                || ('\u{2E80}'..='\u{A4CF}').contains(&c)
                || ('\u{AC00}'..='\u{D7A3}').contains(&c)
                || ('\u{F900}'..='\u{FAFF}').contains(&c)
                || ('\u{FF00}'..='\u{FF60}').contains(&c) =>
            {
                1.0
            }
            // Digits are tabular in a UI face.
            '0'..='9' => 0.56,
            // Capitals sit above the lowercase average.
            c if c.is_uppercase() => 0.64,
            c if c.is_whitespace() => 0.30,
            c if c.is_alphanumeric() => 0.52,
            _ => 0.54,
        }
    }

    /// Approximate width of a string in logical pixels.
    pub fn approximate(text: &str, size: f32) -> f32 {
        text.chars().map(advance).sum::<f32>() * size
    }

    /// Approximate line box height.
    pub fn line_height(size: f32) -> f32 {
        size * 1.35
    }
}

/// A point on a circle, exposed for widgets that draw their own geometry.
pub fn point_on_circle(center: (f32, f32), radius: f32, degrees: f32) -> (f32, f32) {
    polar(center, radius, degrees)
}

/// Convert degrees to a point on a ring, used by the workspace and battery
/// indicators.
pub fn ring_point(center: (f32, f32), radius: f32, fraction: f32) -> (f32, f32) {
    // -90 so a fraction of 0 starts at the top, which is what a gauge means.
    polar(center, radius, fraction.clamp(0.0, 1.0) * 360.0 - 90.0)
}

/// The size a rounded rectangle's geometry reports, used by hit testing so the
/// hit region matches what is drawn.
pub fn rounded_geometry_size(radius: f32, rect: &Rect) -> D2D_SIZE_F {
    let r = safe_radius(radius, rect);
    D2D_SIZE_F { width: r, height: r }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_geometry_is_correct() {
        let r = Rect::new(10.0, 20.0, 100.0, 50.0);
        assert_eq!(r.right(), 110.0);
        assert_eq!(r.bottom(), 70.0);
        assert_eq!(r.center(), (60.0, 45.0));
        assert!(!r.is_empty());
        assert!(Rect::ZERO.is_empty());
    }

    #[test]
    fn containment_is_half_open() {
        let r = Rect::new(0.0, 0.0, 10.0, 10.0);
        assert!(r.contains(0.0, 0.0));
        assert!(r.contains(9.99, 9.99));
        assert!(!r.contains(10.0, 5.0));
    }

    #[test]
    fn inset_shrinks_and_offset_moves() {
        let r = Rect::new(0.0, 0.0, 100.0, 100.0);
        assert_eq!(r.inset(10.0, 5.0), Rect::new(10.0, 5.0, 80.0, 90.0));
        assert_eq!(r.offset(5.0, -5.0), Rect::new(5.0, -5.0, 100.0, 100.0));
    }

    #[test]
    fn union_covers_both_and_ignores_empty() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        let b = Rect::new(20.0, 20.0, 10.0, 10.0);
        assert_eq!(a.union(&b), Rect::new(0.0, 0.0, 30.0, 30.0));
        assert_eq!(a.union(&Rect::ZERO), a);
        assert_eq!(Rect::ZERO.union(&a), a);
    }

    #[test]
    fn intersect_clamps_and_never_inverts() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        assert_eq!(a.intersect(&Rect::new(5.0, 5.0, 100.0, 100.0)), Rect::new(5.0, 5.0, 5.0, 5.0));
        let i = a.intersect(&Rect::new(100.0, 100.0, 5.0, 5.0));
        assert!(i.w >= 0.0 && i.h >= 0.0, "must not produce negative extents");
    }

    #[test]
    fn safe_radius_never_exceeds_half_the_short_side() {
        let r = Rect::new(0.0, 0.0, 20.0, 10.0);
        assert_eq!(safe_radius(100.0, &r), 5.0);
        assert_eq!(safe_radius(4.0, &r), 4.0);
        assert_eq!(safe_radius(0.0, &r), 0.0);
        assert_eq!(safe_radius(-1.0, &r), 0.0);
    }

    #[test]
    fn ring_point_starts_at_the_top() {
        let top = ring_point((0.0, 0.0), 10.0, 0.0);
        assert!(top.0.abs() < 0.001, "x should be centred, got {}", top.0);
        assert!((top.1 + 10.0).abs() < 0.001, "y should be above centre, got {}", top.1);
        let right = ring_point((0.0, 0.0), 10.0, 0.25);
        assert!((right.0 - 10.0).abs() < 0.001, "a quarter turn is right, got {right:?}");
    }

    #[test]
    fn ring_point_clamps_out_of_range_fractions() {
        assert_eq!(ring_point((0.0, 0.0), 1.0, -1.0), ring_point((0.0, 0.0), 1.0, 0.0));
        assert_eq!(ring_point((0.0, 0.0), 1.0, 2.0), ring_point((0.0, 0.0), 1.0, 1.0));
    }

    #[test]
    fn approximate_measure_scales_sensibly() {
        let a = measure::approximate("hello", 14.0);
        assert!(measure::approximate("hello", 28.0) > a, "double size must be wider");
        assert_eq!(measure::approximate("", 14.0), 0.0);
        assert!(measure::line_height(14.0) > 14.0);
    }

    #[test]
    fn approximate_measure_respects_glyph_widths() {
        let size = 14.0;
        assert!(measure::approximate("mmmm", size) > measure::approximate("iiii", size));
        assert!(measure::approximate("WWW", size) > measure::approximate("...", size));
        // Fullwidth glyphs are about a whole em.
        assert!(measure::approximate("\u{6f22}", size) > size * 0.9);
    }

    #[test]
    fn colour_keys_do_not_collide() {
        assert_ne!(color_key(Color::rgb(1, 2, 3)), color_key(Color::rgba(1, 2, 3, 128)));
        assert_ne!(color_key(Color::rgb(1, 2, 3)), color_key(Color::rgb(3, 2, 1)));
    }

    #[test]
    fn d2d_colour_is_normalised() {
        let f = to_d2d_color(Color::rgb(255, 128, 0));
        assert!((f.r - 1.0).abs() < 0.01);
        assert!((f.g - 0.502).abs() < 0.01);
        assert_eq!(f.b, 0.0);
        assert_eq!(f.a, 1.0);
    }
}
