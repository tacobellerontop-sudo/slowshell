//! The compiler: config AST to a reactive element tree.
//!
//! This is where config becomes UI. Three things happen for every element:
//!
//! 1. Its properties are validated against [`crate::props`], so a typo is a
//!    diagnostic with a suggestion rather than a silently ignored line.
//! 2. Every property whose expression reads system state becomes a node in the
//!    reactor, so updates are automatic.
//! 3. Failures are isolated by an error boundary: a broken element becomes a
//!    visible placeholder and the rest of the shell keeps working.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use slowshell_core::diag::{Diag, DiagKind, DiagResult, Severity, Span};
use slowshell_core::eval::{eval, Env};
use slowshell_core::lang::ast::{Expr, Item};
use slowshell_core::react::Reactor;
use slowshell_core::{Color, Value};
use slowshell_ui::element::{ClockFormat, Element, ElementId, ElementKind, Handler};
use slowshell_ui::style::{
    ColorRef, CrossAlign, Edges, MainAlign, Position, ShadowStyle, Size, Style, TextAlign,
};

use crate::props::{self, PropKind};

/// Allocates element ids. Ids are unique across a surface so a click can be
/// routed back to exactly one element.
#[derive(Debug, Default)]
pub struct IdGen {
    next: u32,
}

impl IdGen {
    pub fn new() -> IdGen {
        IdGen { next: 1 }
    }

    pub fn take(&mut self) -> ElementId {
        let id = self.next;
        self.next += 1;
        ElementId(id)
    }
}

/// The result of compiling a config.
#[derive(Clone)]
pub struct Compiled {
    /// Top-level panels, one per surface.
    pub panels: Vec<Element>,
    /// Everything that went wrong, in source order.
    pub diagnostics: Vec<Diag>,
    /// Theme overrides found in the config.
    pub theme: slowshell_ui::Theme,
}

impl Compiled {
    /// A short report for the log and for `shellctl reload`.
    pub fn summary(&self) -> String {
        let mut s = format!(
            "{} panel(s), {} element(s)",
            self.panels.len(),
            self.panels.iter().map(|p| p.count()).sum::<usize>()
        );
        if !self.diagnostics.is_empty() {
            s.push_str(&format!(", {} problem(s)", self.diagnostics.len()));
        }
        s
    }
}

/// Compile a parsed document into element trees.
pub fn compile(
    doc: &slowshell_core::Document,
    env: &Rc<Env>,
    reactor: &Rc<Reactor>,
    mut ids: IdGen,
) -> Compiled {
    let mut c = Compiler {
        env: env.clone(),
        reactor: reactor.clone(),
        ids: &mut ids,
        diagnostics: Vec::new(),
        theme: slowshell_ui::Theme::default(),
    };
    let mut panels = Vec::new();
    for child in doc.roots() {
        let item = child.item.clone();
        // `Theme { … }` is a config block, not a widget: it retints the palette and
        // contributes no surface.
        if &*item.type_name == "Theme" {
            c.apply_theme(&item);
            continue;
        }
        if !props::is_known_type(&item.type_name) {
            let names = props::known_types();
            c.error(
                Diag::error(
                    DiagKind::UnknownType,
                    format!("Unknown element `{}`", item.type_name),
                )
                .with_span(item.span.clone())
                .with_hint(format!("Known elements: {}", names.join(", ")))
                .with_suggestion(&names, &item.type_name),
            );
            continue;
        }
        // Only a `Panel` may sit at the top level; everything else is a widget.
        if &*item.type_name != "Panel" {
            c.error(
                Diag::error(
                    DiagKind::UnknownType,
                    format!(
                        "`{}` cannot start a shell config; a config must begin with `Panel {{ … }}`",
                        item.type_name
                    ),
                )
                .with_span(item.span.clone())
                .with_hint("Wrap it in a `Panel { … }`.".to_string()),
            );
            continue;
        }
        panels.push(c.element(&item, Style::default(), 0));
    }
    Compiled { panels, diagnostics: c.diagnostics, theme: c.theme }
}

struct Compiler<'a> {
    env: Rc<Env>,
    reactor: Rc<Reactor>,
    ids: &'a mut IdGen,
    diagnostics: Vec<Diag>,
    theme: slowshell_ui::Theme,
}

/// A guard against a config that nests elements without end.
const MAX_DEPTH: usize = 32;

impl Compiler<'_> {
    fn error(&mut self, d: Diag) {
        // Two diagnostics for the same span mean the same mistake twice, which
        // would bury the useful ones.
        if self
            .diagnostics
            .iter()
            .any(|e| e.span.as_ref() == d.span.as_ref() && e.message == d.message)
        {
            return;
        }
        self.diagnostics.push(d);
    }

    /// Compile one element, or an error placeholder if it cannot be built.
    fn element(&mut self, item: &Item, parent: Style, depth: usize) -> Element {
        if depth > MAX_DEPTH {
            // Reported, not just truncated. A config nested past the cap used to
            // lose the bottom of its tree with no diagnostic at all, which is the
            // worst possible outcome: the bar renders, it is simply missing
            // whatever was below the cap, and nothing says why.
            self.error(
                Diag::error(
                    DiagKind::Runtime,
                    format!("Elements are nested more than {MAX_DEPTH} deep; this one was dropped"),
                )
                .with_span(item.span.clone())
                .with_note(format!("`{}`", item.type_name))
                .with_hint(
                    "Split the panel across files with `include`, or flatten the nesting. \
                     Layout stops descending past the cap so a runaway cannot exhaust the stack."
                        .to_string(),
                ),
            );
            let mut e = Element::new(
                self.ids.take(),
                ElementKind::Broken { message: "element nesting is too deep".into() },
                Style::default(),
                item.span.clone(),
            );
            e.style.background = ColorRef::token("error");
            return e;
        }

        let type_name = item.type_name.to_string();
        // `Theme { … }` is a config block, not a widget: it retints the palette.
        if type_name == "Theme" {
            self.apply_theme(item);
            return self.element(
                &Item {
                    type_name: Arc::from("Container"),
                    args: Vec::new(),
                    props: Vec::new(),
                    children: item.children.clone(),
                    span: item.span.clone(),
                    includes: Vec::new(),
                },
                parent,
                depth + 1,
            );
        }

        let kind = match self.kind_of(item, &type_name) {
            Ok(k) => k,
            Err(d) => return self.broken(item, d),
        };

        let mut style = self.style_of(item, parent, &type_name);
        // A `Theme { fontSize: … }` block sets the base type size for everything
        // that does not ask for one. The compiler is the only place that knows
        // whether `fontSize` was actually written, so it is the only place the
        // theme's size can be applied without overriding an explicit value.
        if !item.props.iter().any(|p| &*p.name == "fontSize") {
            style.font_size = self.theme.font_size;
        }
        let id = self.ids.take();
        let mut element = Element::new(id, kind, style, item.span.clone());
        self.bind_dynamic(&mut element, item, &type_name);

        // Marked here, in one place, rather than by whoever happens to be the
        // parent. Three separate call sites used to set this and one of them was
        // missed, so `left { Text { onClick: … } }` — the single most common shape
        // in a bar, a widget directly inside a panel band — was never clickable
        // while `left { Row { Text { onClick: … } } }` was.
        //
        // The rule is exactly "has an `onClick`". A background does not make a
        // widget interactive: a `Row` with a fill colour would then swallow
        // clicks across its whole width, and a bar would stop being click-through
        // for reasons the config author never asked for.
        element.interactive.set(has_click_handler(&element));

        // The parser stores both positional children and named groups in
        // `item.children`, because `left { … }` and `Row { … }` are syntactically
        // identical. Partition them here, where the type's group names are known.
        let group_names = props::groups_for(&type_name);
        let (positional, groups): (Vec<_>, Vec<_>) = item
            .children
            .iter()
            .cloned()
            .partition(|c| !group_names.contains(&c.name.to_string().as_str()));

        if props::accepts_children(&type_name) {
            for c in positional {
                let sub = c.item.clone();
                if !props::is_known_type(&sub.type_name) {
                    self.unknown_child(&c);
                    continue;
                }
                let style_for_child = element.style.clone();
                let built = self.element(&sub, style_for_child, depth + 1);
                element.children.push(built);
            }
        } else if !positional.is_empty() {
            for c in positional {
                self.error(
                    Diag::error(
                        DiagKind::UnknownProperty,
                        format!("`{type_name}` cannot contain a `{}` element", c.item.type_name),
                    )
                    .with_span(c.span.clone())
                    .with_hint(format!(
                        "`{type_name}` is a leaf. Wrap it in a `Row` or a `Column` to place it."
                    )),
                );
            }
        }

        // Named groups such as `left { … }` inside a panel.
        for group in groups {
            let name = group.name.to_string();
            if props::is_known_type(&group.item.type_name) {
                // A known type used where a group name was expected: treat it as a
                // child rather than rejecting a perfectly good element.
                let style_for_child = element.style.clone();
                if props::accepts_children(&type_name) {
                    let built = self.element(&group.item, style_for_child, depth + 1);
                    element.children.push(built);
                }
                continue;
            }
            if !group_names.contains(&name.as_str()) {
                let mut names: Vec<String> =
                    group_names.iter().map(|s| s.to_string()).collect();
                names.extend(props::known_types().iter().map(|s| s.to_string()));
                let cands: Vec<&str> = names.iter().map(String::as_str).collect();
                self.error(
                    Diag::error(
                        DiagKind::UnknownType,
                        format!("Unknown element or group `{name}` inside `{type_name}`"),
                    )
                    .with_span(group.span.clone())
                    .with_suggestion(&cands, &name),
                );
                continue;
            }
            let mut built_group = Vec::new();
            let style_for_group = element.style.clone();
            for c in &group.item.children {
                if !props::is_known_type(&c.item.type_name) {
                    self.unknown_child(c);
                    continue;
                }
                built_group.push(self.element(&c.item, style_for_group.clone(), depth + 1));
            }
            element.groups.push((name, built_group));
        }

        // `include` splices another file's elements in place.
        for (path, span) in &item.includes {
            self.error(
                Diag::warning(
                    DiagKind::Io,
                    format!("Could not include `{path}`"),
                )
                .with_span(span.clone())
                .with_note("Includes are resolved by the loader before compilation."),
            );
        }

        element
    }

    fn unknown_child(&mut self, child: &slowshell_core::lang::ast::Child) {
        let names = props::known_types();
        let cands: Vec<&str> = names.clone();
        self.error(
            Diag::error(
                DiagKind::UnknownType,
                format!("Unknown element `{}`", child.item.type_name),
            )
            .with_span(child.span.clone())
            .with_suggestion(&cands, &child.item.type_name),
        );
    }

    /// Replace a failed element with something the user can see.
    fn broken(&mut self, item: &Item, diag: Diag) -> Element {
        self.error(diag.clone());
        let mut e = Element::new(
            self.ids.take(),
            ElementKind::Broken { message: diag.message.clone() },
            Style {
                background: ColorRef::token("error"),
                ..Default::default()
            },
            item.span.clone(),
        );
        e.style.foreground = ColorRef::token("background");
        e
    }

    fn kind_of(&mut self, item: &Item, type_name: &str) -> DiagResult<ElementKind> {
        let get_str = |name: &str| -> Option<String> {
            item.prop(name).and_then(|p| match &p.value {
                Expr::Literal(Value::Str(s)) => Some(s.to_string()),
                // A dynamic string is resolved at build time for structural
                // properties, which is the only thing that can work here.
                _ => self.literal_of(&p.value),
            })
        };
        let get_num = |name: &str| -> Option<f32> {
            item.prop(name).and_then(|p| self.number_of(&p.value))
        };
        let get_bool = |name: &str| -> bool {
            item.prop(name)
                .map(|p| self.bool_of(&p.value).unwrap_or(false))
                .unwrap_or(false)
        };

        Ok(match type_name {
            "Panel" => ElementKind::Panel {
                position: get_str("position")
                    .and_then(|s| Position::parse(&s))
                    .unwrap_or_default(),
                screen: get_str("screen").unwrap_or_else(|| "primary".into()),
                exclusive: get_bool("exclusive"),
                wrap: get_bool("wrap"),
                wrap_size: get_num("wrapSize"),
                // Only the shell's wrap expander ever sets this, never a config,
                // and `bounds` is not an accepted property so it cannot get here
                // from one.
                bounds: None,
                reveal: get_bool("reveal"),
                reveal_size: get_num("revealSize").unwrap_or(8.0),
                // Milliseconds in the config, seconds in the model. The shell's
                // clock is in seconds and the tween divides by a duration, so the
                // conversion happens once here rather than at every call site.
                reveal_duration: get_num("revealDuration").unwrap_or(180.0) / 1000.0,
                reveal_delay: get_num("revealDelay").unwrap_or(260.0) / 1000.0,
                reveal_ease: get_str("revealEase")
                    .map(|s| slowshell_core::ease::Ease::parse(&s))
                    .unwrap_or(slowshell_core::ease::Ease::OutCubic),
                name: get_str("name").unwrap_or_default(),
                hidden: get_bool("hidden"),
                backdrop: get_str("backdrop")
                    .and_then(|s| slowshell_win::Backdrop::parse(&s))
                    .unwrap_or(slowshell_win::Backdrop::None),
            },
            "Row" | "Column" => {
                let gap = get_num("gap").or_else(|| get_num("spacing")).unwrap_or(0.0);
                let main = get_str("justify")
                    .and_then(|s| MainAlign::parse(&s))
                    .unwrap_or_default();
                let cross = get_str("cross")
                    .and_then(|s| CrossAlign::parse(&s))
                    .unwrap_or(CrossAlign::Center);
                if type_name == "Row" {
                    ElementKind::Row { gap, main, cross }
                } else {
                    ElementKind::Column { gap, main, cross }
                }
            }
            "Container" => ElementKind::Container,
            "Spacer" => ElementKind::Spacer,
            "Progress" => ElementKind::Progress,
            "Text" => {
                let content = get_str("text").unwrap_or_default();
                ElementKind::Text { content }
            }
            "Clock" => {
                let spec = get_str("format").unwrap_or_else(|| "HH:mm".into());
                match ClockFormat::parse(&spec) {
                    Some(format) => ElementKind::Clock { format },
                    None => {
                        return Err(Diag::error(
                            DiagKind::TypeMismatch,
                            format!("Invalid clock format \"{spec}\""),
                        )
                        .with_span(item.span.clone())
                        .with_note("A format needs at least one of: HH h mm ss d D a M y.".to_string())
                        .with_hint("Try `HH:mm` for a 24-hour clock.".to_string()))
                    }
                }
            }
            other => {
                return Err(Diag::error(
                    DiagKind::UnknownType,
                    format!("Unknown element `{other}`"),
                )
                .with_span(item.span.clone()))
            }
        })
    }

    fn style_of(&mut self, item: &Item, parent: Style, type_name: &str) -> Style {
        // Only typography and colour cascade from a parent element. Inheriting
        // the whole style would give every child the parent's padding, background
        // and margins, which double-counts the parent's insets.
        let mut style = parent.inheritable();
        let specs = props::props_for(type_name);
        let names: Vec<&str> = specs.iter().map(|p| p.name).collect();

        for prop in &item.props {
            let spec = match specs.iter().find(|s| s.name == &*prop.name) {
                Some(s) => s,
                None => {
                    self.error(
                        Diag::error(
                            DiagKind::UnknownProperty,
                            format!("Unknown property \"{}\" on {type_name}", prop.name),
                        )
                        .with_span(prop.span.clone())
                        .with_suggestion(&names, &prop.name),
                    );
                    continue;
                }
            };
            if spec.kind == PropKind::Handler {
                // Handled in `bind_dynamic`, which needs the element to exist.
                continue;
            }
            self.apply_prop(&mut style, spec, &prop.name, &prop.value, &prop.span);
        }
        style
    }

    fn apply_prop(
        &mut self,
        style: &mut Style,
        spec: &props::PropSpec,
        name: &str,
        value: &Expr,
        span: &Span,
    ) {
        let mismatch = |got: &str| {
            Diag::error(
                DiagKind::TypeMismatch,
                format!("`{name}` expects {}", spec.kind.describe()),
            )
            .with_span(span.clone())
            .with_note(format!("Found {got}."))
        };

        match spec.kind {
            PropKind::Color => {
                // Each colour property has to reach its own slot. Writing them all
                // to `foreground` is not a cosmetic mistake: `foreground` cascades,
                // so a panel's `background: "#11111bcc"` would become the text
                // colour of every widget inside it, and the bar would come up
                // blank with the text painted in its own background.
                let c = self.color_of(value, span, name);
                match name {
                    "color" | "foreground" => style.foreground = c,
                    "background" => style.background = c,
                    "border" => style.border = c,
                    _ => {}
                }
            }
            PropKind::Number => {
                if let Some(v) = self.number_of(value) {
                    match name {
                        "borderWidth" => style.border_width = v.max(0.0),
                        "borderRadius" | "radius" => style.radius = v.max(0.0),
                        "opacity" => style.opacity = v.clamp(0.0, 1.0),
                        "fontSize" => style.font_size = v.max(1.0),
                        "letterSpacing" => style.letter_spacing = v,
                        "minWidth" => style.min_width = v.max(0.0),
                        "minHeight" => style.min_height = v.max(0.0),
                        _ => {}
                    }
                } else if let Some(edges) = self.edges_of(value) {
                    // `padding` and `margin` accept a list.
                    match name {
                        "padding" => style.padding = edges,
                        "margin" => style.margin = edges,
                        _ => {}
                    }
                }
            }
            PropKind::Integer => {
                if let Some(v) = self.number_of(value) {
                    if name == "fontWeight" {
                        style.font_weight = v.clamp(100.0, 900.0) as u16;
                    }
                }
            }
            PropKind::Boolean => {
                if let Some(b) = self.bool_of(value) {
                    if name == "visible" && !b {
                        // Handled by a dynamic prop; recorded statically too.
                    }
                }
            }
            PropKind::String => {
                if let Some(s) = self.literal_of(value) {
                    if name == "fontFamily" {
                        style.font_family = Some(s);
                    }
                }
            }
            PropKind::Edges => {
                if let Some(e) = self.edges_of(value) {
                    match name {
                        "padding" => style.padding = e,
                        "margin" => style.margin = e,
                        _ => {}
                    }
                }
            }
            PropKind::Size => {
                match value {
                    Expr::Literal(Value::Str(s)) => {
                        let size = Size::parse(&Value::Str(s.clone()));
                        match name {
                            "width" => style.width = size,
                            "height" => style.height = size,
                            _ => {}
                        }
                    }
                    _ => {
                        if let Some(v) = self.number_of(value) {
                            match name {
                                "width" => style.width = Size::Fixed(v.max(0.0)),
                                "height" => style.height = Size::Fixed(v.max(0.0)),
                                _ => {}
                            }
                        }
                    }
                }
            }
            PropKind::Enum => {
                if name == "textAlign" {
                    if let Some(s) = self.literal_of(value) {
                        style.text_align = match s.as_str() {
                            "center" => TextAlign::Center,
                            "end" => TextAlign::End,
                            _ => TextAlign::Start,
                        };
                    }
                }
            }
            PropKind::Handler | PropKind::ElementList => {}
        }

        // `background` is handled separately because it is a color, not a
        // foreground, and the two share the `Color` kind.
        if name == "background" {
            style.background = self.color_of(value, span, name);
        }
        if name == "border" {
            style.border = self.color_of(value, span, name);
        }
        if name == "shadow" {
            style.shadow = Some(ShadowStyle {
                color: Color::rgba(0, 0, 0, 90),
                offset_y: 4.0,
                blur: 18.0,
            });
        }
        let _ = mismatch("");
    }

    /// Turn an expression into a reactor node so it re-evaluates on change.
    fn bind_dynamic(&mut self, element: &mut Element, item: &Item, type_name: &str) {
        let mut handlers: Vec<(String, Handler)> = Vec::new();
        for prop in &item.props {
            let name = prop.name.to_string();
            let is_dynamic = matches!(prop.value, Expr::Literal(_) | Expr::ColorLit(_))
                .eq(&false);
            if !is_dynamic && !matches!(name.as_str(), "text" | "value" | "progress" | "visible" | "width" | "height" | "color" | "background" | "opacity" | "onClick") {
                continue;
            }
            let path = format!("{type_name}#{}/{}", element.id.0, name);
            let env = self.env.clone();
            let reactor = self.reactor.clone();
            // One copy for the priming read, one for the binding itself.
            let prime = prop.value.clone();
            // Building the node inside `tracked` records the properties it reads,
            // which is what makes the binding reactive.
            let _ = slowshell_core::react::tracked(|| eval(&env, &prime).unwrap_or(Value::Null)).0;
            let expr = prop.value.clone();
            let id = reactor.derived(path, move || eval(&env, &expr).unwrap_or(Value::Null));
            // Prime the cache so a build-time read sees the value.
            let _ = reactor.get(id);

            match name.as_str() {
                "text" => element.dyn_props.text = Some(id),
                "value" | "progress" => element.dyn_props.progress = Some(id),
                "visible" => element.dyn_props.visible = Some(id),
                "width" => element.dyn_props.width = Some(id),
                "height" => element.dyn_props.height = Some(id),
                "opacity" => element.dyn_props.opacity = Some(id),
                "background" => element.dyn_props.background = Some(id),
                "color" | "foreground" => element.dyn_props.color = Some(id),
                "onClick" | "onShow" | "onHide" => {
                    if let Some(call) = call_of(&prop.value) {
                        // Arguments are evaluated once at build time. A handler
                        // fires on a click, long after the graph it was written
                        // in was torn down, so a reactive argument would have
                        // nothing to recompute against — and a literal one is
                        // what a config means anyway.
                        let args = call
                            .args
                            .iter()
                            .map(|a| eval(&self.env, a).unwrap_or(Value::Null))
                            .collect();
                        handlers.push((
                            name.clone(),
                            Handler {
                                action: call.path.join("."),
                                args,
                                span: prop.span.clone(),
                            },
                        ));
                    } else {
                        self.error(
                            Diag::error(
                                DiagKind::TypeMismatch,
                                format!("`{name}` must be a call, such as `launcher.toggle()`"),
                            )
                            .with_span(prop.span.clone())
                            .with_note(
                                "A handler names what to do. `onClick: \"launcher.toggle\"` is a \
                                 string, not a call, and would never run.",
                            ),
                        );
                    }
                }
                _ => {}
            }
        }
        *element.handlers.borrow_mut() = handlers;
    }

    /// A `Theme { … }` block retints the palette.
    ///
    /// A value may be a colour literal or the name of another token, so a theme
    /// can derive one colour from another instead of repeating a hex code.
    fn apply_theme(&mut self, item: &Item) {
        for prop in &item.props {
            // The shared metrics are checked first, and they `continue`. A theme
            // name is a plain string, so letting the colour path see it first
            // would report `unknown color "topbar"` for a perfectly good block.
            match &*prop.name {
                "name" => {
                    if let Some(n) = self.literal_of(&prop.value) {
                        self.theme.name = n;
                    }
                    continue;
                }
                "dark" => {
                    if let Some(b) = self.bool_of(&prop.value) {
                        self.theme.dark = b;
                    }
                    continue;
                }
                "radius" => {
                    if let Some(v) = self.number_of(&prop.value) {
                        self.theme.radius = v.max(0.0);
                    }
                    continue;
                }
                "fontSize" => {
                    if let Some(v) = self.number_of(&prop.value) {
                        self.theme.font_size = v.max(1.0);
                    }
                    continue;
                }
                _ => {}
            }
            let Some(key) = intern(&prop.name) else { continue };
            match &prop.value {
                Expr::ColorLit(c) => self.theme.set(key, *c),
                Expr::Literal(Value::Color(c)) => self.theme.set(key, *c),
                Expr::Literal(Value::Str(s)) => match Color::parse(s) {
                    Some(c) => self.theme.set(key, c),
                    // Not a literal, so it must name another token.
                    None => match self.theme.get(s) {
                        Some(existing) => self.theme.set(key, existing),
                        None => {
                            let keys = self.theme.keys();
                            self.error(
                                Diag::error(
                                    DiagKind::UnknownProperty,
                                    format!("Theme: unknown color \"{s}\""),
                                )
                                .with_span(prop.span.clone())
                                .with_suggestion(&keys, s),
                            );
                        }
                    },
                },
                Expr::Ident(n) => match self.theme.get(n) {
                    Some(existing) => self.theme.set(key, existing),
                    None => {
                        let keys = self.theme.keys();
                        self.error(
                            Diag::error(
                                DiagKind::UnknownProperty,
                                format!("Theme: unknown color \"{n}\""),
                            )
                            .with_span(prop.span.clone())
                            .with_suggestion(&keys, n),
                        );
                    }
                },
                _ => {}
            }
        }
    }
    // ---- expression readers ------------------------------------------------

    fn literal_of(&self, expr: &Expr) -> Option<String> {
        match expr {
            Expr::Literal(Value::Str(s)) => Some(s.to_string()),
            Expr::Literal(Value::Int(i)) => Some(i.to_string()),
            Expr::Literal(Value::Float(f)) => Some(slowshell_core::value::format_float(*f)),
            Expr::Literal(Value::Bool(b)) => Some(b.to_string()),
            Expr::Ident(n) => Some(n.to_string()),
            _ => None,
        }
    }

    fn number_of(&self, expr: &Expr) -> Option<f32> {
        match expr {
            Expr::Literal(Value::Int(i)) => Some(*i as f32),
            Expr::Literal(Value::Float(f)) => Some(*f as f32),
            _ => None,
        }
    }

    fn bool_of(&self, expr: &Expr) -> Option<bool> {
        match expr {
            Expr::Literal(Value::Bool(b)) => Some(*b),
            _ => None,
        }
    }

    /// A padding or margin value: one number for every side, two for
    /// vertical then horizontal, or four clockwise from the top.
    ///
    /// The list form is `Expr::List`, not a `Value::List` — the parser builds a
    /// list of *expressions*, each of which may be a number. Reading only the
    /// literal form silently dropped every `padding: [0, 12]` in a config, and a
    /// missing inset looks like a bug somewhere else in the layout.
    fn edges_of(&self, expr: &Expr) -> Option<Edges> {
        let numbers: Vec<f32> = match expr {
            Expr::List(items) => items.iter().map(|i| self.number_of(i)).collect::<Option<Vec<_>>>()?,
            other => vec![self.number_of(other)?],
        };
        Some(match numbers.as_slice() {
            [all] => Edges::all(*all),
            [vertical, horizontal] => Edges {
                top: *vertical,
                bottom: *vertical,
                left: *horizontal,
                right: *horizontal,
            },
            [top, right, bottom, left] => {
                Edges { top: *top, right: *right, bottom: *bottom, left: *left }
            }
            // More than four values is a mistake. The first four are still
            // applied, because a bar that is slightly wrong is better than a
            // build that fails over a stray comma.
            _ => Edges {
                top: numbers[0],
                right: *numbers.get(1).unwrap_or(&0.0),
                bottom: *numbers.get(2).unwrap_or(&0.0),
                left: *numbers.get(3).unwrap_or(&0.0),
            },
        })
    }

    /// A colour is either a literal or a theme token name.
    ///
    /// Anything else is a typo, and is reported against the theme's real keys.
    fn color_of(&mut self, expr: &Expr, span: &Span, prop_name: &str) -> ColorRef {
        match expr {
            Expr::ColorLit(c) => ColorRef::literal(*c),
            Expr::Literal(Value::Color(c)) => ColorRef::literal(*c),
            Expr::Literal(Value::Str(s)) => {
                if let Some(c) = Color::parse(s) {
                    return ColorRef::literal(c);
                }
                self.token_or_error(s, prop_name, span)
            }
            Expr::Ident(n) => self.token_or_error(n, prop_name, span),
            _ => ColorRef::token("foreground"),
        }
    }

    fn token_or_error(&mut self, name: &str, prop_name: &str, span: &Span) -> ColorRef {
        if self.theme.get(name).is_some() {
            if let Some(t) = intern(name) {
                return ColorRef::token(t);
            }
        }
        self.unknown_color(name, prop_name, span);
        ColorRef::token("foreground")
    }

    fn unknown_color(&mut self, name: &str, prop_name: &str, span: &Span) {
        let keys = self.theme.keys();
        self.error(
            Diag::error(
                DiagKind::UnknownProperty,
                format!("`{prop_name}`: unknown color \"{name}\""),
            )
            .with_span(span.clone())
            .with_suggestion(&keys, name),
        );
    }
}

/// A small interning table so a token name becomes a `&'static str`.
///
/// `ColorRef` stores a static reference so the renderer can resolve a token with
/// no allocation on the draw path.
fn intern(name: &str) -> Option<&'static str> {
    use std::collections::HashSet;
    use std::sync::Mutex;
    static NAMES: Mutex<Option<HashSet<&'static str>>> = Mutex::new(None);
    let mut guard = NAMES.lock().ok()?;
    let set = guard.get_or_insert_with(HashSet::new);
    if let Some(existing) = set.get(name) {
        return Some(existing);
    }
    let leaked: &'static str = Box::leak(name.to_string().into_boxed_str());
    set.insert(leaked);
    Some(leaked)
}

fn has_click_handler(e: &Element) -> bool {
    e.handlers.borrow().iter().any(|(name, _)| name == "onClick")
}

/// Report every `onClick`/`onShow`/`onHide` that names something unregistered.
///
/// The alternative is a config that compiles clean, reports no problems, and
/// has a button that does nothing when pressed — the one failure mode a shell
/// cannot afford, because it is indistinguishable from "the shell is broken".
///
/// # An empty registry means "not loaded yet", not "nothing exists"
///
/// The registry lives in the language crate and is filled in by the shell at
/// startup. A library user — the test suite, a tool that compiles a config
/// without running it — may have no actions registered at all, and reporting
/// every handler as unknown there would be a lie in the other direction. So an
/// empty registry skips the check and says why in a note on the build.
pub fn check_handlers(panels: &[Element], registry: &slowshell_core::Registry) -> Vec<Diag> {
    if registry.is_empty() {
        return Vec::new();
    }
    fn walk(e: &Element, registry: &slowshell_core::Registry, out: &mut Vec<Diag>) {
        for (name, handler) in e.handlers.borrow().iter() {
            let path: Vec<String> = handler.action.split('.').map(str::to_string).collect();
            if let Err(message) = slowshell_core::actions::check(registry, &path) {
                out.push(
                    Diag::error(DiagKind::Runtime, format!("`{name}`: {message}"))
                        .with_span(handler.span.clone())
                        .with_note(format!(
                            "Registered actions: {}",
                            registry.paths().join(", ")
                        ))
                        .with_hint(
                            "A handler is a call. `onClick: launcher.toggle()` is correct; \
                             `onClick: \"launcher.toggle\"` is a string and never runs."
                                .to_string(),
                        ),
                );
            }
        }
        for c in &e.children {
            walk(c, registry, out);
        }
        for (_, group) in &e.groups {
            for c in group {
                walk(c, registry, out);
            }
        }
    }
    let mut out = Vec::new();
    for p in panels {
        walk(p, registry, &mut out);
    }
    out
}

/// The dotted path and arguments of a call expression, e.g. `launcher.open("x")`.
///
/// The path alone is not enough: a handler that drops its arguments is a handler
/// that silently does the wrong thing, which is worse than one that fails.
fn call_of(expr: &Expr) -> Option<Call> {
    match expr {
        Expr::Call(callee, args) => Some(Call {
            path: slowshell_core::eval::path_of(callee)
                .map(|p| p.iter().map(|s| s.to_string()).collect())?,
            args: args.clone(),
        }),
        _ => None,
    }
}

struct Call {
    path: Vec<String>,
    args: Vec<Expr>,
}

/// Render diagnostics for the log.
pub fn render_diagnostics(diags: &[Diag]) -> String {
    if diags.is_empty() {
        return "no problems".into();
    }
    let mut out = String::new();
    for d in diags {
        out.push_str(&d.render());
        out.push_str("\n\n");
    }
    out
}

/// Severity counts, for the debug overlay.
pub fn count_by_severity(diags: &[Diag]) -> HashMap<&'static str, usize> {
    let mut out = HashMap::new();
    for d in diags {
        *out.entry(d.severity.label()).or_insert(0) += 1;
    }
    out
}

/// A warning is anything not fatal.
pub fn is_actionable(d: &Diag) -> bool {
    d.severity >= Severity::Warning
}

/// Convenience for tests: compile a source string with a bare host.
#[cfg(test)]
pub fn compile_source(source: &str) -> (Compiled, Rc<Reactor>, Rc<Env>) {
    let doc = slowshell_core::parse("shell.config", source).expect("test source must parse");
    let reactor = Rc::new(Reactor::new());
    let env = Rc::new(Env::new(
        reactor.clone(),
        Rc::new(crate::host::StaticHost::new([
            ("clock.time", Value::str("12:00")),
            ("battery.percentage", Value::Int(80)),
        ])),
    ));
    let compiled = compile(&doc, &env, &reactor, IdGen::new());
    (compiled, reactor, env)
}

/// Count of diagnostics, used by tests.
pub fn diag_count(c: &Compiled) -> usize {
    c.diagnostics.len()
}

/// The panel at index `i`, for tests.
pub fn panel(c: &Compiled, i: usize) -> Option<&Element> {
    c.panels.get(i)
}

/// Unused marker so the imports above stay honest if the compiler prunes.
#[allow(dead_code)]
fn _imports(_: &RefCell<u8>, _: Color) {}
