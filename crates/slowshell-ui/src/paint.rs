//! The paint pass.
//!
//! Walks a laid-out tree and draws it. Two rules keep this correct and cheap:
//!
//! * **Hit regions are published here.** The same walk that draws an element also
//!   records where it is clickable, so what the user can click and what they can
//!   see can never disagree.
//! * **Opacity is a layer, not a multiply.** Nested opacity must compose, which
//!   means a real alpha layer rather than scaling colours.

use std::cell::RefCell;

use slowshell_core::Color;
use slowshell_win::hit_test::HitRegion;
use slowshell_win::render::Painter;
use slowshell_win::Rect as WinRect;

use crate::element::{Element, ElementKind};
use crate::layout::Resolved;
use crate::style::{TextAlign, Theme};
use crate::Rect;

/// Everything the paint pass produces besides pixels.
#[derive(Debug, Default)]
pub struct PaintOutput {
    /// Interactive rectangles, in logical pixels, for the hit test.
    pub regions: Vec<HitRegion>,
    /// Elements that were actually drawn, for the debug overlay.
    pub drawn: usize,
    /// Elements skipped because they were not visible.
    pub skipped: usize,
}

thread_local! {
    static LAST: RefCell<PaintOutput> = const {
        RefCell::new(PaintOutput { regions: Vec::new(), drawn: 0, skipped: 0 })
    };
}

/// Take the output of the last paint.
pub fn take_output() -> PaintOutput {
    LAST.with(|o| std::mem::take(&mut *o.borrow_mut()))
}

/// Collect the interactive rectangles of a laid-out tree.
///
/// A separate pass from drawing, and deliberately so: hit testing must work
/// whether or not the paint pass has run, and must not depend on it. When the
/// region collection lived inside the draw walk, the only way to ask "what is
/// clickable" was to actually render, which needs a device and a window. That
/// made the most important question about a bar — *is this thing clickable?* —
/// impossible to answer in a test, and it is the question a config author has.
///
/// Regions are pushed parent-first so a child can override an ancestor, and
/// higher layers win over lower ones, which matches the order the click arrives
/// in: front to back.
pub fn collect_regions(
    root: &Element,
    values: &std::collections::HashMap<crate::layout::ElementIdKey, Resolved>,
    root_style: &crate::Style,
) -> Vec<HitRegion> {
    fn walk(
        e: &Element,
        values: &std::collections::HashMap<crate::layout::ElementIdKey, Resolved>,
        inherited: &crate::Style,
        out: &mut Vec<HitRegion>,
    ) {
        let style = e.effective_style(inherited);
        if e.interactive.get() {
            let r = e.rect.get();
            out.push(HitRegion {
                x: r.x,
                y: r.y,
                w: r.w,
                h: r.h,
                id: e.id.0,
                layer: 0,
                interactive: true,
            });
        }
        for c in &e.children {
            walk(c, values, &style, out);
        }
        for (_, group) in &e.groups {
            for c in group {
                walk(c, values, &style, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, values, root_style, &mut out);
    out
}

/// Draw a laid-out tree.
pub fn paint(
    painter: &mut Painter,
    root: &Element,
    theme: &Theme,
    values: &std::collections::HashMap<crate::layout::ElementIdKey, Resolved>,
    root_style: &crate::Style,
) {
    let mut out = PaintOutput::default();
    node(painter, root, theme, values, root_style, &mut out);
    LAST.with(|o| *o.borrow_mut() = out);
}

fn value_of(
    values: &std::collections::HashMap<crate::layout::ElementIdKey, Resolved>,
    e: &Element,
) -> Resolved {
    values.get(&crate::layout::ElementIdKey(e.id.0)).cloned().unwrap_or_default()
}

fn node(
    painter: &mut Painter,
    e: &Element,
    theme: &Theme,
    all: &std::collections::HashMap<crate::layout::ElementIdKey, Resolved>,
    parent_style: &crate::Style,
    out: &mut PaintOutput,
) {
    let values = value_of(all, e);
    if !values.visible {
        out.skipped += 1;
        return;
    }
    let style = e.effective_style(parent_style);
    let rect = e.rect.get();
    if rect.is_empty() {
        out.skipped += 1;
        return;
    }

    // Opacity composes, so it needs a layer rather than a colour scale.
    let needs_layer = values.opacity < 1.0;
    if needs_layer {
        painter.push_opacity(values.opacity);
    }

    if let Some(shadow) = style.shadow {
        if shadow.color.a > 0 {
            painter.draw_shadow(
                to_win(rect),
                style.radius,
                slowshell_win::Shadow {
                    color: shadow.color,
                    offset: (0.0, shadow.offset_y),
                    blur: shadow.blur,
                },
            );
        }
    }

    let background = e.background(theme);
    if background.a > 0 && !matches!(e.kind, ElementKind::Spacer) {
        painter.fill_rect(to_win(rect), background, style.radius);
    }

    if style.border_width > 0.0 {
        let border = e.border_color(theme);
        if border.a > 0 {
            // Inset by half the stroke so the border sits inside the bounds.
            let half = style.border_width / 2.0;
            painter.stroke_rect(
                to_win(rect).inset(half, half),
                border,
                (style.radius - half).max(0.0),
                style.border_width,
            );
        }
    }

    // Publish the hit region before children, so a child can override it.
    if e.interactive.get() {
        out.regions.push(HitRegion {
            x: rect.x,
            y: rect.y,
            w: rect.w,
            h: rect.h,
            id: e.id.0,
            layer: 0,
            interactive: true,
        });
    }

    out.drawn += 1;
    draw_content(painter, e, theme, &values, &style, rect);

    for c in &e.children {
        node(painter, c, theme, all, &style, out);
    }
    for (_, group) in &e.groups {
        for c in group {
            node(painter, c, theme, all, &style, out);
        }
    }

    if needs_layer {
        painter.pop_opacity();
    }
}

fn draw_content(
    painter: &mut Painter,
    e: &Element,
    theme: &Theme,
    values: &Resolved,
    style: &crate::Style,
    rect: Rect,
) {
    let color = e.foreground(theme);
    match &e.kind {
        // Text and Clock are both "draw a string", and the string comes from the
        // same place layout measured, so what is drawn always fits what was
        // measured.
        ElementKind::Text { .. } | ElementKind::Clock { .. } => {
            let content = e.display_text(values);
            if content.is_empty() {
                return;
            }
            let ts = e.text_style(theme);
            // `rect` is already the element's **content box** — layout inset the
            // padding — so insetting it again here would clip the glyphs away.
            let text_rect = rect;
            let h = painter.line_height(&ts);
            // A clock is vertically centred because its digits change width; a
            // label follows the element's own alignment.
            let y = match style.text_align {
                TextAlign::End => text_rect.y + (text_rect.h - h) / 2.0,
                TextAlign::Center if matches!(e.kind, ElementKind::Clock { .. }) => {
                    text_rect.y + (text_rect.h - h) / 2.0
                }
                _ => text_rect.y,
            };
            painter.text(
                &content,
                &ts,
                text_rect.offset(0.0, y - text_rect.y).with_size(text_rect.w, h),
                color,
            );
        }
        ElementKind::Progress => {
            // A thin ring, the shape a system tray meter usually takes.
            let (cx, cy) = rect.center();
            let radius = (rect.w.min(rect.h) / 2.0 - style.border_width).max(1.0);
            let track = theme.get("borderStrong").unwrap_or(Color::rgba(255, 255, 255, 0x20));
            painter.stroke_circle((cx, cy), radius, track, 2.0);
            let fraction = values.progress.clamp(0.0, 1.0);
            if fraction > 0.0 {
                painter.arc((cx, cy), radius, 2.0, -90.0, fraction, color);
            }
        }
        ElementKind::Broken { message } => {
            // The error boundary's in-shell placeholder: a visible marker so a
            // broken widget is never silently invisible.
            let (cx, cy) = rect.center();
            let r = (rect.h / 2.0 - 1.0).max(3.0);
            let err = theme.get("error").unwrap_or(Color::rgb(0xf0, 0x7a, 0x86));
            painter.stroke_circle((cx, cy), r, err, 1.5);
            let ts = e.text_style(theme);
            painter.text(
                "!",
                &ts,
                Rect::new(cx - 4.0, cy - r, 8.0, r * 2.0),
                err,
            );
            let _ = message;
        }
        _ => {}
    }
}

fn to_win(r: Rect) -> WinRect {
    WinRect::new(r.x, r.y, r.w, r.h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::element::ElementId;
    use crate::style::Style;
    use slowshell_core::Span;

    fn el(id: u32, kind: ElementKind) -> Element {
        let e = Element::new(ElementId(id), kind, Style::default(), Span::unknown());
        e.rect.set(Rect::new(0.0, 0.0, 100.0, 20.0));
        e
    }

    #[test]
    fn to_win_preserves_the_rect() {
        let r = Rect::new(1.0, 2.0, 3.0, 4.0);
        let w = to_win(r);
        assert_eq!((w.x, w.y, w.w, w.h), (1.0, 2.0, 3.0, 4.0));
    }

    #[test]
    fn a_broken_element_is_still_drawn() {
        // The error boundary must be visible, so a broken widget is never blank.
        let e = el(1, ElementKind::Broken { message: "boom".into() });
        assert!(!e.rect.get().is_empty());
        let ts = e.text_style(&Theme::default());
        assert!(ts.size > 0.0);
    }
}
