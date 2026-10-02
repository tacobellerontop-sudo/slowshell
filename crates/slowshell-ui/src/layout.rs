//! The layout engine.
//!
//! A flexbox subset sized for the shapes a desktop shell actually needs: a row of
//! status widgets, a column of launcher results, a spacer that pushes things
//! apart. It is a measure/arrange two-pass algorithm, the same model CSS flexbox
//! uses, which is what makes `Spacer {}` and `justify: center` behave the way a
//! user expects.
//!
//! Layout runs in **logical pixels** and knows nothing about the renderer, so the
//! whole engine is unit tested without a GPU.
//!
//! ## Boxes
//!
//! An element's reported rect is its **content box**: the slot it was given, less
//! its own padding. Margin lives outside it, which is what makes a row's spacing
//! add up and keeps `padding` from affecting where neighbours land.

use std::collections::HashMap;

use slowshell_core::react::Reactor;

use crate::element::{Dyn, Element, ElementKind};
use crate::style::{CrossAlign, MainAlign, Size, Style};
use crate::Rect;

/// Measures intrinsic size, so an element can be sized before its parent exists.
pub trait Measurer {
    fn measure_text(&mut self, content: &str, style: &crate::TextStyle) -> (f32, f32);
    fn line_height(&mut self, style: &crate::TextStyle) -> f32;
}

/// The platform approximation, used before a surface exists.
pub struct ApproxMeasurer;

impl Measurer for ApproxMeasurer {
    fn measure_text(&mut self, content: &str, style: &crate::TextStyle) -> (f32, f32) {
        (
            slowshell_win::render::measure::approximate(content, style.size),
            slowshell_win::render::measure::line_height(style.size),
        )
    }

    fn line_height(&mut self, style: &crate::TextStyle) -> f32 {
        slowshell_win::render::measure::line_height(style.size)
    }
}

/// Real DirectWrite measurement, once a text engine exists.
///
/// The approximation is good to about a pixel per character, which is fine for
/// a pre-layout guess and not fine for the layout that decides whether a label
/// fits: a bold label measured a little narrow comes out trimmed on screen, and
/// the user sees "App" where the config says "Apps". Both passes use this, so a
/// label is never given less room than it takes.
pub struct TextMeasurer<'a> {
    text: &'a mut crate::TextEngine,
    /// Part of the text engine's cache key, and nothing else: measurements come
    /// back in logical units at every scale.
    dpi: f32,
}

impl<'a> TextMeasurer<'a> {
    pub fn new(text: &'a mut crate::TextEngine, dpi: f32) -> TextMeasurer<'a> {
        TextMeasurer { text, dpi }
    }
}

impl Measurer for TextMeasurer<'_> {
    fn measure_text(&mut self, content: &str, style: &crate::TextStyle) -> (f32, f32) {
        self.text.measure(content, style, self.dpi)
    }

    fn line_height(&mut self, style: &crate::TextStyle) -> f32 {
        self.text
            .font_metrics(style, self.dpi)
            .map(|m| m.line_height())
            .unwrap_or_else(|| slowshell_win::render::measure::line_height(style.size))
    }
}

/// The size an element would like to be, along its main and cross axes.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Intrinsic {
    pub main: f32,
    pub cross: f32,
}

/// The resolved values of an element for this frame.
///
/// Reading a reactive value returns a `Value`; this caches the numbers the layout
/// pass needs so the engine does no graph work of its own. Equality is what the
/// frame loop uses to decide whether anything moved, so it is derived rather than
/// hand-written.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub text: String,
    pub visible: bool,
    pub progress: f32,
    pub opacity: f32,
    pub width: Size,
    pub height: Size,
    pub background_present: bool,
    /// Local civil time, as a Unix timestamp shifted by the UTC offset.
    ///
    /// `None` means no clock source has been published, so a `Clock` falls back to
    /// the process clock. Keeping it in the value map rather than reading a global
    /// is what lets the clock be tested.
    pub clock_unix: Option<i64>,
}

impl Default for Resolved {
    fn default() -> Self {
        Resolved {
            text: String::new(),
            visible: true,
            progress: 0.0,
            opacity: 1.0,
            width: Size::Auto,
            height: Size::Auto,
            background_present: true,
            clock_unix: None,
        }
    }
}

impl Resolved {
    /// The values an element has when nothing dynamic applies.
    ///
    /// Reading the element's own kind here is what keeps a static label working
    /// even when a caller supplies a partial value map.
    pub fn from_element(e: &Element) -> Resolved {
        Resolved {
            text: match &e.kind {
                ElementKind::Text { content } => content.clone(),
                _ => String::new(),
            },
            visible: true,
            progress: 0.0,
            opacity: 1.0,
            width: e.style.width,
            height: e.style.height,
            background_present: true,
            clock_unix: None,
        }
    }
}

/// Stable key for the resolved-value map, so a bare `u32` cannot be passed by
/// mistake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ElementIdKey(pub u32);

/// A guard against a pathological tree, not a real limit.
const MAX_DEPTH: usize = 64;

/// Lay an element tree out inside `bounds`.
pub fn layout<M: Measurer>(
    root: &Element,
    bounds: Rect,
    theme: &crate::Theme,
    measurer: &mut M,
    values: &HashMap<ElementIdKey, Resolved>,
) {
    let mut pass = Pass { measurer, theme, values };
    pass.layout(root, bounds, &Style::default(), 0);
}

struct Pass<'a, M: Measurer> {
    measurer: &'a mut M,
    theme: &'a crate::Theme,
    values: &'a HashMap<ElementIdKey, Resolved>,
}

/// A child's resolved main-axis size and whether it wants to share free space.
struct Want {
    size: f32,
    fill: bool,
}

impl<M: Measurer> Pass<'_, M> {
    fn resolved(&self, e: &Element) -> Resolved {
        self.values
            .get(&ElementIdKey(e.id.0))
            .cloned()
            .unwrap_or_else(|| Resolved::from_element(e))
    }

    fn kids(e: &Element) -> Vec<&Element> {
        e.children.iter().chain(e.groups.iter().flat_map(|(_, g)| g.iter())).collect()
    }

    fn layout(&mut self, e: &Element, slot: Rect, parent_style: &Style, depth: usize) {
        let v = self.resolved(e);
        if !v.visible {
            e.rect.set(Rect::ZERO);
            return;
        }
        let style = e.effective_style(parent_style);
        if depth > MAX_DEPTH {
            e.rect.set(Rect::ZERO);
            return;
        }

        // Margin is outside the element, padding inside it.
        let content = slot_within(slot, &style, true);
        let inner = content.inset(style.padding.left, style.padding.top);
        e.rect.set(inner);

        let kids = Self::kids(e);
        if kids.is_empty() {
            return;
        }
        let horizontal = e.kind.is_horizontal();

        // A panel is the one element whose children are placed by *where they
        // are* rather than by their order, so it does not share the container
        // path below.
        if matches!(e.kind, ElementKind::Panel { .. }) {
            self.layout_panel(e, inner, &style, horizontal, depth);
            return;
        }

        self.place_children(
            &kids,
            inner,
            horizontal,
            e.kind.cross_align(),
            e.kind.gap(),
            e.kind.main_align(),
            &style,
            depth,
        );
    }

    /// Lay a panel's anchored groups out along its main axis.
    ///
    /// A panel is the one element whose children are positioned by *where they
    /// are* rather than by their order: `left` sits at the start, `center` in
    /// the middle and `right` at the end. That is what makes a top bar look like
    /// a top bar, and it is why the spec's example puts its widgets into named
    /// groups instead of one long row full of spacers.
    ///
    /// Unnamed children join the start band, so a panel may mix both styles. A
    /// band is placed as wide as it needs; when the three together overflow the
    /// panel they are scaled down equally rather than one of them being dropped,
    /// because a crowded bar is still more useful than a truncated one.
    fn layout_panel(
        &mut self,
        e: &Element,
        inner: Rect,
        style: &Style,
        horizontal: bool,
        depth: usize,
    ) {
        let mut start: Vec<&Element> = e.children.iter().collect();
        let mut centre: Vec<&Element> = Vec::new();
        let mut end: Vec<&Element> = Vec::new();
        for (name, group) in &e.groups {
            let band = match name.as_str() {
                "center" | "middle" => &mut centre,
                "right" | "end" => &mut end,
                // `left`, `start`, `content` and anything else stay at the start.
                _ => &mut start,
            };
            band.extend(group.iter());
        }
        if start.is_empty() && centre.is_empty() && end.is_empty() {
            return;
        }

        // A panel's own gap spaces the members of a group. It is zero by default,
        // so a group with several widgets is normally written as `Row { gap: n }`.
        let gap = e.kind.gap();
        let cross_align = e.kind.cross_align();
        let start_w = self.band_size(&start, horizontal, gap);
        let centre_w = self.band_size(&centre, horizontal, gap);
        let end_w = self.band_size(&end, horizontal, gap);

        let (origin, avail) = if horizontal { (inner.x, inner.w) } else { (inner.y, inner.h) };
        let total = start_w + centre_w + end_w;
        let scale = if total > avail && total > 0.0 { avail / total } else { 1.0 };
        let (start_w, centre_w, end_w) = (start_w * scale, centre_w * scale, end_w * scale);

        // A band spans the panel's whole cross extent, so the band members are
        // aligned by the panel's cross alignment rather than a band-local one.
        let slot = |main_pos: f32, main_size: f32| {
            if horizontal {
                Rect::new(main_pos, inner.y, main_size, inner.h)
            } else {
                Rect::new(inner.x, main_pos, inner.w, main_size)
            }
        };
        let bands = [
            (&start, origin, start_w),
            (&centre, origin + (avail - centre_w) / 2.0, centre_w),
            (&end, origin + avail - end_w, end_w),
        ];
        for (band, pos, size) in bands {
            if band.is_empty() || size <= 0.0 {
                continue;
            }
            self.place_children(
                band,
                slot(pos, size),
                horizontal,
                cross_align,
                gap,
                MainAlign::Start,
                style,
                depth,
            );
        }
    }

    /// How much main-axis room a band of panel children needs.
    fn band_size(&mut self, band: &[&Element], horizontal: bool, gap: f32) -> f32 {
        let mut total = 0.0;
        let mut n = 0usize;
        for k in band {
            let kv = self.resolved(k);
            if !kv.visible {
                continue;
            }
            let ks = k.effective_style(&Style::default());
            total += self.measure_main(k, horizontal, kv).size;
            total += if horizontal { ks.margin.horizontal() } else { ks.margin.vertical() };
            n += 1;
        }
        (total + gap * n.saturating_sub(1) as f32).max(0.0)
    }

    /// Measure and place a set of siblings inside `inner`.
    ///
    /// Three passes, the same model CSS flexbox uses: ask every child how much
    /// main-axis room it wants, hand the free space to the children that asked to
    /// fill, then place them. Split out from [`Pass::layout`] so a panel's bands
    /// can reuse it without a wrapper element to hang it on.
    #[allow(clippy::too_many_arguments)]
    fn place_children(
        &mut self,
        kids: &[&Element],
        inner: Rect,
        horizontal: bool,
        cross_align: CrossAlign,
        gap: f32,
        main_align: MainAlign,
        style: &Style,
        depth: usize,
    ) {
        if kids.is_empty() {
            return;
        }
        let line_main = if horizontal { inner.w } else { inner.h };
        let line_cross = if horizontal { inner.h } else { inner.w };

        // Pass 1: how much main-axis space does each visible child want?
        let mut wants: Vec<Want> = Vec::with_capacity(kids.len());
        for c in kids {
            let cv = self.resolved(c);
            if !cv.visible {
                wants.push(Want { size: 0.0, fill: false });
                continue;
            }
            wants.push(self.measure_main(c, horizontal, cv));
        }

        // Pass 2: hand the free space to the children that asked to fill.
        let visible: Vec<usize> = (0..kids.len()).filter(|i| self.resolved(kids[*i]).visible).collect();
        let gaps = gap * visible.len().saturating_sub(1) as f32;
        let margins: f32 = visible
            .iter()
            .map(|i| {
                let cs = kids[*i].effective_style(style);
                if horizontal { cs.margin.horizontal() } else { cs.margin.vertical() }
            })
            .sum();
        let used: f32 = wants.iter().map(|w| w.size).sum::<f32>() + gaps + margins;
        let free = (line_main - used).max(0.0);
        let fillers: Vec<usize> = visible.iter().copied().filter(|i| wants[*i].fill).collect();
        if !fillers.is_empty() {
            let each = free / fillers.len() as f32;
            for i in fillers {
                wants[i].size = each;
            }
        }

        // Pass 3: place each child along the main axis.
        let consumed: f32 = wants.iter().map(|w| w.size).sum::<f32>() + gaps + margins;
        let slack = (line_main - consumed).max(0.0);
        let (start, spacing) = match main_align {
            MainAlign::Start => (0.0, gap),
            MainAlign::Center => (slack / 2.0, gap),
            MainAlign::End => (slack, gap),
            MainAlign::SpaceBetween => {
                // A single child cannot be pushed anywhere, so the slack is left.
                let n = visible.len();
                if n > 1 {
                    (0.0, gap + slack / (n - 1) as f32)
                } else {
                    (0.0, gap)
                }
            }
        };

        let mut cursor = if horizontal { inner.x } else { inner.y } + start;
        for (idx, c) in kids.iter().enumerate() {
            let cv = self.resolved(c);
            if !cv.visible {
                continue;
            }
            let cstyle = c.effective_style(style);
            // Clamp so a label longer than the panel is clipped by the paint pass
            // rather than drawing outside the window.
            let main_size = wants[idx].size.min(line_main).max(0.0);

            // Cross axis: an explicit size wins, then stretch, then intrinsic.
            let cross_pref = if horizontal { pick(cv.height, cstyle.height) } else { pick(cv.width, cstyle.width) };
            let cross_avail = (line_cross
                - if horizontal { cstyle.margin.vertical() } else { cstyle.margin.horizontal() })
            .max(0.0);
            let cross_size = match cross_pref {
                Size::Fixed(v) if v > 0.0 => v.min(cross_avail.max(v)),
                _ if cross_align == CrossAlign::Stretch => self.intrinsic_cross(c, &cstyle, horizontal, &cv)
                    .max(cross_avail),
                _ => self.intrinsic_cross(c, &cstyle, horizontal, &cv).min(cross_avail),
            }
            .max(0.0);

            let cross_pos = match cross_align {
                CrossAlign::Start | CrossAlign::Stretch => 0.0,
                CrossAlign::Center => (cross_avail - cross_size) / 2.0,
                CrossAlign::End => (cross_avail - cross_size).max(0.0),
            };

            // The slot handed down is the child's **margin box**. The child insets
            // its own margin in `layout`, so the parent must not also apply it, or
            // the offset would be counted twice.
            let margin_main = if horizontal { cstyle.margin.horizontal() } else { cstyle.margin.vertical() };
            let margin_cross = if horizontal { cstyle.margin.vertical() } else { cstyle.margin.horizontal() };
            let child_slot = if horizontal {
                Rect::new(cursor, inner.y + cross_pos, main_size + margin_main, cross_size + margin_cross)
            } else {
                Rect::new(inner.x + cross_pos, cursor, cross_size + margin_cross, main_size + margin_main)
            };
            self.layout(c, child_slot, style, depth + 1);

            cursor += main_size + margin_main + spacing;
        }
    }

    /// How much main-axis space a child wants, and whether it wants to fill.
    ///
    /// The size returned is the child's **margin box** minus its own margin, i.e.
    /// padding plus content. Margin is accounted for by the caller so it is never
    /// applied twice.
    fn measure_main(&mut self, c: &Element, horizontal: bool, cv: Resolved) -> Want {
        let style = c.effective_style(&Style::default());
        let pad_main = if horizontal { style.padding.horizontal() } else { style.padding.vertical() };
        let explicit = if horizontal { pick(cv.width, style.width) } else { pick(cv.height, style.height) };
        // A Spacer exists to absorb free space, so it fills by definition unless
        // the config gave it an explicit size.
        let wants_fill = matches!(c.kind, ElementKind::Spacer);
        match explicit {
            Size::Fill => Want { size: 0.0, fill: true },
            Size::Fixed(v) => Want { size: (v + pad_main).max(0.0), fill: false },
            Size::Auto if wants_fill => Want { size: pad_main, fill: true },
            Size::Auto => {
                let content = self.intrinsic_main(c, horizontal, &cv);
                Want { size: (content + pad_main).max(0.0), fill: false }
            }
        }
    }

    /// A child's natural main-axis size.
    fn intrinsic_main(&mut self, c: &Element, horizontal: bool, cv: &Resolved) -> f32 {
        if matches!(c.kind, ElementKind::Spacer) {
            return 0.0;
        }
        if !c.kind.is_container() {
            return if horizontal {
                self.text_width(c, cv)
            } else {
                self.text_height(c)
            };
        }
        let style = c.style.clone();
        let kids = Self::kids(c);
        let gap = c.kind.gap();
        let mut total = 0.0;
        let mut n: usize = 0;
        for k in &kids {
            let kv = self.resolved(k);
            if !kv.visible || matches!(k.kind, ElementKind::Spacer) {
                // A spacer only absorbs free space; it contributes nothing here.
                continue;
            }
            total += self.measure_main(k, horizontal, kv).size;
            n += 1;
        }
        let _ = style;
        total + gap * n.saturating_sub(1) as f32
    }

    /// A child's natural cross-axis size.
    fn intrinsic_cross(&mut self, c: &Element, style: &Style, horizontal: bool, cv: &Resolved) -> f32 {
        let pad_cross = if horizontal { style.padding.vertical() } else { style.padding.horizontal() };
        let base = match &c.kind {
            ElementKind::Text { .. } | ElementKind::Clock { .. } => {
                if horizontal {
                    self.text_height(c)
                } else {
                    self.text_width(c, cv)
                }
            }
            ElementKind::Progress => style.font_size * 1.6,
            ElementKind::Spacer => 0.0,
            kind if kind.is_container() => {
                // A container is as tall as its tallest child.
                let kids = Self::kids(c);
                let mut tallest: f32 = 0.0;
                for k in &kids {
                    let kv = self.resolved(k);
                    if !kv.visible {
                        continue;
                    }
                    let ks = k.effective_style(style);
                    let h = self.intrinsic_cross(k, &ks, horizontal, &kv)
                        + if horizontal { ks.margin.vertical() } else { ks.margin.horizontal() }
                        + if horizontal { ks.padding.vertical() } else { ks.padding.horizontal() };
                    tallest = tallest.max(h);
                }
                tallest
            }
            _ => self.text_height(c),
        };
        (base + pad_cross).max(0.0)
    }

    fn text_width(&mut self, e: &Element, cv: &Resolved) -> f32 {
        let ts = e.text_style(self.theme);
        // The *displayed* string, so a clock is measured as the digits it will
        // actually draw rather than as the empty text it carries in the tree.
        let measured = self.measurer.measure_text(&e.display_text(cv), &ts).0;
        // One logical pixel of trailing allowance. DirectWrite's trailing-ellipsis
        // trimming is conservative by a fraction of a pixel, so a label given
        // exactly its own advance width loses its last glyph — "Apps" comes out
        // as "App" with no diagnostic anywhere.
        measured + 1.0
    }

    fn text_height(&mut self, e: &Element) -> f32 {
        let ts = e.text_style(self.theme);
        self.measurer.line_height(&ts)
    }
}

/// Strip margin from a slot, producing the element's margin box.
fn slot_within(slot: Rect, style: &Style, _horizontal: bool) -> Rect {
    Rect::new(
        slot.x + style.margin.left,
        slot.y + style.margin.top,
        (slot.w - style.margin.horizontal()).max(0.0),
        (slot.h - style.margin.vertical()).max(0.0),
    )
}

fn pick(dyn_value: Size, from_style: Size) -> Size {
    if dyn_value == Size::Auto {
        from_style
    } else {
        dyn_value
    }
}

/// Resolve the reactive values of a tree into a map the layout pass reads.
pub fn resolve_values(root: &Element, reactor: &Reactor) -> HashMap<ElementIdKey, Resolved> {
    let mut out = HashMap::new();
    // The clock is one value for the whole tree, so it is resolved once.
    let clock = reactor.lookup("clock.unix").map(|i| reactor.get(i).to_f64_lossy() as i64);
    collect(root, reactor, clock, &mut out);
    out
}

fn collect(
    e: &Element,
    reactor: &Reactor,
    clock: Option<i64>,
    out: &mut HashMap<ElementIdKey, Resolved>,
) {
    let d: &Dyn = &e.dyn_props;
    let base = Resolved::from_element(e);
    out.insert(
        ElementIdKey(e.id.0),
        Resolved {
            text: d.text.map(|i| reactor.get(i).to_string_lossy()).unwrap_or(base.text),
            visible: d.visible.map(|i| reactor.get(i).truthy()).unwrap_or(base.visible),
            progress: d.progress.map(|i| reactor.get(i).to_f64_lossy() as f32).unwrap_or(base.progress),
            opacity: d.opacity.map(|i| reactor.get(i).to_f64_lossy() as f32).unwrap_or(base.opacity),
            width: d.width.map(|i| Size::parse(&reactor.get(i))).unwrap_or(base.width),
            height: d.height.map(|i| Size::parse(&reactor.get(i))).unwrap_or(base.height),
            background_present: d.background.is_some(),
            clock_unix: clock,
        },
    );
    for c in &e.children {
        collect(c, reactor, clock, out);
    }
    for (_, g) in &e.groups {
        for c in g {
            collect(c, reactor, clock, out);
        }
    }
}

/// The size a panel needs, so a surface can be created before layout runs.
///
/// Returns `(width, height)` in logical pixels.
pub fn panel_intrinsic(root: &Element, available_width: f32, measurer: &mut impl Measurer) -> (f32, f32) {
    let theme = crate::Theme::default();
    let values: HashMap<ElementIdKey, Resolved> = HashMap::new();
    let mut pass = Pass { measurer, theme: &theme, values: &values };
    let horizontal = root.kind.is_horizontal();
    let v = Resolved::from_element(root);
    let main = pass.intrinsic_main(root, horizontal, &v);
    let cross = pass.intrinsic_cross(root, &root.style, horizontal, &v);
    (
        if horizontal { main + root.style.padding.horizontal() } else { available_width },
        if horizontal { cross + root.style.padding.vertical() } else { main + root.style.padding.vertical() },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::element::ElementId;
    use crate::style::Edges;
    use slowshell_core::Span;

    fn el(id: u32, kind: ElementKind) -> Element {
        Element::new(ElementId(id), kind, Style::default(), Span::unknown())
    }

    fn text(id: u32, content: &str) -> Element {
        el(id, ElementKind::Text { content: content.into() })
    }

    fn row_of(children: Vec<Element>) -> Element {
        el(
            0,
            ElementKind::Row { gap: 8.0, main: MainAlign::Start, cross: CrossAlign::Center },
        )
        .with_children(children)
    }

    fn spacer() -> Element {
        el(99, ElementKind::Spacer)
    }

    fn run(root: &Element, bounds: Rect, values: &HashMap<ElementIdKey, Resolved>) {
        let mut m = ApproxMeasurer;
        let theme = crate::Theme::default();
        layout(root, bounds, &theme, &mut m, values);
    }

    /// A top panel with one widget anchored to each edge, which is the shape the
    /// spec's example config describes.
    fn panel_with_bands() -> Element {
        let mut panel = el(
            1,
            ElementKind::Panel {
                position: crate::style::Position::Top,
                screen: "primary".into(),
                exclusive: false,
                wrap: false,
                wrap_size: None,
                bounds: None,
                reveal: false,
                reveal_size: 8.0,
                reveal_duration: 0.18,
                reveal_delay: 0.26,
                reveal_ease: slowshell_core::ease::Ease::OutCubic,
                name: String::new(),
                hidden: false,
                backdrop: slowshell_win::Backdrop::None,
            },
        );
        panel.groups.push(("left".into(), vec![text(10, "L")]));
        panel.groups.push(("center".into(), vec![text(20, "C")]));
        panel.groups.push(("right".into(), vec![text(30, "R")]));
        panel
    }

    #[test]
    fn panel_groups_are_placed_at_the_panel_edges() {
        let root = panel_with_bands();
        run(&root, Rect::new(0.0, 0.0, 1000.0, 40.0), &HashMap::new());
        let band = |name: &str| {
            root.groups
                .iter()
                .find(|(n, _)| n == name)
                .expect("group")
                .1[0]
                .rect
                .get()
        };
        let l = band("left");
        let c = band("center");
        let r = band("right");
        assert!(l.x < 1.0, "the left group must touch the start edge, got {l:?}");
        assert!(
            (r.right() - 1000.0).abs() < 0.01,
            "the right group must touch the end edge, got {r:?}"
        );
        let centre = (c.x + c.right()) / 2.0;
        assert!(
            (centre - 500.0).abs() < 0.5,
            "the centre group must be centred, its middle was {centre}"
        );
        assert!(l.right() <= c.x + 0.01, "bands must not overlap: {l:?} {c:?}");
        assert!(c.right() <= r.x + 0.01, "bands must not overlap: {c:?} {r:?}");
    }

    #[test]
    fn a_panel_with_no_centre_group_still_pushes_right_to_the_edge() {
        let mut panel = el(
            1,
            ElementKind::Panel {
                position: crate::style::Position::Top,
                screen: "primary".into(),
                exclusive: false,
                wrap: false,
                wrap_size: None,
                bounds: None,
                reveal: false,
                reveal_size: 8.0,
                reveal_duration: 0.18,
                reveal_delay: 0.26,
                reveal_ease: slowshell_core::ease::Ease::OutCubic,
                name: String::new(),
                hidden: false,
                backdrop: slowshell_win::Backdrop::None,
            },
        );
        panel.groups.push(("left".into(), vec![text(10, "L")]));
        panel.groups.push(("right".into(), vec![text(30, "R")]));
        run(&panel, Rect::new(0.0, 0.0, 800.0, 30.0), &HashMap::new());
        let right = panel.groups[1].1[0].rect.get();
        assert!(
            (right.right() - 800.0).abs() < 0.01,
            "the right group must still touch the end edge, got {right:?}"
        );
    }

    #[test]
    fn a_crowded_panel_shrinks_its_bands_rather_than_overlapping_them() {
        let root = panel_with_bands();
        // Far too narrow for three labels: the point is that they still do not
        // overlap and none of them is dropped.
        run(&root, Rect::new(0.0, 0.0, 30.0, 40.0), &HashMap::new());
        let band = |name: &str| {
            root.groups
                .iter()
                .find(|(n, _)| n == name)
                .expect("group")
                .1[0]
                .rect
                .get()
        };
        let (l, c, r) = (band("left"), band("center"), band("right"));
        assert!(l.right() <= c.x + 0.01, "bands overlap: {l:?} {c:?}");
        assert!(c.right() <= r.x + 0.01, "bands overlap: {c:?} {r:?}");
        assert!(l.w > 0.0 && c.w > 0.0 && r.w > 0.0, "no band may vanish");
    }

    #[test]
    fn panel_children_join_the_start_band() {
        let mut root = panel_with_bands();
        root.children.push(text(5, "X"));
        run(&root, Rect::new(0.0, 0.0, 1000.0, 40.0), &HashMap::new());
        let x = root.children[0].rect.get();
        let left = root.groups[0].1[0].rect.get();
        assert!(x.x >= 0.0 && x.right() <= left.x + 0.01, "children come before `left`: {x:?} {left:?}");
    }

    #[test]
    fn a_row_sizes_its_children_side_by_side() {
        let root = row_of(vec![text(1, "aa"), text(2, "bb")]);
        run(&root, Rect::new(0.0, 0.0, 500.0, 40.0), &HashMap::new());
        let a = root.children[0].rect.get();
        let b = root.children[1].rect.get();
        assert!(!a.is_empty() && !b.is_empty(), "a={a:?} b={b:?}");
        assert!(a.x < b.x, "the second child must be to the right");
        assert!(a.right() <= b.x + 0.01, "children must not overlap");
    }

    #[test]
    fn a_gap_separates_children() {
        let root = row_of(vec![text(1, "aa"), text(2, "bb")]);
        run(&root, Rect::new(0.0, 0.0, 500.0, 40.0), &HashMap::new());
        let gap = root.children[1].rect.get().x - root.children[0].rect.get().right();
        assert!((gap - 8.0).abs() < 0.01, "expected an 8px gap, got {gap}");
    }

    #[test]
    fn a_spacer_absorbs_the_free_space() {
        let root = row_of(vec![text(1, "a"), spacer(), text(2, "b")]);
        run(&root, Rect::new(0.0, 0.0, 800.0, 40.0), &HashMap::new());
        let left = root.children[0].rect.get();
        let right = root.children[2].rect.get();
        assert!(right.x > 400.0, "the right child should be pushed across, got {}", right.x);
        assert!(right.x - left.right() > 100.0, "the spacer must take a real gap");
    }

    #[test]
    fn centre_alignment_centres_the_row() {
        let mut root = row_of(vec![text(1, "a")]);
        if let ElementKind::Row { main, .. } = &mut root.kind {
            *main = MainAlign::Center;
        }
        run(&root, Rect::new(0.0, 0.0, 400.0, 40.0), &HashMap::new());
        let c = root.children[0].rect.get();
        assert!(c.x > 100.0, "should start well right of zero, got {}", c.x);
        assert!(c.right() < 300.0, "should end well short of the edge, got {}", c.right());
    }

    #[test]
    fn space_between_pushes_the_ends_apart() {
        let mut root = row_of(vec![text(1, "a"), text(2, "b")]);
        if let ElementKind::Row { main, .. } = &mut root.kind {
            *main = MainAlign::SpaceBetween;
        }
        run(&root, Rect::new(0.0, 0.0, 600.0, 40.0), &HashMap::new());
        assert_eq!(root.children[0].rect.get().x, 0.0);
        let last = root.children[1].rect.get();
        assert!(last.right() > 550.0, "the last child should hug the end, got {}", last.right());
    }

    #[test]
    fn padding_shrinks_the_content_box() {
        let mut root = row_of(vec![text(1, "a")]);
        root.style.padding = Edges::all(10.0);
        run(&root, Rect::new(0.0, 0.0, 400.0, 60.0), &HashMap::new());
        let c = root.children[0].rect.get();
        assert!((c.x - 10.0).abs() < 0.01, "content must start after the left padding, got {}", c.x);
    }

    #[test]
    fn margin_pushes_a_child_inward() {
        let mut root = row_of(vec![text(1, "a")]);
        root.children[0].style.margin = Edges::all(6.0);
        run(&root, Rect::new(0.0, 0.0, 400.0, 40.0), &HashMap::new());
        let c = root.children[0].rect.get();
        assert!((c.x - 6.0).abs() < 0.01, "margin must offset the child, got {}", c.x);
    }

    #[test]
    fn a_hidden_element_takes_no_space() {
        let root = row_of(vec![text(1, "a"), text(2, "b")]);
        let mut values: HashMap<ElementIdKey, Resolved> = HashMap::new();
        values.insert(ElementIdKey(1), Resolved { visible: false, ..Default::default() });
        run(&root, Rect::new(0.0, 0.0, 400.0, 40.0), &values);
        assert!(root.children[0].rect.get().is_empty(), "a hidden element gets a zero rect");
        assert!(!root.children[1].rect.get().is_empty(), "a visible sibling still has space");
    }

    #[test]
    fn a_column_stacks_vertically() {
        let root = el(
            0,
            ElementKind::Column { gap: 4.0, main: MainAlign::Start, cross: CrossAlign::Start },
        )
        .with_children(vec![text(1, "a"), text(2, "b")]);
        run(&root, Rect::new(0.0, 0.0, 200.0, 400.0), &HashMap::new());
        let a = root.children[0].rect.get();
        let b = root.children[1].rect.get();
        assert!(a.y < b.y, "the second child must be below");
    }

    #[test]
    fn nested_groups_are_laid_out_with_their_siblings() {
        let mut root = row_of(vec![text(1, "a")]);
        root.groups.push(("right".into(), vec![text(2, "b")]));
        run(&root, Rect::new(0.0, 0.0, 400.0, 40.0), &HashMap::new());
        assert_eq!(root.children[0].rect.get().x, 0.0);
        assert!(root.groups[0].1[0].rect.get().x > 0.0, "a group is laid out like a child");
    }

    #[test]
    fn a_deep_tree_terminates() {
        let mut root = row_of(vec![text(1, "x")]);
        for _ in 0..400 {
            root.children.push(row_of(vec![text(2, "x")]));
        }
        // Must not overflow the stack.
        run(&root, Rect::new(0.0, 0.0, 100.0, 40.0), &HashMap::new());
    }

    #[test]
    fn a_fill_child_takes_the_free_space() {
        let mut root = row_of(vec![text(1, "a")]);
        root.children[0].style.width = Size::Fill;
        run(&root, Rect::new(0.0, 0.0, 400.0, 40.0), &HashMap::new());
        assert!(
            root.children[0].rect.get().w > 300.0,
            "a fill child takes the free space, got {}",
            root.children[0].rect.get().w
        );
    }

    #[test]
    fn two_fillers_split_evenly() {
        let mut root = row_of(vec![text(1, "a"), text(2, "b")]);
        for c in root.children.iter_mut() {
            c.style.width = Size::Fill;
        }
        run(&root, Rect::new(0.0, 0.0, 408.0, 40.0), &HashMap::new());
        let a = root.children[0].rect.get().w;
        let b = root.children[1].rect.get().w;
        assert!((a - b).abs() < 0.01, "fillers must split evenly, got {a} and {b}");
    }

    #[test]
    fn a_fixed_width_is_respected() {
        let mut root = row_of(vec![text(1, "a very long label indeed")]);
        root.children[0].style.width = Size::Fixed(50.0);
        run(&root, Rect::new(0.0, 0.0, 400.0, 40.0), &HashMap::new());
        assert_eq!(root.children[0].rect.get().w, 50.0);
    }

    #[test]
    fn children_never_overflow_a_small_slot() {
        // A panel narrower than its content must clip rather than draw outside.
        let root = row_of(vec![text(1, "aaaaaaaaaaaaaaaaaaaaaaaaaaaa")]);
        run(&root, Rect::new(0.0, 0.0, 50.0, 20.0), &HashMap::new());
        let c = root.children[0].rect.get();
        assert!(c.right() <= 50.01, "child overflowed the slot: {c:?}");
    }

    #[test]
    fn intrinsic_sizing_reports_the_content_extent() {
        let root = row_of(vec![text(1, "aa"), text(2, "bb")]);
        let mut m = ApproxMeasurer;
        let (w, h) = panel_intrinsic(&root, 1000.0, &mut m);
        assert!(w > 0.0 && h > 0.0, "a panel must have a real size, got {w}x{h}");
    }

    #[test]
    fn resolve_values_reads_the_reactor() {
        let r = Reactor::new();
        let id = r.source("t", slowshell_core::Value::str("hello"));
        let mut e = text(1, "static");
        e.dyn_props.text = Some(id);
        let mut map = HashMap::new();
        collect(&e, &r, None, &mut map);
        assert_eq!(map[&ElementIdKey(1)].text, "hello");
    }

    #[test]
    fn resolve_values_falls_back_to_the_static_text() {
        let r = Reactor::new();
        let e = text(1, "static");
        let mut map = HashMap::new();
        collect(&e, &r, None, &mut map);
        assert_eq!(map[&ElementIdKey(1)].text, "static");
    }

    #[test]
    fn a_static_label_works_without_a_value_map() {
        // The layout engine must not depend on the caller having resolved values.
        let root = row_of(vec![text(1, "hello")]);
        run(&root, Rect::new(0.0, 0.0, 400.0, 40.0), &HashMap::new());
        assert!(root.children[0].rect.get().w > 10.0, "a static label must be measured");
    }
}
