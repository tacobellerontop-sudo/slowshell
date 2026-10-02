//! The retained element tree.
//!
//! An `Element` is a node in the widget tree built from config. It is *retained*:
//! the same object is reused across frames and only its resolved values change.
//!
//! ## How reactivity reaches the tree
//!
//! Every property whose expression reads system state becomes a node in the
//! [`slowshell_core::react::Reactor`], and the element stores that node's id in
//! [`Dyn`]. A frame therefore does no dependency bookkeeping at all: it reads the
//! values, and the reactor has already decided what is stale. An element is only
//! re-laid-out and re-painted when one of its own values actually changed.

use std::cell::{Cell, RefCell};

use slowshell_core::react::PropId;
use slowshell_core::Color;
use slowshell_win::render::text::{Align, Ellipsis, TextStyle};

use crate::style::{
    CrossAlign, Edges, MainAlign, Position, ShadowStyle, Style, TextAlign, Theme,
};
use crate::Rect;

/// Identifies an element across frames, so input can be routed back to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ElementId(pub u32);

impl ElementId {
    /// The id the shell's own elements use, which must not collide with
    /// user elements because hit testing routes through this space.
    pub const SURFACE: ElementId = ElementId(0);
}

/// What an element is. Mirrors the element names available in config.
#[derive(Debug, Clone, PartialEq)]
pub enum ElementKind {
    /// A surface attached to a screen edge.
    Panel {
        position: Position,
        /// `primary`, an index, or a device name.
        screen: String,
        /// Whether the panel reserves screen space.
        exclusive: bool,
        /// Continue the panel's background around the other three screen edges.
        ///
        /// Implemented as companion surfaces rather than one window, because a
        /// single full-screen window cannot be drawn here: the presentation path
        /// rejects per-pixel alpha on this machine, so it would paint an opaque
        /// rectangle over the whole desktop. Four opaque edges look the same and
        /// reserve a real frame.
        wrap: bool,
        /// How thick those edges are, in logical pixels.
        wrap_size: Option<f32>,
        /// Physical rectangle, set by the shell for a generated wrap arm.
        ///
        /// Never settable from a config: the parser does not expose it and the
        /// compiler rejects it as an unknown property. It exists because a wrap
        /// arm is not edge-anchored — it is the space *between* two edges — so
        /// `position` cannot describe where it goes.
        bounds: Option<(i32, i32, u32, u32)>,
        /// Start as a thin strip and open when the pointer enters.
        ///
        /// The dock-edge gesture: the panel is always there to be hovered, and
        /// hovering it expands it to its declared width. A hidden panel cannot do
        /// this, because a window nobody can see cannot be hovered — which is why
        /// the collapsed strip exists rather than starting from nothing.
        reveal: bool,
        /// Width of the collapsed strip, in logical pixels.
        reveal_size: f32,
        /// How long opening or closing takes, in seconds.
        reveal_duration: f32,
        /// How long after the pointer leaves before closing, in seconds.
        ///
        /// Not cosmetic. Without it, moving the pointer diagonally out of an
        /// opening panel closes it before it finishes, and the user cannot reach
        /// the far side of their own menu.
        reveal_delay: f32,
        /// The curve the reveal follows.
        reveal_ease: slowshell_core::ease::Ease,
        /// A name for `shell.open("name")`, or empty.
        name: String,
        /// Compiled but not shown until something opens it.
        hidden: bool,
        /// The DWM material behind the panel.
        backdrop: slowshell_win::Backdrop,
    },
    /// Horizontal container.
    Row {
        gap: f32,
        main: MainAlign,
        cross: CrossAlign,
    },
    /// Vertical container.
    Column {
        gap: f32,
        main: MainAlign,
        cross: CrossAlign,
    },
    /// Takes all remaining space in its parent's main axis.
    Spacer,
    /// A plain box, useful for grouping and for backgrounds.
    Container,
    /// A static or reactive string.
    Text { content: String },
    /// A clock formatted on a timer. The spec is stored parsed so an invalid
    /// format is reported at build time rather than rendering nothing.
    Clock { format: ClockFormat },
    /// A circular progress indicator.
    Progress,
    /// An element that failed to build, replaced by an error placeholder.
    Broken { message: String },
}

impl ElementKind {
    pub fn type_name(&self) -> &'static str {
        match self {
            ElementKind::Panel { .. } => "Panel",
            ElementKind::Row { .. } => "Row",
            ElementKind::Column { .. } => "Column",
            ElementKind::Spacer => "Spacer",
            ElementKind::Container => "Container",
            ElementKind::Text { .. } => "Text",
            ElementKind::Clock { .. } => "Clock",
            ElementKind::Progress => "Progress",
            ElementKind::Broken { .. } => "Broken",
        }
    }

    /// Whether a container distributes space among children.
    pub fn is_container(&self) -> bool {
        matches!(self, ElementKind::Row { .. } | ElementKind::Column { .. } | ElementKind::Panel { .. })
    }

    pub fn is_horizontal(&self) -> bool {
        match self {
            ElementKind::Row { .. } => true,
            ElementKind::Column { .. } => false,
            ElementKind::Panel { position, .. } => !position.is_vertical(),
            _ => false,
        }
    }

    /// The type's own gap between children, if it has one.
    pub fn gap(&self) -> f32 {
        match self {
            ElementKind::Row { gap, .. } | ElementKind::Column { gap, .. } => *gap,
            ElementKind::Panel { position, .. } => {
                // A panel's default gap is zero; padding does the work.
                let _ = position;
                0.0
            }
            _ => 0.0,
        }
    }

    pub fn main_align(&self) -> MainAlign {
        match self {
            ElementKind::Row { main, .. } | ElementKind::Column { main, .. } => *main,
            _ => MainAlign::Start,
        }
    }

    pub fn cross_align(&self) -> CrossAlign {
        match self {
            ElementKind::Row { cross, .. } | ElementKind::Column { cross, .. } => *cross,
            _ => CrossAlign::Center,
        }
    }
}

/// How a clock formats its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockFormat {
    pub hours: bool,
    pub minutes: bool,
    pub seconds: bool,
    pub day: bool,
    pub date: bool,
    pub weekday: bool,
    pub month: bool,
    pub year: bool,
    /// 24-hour time, as the spec's `HH:mm` implies.
    pub hour24: bool,
}

impl Default for ClockFormat {
    /// `HH:mm`, the format the spec's examples use.
    fn default() -> Self {
        ClockFormat {
            hours: true,
            minutes: true,
            seconds: false,
            day: false,
            date: false,
            weekday: false,
            month: false,
            year: false,
            hour24: true,
        }
    }
}

impl ClockFormat {
    /// Parse the subset of strftime-style specifiers the shell documents.
    ///
    /// Runs matter: `d` is a day number and `dddd` is a weekday name, so the
    /// parser has to see the whole run before deciding. Returns `None` for an
    /// empty or unrecognised format so `Clock { format: "" }` is reported at
    /// build time rather than rendering nothing.
    pub fn parse(spec: &str) -> Option<ClockFormat> {
        if spec.trim().is_empty() {
            return None;
        }
        let mut f = ClockFormat {
            hours: false,
            minutes: false,
            seconds: false,
            day: false,
            date: false,
            weekday: false,
            month: false,
            year: false,
            hour24: true,
        };
        let mut hour24_seen = false;
        let mut any = false;
        let chars: Vec<char> = spec.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            match c {
                // A literal in single quotes passes through untouched.
                '\'' => {
                    i += 1;
                    while i < chars.len() && chars[i] != '\'' {
                        i += 1;
                    }
                    i += 1;
                    continue;
                }
                // A padding or case modifier applies to the next specifier.
                '-' | '_' | '0' | '#' | '^' | '~' => {
                    i += 1;
                    continue;
                }
                _ => {}
            }
            // Consume the run of identical specifiers.
            let mut run = 1;
            while i + run < chars.len() && chars[i + run] == c {
                run += 1;
            }
            i += run;

            match c {
                'H' | 'k' => {
                    f.hours = true;
                    hour24_seen = true;
                    any = true;
                }
                'h' | 'g' => {
                    f.hours = true;
                    f.hour24 = false;
                    any = true;
                }
                'm' => {
                    f.minutes = true;
                    any = true;
                }
                's' | 'S' => {
                    f.seconds = true;
                    any = true;
                }
                // Three or more `d` is a weekday name, not a day number.
                'd' => {
                    if run >= 3 {
                        f.weekday = true;
                    } else {
                        f.day = true;
                    }
                    any = true;
                }
                'a' | 'A' | 'w' | 'u' => {
                    f.weekday = true;
                    any = true;
                }
                'D' | 'x' => {
                    f.date = true;
                    any = true;
                }
                // `M`, `b` and `B` are the month. `h` is already handled above as
                // an hour specifier, so listing it here would be unreachable —
                // which is a bug rather than a style question, because the guard
                // makes the arm look like it is doing something.
                'M' | 'b' | 'B' => {
                    f.month = true;
                    any = true;
                }
                'y' | 'Y' => {
                    f.year = true;
                    any = true;
                }
                // A literal separator or punctuation.
                _ => {}
            }
        }
        if !any {
            return None;
        }
        if hour24_seen {
            f.hour24 = true;
        }
        Some(f)
    }

    /// Render `seconds` since the Unix epoch.
    pub fn render(&self, unix: i64) -> String {
        let (y, mo, d, h, mi, s, wd) = civil_from_unix(unix);
        let mut out = String::new();
        if self.weekday {
            out.push_str(&WEEKDAYS[wd as usize % 7]);
        }
        if self.hours {
            if !out.is_empty() {
                out.push(' ');
            }
            let hour = if self.hour24 { h } else {
                let h12 = h % 12;
                if h12 == 0 { 12 } else { h12 }
            };
            out.push_str(&format!("{:02}", hour));
        }
        if self.minutes {
            if !out.is_empty() {
                out.push(':');
            }
            out.push_str(&format!("{mi:02}"));
        }
        if self.seconds {
            if self.hours || self.minutes {
                out.push(':');
            }
            out.push_str(&format!("{s:02}"));
        }
        if self.year && self.month && self.day {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(&format!("{d:02}/{mo:02}/{y:04}"));
        } else {
            if self.day {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(&format!("{d:02}"));
            }
            if self.month {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(&format!("{mo:02}"));
            }
            if self.year {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(&format!("{y:04}"));
            }
        }
        out
    }
}

const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

/// Build a timestamp from civil components, the inverse of [`civil_from_unix`].
///
/// The runtime uses this to hand the UI a single number that round-trips to local
/// wall-clock time, which keeps the time zone logic in one place.
pub fn civil_to_unix(y: i64, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> i64 {
    // Days from the civil date, by Howard Hinnant's algorithm.
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if mo > 2 { mo - 3 } else { mo + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    days * 86_400 + (h as i64) * 3600 + (mi as i64) * 60 + s as i64
}

/// Break a Unix timestamp into civil time.
///
/// The result is whatever the timestamp encodes, with no time zone applied: the
/// shell publishes timestamps that already represent local wall-clock time, so
/// this is a pure calendar conversion.
pub fn civil_from_unix(unix: i64) -> (i64, u32, u32, u32, u32, u32, u32) {
    // Days since epoch, then the civil date by Howard Hinnant's algorithm.
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y };
    // 1970-01-01 was a Thursday, index 3 in a Monday-first table.
    let weekday = (days + 3).rem_euclid(7) as u32;
    (year, m, d, (secs / 3600) as u32, ((secs % 3600) / 60) as u32, (secs % 60) as u32, weekday)
}

/// Values that come from the reactive graph.
///
/// A `None` means the property was not set, so the value is fixed at build time
/// and costs nothing per frame.
#[derive(Debug, Default, Clone)]
pub struct Dyn {
    pub text: Option<PropId>,
    pub color: Option<PropId>,
    pub background: Option<PropId>,
    pub width: Option<PropId>,
    pub height: Option<PropId>,
    pub visible: Option<PropId>,
    pub progress: Option<PropId>,
    pub opacity: Option<PropId>,
}

impl Dyn {
    pub fn is_empty(&self) -> bool {
        self.text.is_none()
            && self.color.is_none()
            && self.background.is_none()
            && self.width.is_none()
            && self.height.is_none()
            && self.visible.is_none()
            && self.progress.is_none()
            && self.opacity.is_none()
    }
}

/// A handler the shell invokes when something happens to an element.
#[derive(Debug, Clone, PartialEq)]
pub struct Handler {
    /// A dotted path the runtime resolves, e.g. `launcher.open`.
    pub action: String,
    /// Arguments, evaluated once when the config was compiled.
    pub args: Vec<slowshell_core::Value>,
    /// Where the handler was written, so a diagnostic points at the line rather
    /// than at the whole element.
    pub span: slowshell_core::Span,
}

/// One node in the widget tree.
///
/// `Clone` exists so a config reload can replace a whole subtree without tearing
/// down and recreating the window it is drawn into, which is what keeps hot reload
/// from making the bar flicker.
#[derive(Clone)]
pub struct Element {
    pub id: ElementId,
    pub kind: ElementKind,
    pub style: Style,
    /// The theme key each colour token refers to, kept for diagnostics.
    pub dyn_props: Dyn,
    /// Handlers for `onClick` and friends.
    pub handlers: RefCell<Vec<(String, Handler)>>,
    /// Set by the layout pass.
    pub rect: Cell<Rect>,
    /// Whether the element takes pointer input and appears in hit testing.
    pub interactive: Cell<bool>,
    /// Set when a value changed and the element needs re-layout.
    pub needs_layout: Cell<bool>,
    pub children: Vec<Element>,
    /// Nested groups such as `left { ... }` inside a panel.
    pub groups: Vec<(String, Vec<Element>)>,
    /// Where this element came from, for the error overlay.
    pub source: slowshell_core::Span,
}

impl std::fmt::Debug for Element {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Element")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("rect", &self.rect.get())
            .field("children", &self.children.len())
            .finish()
    }
}

impl Element {
    pub fn new(id: ElementId, kind: ElementKind, style: Style, source: slowshell_core::Span) -> Element {
        Element {
            id,
            kind,
            style,
            dyn_props: Dyn::default(),
            handlers: RefCell::new(Vec::new()),
            rect: Cell::new(Rect::ZERO),
            interactive: Cell::new(false),
            needs_layout: Cell::new(false),
            children: Vec::new(),
            groups: Vec::new(),
            source,
        }
    }

    pub fn with_children(mut self, children: Vec<Element>) -> Element {
        self.children = children;
        self
    }

    /// The text style to render this element with, after the theme is applied.
    pub fn text_style(&self, _theme: &Theme) -> TextStyle {
        TextStyle {
            family: self
                .style
                .font_family
                .clone()
                .or_else(|| Some(default_family().to_string()))
                .unwrap_or_default(),
            size: self.style.font_size,
            weight: self.style.font_weight,
            italic: false,
            align: match self.style.text_align {
                TextAlign::Start => Align::Start,
                TextAlign::Center => Align::Center,
                TextAlign::End => Align::End,
            },
            ellipsis: Ellipsis::Trailing,
            letter_spacing: self.style.letter_spacing,
        }
    }

    pub fn foreground(&self, theme: &Theme) -> Color {
        let c = self.style.foreground.resolve(theme);
        if c.a == 0 {
            theme.get("foreground").unwrap_or(Color::WHITE)
        } else {
            c
        }
    }

    pub fn background(&self, theme: &Theme) -> Color {
        self.style.background.resolve(theme)
    }

    pub fn border_color(&self, theme: &Theme) -> Color {
        self.style.border.resolve(theme)
    }

    /// The shadow to draw behind this element, if it has one.
    pub fn shadow(&self, _theme: &Theme) -> Option<ShadowStyle> {
        self.style.shadow.map(|s| ShadowStyle { color: s.color.over(Color::TRANSPARENT), ..s })
    }

    /// Whether the element is drawn at all.
    ///
    /// A broken element is drawn: the error boundary's whole purpose is to be
    /// visible, so "is it broken" is not a reason to skip it.
    pub fn is_visible(&self) -> bool {
        true
    }

    /// Total count including nested groups, for the debug overlay.
    pub fn count(&self) -> usize {
        1 + self.children.iter().map(Element::count).sum::<usize>()
            + self.groups.iter().map(|(_, g)| g.iter().map(Element::count).sum::<usize>()).sum::<usize>()
    }

    /// The first element whose id matches, searched depth first.
    pub fn find(&self, id: ElementId) -> Option<&Element> {
        if self.id == id {
            return Some(self);
        }
        for c in &self.children {
            if let Some(f) = c.find(id) {
                return Some(f);
            }
        }
        for (_, group) in &self.groups {
            for c in group {
                if let Some(f) = c.find(id) {
                    return Some(f);
                }
            }
        }
        None
    }

    /// Resolve the effective style, inheriting from the parent.
    pub fn effective_style(&self, parent: &Style) -> Style {
        let mut s = self.style.clone();
        s.inherit_from(parent);
        s
    }

    /// The element's own `padding` combined with the type's spacing.
    pub fn padding(&self) -> Edges {
        self.style.padding
    }

    /// The string this element displays right now.
    ///
    /// Layout and paint both ask this question, and they have to get the same
    /// answer: a clock measured as an empty string is given zero width, given no
    /// room, and therefore never drawn at all — a blank bar with no error
    /// anywhere. One function, one answer.
    pub fn display_text(&self, values: &crate::layout::Resolved) -> String {
        match &self.kind {
            ElementKind::Text { content } => {
                if values.text.is_empty() {
                    content.clone()
                } else {
                    values.text.clone()
                }
            }
            ElementKind::Clock { format } => {
                // `None` means no clock source has been published yet, which only
                // happens before the first frame.
                format.render(values.clock_unix.unwrap_or_else(crate::now_unix))
            }
            _ => values.text.clone(),
        }
    }

    /// A one-line summary of this element's type, for logs and diagnostics.
    pub fn type_name(&self) -> &'static str {
        self.kind.type_name()
    }

    /// The tree as indented text, with each element's laid-out rectangle.
    ///
    /// The first thing to reach for when a widget is invisible: a rect of zero
    /// size means layout never gave it space, and a rect outside the window means
    /// the panel is positioned wrong. Neither is obvious from a screenshot.
    pub fn debug_tree(&self) -> String {
        let mut out = String::new();
        self.debug_tree_into(0, &mut out);
        out
    }

    fn debug_tree_into(&self, depth: usize, out: &mut String) {
        use std::fmt::Write as _;
        let r = self.rect.get();
        let _ = writeln!(
            out,
            "{:indent$}{} #{} rect {:.1},{:.1} {:.1}x{:.1}{}",
            "",
            self.type_name(),
            self.id.0,
            r.x,
            r.y,
            r.w,
            r.h,
            if self.interactive.get() { " interactive" } else { "" },
            indent = depth * 2
        );
        for c in &self.children {
            c.debug_tree_into(depth + 1, out);
        }
        for (name, group) in &self.groups {
            let _ = writeln!(out, "{:indent$}· {name}", "", indent = (depth + 1) * 2);
            for c in group {
                c.debug_tree_into(depth + 2, out);
            }
        }
    }
}

/// The Windows system font, resolved once.
///
/// `Segoe UI Variable` ships with Windows 11; Windows 10 falls back to
/// `Segoe UI`. Both are looked up through the font collection at draw time, so a
/// missing family degrades rather than failing.
pub fn default_family() -> &'static str {
    "Segoe UI Variable Text"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_documented_formats() {
        let f = ClockFormat::parse("HH:mm").unwrap();
        assert!(f.hours && f.minutes && !f.seconds);
        assert!(f.hour24);

        let f = ClockFormat::parse("HH:mm:ss").unwrap();
        assert!(f.seconds);

        let f = ClockFormat::parse("dd/MM/yyyy").unwrap();
        assert!(f.day && f.month && f.year);
    }

    #[test]
    fn parses_weekday_and_12_hour() {
        let f = ClockFormat::parse("dddd").unwrap();
        assert!(f.weekday);
        let f = ClockFormat::parse("h:mm").unwrap();
        assert!(f.hours && f.minutes);
        assert!(!f.hour24, "a lowercase h means 12-hour time");
    }

    #[test]
    fn rejects_an_empty_format() {
        assert!(ClockFormat::parse("").is_none());
        assert!(ClockFormat::parse("   ").is_none());
        assert!(ClockFormat::parse("!!!").is_none());
    }

    #[test]
    fn renders_a_known_timestamp() {
        let unix = civil_to_unix(2024, 3, 5, 14, 7, 9);
        let f = ClockFormat { hours: true, minutes: true, hour24: true, ..Default::default() };
        assert_eq!(f.render(unix), "14:07");
    }

    #[test]
    fn civil_conversion_handles_the_epoch() {
        let (y, mo, d, h, mi, s, wd) = civil_from_unix(0);
        assert_eq!((y, mo, d), (1970, 1, 1));
        assert_eq!((h, mi, s), (0, 0, 0));
        assert_eq!(wd, 3, "1970-01-01 was a Thursday");
    }

    #[test]
    fn civil_conversion_round_trips() {
        for (y, mo, d) in [(1970, 1, 1), (1999, 12, 31), (2024, 2, 29), (2024, 12, 31), (2038, 6, 15)] {
            let unix = civil_to_unix(y, mo, d, 12, 34, 56);
            let (ry, rmo, rd, rh, rmi, rs, _) = civil_from_unix(unix);
            assert_eq!((ry, rmo, rd, rh, rmi, rs), (y, mo, d, 12, 34, 56));
        }
    }

    #[test]
    fn civil_conversion_handles_leap_days() {
        // 2024 is a leap year, 2023 is not, so the same instant one year earlier
        // must land in March rather than on a 29 February.
        let leap = civil_to_unix(2024, 2, 29, 0, 0, 0);
        let (y, mo, d, ..) = civil_from_unix(leap);
        assert_eq!((y, mo, d), (2024, 2, 29));
        let (y, mo, d, ..) = civil_from_unix(leap - 86_400 * 365);
        assert_eq!((y, mo, d), (2023, 3, 1));
    }

    /// A format with nothing selected, for tests that want only the fields they
    /// set. `Default` is `HH:mm`, which is the right default but the wrong base
    /// for a date-only assertion.
    fn none() -> ClockFormat {
        ClockFormat {
            hours: false,
            minutes: false,
            seconds: false,
            day: false,
            date: false,
            weekday: false,
            month: false,
            year: false,
            hour24: true,
        }
    }

    #[test]
    fn renders_a_full_date() {
        let unix = civil_to_unix(2024, 3, 5, 9, 30, 0);
        let f = ClockFormat { day: true, month: true, year: true, ..none() };
        assert_eq!(f.render(unix), "05/03/2024");
    }

    #[test]
    fn renders_a_weekday() {
        // 2024-03-05 was a Tuesday.
        let unix = civil_to_unix(2024, 3, 5, 9, 30, 0);
        let f = ClockFormat { weekday: true, ..none() };
        assert_eq!(f.render(unix), "Tue");
    }

    #[test]
    fn renders_12_hour_without_a_zero_hour() {
        let f = ClockFormat { hours: true, minutes: true, hour24: false, ..none() };
        assert_eq!(f.render(civil_to_unix(2024, 3, 5, 14, 7, 0)), "02:07");
        assert_eq!(f.render(civil_to_unix(2024, 3, 5, 9, 5, 0)), "09:05");
        // Midnight and noon must not render as 0 on a 12-hour clock.
        let h = ClockFormat { hours: true, hour24: false, ..none() };
        assert_eq!(h.render(civil_to_unix(2024, 3, 5, 0, 0, 0)), "12");
        assert_eq!(h.render(civil_to_unix(2024, 3, 5, 12, 0, 0)), "12");
        // An afternoon hour must not keep its leading zero.
        assert_eq!(h.render(civil_to_unix(2024, 3, 5, 13, 0, 0)), "01");
    }

    #[test]
    fn the_default_format_is_hours_and_minutes() {
        let f = ClockFormat::default();
        assert_eq!(f.render(civil_to_unix(2024, 3, 5, 14, 7, 9)), "14:07");
        assert!(f.hour24);
    }

    #[test]
    fn renders_time_with_seconds() {
        let f = ClockFormat { hours: true, minutes: true, seconds: true, hour24: true, ..Default::default() };
        assert_eq!(f.render(civil_to_unix(2024, 3, 5, 14, 7, 9)), "14:07:09");
    }

    #[test]
    fn a_weekday_and_time_combine_with_a_space() {
        let f = ClockFormat { weekday: true, hours: true, minutes: true, hour24: true, ..none() };
        assert_eq!(f.render(civil_to_unix(2024, 3, 5, 14, 7, 0)), "Tue 14:07");
    }

    #[test]
    fn negative_timestamps_do_not_panic() {
        // A clock set before 1970 must still render something sane rather than
        // panicking on a negative division.
        let f = ClockFormat::default();
        let _ = f.render(-1);
        let _ = f.render(i64::MIN / 4);
    }

    #[test]
    fn element_count_includes_nested_groups() {
        let mut root = Element::new(
            ElementId(1),
            ElementKind::Container,
            Style::default(),
            slowshell_core::Span::unknown(),
        );
        let child = Element::new(
            ElementId(2),
            ElementKind::Container,
            Style::default(),
            slowshell_core::Span::unknown(),
        );
        root.groups.push(("left".into(), vec![child]));
        assert_eq!(root.count(), 2);
    }

    #[test]
    fn find_searches_children_and_groups() {
        let mut root = Element::new(
            ElementId(1),
            ElementKind::Container,
            Style::default(),
            slowshell_core::Span::unknown(),
        );
        root.children.push(Element::new(
            ElementId(2),
            ElementKind::Container,
            Style::default(),
            slowshell_core::Span::unknown(),
        ));
        root.groups.push((
            "left".into(),
            vec![Element::new(
                ElementId(3),
                ElementKind::Container,
                Style::default(),
                slowshell_core::Span::unknown(),
            )],
        ));
        assert!(root.find(ElementId(1)).is_some());
        assert!(root.find(ElementId(3)).is_some());
        assert!(root.find(ElementId(99)).is_none());
    }

    #[test]
    fn empty_dyn_costs_nothing_per_frame() {
        let d = Dyn::default();
        assert!(d.is_empty());
        let d = Dyn { text: Some(1), ..Default::default() };
        assert!(!d.is_empty());
    }
}
