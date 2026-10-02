//! Visual style: colours, metrics and the default theme.
//!
//! A theme is data, not code, so a user can retint a shell without recompiling.
//! Style resolution happens once at build time; the renderer only ever sees
//! resolved values.

use slowshell_core::{Color, Value};

/// A colour that may come from the theme rather than the config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ColorRef {
    /// Theme key such as `accent`.
    pub token: Option<&'static str>,
    /// Literal colour, used when no token is named.
    pub literal: Color,
}

impl ColorRef {
    pub fn token(name: &'static str) -> ColorRef {
        ColorRef { token: Some(name), literal: Color::TRANSPARENT }
    }

    pub fn literal(c: Color) -> ColorRef {
        ColorRef { token: None, literal: c }
    }

    /// Resolve against a theme, falling back to the literal.
    pub fn resolve(&self, theme: &Theme) -> Color {
        match self.token.and_then(|t| theme.get(t)) {
            Some(c) if c.a > 0 => c,
            _ => self.literal,
        }
    }

    pub fn is_transparent(&self) -> bool {
        self.token.is_none() && self.literal.a == 0
    }
}

impl From<Color> for ColorRef {
    fn from(c: Color) -> Self {
        ColorRef::literal(c)
    }
}

/// Edge lengths. A single value means all four sides.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Edges {
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub left: f32,
}

impl Edges {
    pub const ZERO: Edges = Edges { top: 0.0, right: 0.0, bottom: 0.0, left: 0.0 };

    pub fn all(v: f32) -> Edges {
        Edges { top: v, right: v, bottom: v, left: v }
    }

    pub fn from_value(v: &Value) -> Edges {
        match v {
            Value::List(items) if items.len() == 2 => Edges {
                top: items[0].to_f64_lossy() as f32,
                right: items[1].to_f64_lossy() as f32,
                bottom: items[1].to_f64_lossy() as f32,
                left: items[0].to_f64_lossy() as f32,
            },
            Value::List(items) if items.len() == 4 => Edges {
                top: items[0].to_f64_lossy() as f32,
                right: items[1].to_f64_lossy() as f32,
                bottom: items[2].to_f64_lossy() as f32,
                left: items[3].to_f64_lossy() as f32,
            },
            other => Edges::all(other.to_f64_lossy() as f32),
        }
    }

    pub fn horizontal(&self) -> f32 {
        self.left + self.right
    }

    pub fn vertical(&self) -> f32 {
        self.top + self.bottom
    }
}

/// Where a panel attaches to the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Position {
    #[default]
    Top,
    Bottom,
    Left,
    Right,
    /// A panel that floats and is positioned explicitly.
    Floating,
}

impl Position {
    pub fn parse(s: &str) -> Option<Position> {
        Some(match s.to_ascii_lowercase().as_str() {
            "top" => Position::Top,
            "bottom" => Position::Bottom,
            "left" => Position::Left,
            "right" => Position::Right,
            "floating" | "float" => Position::Floating,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Position::Top => "top",
            Position::Bottom => "bottom",
            Position::Left => "left",
            Position::Right => "right",
            Position::Floating => "floating",
        }
    }

    pub fn is_vertical(self) -> bool {
        matches!(self, Position::Left | Position::Right)
    }
}

/// Alignment of children along the main axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MainAlign {
    #[default]
    Start,
    Center,
    End,
    /// Distribute free space evenly, as `space-between` does.
    SpaceBetween,
}

impl MainAlign {
    pub fn parse(s: &str) -> Option<MainAlign> {
        Some(match s.to_ascii_lowercase().as_str() {
            "start" | "left" | "flex-start" => MainAlign::Start,
            "center" | "centre" | "middle" => MainAlign::Center,
            "end" | "right" | "flex-end" => MainAlign::End,
            "space-between" | "between" => MainAlign::SpaceBetween,
            _ => return None,
        })
    }
}

/// Alignment of children across the main axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CrossAlign {
    #[default]
    Center,
    Start,
    End,
    Stretch,
}

impl CrossAlign {
    pub fn parse(s: &str) -> Option<CrossAlign> {
        Some(match s.to_ascii_lowercase().as_str() {
            "center" | "centre" | "middle" => CrossAlign::Center,
            "start" | "top" | "flex-start" => CrossAlign::Start,
            "end" | "bottom" | "flex-end" => CrossAlign::End,
            "stretch" | "fill" => CrossAlign::Stretch,
            _ => return None,
        })
    }
}

/// How an element is sized in its parent's main axis.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Size {
    /// Take the space the element asks for.
    #[default]
    Auto,
    /// Take everything the parent has left.
    Fill,
    /// A fixed length.
    Fixed(f32),
}

impl Size {
    pub fn parse(v: &Value) -> Size {
        match v {
            Value::Str(s) if &**s == "fill" || &**s == "1fr" => Size::Fill,
            Value::Str(s) if &**s == "auto" => Size::Auto,
            other => Size::Fixed(other.to_f64_lossy() as f32),
        }
    }
}

/// The resolved visual style of an element.
#[derive(Debug, Clone, PartialEq)]
pub struct Style {
    pub background: ColorRef,
    pub foreground: ColorRef,
    pub border: ColorRef,
    pub border_width: f32,
    pub radius: f32,
    pub margin: Edges,
    pub padding: Edges,
    pub opacity: f32,
    /// `None` means no shadow.
    pub shadow: Option<ShadowStyle>,
    pub font_size: f32,
    pub font_weight: u16,
    pub font_family: Option<String>,
    pub letter_spacing: f32,
    pub text_align: TextAlign,
    pub width: Size,
    pub height: Size,
    pub min_width: f32,
    pub min_height: f32,
}

impl Default for Style {
    fn default() -> Self {
        Style {
            background: ColorRef::default(),
            foreground: ColorRef::default(),
            border: ColorRef::default(),
            border_width: 0.0,
            radius: 0.0,
            margin: Edges::ZERO,
            padding: Edges::ZERO,
            opacity: 1.0,
            shadow: None,
            font_size: 14.0,
            font_weight: 400,
            font_family: None,
            letter_spacing: 0.0,
            text_align: TextAlign::default(),
            width: Size::Auto,
            height: Size::Auto,
            min_width: 0.0,
            min_height: 0.0,
        }
    }
}

impl Style {
    /// The part of a style that a child inherits.
    ///
    /// Typography and colour cascade, so a bar sets them once instead of
    /// repeating them on every widget. Layout properties do **not**: padding in
    /// particular has to stay with the element that declared it. If it cascaded,
    /// a `Row { padding: [0, 12] }` would apply its padding once to its own
    /// content box and again to each child, and the labels inside would end up
    /// squeezed by 24 pixels with nothing to show for it — which reads as "the
    /// text is invisible" rather than as a padding bug.
    ///
    /// [`Style::inherit_from`] is the same rule applied at paint time; this is
    /// the same rule applied while compiling, so the two cannot disagree about
    /// which properties cascade.
    pub fn inheritable(&self) -> Style {
        Style {
            foreground: self.foreground,
            font_family: self.font_family.clone(),
            font_size: self.font_size,
            font_weight: self.font_weight,
            letter_spacing: self.letter_spacing,
            radius: self.radius,
            text_align: self.text_align,
            ..Style::default()
        }
    }

    /// Inherit the properties a child should take from its parent when unset.
    ///
    /// Text colour and typography cascade because a bar sets them once instead of
    /// repeating them on every widget.
    pub fn inherit_from(&mut self, parent: &Style) {
        if self.foreground.is_transparent() {
            self.foreground = parent.foreground;
        }
        if self.font_family.is_none() {
            self.font_family = parent.font_family.clone();
        }
        // Only the type scale cascades when the child did not ask for a size.
        if self.font_size == 14.0 {
            self.font_size = parent.font_size;
        }
        if self.font_weight == 400 {
            self.font_weight = parent.font_weight;
        }
        if self.letter_spacing == 0.0 {
            self.letter_spacing = parent.letter_spacing;
        }
        if self.radius == 0.0 {
            self.radius = parent.radius;
        }
        if self.text_align == TextAlign::default() && parent.text_align != TextAlign::default() {
            self.text_align = parent.text_align;
        }
    }
}

/// Shadow parameters, resolved.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShadowStyle {
    pub color: Color,
    pub offset_y: f32,
    pub blur: f32,
}

impl Default for ShadowStyle {
    fn default() -> Self {
        ShadowStyle { color: Color::rgba(0, 0, 0, 90), offset_y: 4.0, blur: 16.0 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextAlign {
    #[default]
    Start,
    Center,
    End,
}

/// A named palette.
///
/// The default is a modern dark theme: low-chroma backgrounds so translucent
/// panels read cleanly, one accent, and a foreground ramp for hierarchy.
#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    pub name: String,
    pub colors: Vec<(&'static str, Color)>,
    pub radius: f32,
    /// Base type size, which widgets scale from.
    pub font_size: f32,
    pub dark: bool,
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            name: "slowshell-dark".into(),
            dark: true,
            radius: 10.0,
            font_size: 14.0,
            colors: vec![
                // Surfaces
                ("background", Color::rgba(0x1a, 0x1b, 0x26, 0xf2)),
                ("surface", Color::rgba(0x24, 0x25, 0x32, 0xf5)),
                ("surfaceAlt", Color::rgba(0x2e, 0x30, 0x40, 0xf7)),
                ("overlay", Color::rgba(0x1e, 0x1f, 0x2b, 0xfa)),
                // Text
                ("foreground", Color::rgb(0xe6, 0xe7, 0xef)),
                ("foregroundMuted", Color::rgb(0x9a, 0x9d, 0xb0)),
                ("foregroundSubtle", Color::rgb(0x6b, 0x6e, 0x80)),
                // Accent
                ("accent", Color::rgb(0x7c, 0x9c, 0xff)),
                ("accentHover", Color::rgb(0x93, 0xae, 0xff)),
                ("accentPressed", Color::rgb(0x63, 0x83, 0xe0)),
                // Semantic
                ("success", Color::rgb(0x6b, 0xd0, 0x8c)),
                ("warning", Color::rgb(0xe8, 0xc0, 0x74)),
                ("error", Color::rgb(0xf0, 0x7a, 0x86)),
                ("batteryLow", Color::rgb(0xf0, 0xa8, 0x5a)),
                ("batteryCharging", Color::rgb(0x7c, 0xd0, 0x8c)),
                // Lines
                ("border", Color::rgba(0xff, 0xff, 0xff, 0x0e)),
                ("borderStrong", Color::rgba(0xff, 0xff, 0xff, 0x1f)),
                // A dimmed backdrop for acrylic that is not available.
                ("scrim", Color::rgba(0x0d, 0x0e, 0x14, 0x99)),
            ],
        }
    }
}

impl Theme {
    pub fn get(&self, key: &str) -> Option<Color> {
        self.colors.iter().find(|(k, _)| *k == key).map(|(_, c)| *c)
    }

    pub fn set(&mut self, key: &'static str, color: Color) {
        match self.colors.iter_mut().find(|(k, _)| *k == key) {
            Some((_, c)) => *c = color,
            None => self.colors.push((key, color)),
        }
    }

    /// A readable foreground for a background, for themes that do not say.
    pub fn contrast(&self, bg: Color) -> Color {
        if bg.luminance() > 0.45 {
            Color::rgb(0x1a, 0x1b, 0x26)
        } else {
            Color::rgb(0xe6, 0xe7, 0xef)
        }
    }

    /// Every key, for `did you mean` hints on a misspelled colour token.
    pub fn keys(&self) -> Vec<&'static str> {
        self.colors.iter().map(|(k, _)| *k).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_parsing_is_case_insensitive() {
        assert_eq!(Position::parse("top"), Some(Position::Top));
        assert_eq!(Position::parse("BOTTOM"), Some(Position::Bottom));
        assert_eq!(Position::parse("floating"), Some(Position::Floating));
        assert_eq!(Position::parse("middle"), None);
        assert!(Position::Left.is_vertical());
        assert!(!Position::Top.is_vertical());
    }

    #[test]
    fn align_parsing_covers_synonyms() {
        assert_eq!(MainAlign::parse("center"), Some(MainAlign::Center));
        assert_eq!(MainAlign::parse("between"), Some(MainAlign::SpaceBetween));
        assert_eq!(MainAlign::parse("nope"), None);
        assert_eq!(CrossAlign::parse("stretch"), Some(CrossAlign::Stretch));
    }

    #[test]
    fn size_parsing_handles_keywords_and_numbers() {
        assert_eq!(Size::parse(&Value::str("fill")), Size::Fill);
        assert_eq!(Size::parse(&Value::str("auto")), Size::Auto);
        assert_eq!(Size::parse(&Value::Int(40)), Size::Fixed(40.0));
    }

    #[test]
    fn edges_parse_from_one_two_or_four_values() {
        assert_eq!(Edges::from_value(&Value::Int(4)), Edges::all(4.0));
        let two = Edges::from_value(&Value::list(vec![Value::Int(1), Value::Int(2)]));
        assert_eq!((two.top, two.right, two.bottom, two.left), (1.0, 2.0, 2.0, 1.0));
        let four = Edges::from_value(&Value::list(vec![
            Value::Int(1),
            Value::Int(2),
            Value::Int(3),
            Value::Int(4),
        ]));
        assert_eq!((four.top, four.right, four.bottom, four.left), (1.0, 2.0, 3.0, 4.0));
    }

    #[test]
    fn style_inherits_typography_but_not_layout() {
        let parent = Style {
            foreground: ColorRef::token("foreground"),
            font_size: 18.0,
            font_weight: 600,
            radius: 8.0,
            ..Default::default()
        };
        let mut child = Style { background: ColorRef::literal(Color::WHITE), ..Default::default() };
        child.inherit_from(&parent);
        assert_eq!(child.foreground, ColorRef::token("foreground"));
        assert_eq!(child.font_size, 18.0);
        assert_eq!(child.font_weight, 600);
        assert_eq!(child.radius, 8.0);
        // An explicit background must survive inheritance.
        assert_eq!(child.background, ColorRef::literal(Color::WHITE));
    }

    #[test]
    fn style_does_not_inherit_over_an_explicit_value() {
        let parent = Style { font_size: 18.0, ..Default::default() };
        let mut child = Style { font_size: 11.0, ..Default::default() };
        child.inherit_from(&parent);
        assert_eq!(child.font_size, 11.0, "an explicit size must win");
    }

    #[test]
    fn default_theme_has_the_tokens_widgets_need() {
        let t = Theme::default();
        for key in ["background", "surface", "foreground", "accent", "border"] {
            assert!(t.get(key).is_some(), "missing token {key}");
        }
        assert!(t.dark);
        assert!(t.get("background").unwrap().a > 0, "the default background must be opaque");
    }

    #[test]
    fn contrast_picks_a_readable_foreground() {
        let t = Theme::default();
        assert_eq!(t.contrast(Color::WHITE).r, 0x1a);
        assert_eq!(t.contrast(Color::BLACK).r, 0xe6);
    }

    #[test]
    fn theme_overrides_and_adds_tokens() {
        let mut t = Theme::default();
        t.set("accent", Color::rgb(255, 0, 0));
        assert_eq!(t.get("accent"), Some(Color::rgb(255, 0, 0)));
        let added = Color::rgb(0, 0, 255);
        t.set("custom", added);
        assert_eq!(t.get("custom"), Some(added));
        assert!(t.keys().contains(&"custom"));
    }
}
