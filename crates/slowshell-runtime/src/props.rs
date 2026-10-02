//! Property tables.
//!
//! Every element type declares what it accepts. That table is what turns a typo
//! into `Unknown property "foregorund"` plus `Did you mean "foreground"?` instead
//! of a silently ignored line, and it is the single reference the documentation is
//! generated from.

/// The kind of value a property takes, used for coercion and for error text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropKind {
    /// One of a fixed set of words, e.g. `position: "top"`.
    Enum,
    /// A colour literal or a theme token name.
    Color,
    Number,
    Integer,
    Boolean,
    String,
    /// A number, `fill`, or a two/four element list.
    Edges,
    /// A number, `fill` or `auto`.
    Size,
    /// A list of child elements.
    ElementList,
    /// A handler, evaluated when the event fires.
    Handler,
}

impl PropKind {
    pub fn describe(self) -> &'static str {
        match self {
            PropKind::Enum => "one of a fixed set of words",
            PropKind::Color => "a color such as \"#89b4fa\" or \"accent\"",
            PropKind::Number => "a number",
            PropKind::Integer => "a whole number",
            PropKind::Boolean => "true or false",
            PropKind::String => "a string",
            PropKind::Edges => "a number or a [vertical, horizontal] list",
            PropKind::Size => "a number, \"fill\" or \"auto\"",
            PropKind::ElementList => "a list of elements",
            PropKind::Handler => "a call such as launcher.open()",
        }
    }
}

/// One accepted property.
#[derive(Debug, Clone, Copy)]
pub struct PropSpec {
    pub name: &'static str,
    pub kind: PropKind,
    /// The words accepted by an `Enum` property.
    pub variants: &'static [&'static str],
    pub doc: &'static str,
}

const fn p(name: &'static str, kind: PropKind, doc: &'static str) -> PropSpec {
    PropSpec { name, kind, variants: &[], doc }
}

const fn e(
    name: &'static str,
    variants: &'static [&'static str],
    doc: &'static str,
) -> PropSpec {
    PropSpec { name, kind: PropKind::Enum, variants, doc }
}

const POSITION: &[&str] = &["top", "bottom", "left", "right", "floating"];
const SCREEN: &[&str] = &["primary", "all"];
const ALIGN: &[&str] = &["start", "center", "end", "stretch"];
const JUSTIFY: &[&str] = &["start", "center", "end", "space-between"];
const TEXT_ALIGN: &[&str] = &["start", "center", "end"];
const BACKDROP: &[&str] = &["none", "acrylic", "mica", "micaAlt", "blur"];

/// Properties every element accepts, so a theme can set `color` on a `Row` and
/// have it cascade.
pub const COMMON: &[PropSpec] = &[
    p("color", PropKind::Color, "Text and icon colour. Cascades to children."),
    p("background", PropKind::Color, "Fill colour behind the element."),
    p("foreground", PropKind::Color, "Alias for `color`."),
    p("border", PropKind::Color, "Outline colour."),
    p("borderWidth", PropKind::Number, "Outline thickness."),
    p("borderRadius", PropKind::Number, "Corner radius."),
    p("radius", PropKind::Number, "Alias for `borderRadius`."),
    p("padding", PropKind::Edges, "Space inside the element."),
    p("margin", PropKind::Edges, "Space outside the element."),
    p("opacity", PropKind::Number, "0 to 1, composes with children."),
    p("width", PropKind::Size, "Main-axis width, or `fill`."),
    p("height", PropKind::Size, "Cross-axis height."),
    p("minWidth", PropKind::Number, "Smallest allowed width."),
    p("minHeight", PropKind::Number, "Smallest allowed height."),
    p("fontSize", PropKind::Number, "Text size in logical pixels."),
    p("fontWeight", PropKind::Integer, "100 to 900."),
    p("fontFamily", PropKind::String, "Font family name."),
    p("letterSpacing", PropKind::Number, "Extra space between glyphs."),
    e("textAlign", TEXT_ALIGN, "Horizontal text alignment."),
    p("shadow", PropKind::String, "Shadow preset name."),
    p("visible", PropKind::Boolean, "Whether the element takes part in layout."),
    p("onClick", PropKind::Handler, "Runs when the element is clicked."),
    p("onShow", PropKind::Handler, "Runs when the element becomes visible."),
    p("onHide", PropKind::Handler, "Runs when the element is hidden."),
];

const PANEL: &[PropSpec] = &[
    e("position", POSITION, "Which screen edge the panel attaches to."),
    e("screen", SCREEN, "Which display to appear on."),
    p("exclusive", PropKind::Boolean, "Reserve screen space so maximized windows stop at the panel."),
    p(
        "wrap",
        PropKind::Boolean,
        "Continue the panel's background around the other three screen edges.",
    ),
    p(
        "wrapSize",
        PropKind::Number,
        "How thick the wrapped edges are, in logical pixels. Default 6.",
    ),
    p(
        "reveal",
        PropKind::Boolean,
        "Start collapsed to `revealSize` and open when the pointer enters, the way a dock edge does.",
    ),
    p(
        "revealSize",
        PropKind::Number,
        "Width of the collapsed strip when `reveal` is set. Default 8.",
    ),
    p(
        "revealDuration",
        PropKind::Number,
        "Milliseconds to open or close. Default 180.",
    ),
    p(
        "revealDelay",
        PropKind::Number,
        "Milliseconds to wait after the pointer leaves before closing. Default 260.",
    ),
    p("revealEase", PropKind::String, "Easing curve for the reveal. `outCubic` by default."),
    e("backdrop", BACKDROP, "Acrylic, Mica, MicaAlt, Blur or None."),
    p("alwaysOnTop", PropKind::Boolean, "Keep above other windows."),
    p("clickThrough", PropKind::Boolean, "Ignore all pointer input."),
    p("layer", PropKind::Integer, "Stacking order among the shell's own surfaces."),
    p("anchorX", PropKind::Number, "Horizontal position when floating."),
    p("anchorY", PropKind::Number, "Vertical position when floating."),
    p("name", PropKind::String, "A name for `shell.open(\"…\")`."),
    p(
        "hidden",
        PropKind::Boolean,
        "Compile the panel but do not show it until `shell.open` is called.",
    ),
];

const ROW: &[PropSpec] = &[
    p("gap", PropKind::Number, "Space between children."),
    p("spacing", PropKind::Number, "Alias for `gap`."),
    e("cross", ALIGN, "Alignment across the row."),
    e("justify", JUSTIFY, "Distribution along the row."),
];

const COLUMN: &[PropSpec] = &[
    p("gap", PropKind::Number, "Space between children."),
    p("spacing", PropKind::Number, "Alias for `gap`."),
    e("cross", ALIGN, "Alignment across the column."),
    e("justify", JUSTIFY, "Distribution along the column."),
];

const TEXT: &[PropSpec] = &[
    p("text", PropKind::String, "The string to display. May be an expression."),
];

const CLOCK: &[PropSpec] = &[p("format", PropKind::String, "strftime-style format, e.g. `HH:mm`.")];

const PROGRESS: &[PropSpec] = &[p("value", PropKind::Number, "Fraction from 0 to 1."), p(
    "progress",
    PropKind::Number,
    "Alias for `value`.",
)];

/// Properties for a type, including the common set.
pub fn props_for(type_name: &str) -> Vec<PropSpec> {
    let own: &[PropSpec] = match type_name {
        "Panel" => PANEL,
        "Row" => ROW,
        // A `Container` lays out as a vertical stack, so it takes the same layout
        // properties as a `Column`. Declaring them here is what stops
        // `Container { gap: 8 }` from being an error for a box that plainly
        // lays out its children in a line.
        "Column" | "Container" => COLUMN,
        "Text" => TEXT,
        "Clock" => CLOCK,
        "Progress" => PROGRESS,
        _ => &[],
    };
    let mut all = Vec::with_capacity(own.len() + COMMON.len());
    all.extend_from_slice(own);
    all.extend_from_slice(COMMON);
    all
}

/// Whether the type is known, and its canonical name for a "did you mean".
pub fn is_known_type(name: &str) -> bool {
    matches!(
        name,
        "Panel" | "Row" | "Column" | "Spacer" | "Container" | "Text" | "Clock" | "Progress"
    )
}

/// Every known type name, for suggestions.
pub fn known_types() -> Vec<&'static str> {
    vec![
        "Panel",
        "Row",
        "Column",
        "Container",
        "Spacer",
        "Text",
        "Clock",
        "Progress",
    ]
}

/// The named child groups each type accepts.
///
/// `Panel { left { … } center { … } }` is how the spec's examples are written, so
/// a panel must accept groups even though it has no positional arguments.
pub fn groups_for(type_name: &str) -> &'static [&'static str] {
    match type_name {
        "Panel" => &["left", "center", "right", "content", "start", "end"],
        "Row" | "Column" => &[],
        _ => &[],
    }
}

/// Whether the type is a container that accepts children.
pub fn accepts_children(type_name: &str) -> bool {
    matches!(type_name, "Panel" | "Row" | "Column" | "Container")
}

#[cfg(test)]
mod tests {
    use super::*;
    use slowshell_core::closest;

    #[test]
    fn every_type_accepts_the_common_properties() {
        for t in known_types() {
            let names: Vec<&str> = props_for(t).iter().map(|p| p.name).collect();
            assert!(names.contains(&"color"), "{t} must accept `color`");
            assert!(names.contains(&"onClick"), "{t} must accept `onClick`");
        }
    }

    #[test]
    fn a_panel_declares_its_own_properties() {
        let names: Vec<&str> = props_for("Panel").iter().map(|p| p.name).collect();
        for expected in [
            "position",
            "screen",
            "exclusive",
            "wrap",
            "wrapSize",
            "backdrop",
            "alwaysOnTop",
        ] {
            assert!(names.contains(&expected), "Panel must accept `{expected}`");
        }
    }

    #[test]
    fn a_misspelling_suggests_the_real_property() {
        let names: Vec<&str> = props_for("Text").iter().map(|p| p.name).collect();
        let got = closest("textAlgin", &names, 2);
        assert!(got.contains(&"textAlign"), "expected textAlign, got {got:?}");
    }

    #[test]
    fn a_typo_in_a_panel_property_is_caught() {
        let names: Vec<&str> = props_for("Panel").iter().map(|p| p.name).collect();
        let got = closest("positon", &names, 2);
        assert!(got.contains(&"position"), "expected position, got {got:?}");
    }

    #[test]
    fn every_enum_property_documents_its_words() {
        for t in known_types() {
            for spec in props_for(t) {
                if spec.kind == PropKind::Enum {
                    assert!(
                        !spec.variants.is_empty(),
                        "{} must list its accepted words",
                        spec.name
                    );
                }
            }
        }
    }

    #[test]
    fn every_property_has_documentation() {
        for t in known_types() {
            for spec in props_for(t) {
                assert!(!spec.doc.is_empty(), "{t}.{} has no doc", spec.name);
            }
        }
    }

    #[test]
    fn panels_accept_the_documented_groups() {
        let groups = groups_for("Panel");
        for g in ["left", "center", "right", "content"] {
            assert!(groups.contains(&g), "Panel must accept a `{g}` group");
        }
        assert!(groups_for("Row").is_empty());
    }

    #[test]
    fn only_containers_accept_children() {
        assert!(accepts_children("Panel"));
        assert!(accepts_children("Row"));
        assert!(accepts_children("Container"));
        assert!(!accepts_children("Text"));
        assert!(!accepts_children("Clock"));
    }

    #[test]
    fn a_container_lays_out_like_a_column() {
        // It stacks vertically and sizes to its tallest child, so `gap` and
        // `cross` have to mean something on it rather than being rejected.
        let container: Vec<&str> = props_for("Container").iter().map(|p| p.name).collect();
        let column: Vec<&str> = props_for("Column").iter().map(|p| p.name).collect();
        for expected in ["gap", "cross", "justify", "padding", "background"] {
            assert!(container.contains(&expected), "Container must accept `{expected}`");
        }
        assert_eq!(container, column, "Container and Column must stay in step");
    }

    #[test]
    fn unknown_types_are_rejected() {
        assert!(!is_known_type("Widgit"));
        assert!(is_known_type("Panel"));
    }
}
