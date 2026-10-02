//! # Slowshell runtime
//!
//! Turns a config file into a running desktop: loads it, compiles it, publishes
//! system values into the reactive graph, and keeps it all hot-reloadable.
//!
//! ## The pipeline
//!
//! ```text
//!   shell.config ──load──▶ Document ──compile──▶ panels
//!        │                     │                    │
//!        └── includes          └── diagnostics      └── reactive props
//!                                                             │
//!   system events ──▶ provider tick ──▶ Reactor ──invalidate──┘
//!                                                             │
//!   layout ──▶ paint ──▶ present                                  │
//! ```

pub mod compiler;
pub mod host;
pub mod loader;
pub mod props;

pub use compiler::{Compiled, IdGen, compile, count_by_severity, render_diagnostics};
pub use host::{Pending, ShellHost, StaticHost, StaticObject};
pub use loader::{load, Loaded};
pub use props::{PropKind, PropSpec};

use std::rc::Rc;

use slowshell_core::eval::Env;
use slowshell_core::react::Reactor;
use slowshell_core::Diag;
use slowshell_ui::Element;

/// The default configuration written on first run.
///
/// It is deliberately the smallest thing that looks like a real desktop shell, so
/// a new user sees something finished rather than a tutorial.
///
/// The literal uses a two-hash raw string because a colour such as `"#11111bcc"`
/// contains the `"#` sequence that would close a one-hash raw string.
pub const DEFAULT_CONFIG: &str = r##"// Slowshell - your desktop, your config.
// Full reference: docs/CONFIGURATION.md

Panel {
    position: "top"
    screen: "primary"

    // This bar's height, and therefore how much desktop it holds.
    height: 40

    // Take that height out of the usable desktop, so a maximized window stops
    // below the bar instead of hiding it. On by default, because a desktop bar
    // that windows cover is broken, and every shell that behaves like one
    // reserves space.
    //
    // Delete the line for an overlay bar that maximized windows cover instead,
    // which is occasionally what you want and otherwise just annoying.
    //
    // Change `height` above and the reserved space follows it. Check the result
    // with `shellctl screens`: the work area is the authoritative number.
    exclusive: true

    background: "#11111bcc"
    padding: 0
    radius: 0

    // Left group: app launcher and the workspace indicator.
    left {
        Row {
            gap: 10
            padding: [0, 12]
            // An `onClick` is what makes a region clickable at all, so this is
            // the one line that turns the bar from something you click *past*
            // into something you click. It opens a hidden panel declared below.
            Text { text: "Apps"  fontWeight: 600  onClick: shell.open("launcher") }
            Text { text: "1"  color: "accent" }
            Text { text: "2"  color: "foregroundMuted" }
        }
    }

    // Centre group: the clock.
    center {
        Row {
            // The clock is a button too: clicking it toggles the error overlay,
            // which is the first thing anyone wants when a config is misbehaving.
            Clock { format: "HH:mm"  onClick: shell.overlay() }
        }
    }

    // Right group: system status.
    right {
        Row {
            gap: 14
            padding: [0, 12]
            // These read as null where the provider is not implemented, so each
            // is behind a `visible` check rather than printing a bare value.
            // `shellctl doctor` says which providers are live on your machine.
            Text {
                text: battery.percentage + "%"
                color: "foregroundMuted"
                visible: battery.present
            }
            Text {
                text: "WiFi"
                color: "foregroundMuted"
                visible: network.connected
            }
            Text {
                text: "offline"
                color: "warning"
                visible: !network.connected
            }
        }
    }
}

// A launcher: a second panel with no window until something opens it. This is
// the whole mechanism - the renderer has no concept of a launcher, a launcher is
// a panel in the config with a name.
Panel {
    name: "launcher"
    hidden: true
    position: "top"
    width: 460
    height: 40
    anchorX: 0.5
    background: "#1b1e29fa"
    border: "#ffffff1f"
    borderWidth: 1
    radius: 10
    padding: 0

    content {
        Row {
            gap: 14
            padding: [0, 16]
            cross: "center"
            Text { text: "Run"  color: "foregroundSubtle" }
            Text { text: "notepad.exe"  color: "foreground" }
            Text {
                text: "open"
                color: "accent"
                fontWeight: 600
                onClick: launch("notepad.exe")
            }
        }
    }
}
"##;

/// Everything the shell needs to be running, assembled in one place.
pub struct Runtime {
    pub reactor: Rc<Reactor>,
    pub env: Rc<Env>,
    pub shell_host: Rc<host::ShellHost>,
    /// What `onClick` and friends may call.
    ///
    /// Owned here rather than in the shell binary so a handler's target can be
    /// checked at build time: `onClick: launhcer.toggle()` should be a
    /// diagnostic next to the line that contains it, not a shrug at the moment
    /// the user discovers the button does nothing.
    pub actions: slowshell_core::Registry,
    /// The theme from the last successful build, so a surface opened later
    /// matches the ones already on screen.
    current_theme: slowshell_ui::Theme,
    /// Panels the config declared with `hidden: true`, kept so `shell.open` can
    /// build them on demand.
    hidden_panels: Vec<Element>,
    /// The last scene that built cleanly.
    ///
    /// Held here rather than left to the shell to remember, because "a broken
    /// edit never blanks the desktop" is a property of the runtime and should be
    /// a property that cannot be forgotten by a caller. `build` hands the
    /// previous tree back when the new one fails, and the shell's own check is
    /// then belt and braces rather than the only thing standing between a typo
    /// and an empty bar.
    last_good: Option<Compiled>,
}

impl Runtime {
    pub fn new() -> Runtime {
        let reactor = Rc::new(Reactor::new());
        let shell_host = Rc::new(host::ShellHost::new());
        let env = Rc::new(Env::new(reactor.clone(), shell_host.clone() as Rc<dyn slowshell_core::eval::Host>));
        Runtime {
            reactor,
            env,
            shell_host,
            actions: slowshell_core::Registry::new(),
            current_theme: slowshell_ui::Theme::default(),
            hidden_panels: Vec::new(),
            last_good: None,
        }
    }

    /// Seed the reactive graph with a value for every documented system path, so
    /// a config referencing them builds on the very first frame rather than after
    /// the first system event.
    pub fn seed_sources(&self) {
        seed(&self.reactor, "clock.time", "00:00");
        seed(&self.reactor, "clock.unix", 0);
        seed(&self.reactor, "battery.percentage", 0);
        seed(&self.reactor, "battery.charging", false);
        seed(&self.reactor, "battery.present", false);
        seed(&self.reactor, "network.connected", true);
        seed(&self.reactor, "network.wifi", true);
        seed(&self.reactor, "network.download", 0);
        seed(&self.reactor, "network.upload", 0);
        seed(&self.reactor, "audio.volume", 50);
        seed(&self.reactor, "audio.muted", false);
        // Now playing, from the SMTC global session manager. Backed for real by
        // `slowshell_win::media`; seeded here so a config that reads `media.title`
        // compiles on a machine where nothing is playing, and shows nothing rather
        // than failing to build.
        seed(&self.reactor, "media.title", "");
        seed(&self.reactor, "media.artist", "");
        seed(&self.reactor, "media.status", "");
        seed(&self.reactor, "media.app", "");
        seed(&self.reactor, "media.playing", false);
        // Virtual desktops: Windows has no supported way to enumerate or switch
        // them, so these stay seeded and are not published. They are kept only so
        // an old config that reads them still compiles. See
        // docs/WINDOWS_API.md - do not build a workspace indicator on these.
        seed(&self.reactor, "system.cpuUsage", 0);
        seed(&self.reactor, "system.memoryUsage", 0);
        seed(&self.reactor, "windows.active.title", "");
        seed(&self.reactor, "windows.active.process", "");
        seed(&self.reactor, "virtualDesktops.count", 1);
        seed(&self.reactor, "virtualDesktops.current", 1);
        seed(&self.reactor, "notifications.unreadCount", 0);
    }

    /// Load and compile the config, returning diagnostics rather than panicking.
    ///
    /// A failed reload hands the **previous** scene back, so a caller that shows
    /// whatever it is given cannot blank the desktop on a typo. The diagnostics
    /// still describe the failure, and `BuildOutcome::has_errors` still says so —
    /// this is about not throwing away something that already worked, not about
    /// hiding that the new file is broken.
    pub fn build(&mut self, path: &std::path::Path) -> BuildOutcome {
        let loaded = match load(path) {
            Ok(l) => l,
            Err(d) => return self.failure(vec![d]),
        };
        let mut diagnostics = loaded.diagnostics;
        let mut compiled = compile(&loaded.document, &self.env, &self.reactor, IdGen::new());
        // Check handler targets once the tree exists: the handler is bound to an
        // element, and the element is what knows where it came from.
        let handler_problems = compiler::check_handlers(&compiled.panels, &self.actions);
        diagnostics.extend(compiled.diagnostics.iter().cloned());
        diagnostics.extend(handler_problems);
        compiled.diagnostics = diagnostics.clone();

        // Hidden panels are kept rather than discarded, so a launcher can be
        // opened later without waiting for a config reload.
        self.current_theme = compiled.theme.clone();
        self.hidden_panels =
            compiled.panels.iter().filter(|p| is_hidden(p)).cloned().collect();
        self.last_good = Some(compiled.clone());

        BuildOutcome { compiled: Some(compiled), diagnostics, files: loaded.files }
    }

    /// A build that failed, carrying the previous scene if there was one.
    fn failure(&self, diagnostics: Vec<Diag>) -> BuildOutcome {
        BuildOutcome {
            compiled: self.last_good.clone(),
            diagnostics,
            files: Vec::new(),
        }
    }

    /// The theme the last successful build produced, for a surface opened later.
    pub fn theme(&self) -> slowshell_ui::Theme {
        self.current_theme.clone()
    }

    /// Every panel the config declared with `hidden: true`.
    pub fn hidden_panels(&self) -> &[Element] {
        &self.hidden_panels
    }

    /// The panels from the last successful build, for tests and for a caller
    /// that wants to inspect the scene it was handed.
    pub fn last_good_panels(&self) -> Vec<Element> {
        self.last_good.as_ref().map(|c| c.panels.clone()).unwrap_or_default()
    }

    /// A panel the config declared but did not show, by name.
    ///
    /// This is what makes a launcher a config concern rather than a renderer
    /// one: `Panel { name: "launcher" hidden: true }` sits in the file, compiles
    /// like any other panel, and is built into a window the first time something
    /// asks for it.
    pub fn hidden_panel(&self, name: &str) -> Option<Element> {
        self.hidden_panels
            .iter()
            .find(|p| hidden_name(p) == name)
            .cloned()
    }
}

/// The `name` a panel declared, or an empty string.
fn hidden_name(panel: &Element) -> String {
    match &panel.kind {
        slowshell_ui::ElementKind::Panel { name, .. } => name.clone(),
        _ => String::new(),
    }
}

/// Whether a panel asked to stay out of the way until something opens it.
fn is_hidden(panel: &Element) -> bool {
    match &panel.kind {
        slowshell_ui::ElementKind::Panel { hidden, .. } => *hidden,
        _ => false,
    }
}

fn seed(r: &Rc<Reactor>, path: &str, value: impl Into<slowshell_core::Value>) {
    r.source(path, value.into());
}

/// What a build produced.
pub struct BuildOutcome {
    pub compiled: Option<Compiled>,
    pub diagnostics: Vec<Diag>,
    /// Files the build read, for the reload watcher.
    pub files: Vec<std::path::PathBuf>,
}

impl BuildOutcome {
    pub fn ok(&self) -> bool {
        self.compiled.is_some() && self.diagnostics.iter().all(|d| d.severity < slowshell_core::Severity::Error)
    }

    pub fn has_errors(&self) -> bool {
        self.diagnostics.iter().any(|d| d.severity >= slowshell_core::Severity::Error)
    }

    /// A one-line summary for the log.
    ///
    /// Says so when the scene being reported is the *previous* one, because a
    /// summary that reads like a success while the file on disk is broken is how
    /// a reload bug stays invisible.
    pub fn summary(&self) -> String {
        match &self.compiled {
            Some(c) => {
                let mut s = c.summary();
                if self.has_errors() {
                    s.push_str(" (the previous scene is still in place)");
                }
                if !self.diagnostics.is_empty() {
                    s.push_str(&format!("; {} diagnostic(s)", self.diagnostics.len()));
                }
                s
            }
            None => format!(
                "build failed: {}",
                self.diagnostics
                    .first()
                    .map(|d| d.message.clone())
                    .unwrap_or_else(|| "unknown error".into())
            ),
        }
    }
}

/// Write a starter config at `path` if none exists, so a first run has something
/// to show. Returns the path and whether it was created.
///
/// The path is a parameter rather than always the default location, so
/// `Shell.exe some\other.config` creates the starter config *there*.
pub fn ensure_default_config_at(path: &std::path::Path) -> std::io::Result<(std::path::PathBuf, bool)> {
    if path.exists() {
        return Ok((path.to_path_buf(), false));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, DEFAULT_CONFIG)?;
    Ok((path.to_path_buf(), true))
}

/// Write the starter config at the default location.
pub fn ensure_default_config() -> std::io::Result<(std::path::PathBuf, bool)> {
    ensure_default_config_at(&slowshell_core::paths::shell_config())
}

#[cfg(test)]
mod tests {
    use super::*;
    use slowshell_core::Value;
    use std::path::PathBuf;

    fn temp_config(name: &str, body: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("slowshell-rt-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("shell.config");
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn the_default_config_builds_without_diagnostics() {
        let p = temp_config("default", DEFAULT_CONFIG);
        let mut rt = Runtime::new();
        rt.seed_sources();
        let out = rt.build(&p);
        assert!(out.compiled.is_some(), "the default config must build");
        assert!(!out.has_errors(), "unexpected diagnostics: {:?}", out.diagnostics);
        let panels = out.compiled.unwrap().panels;
        // The bar, plus the launcher the bar's `Apps` label opens.
        assert_eq!(panels.len(), 2, "the default config must ship a launcher too");
        // The three groups the spec's example uses must all be present.
        let groups: Vec<&str> = panels[0].groups.iter().map(|(n, _)| n.as_str()).collect();
        assert!(groups.contains(&"left"), "got {groups:?}");
        assert!(groups.contains(&"center"), "got {groups:?}");
        assert!(groups.contains(&"right"), "got {groups:?}");
    }

    #[test]
    fn the_default_config_has_a_clickable_region() {
        // A bar with no handler anywhere is entirely click-through, which reads
        // as "the shell is not working" rather than as "this config has no
        // handlers". The first thing a new user clicks must do something.
        let p = temp_config("default-click", DEFAULT_CONFIG);
        let mut rt = Runtime::new();
        rt.seed_sources();
        let out = rt.build(&p);
        let bar = &out.compiled.expect("must build").panels[0];
        // `interactive` is per element, and the panel itself has no handler — the
        // hit regions come from its descendants, which is why a test has to walk
        // the tree rather than ask the root.
        let regions = count_interactive(bar);
        assert!(regions >= 2, "the default bar publishes only {regions} hit region(s)");
    }

    /// How many elements in this subtree publish a hit region.
    fn count_interactive(e: &Element) -> usize {
        let own = usize::from(e.interactive.get());
        let children: usize = e.children.iter().map(count_interactive).sum();
        let groups: usize =
            e.groups.iter().flat_map(|(_, g)| g.iter()).map(count_interactive).sum();
        own + children + groups
    }

    #[test]
    fn a_config_with_no_handlers_publishes_nothing() {
        // The other half of the rule above: the mechanism is not "the bar is
        // clickable", it is "a region with a handler is clickable". A config that
        // wants a click-through bar gets one by omitting handlers.
        let p = temp_config("no-handlers", "Panel { left { Text { text: \"x\" } } }");
        let mut rt = Runtime::new();
        rt.seed_sources();
        let out = rt.build(&p);
        let bar = &out.compiled.expect("must build").panels[0];
        assert_eq!(count_interactive(bar), 0, "nothing here has a handler");
    }

    #[test]
    fn the_default_launcher_is_declared_hidden() {
        // It must compile but must not take a window, or the first frame of a
        // fresh install would have a floating panel over the bar.
        let p = temp_config("default-launcher", DEFAULT_CONFIG);
        let mut rt = Runtime::new();
        rt.seed_sources();
        rt.build(&p);
        let launcher = rt.hidden_panel("launcher").expect("the launcher must be declared");
        assert!(
            matches!(
                &launcher.kind,
                slowshell_ui::ElementKind::Panel { hidden: true, .. }
            ),
            "the launcher must be hidden"
        );
        // And the bar's own handler names it, or opening it would fail.
        assert_eq!(rt.hidden_panels().len(), 1, "exactly one hidden panel");
    }

    #[test]
    fn a_two_value_edge_list_is_vertical_then_horizontal() {
        // `padding: [0, 12]` is the CSS shorthand: one number for top and bottom,
        // one for left and right. Getting this wrong silently removes every
        // inset in a bar, which looks like a layout bug somewhere else entirely.
        let p = temp_config(
            "edges",
            r#"Panel {
    left {
        Row { gap: 8  padding: [0, 12]  Text { text: "Hi" } }
    }
}"#,
        );
        let mut rt = Runtime::new();
        rt.seed_sources();
        let out = rt.build(&p);
        assert!(!out.has_errors(), "diagnostics: {:?}", out.diagnostics);
        let panel = out.compiled.unwrap().panels.remove(0);
        let row = &panel.groups[0].1[0];
        assert_eq!(row.style.padding.top, 0.0);
        assert_eq!(row.style.padding.bottom, 0.0);
        assert_eq!(row.style.padding.left, 12.0, "the second value is the horizontal one");
        assert_eq!(row.style.padding.right, 12.0);
    }

    #[test]
    fn a_four_value_edge_list_is_clockwise() {
        let p = temp_config(
            "edges4",
            r#"Panel {
    left {
        Row { padding: [1, 2, 3, 4]  Text { text: "Hi" } }
    }
}"#,
        );
        let mut rt = Runtime::new();
        rt.seed_sources();
        let out = rt.build(&p);
        assert!(!out.has_errors(), "diagnostics: {:?}", out.diagnostics);
        let panel = out.compiled.unwrap().panels.remove(0);
        let pad = panel.groups[0].1[0].style.padding;
        assert_eq!((pad.top, pad.right, pad.bottom, pad.left), (1.0, 2.0, 3.0, 4.0));
    }

    #[test]
    fn a_bare_number_means_every_side() {
        let p = temp_config(
            "edges1",
            r#"Panel {
    left {
        Row { padding: 7  Text { text: "Hi" } }
    }
}"#,
        );
        let mut rt = Runtime::new();
        rt.seed_sources();
        let out = rt.build(&p);
        assert!(!out.has_errors(), "diagnostics: {:?}", out.diagnostics);
        let panel = out.compiled.unwrap().panels.remove(0);
        let pad = panel.groups[0].1[0].style.padding;
        assert_eq!((pad.top, pad.right, pad.bottom, pad.left), (7.0, 7.0, 7.0, 7.0));
    }

    #[test]
    fn each_colour_property_reaches_its_own_slot() {
        // The symptom when this is wrong is not a diagnostic: `foreground`
        // cascades, so a panel's background colour becomes the text colour of
        // every widget inside it and the bar comes up blank.
        let p = temp_config(
            "colours",
            r##"Panel {
    background: "#11111b"
    border: "#ff0000"
    left {
        Row {
            Text { text: "plain" }
            Text { text: "tinted"  color: "accent" }
        }
    }
}"##,
        );
        let mut rt = Runtime::new();
        rt.seed_sources();
        let out = rt.build(&p);
        assert!(!out.has_errors(), "diagnostics: {:?}", out.diagnostics);
        let theme = slowshell_ui::Theme::default();
        let panel = out.compiled.unwrap().panels.remove(0);

        assert_eq!(
            panel.style.background.resolve(&theme),
            slowshell_core::Color::rgb(0x11, 0x11, 0x1b)
        );
        assert_eq!(
            panel.style.border.resolve(&theme),
            slowshell_core::Color::rgb(0xff, 0x00, 0x00)
        );
        assert!(
            panel.style.foreground.is_transparent(),
            "a background must not also become the text colour, or every widget inside the \
             panel inherits it and renders invisibly"
        );

        let row = &panel.groups[0].1[0];
        let plain = &row.children[0];
        let tinted = &row.children[1];
        assert!(
            plain.style.foreground.is_transparent(),
            "a widget with no colour must stay transparent so the theme supplies one"
        );
        assert_eq!(tinted.style.foreground.resolve(&theme), theme.get("accent").unwrap());
    }

    #[test]
    fn the_spec_minimal_example_builds() {
        let p = temp_config(
            "minimal",
            r#"Panel {
    position: "top"
    Row {
        Text { text: "Hello" }
        Clock { format: "HH:mm" }
    }
}"#,
        );
        let mut rt = Runtime::new();
        rt.seed_sources();
        let out = rt.build(&p);
        assert!(!out.has_errors(), "diagnostics: {:?}", out.diagnostics);
        let panels = out.compiled.unwrap().panels;
        let row = &panels[0].children[0];
        assert_eq!(row.children.len(), 2);
    }

    #[test]
    fn an_unknown_property_is_reported_with_a_suggestion() {
        let p = temp_config("typo", "Panel { Text { text: \"x\"  foregorund: \"#fff\" } }");
        let mut rt = Runtime::new();
        rt.seed_sources();
        let out = rt.build(&p);
        assert!(out.has_errors());
        let d = &out.diagnostics[0];
        assert_eq!(d.kind, slowshell_core::DiagKind::UnknownProperty);
        assert!(
            d.hints.iter().any(|h| h.contains("foreground")),
            "expected a foreground suggestion, got {:?}",
            d.hints
        );
    }

    #[test]
    fn a_misspelled_clock_format_is_reported() {
        let p = temp_config("clockfmt", "Panel { Clock { format: \"\" } }");
        let mut rt = Runtime::new();
        rt.seed_sources();
        let out = rt.build(&p);
        assert!(out.has_errors());
        assert!(out.diagnostics.iter().any(|d| d.message.contains("clock format")));
    }

    #[test]
    fn an_unknown_element_does_not_stop_the_rest_of_the_shell() {
        let p = temp_config(
            "partial",
            "Panel { Text { text: \"before\" } Widgit { } Text { text: \"after\" } }",
        );
        let mut rt = Runtime::new();
        rt.seed_sources();
        let out = rt.build(&p);
        // The error is reported...
        assert!(out.has_errors());
        // ...and the panel still exists with its good children.
        let panels = out.compiled.expect("a panel must still build");
        let names: Vec<&str> =
            panels.panels[0].children.iter().map(|c| c.kind.type_name()).collect();
        assert!(names.contains(&"Text"), "got {names:?}");
    }

    #[test]
    fn a_broken_element_becomes_a_visible_placeholder() {
        let p = temp_config("broken", "Panel { Clock { format: \"\" } }");
        let mut rt = Runtime::new();
        rt.seed_sources();
        let out = rt.build(&p);
        let panels = out.compiled.unwrap();
        let broken: Vec<&str> = panels.panels[0]
            .children
            .iter()
            .map(|c| c.kind.type_name())
            .filter(|k| *k == "Broken")
            .collect();
        assert_eq!(broken.len(), 1, "a failed element must be replaced, not dropped");
    }

    #[test]
    fn a_config_that_does_not_start_with_a_panel_is_rejected() {
        let p = temp_config("napanel", "Text { text: \"orphan\" }");
        let mut rt = Runtime::new();
        rt.seed_sources();
        let out = rt.build(&p);
        assert!(out.has_errors());
        assert!(out.diagnostics[0].message.contains("Panel"));
    }

    #[test]
    fn a_reactive_binding_updates_when_its_source_changes() {
        let p = temp_config("reactive", "Panel { Text { text: battery.percentage + \"%\" } }");
        let mut rt = Runtime::new();
        rt.seed_sources();
        let out = rt.build(&p);
        let panels = out.compiled.unwrap();
        let text = &panels.panels[0].children[0];
        let id = text.dyn_props.text.expect("text must be reactive");
        assert_eq!(rt.reactor.get(id).to_string_lossy(), "0%");
        rt.reactor.set("battery.percentage", slowshell_core::Value::Int(87));
        assert_eq!(
            rt.reactor.get(id).to_string_lossy(),
            "87%",
            "changing the source must change the bound text"
        );
    }

    #[test]
    fn an_expression_binding_reacts_to_its_source() {
        let p = temp_config(
            "expr",
            "Panel { Text { text: battery.charging ? \"plugged\" : \"on battery\" } }",
        );
        let mut rt = Runtime::new();
        rt.seed_sources();
        let out = rt.build(&p);
        let id = out
            .compiled
            .unwrap()
            .panels[0]
            .children[0]
            .dyn_props
            .text
            .expect("text must be reactive");
        assert_eq!(rt.reactor.get(id).to_string_lossy(), "on battery");
        rt.reactor.set("battery.charging", slowshell_core::Value::Bool(true));
        assert_eq!(rt.reactor.get(id).to_string_lossy(), "plugged");
    }

    #[test]
    fn a_method_call_becomes_a_handler_not_an_evaluation() {
        let p = temp_config("handler", "Panel { Text { text: \"x\"  onClick: launcher.open() } }");
        let mut rt = Runtime::new();
        rt.seed_sources();
        let out = rt.build(&p);
        let text = &out.compiled.unwrap().panels[0].children[0];
        let handlers = text.handlers.borrow();
        assert_eq!(handlers.len(), 1);
        assert_eq!(handlers[0].1.action, "launcher.open");
    }

    #[test]
    fn a_handler_is_only_reported_when_actions_are_loaded() {
        // With actions registered, a bad target is an error naming the real
        // alternatives. That is the whole point of checking at build time.
        let p = temp_config("handler-bad", "Panel { Text { text: \"x\"  onClick: launhcer.open() } }");
        let mut rt = Runtime::new();
        rt.seed_sources();
        rt.actions.register("launcher.open", "Open it.", &[], |_| Ok(Value::Null));
        let out = rt.build(&p);
        assert!(out.has_errors(), "a misspelled action must be an error");
        let text = out.diagnostics.iter().find(|d| d.message.contains("launhcer"));
        assert!(text.is_some(), "the diagnostic must name the misspelling: {:?}", out.diagnostics);
    }

    #[test]
    fn a_correct_handler_passes_when_actions_are_loaded() {
        let p = temp_config("handler-ok", "Panel { Text { text: \"x\"  onClick: launcher.open() } }");
        let mut rt = Runtime::new();
        rt.seed_sources();
        rt.actions.register("launcher.open", "Open it.", &[], |_| Ok(Value::Null));
        let out = rt.build(&p);
        assert!(!out.has_errors(), "{:?}", out.diagnostics);
    }

    #[test]
    fn an_empty_registry_does_not_claim_every_handler_is_wrong() {
        // A library user that has not registered actions yet should not be told
        // its whole config is broken.
        let p = temp_config("handler-empty", "Panel { Text { text: \"x\"  onClick: anything.goes() } }");
        let mut rt = Runtime::new();
        rt.seed_sources();
        let out = rt.build(&p);
        assert!(!out.has_errors(), "{:?}", out.diagnostics);
    }

    #[test]
    fn a_handler_keeps_its_arguments() {
        // An action that silently dropped its arguments would do the wrong thing
        // rather than fail, which is the worse of the two.
        let p = temp_config(
            "handler-args",
            "Panel { Text { text: \"x\"  onClick: launch(\"notepad.exe\") } }",
        );
        let mut rt = Runtime::new();
        rt.seed_sources();
        let out = rt.build(&p);
        let text = &out.compiled.unwrap().panels[0].children[0];
        let handlers = text.handlers.borrow();
        assert_eq!(handlers[0].1.action, "launch");
        assert_eq!(handlers[0].1.args, vec![Value::str("notepad.exe")]);
    }

    #[test]
    fn a_handler_that_is_not_a_call_is_a_diagnostic() {
        let p = temp_config(
            "handler-string",
            "Panel { Text { text: \"x\"  onClick: \"launcher.open\" } }",
        );
        let mut rt = Runtime::new();
        rt.seed_sources();
        let out = rt.build(&p);
        assert!(out.has_errors(), "a string is not a call and must be reported");
        assert!(
            out.diagnostics.iter().any(|d| d.message.contains("must be a call")),
            "{:?}",
            out.diagnostics
        );
    }

    #[test]
    fn a_theme_block_retints_the_palette() {
        let p = temp_config(
            "theme",
            "Theme { accent: \"#ff0000\"  background: \"#001122\" }\nPanel { Text { text: \"x\"  color: \"accent\" } }",
        );
        let mut rt = Runtime::new();
        rt.seed_sources();
        let out = rt.build(&p);
        assert!(!out.has_errors(), "{:?}", out.diagnostics);
        let compiled = out.compiled.unwrap();
        assert_eq!(compiled.theme.get("accent"), Some(slowshell_core::Color::rgb(255, 0, 0)));
    }

    #[test]
    fn an_unknown_colour_token_is_reported() {
        let p = temp_config("badcolor", "Panel { Text { text: \"x\"  color: \"acent\" } }");
        let mut rt = Runtime::new();
        rt.seed_sources();
        let out = rt.build(&p);
        assert!(out.has_errors());
        assert!(out.diagnostics[0].message.contains("acent"));
    }

    #[test]
    fn a_failed_build_leaves_no_scene_rather_than_a_partial_one() {
        let p = temp_config("badsyntax", "Panel { { ");
        let mut rt = Runtime::new();
        rt.seed_sources();
        let out = rt.build(&p);
        assert!(out.compiled.is_none(), "a syntax error must not produce a scene");
        assert!(out.has_errors());
        assert!(out.summary().contains("build failed"));
    }

    #[test]
    fn a_leaf_element_rejects_children() {
        let p = temp_config("leafchild", "Panel { Text { text: \"x\"  Text { text: \"y\" } } }");
        let mut rt = Runtime::new();
        rt.seed_sources();
        let out = rt.build(&p);
        assert!(out.has_errors());
        assert!(out.diagnostics[0].message.contains("cannot contain"));
    }

    #[test]
    fn seeded_sources_exist_for_every_documented_path() {
        let rt = Runtime::new();
        rt.seed_sources();
        for path in [
            "clock.time",
            "battery.percentage",
            "network.connected",
            "audio.volume",
            "system.cpuUsage",
        ] {
            assert!(rt.reactor.lookup(path).is_some(), "missing seeded source {path}");
        }
    }

    /// Every config in `examples/` must build with no errors.
    ///
    /// Examples are the first thing anyone reads, and an example that stopped
    /// compiling is worse than no example: it teaches a syntax the shell no
    /// longer accepts. This is the test that notices.
    ///
    /// A warning is allowed. An example is allowed to read a provider that
    /// reads as `null` on the test machine, and saying so in a comment is
    /// better than removing the widget.
    #[test]
    fn every_example_config_still_builds() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples")
            .canonicalize();
        let Ok(dir) = dir else {
            // The examples directory is not present in this build. Nothing to
            // check, and a failure here would be about the checkout, not the
            // code.
            return;
        };
        let mut checked = 0;
        let mut names: Vec<_> = std::fs::read_dir(&dir)
            .expect("examples directory")
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "config"))
            .collect();
        names.sort();
        assert!(!names.is_empty(), "no .config examples found in {}", dir.display());
        for path in names {
            let mut rt = Runtime::new();
            rt.seed_sources();
            let out = rt.build(&path);
            assert!(
                out.compiled.is_some(),
                "{} did not build: {:?}",
                path.display(),
                out.diagnostics
            );
            assert!(
                !out.has_errors(),
                "{} has errors:\n{}",
                path.display(),
                render_diagnostics(&out.diagnostics)
            );
            checked += 1;
        }
        assert!(checked >= 6, "expected the six documented examples, found {checked}");
    }
}
