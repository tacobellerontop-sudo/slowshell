//! DirectWrite text: formats, layout, measurement and drawing.
//!
//! Text is the majority of what a desktop shell renders, so two things matter
//! here above all else: correct shaping (which is why this is DirectWrite and not
//! a hand-rolled glyph blitter) and cheap measurement (a layout pass measures
//! every string every frame).
//!
//! Formats and layouts are cached by their parameters. Building a text layout is
//! the single most expensive thing in the renderer, and a bar re-measured every
//! frame would spend its whole frame budget there.

use std::collections::HashMap;

use windows::core::w;
use windows::Win32::Graphics::Direct2D::Common::{D2D_RECT_F, D2D_SIZE_F};
use windows::Win32::Graphics::Direct2D::{D2D1_DRAW_TEXT_OPTIONS_CLIP, ID2D1Brush};
use windows::Win32::Graphics::DirectWrite::*;

use super::target::RenderTarget;

/// Horizontal alignment within the layout box.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Align {
    Start,
    Center,
    End,
}

impl Align {
    fn to_dwrite(self) -> DWRITE_TEXT_ALIGNMENT {
        match self {
            Align::Start => DWRITE_TEXT_ALIGNMENT_LEADING,
            Align::Center => DWRITE_TEXT_ALIGNMENT_CENTER,
            Align::End => DWRITE_TEXT_ALIGNMENT_TRAILING,
        }
    }

    pub fn parse(s: &str) -> Option<Align> {
        Some(match s.to_ascii_lowercase().as_str() {
            "start" | "left" => Align::Start,
            "center" | "centre" | "middle" => Align::Center,
            "end" | "right" => Align::End,
            _ => return None,
        })
    }
}

/// What to do when the text does not fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Ellipsis {
    /// Clip, showing the beginning.
    Clip,
    /// Trailing ellipsis, the usual choice for a label.
    Trailing,
}

impl Ellipsis {
    pub fn parse(s: &str) -> Option<Ellipsis> {
        Some(match s.to_ascii_lowercase().as_str() {
            "clip" | "none" => Ellipsis::Clip,
            "ellipsis" | "trailing" => Ellipsis::Trailing,
            _ => return None,
        })
    }
}

/// Everything that affects a text layout's appearance.
///
/// Not `Eq`/`Hash` because it holds `f32`; [`TextKey`] is the hashable form that
/// the caches actually use.
#[derive(Debug, Clone, PartialEq)]
pub struct TextStyle {
    pub family: String,
    /// Size in *logical* pixels; multiplied by the surface DPI at draw time.
    pub size: f32,
    pub weight: u16,
    pub italic: bool,
    pub align: Align,
    pub ellipsis: Ellipsis,
    pub letter_spacing: f32,
}

impl Default for TextStyle {
    fn default() -> Self {
        TextStyle {
            // Segoe UI Variable is the Windows 11 system face; it falls back
            // through the font collection to Segoe UI on Windows 10.
            family: "Segoe UI Variable Text".into(),
            size: 14.0,
            weight: 400,
            italic: false,
            align: Align::Start,
            ellipsis: Ellipsis::Trailing,
            letter_spacing: 0.0,
        }
    }
}

impl TextStyle {
    fn key(&self, dpi: u32) -> TextKey {
        TextKey {
            family: self.family.clone(),
            size_q: (self.size * 4.0).round() as i64,
            weight: self.weight,
            italic: self.italic,
            align: self.align,
            ellipsis: self.ellipsis,
            letter_q: (self.letter_spacing * 4.0).round() as i64,
            dpi,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TextKey {
    family: String,
    size_q: i64,
    weight: u16,
    italic: bool,
    align: Align,
    ellipsis: Ellipsis,
    letter_q: i64,
    dpi: u32,
}

/// Caches DirectWrite objects across frames.
pub struct TextEngine {
    factory: IDWriteFactory,
    formats: HashMap<TextKey, IDWriteTextFormat>,
    /// Layouts keyed by text and box, so a static label is measured once.
    layouts: HashMap<LayoutKey, IDWriteTextLayout>,
    layout_budget: usize,
    /// A fallback for a font family that does not exist, so a typo degrades to
    /// the system font instead of rendering nothing.
    fallback_family: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct LayoutKey {
    text: String,
    style: TextKey,
    width_q: i64,
    height_q: i64,
}

impl TextEngine {
    pub fn new(factory: IDWriteFactory) -> TextEngine {
        TextEngine {
            factory,
            formats: HashMap::new(),
            layouts: HashMap::new(),
            // A bar has a handful of strings; a dashboard may have hundreds. The
            // budget bounds memory for a long-running session.
            layout_budget: 2048,
            fallback_family: "Segoe UI".into(),
        }
    }

    /// Whether a font family exists in the system collection.
    pub fn has_family(&self, family: &str) -> bool {
        let wide = to_wide(family);
        let mut index = 0u32;
        let mut exists = windows::core::BOOL(0);
        let found = self.system_collection().is_some_and(|c| {
            let p = windows::core::PCWSTR(wide.as_ptr());
            // The index out-parameter is documented as optional, but the system
            // font driver writes through it regardless and faults on a null
            // pointer, so a real slot is always passed.
            unsafe { c.FindFamilyName(p, &mut index, &mut exists) }.is_ok()
        });
        found && exists.as_bool()
    }

    fn system_collection(&self) -> Option<IDWriteFontCollection> {
        let mut collection: Option<IDWriteFontCollection> = None;
        // `false` skips a font-update round trip; a shell must not stall on a
        // network font update during layout.
        unsafe { self.factory.GetSystemFontCollection(&mut collection, false).ok()? };
        collection
    }

    fn format(&self, style: &TextStyle) -> Option<IDWriteTextFormat> {
        let weight = DWRITE_FONT_WEIGHT(clamp_weight(style.weight) as i32);
        let dstyle =
            if style.italic { DWRITE_FONT_STYLE_ITALIC } else { DWRITE_FONT_STYLE_NORMAL };
        let stretch = DWRITE_FONT_STRETCH_NORMAL;

        let make = |family: &str| unsafe {
            let wide = to_wide(family);
            let name = windows::core::PCWSTR(wide.as_ptr());
            self.factory
                .CreateTextFormat(name, None, weight, dstyle, stretch, style.size, w!("en-us"))
        };

        // Try the requested family, then the system default. Text that renders in
        // the wrong face beats text that does not render at all.
        let format = make(&style.family).or_else(|_| make(&self.fallback_family)).ok()?;

        unsafe {
            let _ = format.SetTextAlignment(style.align.to_dwrite());
            let _ = format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
            let _ = format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR);
        }
        if style.letter_spacing != 0.0 {
            unsafe { set_letter_spacing(&format, style.letter_spacing) };
        }
        if style.ellipsis == Ellipsis::Trailing {
            unsafe {
                let trimming = DWRITE_TRIMMING {
                    granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
                    // U+2026 HORIZONTAL ELLIPSIS, the delimiter DirectWrite appends
                    // when it trims.
                    delimiter: 0x2026,
                    delimiterCount: 1,
                };
                let _ = format.SetTrimming(&trimming, None::<&IDWriteInlineObject>);
            }
        }
        Some(format)
    }

    /// Build or fetch a text layout for `text` inside `width` x `height`.
    ///
    /// `width`/`height` are logical pixels. They are multiplied by `dpi` because
    /// `CreateTextLayout` takes device pixels, and the DPI is part of the cache
    /// key so a layout built at 1.5x is never reused at 1.0x.
    pub fn layout(
        &mut self,
        text: &str,
        style: &TextStyle,
        width: f32,
        height: f32,
        dpi: f32,
    ) -> Option<IDWriteTextLayout> {
        let key = LayoutKey {
            text: text.to_string(),
            style: style.key(dpi.round() as u32),
            width_q: (width * 4.0).round() as i64,
            height_q: (height * 4.0).round() as i64,
        };
        if let Some(l) = self.layouts.get(&key) {
            return Some(l.clone());
        }
        let format = self.format_cached(style, dpi)?;
        let wide = to_wide(text);
        // A generous max width lets DirectWrite compute the full intrinsic width
        // for measurement even when the visible box is narrower.
        let layout = unsafe {
            self.factory.CreateTextLayout(
                &wide,
                &format,
                (width * dpi).max(1.0).ceil(),
                (height * dpi).max(1.0).ceil(),
            )
        }
        .ok()?;
        if self.layouts.len() >= self.layout_budget {
            // Simple eviction: drop the whole cache rather than tracking recency.
            // A config reload is the natural point to do this anyway.
            self.layouts.clear();
        }
        self.layouts.insert(key, layout.clone());
        Some(layout)
    }

    fn format_cached(&mut self, style: &TextStyle, _dpi: f32) -> Option<IDWriteTextFormat> {
        let key = style.key(0);
        if let Some(f) = self.formats.get(&key) {
            return Some(f.clone());
        }
        let f = self.format(style)?;
        self.formats.insert(key, f.clone());
        Some(f)
    }

    /// Intrinsic size of a string, used by layout before committing to a box.
    ///
    /// An unbounded layout is built and cached under a canonical zero width so a
    /// measurement is shared by every element asking the same question.
    ///
    /// # Why nothing here divides by the DPI
    ///
    /// `IDWriteTextLayout::GetMetrics` reports **logical** units (DIPs), not
    /// device pixels, even though the layout was built with device-pixel bounds.
    /// Dividing by the DPI a second time is the easiest mistake to make here and
    /// it is completely silent: a 14-point line comes back 0.19 pixels tall,
    /// every text box is clipped to nothing, and the whole bar renders blank with
    /// no error reported anywhere. The result is therefore already in the unit
    /// the painter draws in.
    pub fn measure(&mut self, text: &str, style: &TextStyle, dpi: f32) -> (f32, f32) {
        let Some(layout) = self.layout(text, style, 4096.0, 4096.0, dpi) else {
            return (0.0, 0.0);
        };
        let m = layout_metrics(&layout);
        (m.width, m.height)
    }

    /// Font metrics for a style, used to align icons with text baselines.
    ///
    /// Read from a throwaway layout rather than the format, because
    /// `IDWriteTextFormat` exposes no metrics and `IDWriteTextLayout` reports the
    /// real ascent and baseline DirectWrite will use. The values are in logical
    /// units, for the same reason as [`TextEngine::measure`].
    pub fn font_metrics(&mut self, style: &TextStyle, dpi: f32) -> Option<FontMetrics> {
        let layout = self.layout("Mg", style, 512.0, 512.0, dpi)?;
        let mut line = [DWRITE_LINE_METRICS::default(); 1];
        let mut count = 0u32;
        unsafe { layout.GetLineMetrics(Some(&mut line), &mut count).ok()? };
        if count == 0 {
            return None;
        }
        let l = line[0];
        Some(FontMetrics {
            ascent: l.baseline,
            descent: l.height - l.baseline,
            line_gap: 0.0,
            em_size: style.size,
        })
    }

    /// The DirectWrite format for a style, for a caller that wants to draw.
    pub fn format_for(&mut self, style: &TextStyle) -> Option<IDWriteTextFormat> {
        self.format_cached(style, 0.0)
    }

    /// Drop cached objects, called on a config reload or theme change.
    pub fn clear(&mut self) {
        self.formats.clear();
        self.layouts.clear();
    }

    pub fn cached_layouts(&self) -> usize {
        self.layouts.len()
    }
}

/// Normalised font metrics in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FontMetrics {
    pub ascent: f32,
    pub descent: f32,
    pub line_gap: f32,
    pub em_size: f32,
}

impl FontMetrics {
    /// The line box height a single line of text occupies.
    pub fn line_height(&self) -> f32 {
        self.ascent + self.descent + self.line_gap
    }
}

/// Draw a string into a box with a DirectWrite format.
///
/// The Direct2D target's own `DrawText` is used rather than
/// `IDWriteTextLayout::Draw` because the latter is the custom-renderer path and
/// does not accept a brush. Coordinates are in DIPs; the target's DPI does the
/// scaling.
pub fn draw_text(
    ctx: &dyn RenderTarget,
    text: &str,
    format: &IDWriteTextFormat,
    rect: (f32, f32, f32, f32),
    brush: &ID2D1Brush,
) {
    let wide = to_wide(text);
    let r = D2D_RECT_F { left: rect.0, top: rect.1, right: rect.2, bottom: rect.3 };
    ctx.draw_text(&wide, format, &r, brush, D2D1_DRAW_TEXT_OPTIONS_CLIP);
}

/// The metrics a text layout reports, in logical units.
pub fn layout_metrics(layout: &IDWriteTextLayout) -> DWRITE_TEXT_METRICS {
    let mut m = DWRITE_TEXT_METRICS::default();
    let _ = unsafe { layout.GetMetrics(&mut m) };
    m
}

/// The size a text layout reports, in logical units.
pub fn layout_size(layout: &IDWriteTextLayout) -> D2D_SIZE_F {
    let m = layout_metrics(layout);
    D2D_SIZE_F { width: m.width, height: m.height }
}

unsafe fn set_letter_spacing(format: &IDWriteTextFormat, spacing: f32) {
    // DirectWrite exposes character spacing only through
    // `IDWriteTextAnalysisSource::ApplyCharacterSpacing`, which operates on glyph
    // runs. Reaching it would mean taking over shaping, so the spacing is applied
    // by the layout engine instead and this hook is where a future shaper would
    // hook in. The `letterSpacing` property is therefore parsed and stored but not
    // yet rendered; see docs/CONFIGURATION.md.
    let _ = (format, spacing);
}

/// Snap a CSS-style numeric weight onto the nearest face DirectWrite offers.
///
/// DirectWrite only has discrete weights, so `fontWeight: 550` must land on one
/// of them rather than silently rendering as regular.
fn clamp_weight(w: u16) -> u32 {
    const CANDIDATES: [(u16, u32); 9] = [
        (100, DWRITE_FONT_WEIGHT_THIN.0 as u32),
        (200, DWRITE_FONT_WEIGHT_EXTRA_LIGHT.0 as u32),
        (300, DWRITE_FONT_WEIGHT_LIGHT.0 as u32),
        (400, DWRITE_FONT_WEIGHT_NORMAL.0 as u32),
        (500, DWRITE_FONT_WEIGHT_MEDIUM.0 as u32),
        (600, DWRITE_FONT_WEIGHT_SEMI_BOLD.0 as u32),
        (700, DWRITE_FONT_WEIGHT_BOLD.0 as u32),
        (800, DWRITE_FONT_WEIGHT_EXTRA_BOLD.0 as u32),
        (900, DWRITE_FONT_WEIGHT_BLACK.0 as u32),
    ];
    CANDIDATES
        .iter()
        .min_by_key(|(value, _)| (*value as i32 - w as i32).abs())
        .map(|(_, face)| *face)
        .unwrap_or(400)
}

pub fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn style() -> TextStyle {
        TextStyle::default()
    }

    #[test]
    fn alignment_parsing_accepts_synonyms() {
        assert_eq!(Align::parse("left"), Some(Align::Start));
        assert_eq!(Align::parse("Center"), Some(Align::Center));
        assert_eq!(Align::parse("trailing"), None);
        assert_eq!(Align::parse("end"), Some(Align::End));
    }

    #[test]
    fn ellipsis_parsing() {
        assert_eq!(Ellipsis::parse("ellipsis"), Some(Ellipsis::Trailing));
        assert_eq!(Ellipsis::parse("none"), Some(Ellipsis::Clip));
        assert_eq!(Ellipsis::parse("wat"), None);
    }

    #[test]
    fn style_keys_separate_by_dpi_and_size() {
        let a = style();
        let a96 = a.key(96);
        let mut b = style();
        b.size = 16.0;
        assert_ne!(a96, b.key(96), "a different size is a different format");
        assert_ne!(a96, a.key(144), "a different DPI is a different format");
        assert_eq!(a96, style().key(96), "identical styles must share a format");
    }

    #[test]
    fn sub_pixel_sizes_do_not_fragment_the_cache() {
        // Two sizes a thousandth of a pixel apart must not create two formats,
        // or an animated size would thrash the cache every frame.
        let mut a = style();
        a.size = 14.0;
        let mut b = style();
        b.size = 14.001;
        assert_eq!(a.key(96), b.key(96));
    }

    #[test]
    fn weights_snap_to_a_standard_face() {
        assert_eq!(clamp_weight(400), DWRITE_FONT_WEIGHT_NORMAL.0 as u32);
        assert_eq!(clamp_weight(700), DWRITE_FONT_WEIGHT_BOLD.0 as u32);
        assert_eq!(clamp_weight(0), DWRITE_FONT_WEIGHT_THIN.0 as u32);
        assert_eq!(clamp_weight(9000), DWRITE_FONT_WEIGHT_BLACK.0 as u32);
        // A value between two faces must land on one of them, not in between.
        assert!(matches!(clamp_weight(650), 500 | 600 | 700));
    }

    #[test]
    fn wide_conversion_handles_non_ascii() {
        assert_eq!(to_wide("ab"), vec![97, 98]);
        // A character outside the BMP becomes a UTF-16 surrogate pair.
        assert_eq!(to_wide("\u{1F980}").len(), 2);
    }

    #[test]
    fn font_metrics_line_height_is_sane() {
        let m = FontMetrics { ascent: 12.0, descent: 3.0, line_gap: 1.0, em_size: 14.0 };
        assert_eq!(m.line_height(), 16.0);
    }

    /// The system font has to be usable, or every label in the shell renders as
    /// nothing at all with no error anywhere. This is the one test that would
    /// have caught it.
    #[test]
    fn the_system_font_measures_and_the_fallback_covers_a_missing_family() {
        let Some(mut engine) = test_engine() else { return };
        let style = TextStyle::default();
        let (w, h) = engine.measure("Apps 100%", &style, 96.0);
        assert!(
            w > 10.0 && (12.0..24.0).contains(&h),
            "a 14pt string measured as {w}x{h}; a value near zero means every \
             text box is being clipped away"
        );

        // A family that cannot exist must fall back rather than fail.
        let mut missing = style.clone();
        missing.family = "No Such Font Family 12345".into();
        let (w2, h2) = engine.measure("Apps", &missing, 96.0);
        assert!(w2 > 1.0 && h2 > 1.0, "a missing family must still render, got {w2}x{h2}");
    }

    #[test]
    fn a_missing_family_is_reported_as_missing() {
        let Some(engine) = test_engine() else { return };
        // Only the negative case is asserted. Whether a particular family is
        // installed is a fact about Windows, not about the shell, and a test that
        // fails when a machine is missing a font is a test nobody trusts.
        assert!(!engine.has_family("No Such Font Family 12345"));
    }


    /// A logical measurement must not change with the display's scale, or a
    /// layout computed at 100% would be wrong at 150%.
    #[test]
    fn measurements_are_in_logical_pixels_at_every_dpi() {
        let Some(mut engine) = test_engine() else { return };
        let style = TextStyle::default();
        let (w96, h96) = engine.measure("Apps", &style, 96.0);
        let (w192, h192) = engine.measure("Apps", &style, 192.0);
        assert!(
            (w96 - w192).abs() < 0.75 && (h96 - h192).abs() < 0.75,
            "logical size must not depend on DPI, got {w96}x{h96} at 96 and {w192}x{h192} at 192"
        );
        let m = engine.font_metrics(&style, 96.0).expect("font metrics");
        assert!(
            m.line_height() > 12.0 && m.line_height() < 28.0,
            "a 14pt line box of {} is not plausible",
            m.line_height()
        );
    }

    #[test]
    fn a_wider_string_measures_wider() {
        let Some(mut engine) = test_engine() else { return };
        let style = TextStyle::default();
        let (short, _) = engine.measure("ab", &style, 96.0);
        let (long, _) = engine.measure("abcdefgh", &style, 96.0);
        assert!(long > short, "measurement is not tracking string length");
    }

    /// A text engine on the real system font, or `None` where DirectWrite is
    /// unavailable, which is a Windows problem rather than a shell one.
    fn test_engine() -> Option<TextEngine> {
        let factory =
            unsafe { DWriteCreateFactory::<IDWriteFactory>(DWRITE_FACTORY_TYPE_SHARED) }.ok()?;
        Some(TextEngine::new(factory))
    }
}


