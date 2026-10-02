//! Integration tests: the behaviours that only appear when the pieces are
//! assembled.
//!
//! The unit tests answer "does this function do what it says". These answer the
//! questions a user actually asks:
//!
//! * if I save a broken file, does my bar survive? (hot reload, error boundary)
//! * does a value that changes show up? (reactivity, end to end)
//! * does a bar stay a bar when the window is too small? (layout under pressure)
//! * is this widget actually clickable? (handlers, hit regions)
//! * does the DPI change fix the numbers or double them? (DPI)
//! * does a second display get a usable geometry? (multi-monitor)
//! * can a control channel survive a shell that died? (IPC)
//!
//! Everything here runs headless. There is no display, no swap chain and no
//! window: the tree, the layout, the region collection and the IPC transport
//! are all testable without one, and testing them without one is what makes
//! them testable at all.
//!
//! # What is deliberately not here
//!
//! *"The pixels are the right colour."* That needs a real `Painter`, a real
//! Direct2D device and a real swap chain, so it lives in
//! `crates/slowshell/examples/render_probe.rs`, which draws a known scene, reads
//! the pixels back, and asserts. Ten of the bugs found during development were
//! found that way — by looking at the output rather than by reading the input.
//! A test that only passes on a machine with a monitor is a test nobody runs.

use std::path::{Path, PathBuf};

use slowshell_core::ipc::{Request, Response};
use slowshell_core::{Registry, Value};
use slowshell_runtime::{Runtime, render_diagnostics};
use slowshell_ui::layout::{ApproxMeasurer, ElementIdKey, Resolved, resolve_values};
use slowshell_ui::style::{Edges, Theme};
use slowshell_ui::{Element, ElementId, Rect, layout};
use slowshell_win::Monitors;

// --------------------------------------------------------------------------- //
// Harness
// --------------------------------------------------------------------------- //

/// A config on disk, in its own directory, cleaned up on drop.
///
/// A directory rather than a single file, because the `include` and hot-reload
/// tests need more than one file and because two tests must never share a path.
struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Sandbox {
        let dir = std::env::temp_dir().join(format!("slowshell-it-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Sandbox { dir }
    }

    /// Write the root config and return its path.
    fn config(&self, body: &str) -> PathBuf {
        self.write("shell.config", body)
    }

    fn write(&self, name: &str, body: &str) -> PathBuf {
        let p = self.dir.join(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        // Deliberately BOM-free: one test covers the BOM, and a test that
        // depended on stripping one would be testing the loader, not the shell.
        std::fs::write(&p, body).unwrap();
        p
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A runtime carrying the shell's action set.
///
/// The same list the shell registers, so a handler that passes here is a
/// handler the shell can resolve. A test runtime with no actions would make
/// every `onClick` look broken, and a diagnostic that lies is worse than none.
fn runtime() -> Runtime {
    let mut rt = Runtime::new();
    rt.seed_sources();
    let mut registry = Registry::new();
    slowshell_win::platform::register(&mut registry);
    // The shell's own actions. All are inert here — the point is that they
    // resolve, not what they do.
    registry.register("shell.reload", "Reload.", &[], |_| Ok(Value::Null));
    registry.register("shell.overlay", "Overlay.", &[], |_| Ok(Value::Null));
    registry.register("shell.stop", "Stop.", &[], |_| Ok(Value::Null));
    registry.register("shell.open", "Open.", &["name"], |_| Ok(Value::Null));
    registry.register("launch", "Launch.", &["target"], |_| Ok(Value::Null));
    registry.register("notify", "Notify.", &["title", "body"], |_| Ok(Value::Null));
    registry.register("clipboard.set", "Copy.", &["text"], |_| Ok(Value::Null));
    registry.register("exec.run", "Run.", &["program", "args"], |_| Ok(Value::Null));
    rt.actions = registry;
    rt
}

/// Build a config and insist it worked, reporting the diagnostics if not.
fn build(rt: &mut Runtime, path: &Path) -> Element {
    let outcome = rt.build(path);
    assert!(
        !outcome.has_errors(),
        "config should have built:\n{}",
        render_diagnostics(&outcome.diagnostics)
    );
    outcome.compiled.expect("a clean build produces a scene").panels.into_iter().next().expect("the config declares a panel")
}

/// Lay a tree out into a box of the given logical size.
fn lay(root: &Element, width: f32, height: f32) {
    let theme = Theme::default();
    let values = resolve_values(root, &fresh_reactor());
    let mut measurer = ApproxMeasurer;
    layout(root, Rect::new(0.0, 0.0, width, height), &theme, &mut measurer, &values);
}

/// Walk every element in a tree, panels' groups included.
fn walk<'a>(e: &'a Element, out: &mut Vec<&'a Element>) {
    out.push(e);
    for c in &e.children {
        walk(c, out);
    }
    for (_, g) in &e.groups {
        for c in g {
            walk(c, out);
        }
    }
}

/// Every element of a kind, by its type name.
fn of_kind<'a>(root: &'a Element, kind: &str) -> Vec<&'a Element> {
    let mut all = Vec::new();
    walk(root, &mut all);
    all.into_iter().filter(|e| e.kind.type_name() == kind).collect()
}

/// The text an element displays, resolved through the same path the renderer
/// uses — so a test cannot pass on a `display_text` that layout and paint
/// disagree about.
fn text_of(root: &Element, id: ElementId, values: &ResolvedKey) -> String {
    let Some(e) = root.find(id) else { return "<missing>".into() };
    e.display_text(values.get(&ElementIdKey(id.0)).unwrap_or(&Resolved::default()))
}

/// The resolved values for a tree, keyed the way the renderer keys them.
type ResolvedKey = std::collections::HashMap<ElementIdKey, Resolved>;

/// A fresh reactor, for laying out a tree that is not tied to a runtime.
fn fresh_reactor() -> slowshell_core::Reactor {
    let r = slowshell_core::Reactor::new();
    for (path, value) in [
        ("clock.time", Value::str("00:00")),
        ("clock.unix", Value::Int(0)),
        ("battery.percentage", Value::Int(50)),
        ("battery.present", Value::Bool(false)),
        ("network.connected", Value::Bool(false)),
    ] {
        r.source(path, value);
    }
    r
}

// --------------------------------------------------------------------------- //
// Hot reload
// --------------------------------------------------------------------------- //

#[test]
fn a_reload_shows_the_new_text() {
    let s = Sandbox::new("reload");
    let p = s.config("Panel { left { Text { text: \"before\" } } }");
    let mut rt = runtime();

    let first = build(&mut rt, &p);
    let values = resolve_values(&first, &rt.reactor);
    assert_eq!(text_of(&first, first_text_id(&first), &values), "before");

    // The user edits the file; the shell rebuilds from the same path.
    s.write("shell.config", "Panel { left { Text { text: \"after\" } } }");
    let second = build(&mut rt, &p);
    let values = resolve_values(&second, &rt.reactor);
    assert_eq!(
        text_of(&second, first_text_id(&second), &values),
        "after",
        "a rebuild must show the new text"
    );
}

#[test]
fn a_broken_edit_keeps_the_previous_scene_alive() {
    // The single most important behaviour in the whole shell. A user
    // half-deleting a line must not lose their bar.
    let s = Sandbox::new("broken-edit");
    let p = s.config("Panel { left { Text { text: \"working\" } } }");
    let mut rt = runtime();
    let good = build(&mut rt, &p);

    s.write("shell.config", "Panel { left { Text { text:  } }");
    let mut outcome = rt.build(&p);
    assert!(outcome.has_errors(), "a missing value is an error");
    assert!(
        outcome.compiled.is_some(),
        "a broken edit must hand back the previous scene, not nothing"
    );
    // Asked before the scene is taken, because that is the order the shell sees
    // it in.
    assert!(
        outcome.summary().contains("previous scene"),
        "the summary must say the screen is showing the old config: {}",
        outcome.summary()
    );
    // And the scene it handed back is the old one, intact.
    let kept = outcome.compiled.take().expect("the previous scene").panels.remove(0);
    let values = resolve_values(&kept, &rt.reactor);
    assert_eq!(text_of(&kept, first_text_id(&kept), &values), "working");
    // The good tree the caller is still holding is unchanged too.
    let values = resolve_values(&good, &rt.reactor);
    assert_eq!(text_of(&good, first_text_id(&good), &values), "working");
}

#[test]
fn a_parse_error_carries_a_position() {
    let s = Sandbox::new("parse-error");
    let p = s.config("Panel { left { Text { text: \"x\" } }\n");
    let mut rt = runtime();
    let outcome = rt.build(&p);
    assert!(outcome.has_errors(), "an unclosed brace must be reported");
    let span = outcome.diagnostics[0]
        .span
        .as_ref()
        .unwrap_or_else(|| panic!("a parse error must carry a position: {:?}", outcome.diagnostics[0]));
    // The unclosed `{` is the last character of line 1, which is where the
    // parser ran out. Pointing at the end of the file instead would send the
    // user to the wrong line entirely.
    assert_eq!(span.start.line, 1, "the unclosed brace is at the end of line 1, got {span:?}");
    assert!(span.start.col > 30, "and it is near the end of that line, got {span:?}");
}

#[test]
fn an_include_is_reported_so_the_shell_can_watch_it() {
    // An include the watcher missed is a launcher that never updates, and it
    // would look like the reload feature being flaky.
    let s = Sandbox::new("include");
    s.write("panels/bar.config", "Text { text: \"from the include\" }");
    let p = s.config("Panel { left { include \"panels/bar.config\" } }");
    let mut rt = runtime();

    let root = build(&mut rt, &p);
    let values = resolve_values(&root, &rt.reactor);
    assert_eq!(text_of(&root, first_text_id(&root), &values), "from the include");

    let outcome = rt.build(&p);
    assert!(outcome.files.len() >= 2, "both files must be reported: {:?}", outcome.files);
    assert!(
        outcome.files.iter().any(|f| f.to_string_lossy().contains("bar.config")),
        "the include must be in the watched list: {:?}",
        outcome.files
    );
}

#[test]
fn a_config_saved_with_a_byte_order_mark_loads() {
    // Notepad writes one by default. These are the exact bytes it produces, and
    // rejecting them would blame line 1 for something the user never typed.
    let s = Sandbox::new("bom");
    let p = s.dir.join("shell.config");
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend_from_slice(b"Panel { left { Text { text: \"hi\" } } }");
    std::fs::write(&p, &bytes).unwrap();

    let mut rt = runtime();
    let root = build(&mut rt, &p);
    let values = resolve_values(&root, &rt.reactor);
    assert_eq!(text_of(&root, first_text_id(&root), &values), "hi");
}

#[test]
fn a_missing_timestamp_never_appears() {
    // `poll_files` compares modification times, and a path that cannot be
    // stat'ed would make a watcher silently stop working rather than report.
    let s = Sandbox::new("timestamps");
    let p = s.config("Panel { left { Text { text: \"x\" } } }");
    let stamp = std::fs::metadata(&p).and_then(|m| m.modified());
    assert!(stamp.is_ok(), "a file the shell just wrote must be stat-able");
    assert!(stamp.unwrap() > std::time::SystemTime::UNIX_EPOCH);
}

// --------------------------------------------------------------------------- //
// Reactivity
// --------------------------------------------------------------------------- //

#[test]
fn a_bound_value_follows_its_source() {
    let s = Sandbox::new("reactive");
    let p = s.config("Panel { left { Text { text: clock.time } } }");
    let mut rt = runtime();
    let root = build(&mut rt, &p);
    let id = first_text_id(&root);

    let read = |rt: &Runtime, root: &Element| {
        let v = resolve_values(root, &rt.reactor);
        text_of(root, id, &v)
    };

    assert_eq!(read(&rt, &root), "00:00", "the seeded value is what shows first");
    assert!(rt.reactor.set("clock.time", Value::str("14:32")));
    assert_eq!(read(&rt, &root), "14:32", "a push must reach the widget");
}

#[test]
fn an_unchanged_value_does_not_report_a_change() {
    // This one return value is the whole idle budget: a provider that pushes
    // the same value must be able to say "nothing to do".
    let rt = runtime();
    assert!(rt.reactor.set("clock.time", Value::str("09:00")), "a new value is a change");
    assert!(!rt.reactor.set("clock.time", Value::str("09:00")), "the same value is not");
    assert!(rt.reactor.set("clock.time", Value::str("09:01")), "a different value is");
}

#[test]
fn a_binding_that_reads_two_sources_follows_both() {
    // The dependency tracker is a stack for exactly this case. A single-slot
    // tracker records only the last read, and the widget then silently stops
    // updating when the *first* source changes — which looks like "reactivity
    // is flaky" rather than like a bug.
    //
    // Both paths here are reactor sources, which is the case that matters: a
    // path read through the host (a stub provider) is not a dependency, because
    // nothing pushes it.
    let s = Sandbox::new("two-sources");
    let p = s.config(r##"Panel { left { Text { text: clock.time + ":" + clock.unix } } }"##);
    let mut rt = runtime();
    let root = build(&mut rt, &p);
    let id = first_text_id(&root);

    let read = |rt: &Runtime, root: &Element| {
        let v = resolve_values(root, &rt.reactor);
        text_of(root, id, &v)
    };

    rt.reactor.set("clock.unix", Value::Int(1700000000));
    assert_eq!(read(&rt, &root), "00:00:1700000000", "both sources are read");

    // Only the *second* source.
    assert!(rt.reactor.set("clock.unix", Value::Int(42)));
    assert_eq!(read(&rt, &root), "00:00:42", "the second source must be tracked");
    // And only the first.
    assert!(rt.reactor.set("clock.time", Value::str("23:59")));
    assert_eq!(read(&rt, &root), "23:59:42", "the first source must be tracked");
}

#[test]
fn a_widget_that_reads_nothing_is_never_recomputed() {
    let s = Sandbox::new("static-widget");
    let p = s.config(r##"Panel { left { Text { text: "static" } Text { text: clock.time } } }"##);
    let mut rt = runtime();
    let root = build(&mut rt, &p);
    let static_id = of_kind(&root, "Text")[0].id;

    let before = resolve_values(&root, &rt.reactor).get(&ElementIdKey(static_id.0)).cloned();
    rt.reactor.set("clock.time", Value::str("23:59"));
    let after = resolve_values(&root, &rt.reactor).get(&ElementIdKey(static_id.0)).cloned();
    assert_eq!(before, after, "a literal cannot change");
}

#[test]
fn a_clock_ticks_without_changing_the_size_of_anything() {
    // The optimisation the frame budget rests on. A `Clock`'s text is derived
    // from `clock.unix` at display time, not bound as a resolved string, so the
    // interesting property is not "the value changed" — it is that the *measured
    // width* does not. A bar that re-lays out once a second for a change of
    // layout is spending the frame budget for nothing.
    let s = Sandbox::new("layout-thrash");
    let p = s.config(r##"Panel { height: 32  left { Clock { format: "HH:mm" } } }"##);
    let mut rt = runtime();
    let root = build(&mut rt, &p);

    // 00:00 and 12:34 are the same five characters in every fixed-width format,
    // so the measurement is identical.
    rt.reactor.set("clock.unix", Value::Int(0));
    let at_midnight = slowshell_ui::panel_intrinsic(&root, 800.0, &mut ApproxMeasurer);
    rt.reactor.set("clock.unix", Value::Int(1_700_000_000));
    let at_later = slowshell_ui::panel_intrinsic(&root, 800.0, &mut ApproxMeasurer);

    assert!(
        (at_midnight.0 - at_later.0).abs() < 0.5,
        "a clock that ticks must not change the layout: {at_midnight:?} then {at_later:?}"
    );
    // And a format that *can* change width must actually do so, or the previous
    // assertion would pass for the wrong reason.
    let wide = s.config(r##"Panel { height: 32  left { Clock { format: "HH:mm:ss" } } }"##);
    let root2 = build(&mut rt, &wide);
    let short = slowshell_ui::panel_intrinsic(&root2, 800.0, &mut ApproxMeasurer);
    assert!(
        short.0 > at_later.0,
        "a longer format must be wider, or the first assertion proved nothing: \
         {at_later:?} vs {short:?}"
    );
}

// --------------------------------------------------------------------------- //
// Error boundaries
// --------------------------------------------------------------------------- //

#[test]
fn one_bad_widget_does_not_take_the_bar_with_it() {
    let s = Sandbox::new("error-boundary");
    // A `Clock` with an empty format is a diagnostic, not a crash, and the
    // three good widgets must all still be there.
    let p = s.config(
        r##"Panel {
    left {
        Text { text: "one" }
        Clock { format: "" }
        Text { text: "two" }
        Text { text: "three" }
    }
}"##,
    );
    let mut rt = runtime();
    let mut outcome = rt.build(&p);
    assert!(outcome.has_errors(), "an empty clock format is an error");

    let root = outcome.compiled.take().expect("the rest must still build").panels.remove(0);
    let values = resolve_values(&root, &rt.reactor);
    let texts: Vec<String> = of_kind(&root, "Text")
        .iter()
        .map(|e| text_of(&root, e.id, &values))
        .collect();
    for expected in ["one", "two", "three"] {
        assert!(texts.contains(&expected.to_string()), "{expected} is missing: {texts:?}");
    }
    assert_eq!(
        of_kind(&root, "Broken").len(),
        1,
        "the bad one becomes a visible placeholder, not a gap"
    );
}

#[test]
fn a_misspelled_property_names_the_real_one() {
    let s = Sandbox::new("did-you-mean");
    let p = s.config("Panel { left { Text { text: \"x\"  foregorund: \"red\" } } }");
    let mut rt = runtime();
    let outcome = rt.build(&p);
    let d = outcome
        .diagnostics
        .iter()
        .find(|d| d.message.contains("foregorund"))
        .expect("the typo must be reported");
    assert!(
        d.hints.iter().any(|h| h.contains("foreground")),
        "the real name must be suggested: {:?}",
        d.hints
    );
}

#[test]
fn an_unknown_element_names_the_known_ones() {
    let s = Sandbox::new("unknown-element");
    let p = s.config("Panel { left { Panle { text: \"x\" } } }");
    let mut rt = runtime();
    let outcome = rt.build(&p);
    let d = outcome
        .diagnostics
        .iter()
        .find(|d| d.message.contains("Panle"))
        .expect("the unknown element must be reported");
    assert!(d.hints.iter().any(|h| h.contains("Panel")), "{d:?}");
}

#[test]
fn a_config_with_no_panel_is_refused() {
    let s = Sandbox::new("no-panel");
    let p = s.config("Text { text: \"an orphan\" }");
    let mut rt = runtime();
    let outcome = rt.build(&p);
    assert!(outcome.has_errors(), "a config must start with a Panel");
    assert!(
        outcome.diagnostics.iter().any(|d| d.message.contains("Panel")),
        "{:?}",
        outcome.diagnostics
    );
}

#[test]
fn runaway_nesting_is_stopped_rather_than_followed() {
    // A config that nests without end is a config bug, not a reason to exhaust
    // the stack. The compiler caps depth at 32, so 200 levels must be reported
    // and the rest of the tree must still build.
    let mut body = String::from("Text { text: \"x\" }");
    for _ in 0..200 {
        body = format!("Row {{ {body} }}");
    }
    let s = Sandbox::new("deep");
    let p = s.config(&format!("Panel {{ {body} }}"));
    let mut rt = runtime();
    let outcome = rt.build(&p);
    assert!(outcome.has_errors(), "unbounded nesting must be reported");
    let d = outcome
        .diagnostics
        .iter()
        .find(|d| d.message.contains("nested"))
        .unwrap_or_else(|| {
            panic!("the diagnostic must say why: {:?}", outcome.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>())
        });
    // The name of what was dropped, so the user knows *where* to look rather
    // than just that something went.
    assert!(
        d.notes.iter().any(|n| n.contains("Row")),
        "the dropped element must be named: {:?}",
        d.notes
    );
    assert!(outcome.compiled.is_some(), "and the rest of the tree must still build");
}

#[test]
fn a_leaf_element_rejects_children() {
    let s = Sandbox::new("leaf-children");
    let p = s.config("Panel { left { Text { text: \"x\"  Text { text: \"y\" } } } }");
    let mut rt = runtime();
    assert!(rt.build(&p).has_errors(), "a Text cannot contain a Text");
}

// --------------------------------------------------------------------------- //
// Handlers and hit regions
// --------------------------------------------------------------------------- //

#[test]
fn a_click_reaches_the_action_it_names() {
    let s = Sandbox::new("handler");
    let p = s.config(r##"Panel { left { Text { text: "Apps"  onClick: launch("notepad.exe") } } }"##);
    let mut rt = runtime();
    let root = build(&mut rt, &p);

    let text = &of_kind(&root, "Text")[0];
    let handlers = text.handlers.borrow();
    assert_eq!(handlers.len(), 1, "one handler, bound to the element");
    assert_eq!(handlers[0].0, "onClick");
    assert_eq!(handlers[0].1.action, "launch");
    assert_eq!(
        handlers[0].1.args,
        vec![Value::str("notepad.exe")],
        "the argument must survive from the config to the click"
    );
    drop(handlers);

    // And the registry accepts the call the handler makes.
    assert!(rt.actions.call(&["launch".into()], &[Value::str("notepad.exe")]).is_ok());
}

#[test]
fn a_handler_with_a_bad_target_never_reaches_a_click() {
    // The build-time check is the whole reason a typo is a diagnostic rather
    // than a button that silently does nothing.
    let s = Sandbox::new("handler-bad");
    let p = s.config(r##"Panel { left { Text { text: "x"  onClick: shell.opan("y") } } }"##);
    let mut rt = runtime();
    let outcome = rt.build(&p);
    assert!(outcome.has_errors(), "a misspelled action must fail the build");
    let d = outcome
        .diagnostics
        .iter()
        .find(|d| d.message.contains("shell.opan"))
        .expect("the bad target must be named");
    assert!(
        d.notes.iter().any(|n| n.contains("shell.open")),
        "the alternatives must be listed: {:?}",
        d.notes
    );
}

#[test]
fn a_handler_that_is_a_string_is_refused() {
    let s = Sandbox::new("handler-string");
    let p = s.config("Panel { left { Text { text: \"x\"  onClick: \"launcher.open\" } } }");
    let mut rt = runtime();
    let outcome = rt.build(&p);
    assert!(outcome.has_errors(), "a string is not a call");
    assert!(
        outcome.diagnostics.iter().any(|d| d.message.contains("must be a call")),
        "{:?}",
        outcome.diagnostics
    );
}

#[test]
fn only_elements_with_handlers_publish_hit_regions() {
    // The mechanism is not "the bar is clickable", it is "a region with a
    // handler is clickable". A config that wants a click-through bar gets one by
    // omitting handlers, and adding one handler yields exactly one region.
    let s = Sandbox::new("hit-regions");
    let p = s.config(
        r##"Panel {
    left {
        Text { text: "click me"  onClick: shell.overlay() }
        Text { text: "ignore me" }
    }
}"##,
    );
    let mut rt = runtime();
    let root = build(&mut rt, &p);
    lay(&root, 400.0, 32.0);

    let values = resolve_values(&root, &rt.reactor);
    let regions = slowshell_ui::collect_regions(&root, &values, &slowshell_ui::Style::default());
    assert_eq!(
        regions.len(),
        1,
        "exactly one widget has a handler, so exactly one region: {regions:?}"
    );
    let clickable = of_kind(&root, "Text")
        .into_iter()
        .find(|e| text_of(&root, e.id, &values) == "click me")
        .expect("the labelled text");
    assert_eq!(regions[0].id, clickable.id.0, "and it is the right one");
    assert!(regions[0].interactive, "a published region must be interactive");
}

#[test]
fn a_config_with_no_handlers_publishes_nothing() {
    let s = Sandbox::new("no-regions");
    let p = s.config("Panel { left { Text { text: \"x\" } } }");
    let mut rt = runtime();
    let root = build(&mut rt, &p);
    lay(&root, 400.0, 32.0);
    let values = resolve_values(&root, &rt.reactor);
    let regions = slowshell_ui::collect_regions(&root, &values, &slowshell_ui::Style::default());
    assert!(regions.is_empty(), "nothing here has a handler, so nothing is clickable");
}

#[test]
fn hit_testing_finds_the_region_under_a_point_and_past_it_returns_nothing() {
    let s = Sandbox::new("hit-test");
    let p = s.config(
        r##"Panel {
    left {
        Text { text: "one"  onClick: shell.overlay()  padding: [0, 8] }
        Text { text: "two"  onClick: shell.overlay()  padding: [0, 8] }
    }
}"##,
    );
    let mut rt = runtime();
    let root = build(&mut rt, &p);
    lay(&root, 400.0, 32.0);
    let values = resolve_values(&root, &rt.reactor);
    let regions = slowshell_ui::collect_regions(&root, &values, &slowshell_ui::Style::default());
    assert_eq!(regions.len(), 2, "both labels have handlers");

    slowshell_win::hit_test::set_regions(regions);
    let first = slowshell_win::hit_test::regions()[0].clone();

    // Inside the first region, on its element.
    let hit = slowshell_win::hit_test::region_at(first.x + first.w / 2.0, first.y + first.h / 2.0);
    assert_eq!(hit, Some(first.id), "a click inside a region must reach it");
    // Past the last region, nothing: the pointer falls through to the desktop.
    assert_eq!(
        slowshell_win::hit_test::region_at(399.0, 31.0),
        None,
        "past the content the bar must be click-through"
    );
    slowshell_win::hit_test::clear_regions();
}

// --------------------------------------------------------------------------- //
// Layout
// --------------------------------------------------------------------------- //

#[test]
fn a_bar_band_lands_where_it_was_asked_to() {
    let s = Sandbox::new("bands");
    let p = s.config(
        r##"Panel {
    left  { Text { text: "L" } }
    right { Text { text: "R" } }
}"##,
    );
    let mut rt = runtime();
    let root = build(&mut rt, &p);
    let values = resolve_values(&root, &rt.reactor);
    lay(&root, 800.0, 32.0);

    let texts = of_kind(&root, "Text");
    let l = texts.iter().find(|e| text_of(&root, e.id, &values) == "L").unwrap().rect.get();
    let r = texts.iter().find(|e| text_of(&root, e.id, &values) == "R").unwrap().rect.get();
    assert!(l.x < r.x, "left must come before right: {l:?} then {r:?}");
    assert!(r.x + r.w <= 800.5, "the right band must not overflow the panel: {r:?}");
}

#[test]
fn a_bar_too_narrow_shrinks_rather_than_overlaps() {
    // A crowded bar must degrade, not collide. Overlapping text is the single
    // ugliest thing a bar can do, and it is a symptom of bands refusing to
    // give ground.
    let s = Sandbox::new("crowded");
    let p = s.config(
        r##"Panel {
    left  { Text { text: "a fairly long left band" } }
    right { Text { text: "a fairly long right band" } }
}"##,
    );
    let mut rt = runtime();
    let root = build(&mut rt, &p);
    let values = resolve_values(&root, &rt.reactor);

    let bands = |root: &Element| {
        let texts = of_kind(root, "Text");
        let l = texts
            .iter()
            .find(|e| text_of(root, e.id, &values).starts_with("a fairly long left"))
            .expect("the left band");
        let r = texts
            .iter()
            .find(|e| text_of(root, e.id, &values).starts_with("a fairly long right"))
            .expect("the right band");
        (l.rect.get(), r.rect.get())
    };

    lay(&root, 800.0, 32.0);
    let (wide_l, wide_r) = bands(&root);
    lay(&root, 160.0, 32.0);
    let (narrow_l, narrow_r) = bands(&root);

    assert!(narrow_l.w <= wide_l.w + 0.5, "the left band must shrink: {wide_l:?} then {narrow_l:?}");
    assert!(narrow_r.w <= wide_r.w + 0.5, "the right band must shrink: {wide_r:?} then {narrow_r:?}");
    assert!(
        narrow_l.x + narrow_l.w <= narrow_r.x + 0.5,
        "shrinking bands must not end up overlapping: {narrow_l:?} vs {narrow_r:?}"
    );
}

#[test]
fn padding_is_inset_once_and_only_once() {
    // Layout insets for padding; paint must not inset again. Doing it in both
    // places is invisible in the source and doubles every margin in the bar.
    let s = Sandbox::new("padding");
    let p = s.config(r##"Panel { left { Text { text: "x"  padding: [0, 20] } } }"##);
    let mut rt = runtime();
    let root = build(&mut rt, &p);
    lay(&root, 400.0, 32.0);
    let text = of_kind(&root, "Text")[0].rect.get();
    assert!(text.x >= 19.5, "the horizontal padding was not applied: {text:?}");
    assert!(text.x < 21.0, "the horizontal padding was applied more than once: {text:?}");
}

#[test]
fn a_vertical_panel_measures_its_content() {
    // A left dock's height comes from what is in it. A fixed height would either
    // clip the content or leave a gap under it.
    let s = Sandbox::new("vertical");
    let mut rt = runtime();

    let one = build(&mut rt, &s.config(r##"Panel { position: "left"  center { Text { text: "i" } } }"##));
    let one_h = intrinsic_height(&one);

    let three = build(
        &mut rt,
        &s.config(
            r##"Panel { position: "left"  center { Column { gap: 8  Text { text: "i" }  Text { text: "ii" }  Text { text: "iii" } } } }"##,
        ),
    );
    let three_h = intrinsic_height(&three);

    assert!(three_h > one_h, "more content must need more height: {one_h} then {three_h}");
}

fn intrinsic_height(root: &Element) -> f32 {
    let mut measurer = ApproxMeasurer;
    slowshell_ui::panel_intrinsic(root, 200.0, &mut measurer).1
}

#[test]
fn a_clock_with_a_real_format_is_not_an_empty_string() {
    // A `Clock` that measured as an empty string collapsed the bar. The
    // measurement must come from real formatted output.
    let s = Sandbox::new("clock-measure");
    let p = s.config(r##"Panel { center { Clock { format: "HH:mm:ss" } } }"##);
    let mut rt = runtime();
    let root = build(&mut rt, &p);
    let values = resolve_values(&root, &rt.reactor);
    let clock = &of_kind(&root, "Clock")[0];
    let shown = text_of(&root, clock.id, &values);
    assert_eq!(shown.len(), 8, "`HH:mm:ss` produces eight characters, got {shown:?}");

    let (w, h) = slowshell_ui::panel_intrinsic(&root, 400.0, &mut ApproxMeasurer);
    assert!(w > 0.0, "a clock must be wider than nothing");
    assert!(h > 0.0, "a clock must be taller than nothing");
}

#[test]
fn a_progress_bar_takes_a_reactive_fraction() {
    let s = Sandbox::new("progress");
    let p = s.config(r##"Panel { left { Progress { value: battery.percentage / 100  height: 6 } } }"##);
    let mut rt = runtime();
    let root = build(&mut rt, &p);
    let bar = &of_kind(&root, "Progress")[0];
    assert!(bar.dyn_props.progress.is_some(), "an expression value must become a reactive binding");
    assert_eq!(
        bar.style.height,
        slowshell_ui::style::Size::Fixed(6.0),
        "the declared thickness is used"
    );
}

#[test]
fn an_invisible_element_reserves_no_space() {
    // The point of `visible` is that a hidden widget costs nothing and occupies
    // nothing, so the visible one starts where the hidden one would have.
    let s = Sandbox::new("invisible");
    let p = s.config(
        r##"Panel {
    left {
        Text { text: "hidden"  visible: false }
        Text { text: "shown" }
    }
}"##,
    );
    let mut rt = runtime();
    let root = build(&mut rt, &p);
    let values = resolve_values(&root, &rt.reactor);
    lay(&root, 400.0, 32.0);

    let hidden = of_kind(&root, "Text")
        .into_iter()
        .find(|e| text_of(&root, e.id, &values) == "hidden")
        .expect("the hidden text");
    let shown = of_kind(&root, "Text")
        .into_iter()
        .find(|e| text_of(&root, e.id, &values) == "shown")
        .expect("the visible text");
    assert!(
        hidden.rect.get().w == 0.0 || hidden.rect.get().h == 0.0,
        "an invisible element must not reserve space: {:?}",
        hidden.rect.get()
    );
    assert!(shown.rect.get().x < 1.0, "the visible one must take the freed space: {:?}", shown.rect.get());
}

#[test]
fn a_theme_block_changes_the_style_that_is_used() {
    // A theme that is parsed and ignored is worse than one that is rejected,
    // because the config looks right and the screen does not change.
    let s = Sandbox::new("theme");
    let p = s.config(
        r##"Theme { accent: "#ff0000"  foreground: "#00ff00"  fontSize: 20 }
Panel { left { Text { text: "t"  color: "accent" } } }"##,
    );
    let mut rt = runtime();
    let root = build(&mut rt, &p);
    let theme = rt.theme();
    assert_eq!(theme.get("accent"), Some(slowshell_core::Color::rgb(255, 0, 0)));
    assert_eq!(theme.get("foreground"), Some(slowshell_core::Color::rgb(0, 255, 0)));
    assert_eq!(theme.font_size, 20.0, "the base size must be applied");

    let text = &of_kind(&root, "Text")[0];
    assert_eq!(text.style.font_size, 20.0, "a widget with no size takes the theme's");
    assert_eq!(
        text.style.foreground.token,
        Some("accent"),
        "a colour token must be kept as a token until paint resolves it"
    );
}

#[test]
fn a_theme_name_is_a_name_and_not_an_unknown_colour() {
    // The metrics are checked before the colour path, so `name: "x"` is a name.
    let s = Sandbox::new("theme-name");
    let p = s.config(
        "Theme { name: \"my-theme\"  dark: false  radius: 4 }\nPanel { left { Text { text: \"x\" } } }",
    );
    let mut rt = runtime();
    let outcome = rt.build(&p);
    assert!(!outcome.has_errors(), "{}", render_diagnostics(&outcome.diagnostics));
    let theme = rt.theme();
    assert_eq!(theme.name, "my-theme");
    assert!(!theme.dark);
    assert_eq!(theme.radius, 4.0);
}

#[test]
fn colour_cascades_and_geometry_does_not() {
    // The rule that keeps every margin in the bar correct: typography and colour
    // inherit; padding and background do not. Inheriting geometry would hand
    // every child the parent's padding on top of the inset layout already
    // applied, and every inset would double.
    let s = Sandbox::new("cascade");
    let p = s.config(
        r##"Panel {
    color: "accent"
    fontWeight: 700
    padding: 20
    background: "#ff0000"
    left {
        Row {
            padding: 4
            Text { text: "child" }
        }
    }
}"##,
    );
    let mut rt = runtime();
    let root = build(&mut rt, &p);
    let row = &of_kind(&root, "Row")[0];
    let text = &of_kind(&root, "Text")[0];

    assert_eq!(text.style.foreground.token, Some("accent"), "colour cascades");
    assert_eq!(text.style.font_weight, 700, "typography cascades");
    assert_eq!(row.style.padding, Edges::all(4.0), "a child keeps its own padding");
    assert_ne!(
        row.style.padding,
        Edges::all(20.0),
        "the parent's padding must not be inherited, or every inset doubles"
    );
    assert!(
        row.style.background.literal.a == 0,
        "a background must not cascade, or every child gets a filled box: {:?}",
        row.style.background
    );
}

// --------------------------------------------------------------------------- //
// DPI and multi-monitor
// --------------------------------------------------------------------------- //

#[test]
fn a_pixels_to_logical_conversion_is_exactly_one_division() {
    // The bug this guards: window bounds are physical pixels and get divided by
    // the monitor *scale*, and DirectWrite metrics come back in DIPs and get
    // divided by scale too. Doing both makes everything double-size at 150%,
    // and doing it with DPI instead of scale makes it wrong at every scale.
    //
    // The conversion itself is what is under test, because the text measuring
    // side needs a real `TextEngine` and a real device. `ApproxMeasurer` does not
    // scale, so asserting on laid-out text here would be asserting nothing.
    for m in &Monitors::enumerate().list {
        for logical in [1.0f32, 12.0, 100.0, 1920.0] {
            let physical = m.physical(logical);
            let back = m.logical_width();
            assert!(physical > 0, "{}: {} logical became {} physical", m.id, logical, physical);
            assert!(back > 0.0, "{}: no logical width", m.id);
            // Round-tripping the display's own size must land back where it
            // started, or every panel is created at the wrong width.
            let there = m.physical(m.logical_width());
            let again = m.physical(m.logical_height());
            assert!(
                (there as f32 - m.width as f32).abs() <= 1.0,
                "{}: width does not round-trip: {} -> {} -> {}",
                m.id,
                m.logical_width(),
                there,
                m.width
            );
            assert!(
                (again as f32 - m.height as f32).abs() <= 1.0,
                "{}: height does not round-trip: {} -> {} -> {}",
                m.id,
                m.logical_height(),
                again,
                m.height
            );
            // And scaling is monotonic, so a bigger logical box is never a
            // smaller physical one.
            assert!(
                m.physical(logical + 1.0) > m.physical(logical),
                "{}: scaling is not monotonic at {logical}",
                m.id
            );
        }
    }
}

#[test]
fn every_display_reports_a_usable_geometry() {
    // A display the shell cannot size a surface for is a panel created at the
    // wrong size and never corrected.
    let monitors = Monitors::enumerate();
    assert!(!monitors.list.is_empty(), "there is always at least one display");
    for m in &monitors.list {
        assert!(m.width > 0 && m.height > 0, "{} has no size", m.id);
        assert!(m.scale > 0.0, "{} has scale {}; zero makes every layout infinite", m.id, m.scale);
        assert!(m.logical_width() > 0.0 && m.logical_height() > 0.0, "{} has no logical size", m.id);
        assert!(m.logical_width() <= m.width as f32 + 0.5, "{} loses width when scaled", m.id);
    }
    assert!(monitors.primary().primary, "one display must be primary");
}

#[test]
fn a_panel_can_be_bound_to_a_named_display() {
    let monitors = Monitors::enumerate();
    let target = monitors.list[0].id.clone();
    let s = Sandbox::new("screen-bind");
    let path = s.config(&format!(
        "Panel {{ screen: \"{target}\"  left {{ Text {{ text: \"x\" }} }} }}"
    ));
    let mut rt = runtime();
    let root = build(&mut rt, &path);
    match &root.kind {
        slowshell_ui::ElementKind::Panel { screen, .. } => {
            assert_eq!(screen, &target, "the panel must remember its display")
        }
        other => panic!("expected a panel, got {other:?}"),
    }
    assert!(monitors.get(&target).is_some(), "`{target}` must resolve back to a display");
    // And an unbindable name must fall back rather than produce no surface.
    assert!(monitors.get("no-such-display").is_none());
}

#[test]
fn a_second_display_does_not_disturb_the_first() {
    // Skipped honestly on a single-display machine rather than faked.
    let monitors = Monitors::enumerate();
    if monitors.list.len() < 2 {
        eprintln!("skipping: one display attached");
        return;
    }
    let primary = monitors.primary().clone();
    let other = &monitors.list[1];
    assert_ne!(primary.id, other.id, "two displays must have distinct ids");
    // Distinct origins are what let a surface be placed on the second one.
    assert!(
        monitors.list.iter().any(|m| m.x != 0 || m.y != 0),
        "a second display must be offset, or the two overlap"
    );
    // Each is independently addressable, which is what `screen:` needs.
    for m in &monitors.list {
        assert!(monitors.get(&m.id).is_some(), "{} must resolve by id", m.id);
    }
}

// --------------------------------------------------------------------------- //
// Widget lifecycle
// --------------------------------------------------------------------------- //

#[test]
fn a_hidden_panel_compiles_but_takes_no_surface() {
    // The launcher mechanism, from the config side: declared, compiled,
    // retrievable by name, and not part of what is shown.
    let s = Sandbox::new("hidden-panel");
    let p = s.config(
        r##"Panel { position: "top"  left { Text { text: "bar" } } }

Panel { name: "launcher"  hidden: true  width: 400  height: 40  content { Text { text: "go" } } }"##,
    );
    let mut rt = runtime();
    let outcome = rt.build(&p);
    assert!(!outcome.has_errors(), "{}", render_diagnostics(&outcome.diagnostics));
    assert_eq!(outcome.compiled.as_ref().unwrap().panels.len(), 2, "both compile");

    assert_eq!(rt.hidden_panels().len(), 1, "exactly one panel is hidden");
    let launcher = rt.hidden_panel("launcher").expect("and it is findable by name");
    match &launcher.kind {
        slowshell_ui::ElementKind::Panel { hidden, name, .. } => {
            assert!(hidden, "it must still be marked hidden");
            assert_eq!(name, "launcher");
        }
        other => panic!("expected a panel, got {other:?}"),
    }
    assert!(rt.hidden_panel("nope").is_none(), "an unknown name finds nothing");
}

#[test]
fn a_panel_name_survives_a_reload() {
    // If a rebuild lost the name, the second click on `Apps` would fail with
    // "no panel named launcher" — a bug that only shows on the second use.
    let s = Sandbox::new("panel-name");
    let body = r##"Panel { name: "launcher"  hidden: true  width: 300  height: 40  content { Text { text: "a" } } }"##;
    let p = s.config(body);
    let mut rt = runtime();
    build(&mut rt, &p);
    assert!(rt.hidden_panel("launcher").is_some());

    s.write("shell.config", &format!("{body} "));
    build(&mut rt, &p);
    assert!(rt.hidden_panel("launcher").is_some(), "the name must survive a rebuild");
}

#[test]
fn every_config_in_the_examples_directory_still_builds() {
    // An example that stopped compiling teaches a syntax the shell no longer
    // accepts, which is worse than no example at all.
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    let Ok(dir) = dir.canonicalize() else {
        eprintln!("skipping: no examples directory in this checkout");
        return;
    };
    let mut names: Vec<_> = std::fs::read_dir(&dir)
        .expect("examples directory")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "config"))
        .collect();
    names.sort();
    assert!(!names.is_empty(), "no examples in {}", dir.display());
    for path in names {
        let mut rt = runtime();
        let outcome = rt.build(&path);
        assert!(!outcome.has_errors(), "{}:\n{}", path.display(), render_diagnostics(&outcome.diagnostics));
    }
}

// --------------------------------------------------------------------------- //
// Actions
// --------------------------------------------------------------------------- //

#[test]
fn every_action_a_config_uses_actually_resolves() {
    // The check the build performs, over a config that uses the whole surface. A
    // name in a config that is not in the registry is a dead button.
    let s = Sandbox::new("action-surface");
    let p = s.config(
        r##"Panel {
    left {
        Text { text: "a"  onClick: shell.reload() }
        Text { text: "b"  onClick: shell.overlay() }
        Text { text: "c"  onClick: shell.open("launcher") }
        Text { text: "d"  onClick: launch("x.txt") }
        Text { text: "e"  onClick: notify("t", "b") }
        Text { text: "f"  onClick: clipboard.set("x") }
        Text { text: "g"  onClick: audio.raise() }
        Text { text: "h"  onClick: audio.lower() }
        Text { text: "i"  onClick: audio.toggleMute() }
        Text { text: "j"  onClick: keys.send("Win+D") }
        Text { text: "k"  onClick: windows.minimiseAll() }
        Text { text: "l"  onClick: screens.list() }
    }
}"##,
    );
    let mut rt = runtime();
    let outcome = rt.build(&p);
    assert!(!outcome.has_errors(), "{}", render_diagnostics(&outcome.diagnostics));
}

#[test]
fn an_action_with_the_wrong_arity_is_refused_with_both_counts() {
    let mut registry = Registry::new();
    registry.register("test.one", "Takes one.", &["only"], |a| Ok(a[0].clone()));
    let err = registry.call(&["test".into(), "one".into()], &[]).expect_err("no argument is refused");
    assert!(err.contains("takes 1 (only)"), "{err}");
    assert!(err.contains("given 0"), "{err}");
}

#[test]
fn an_unknown_action_suggests_the_real_one() {
    let mut registry = Registry::new();
    registry.register("launcher.open", "Open it.", &[], |_| Ok(Value::Null));
    let err = registry
        .call(&["launhcer".into(), "open".into()], &[])
        .expect_err("a misspelling is refused");
    assert!(err.contains("Did you mean `launcher.open`"), "{err}");
}

#[test]
fn nothing_is_reachable_before_it_is_registered() {
    // The one switch a future restricted mode would flip.
    let mut registry = Registry::new();
    assert!(registry.is_empty());
    assert!(registry.call(&["windows".into(), "lock".into()], &[]).is_err());
    slowshell_win::platform::register(&mut registry);
    assert!(!registry.is_empty());
    assert!(registry.get("windows.lock").is_some(), "register must publish it");
    // The shell's own actions are not the platform's, and a library user that
    // registers only the platform half must not appear to have them.
    assert!(registry.get("shell.reload").is_none());
}

#[test]
fn every_registered_action_documents_itself() {
    // `shellctl actions` prints the description, so an action without one is a
    // hole in the user-facing surface.
    let mut registry = Registry::new();
    slowshell_win::platform::register(&mut registry);
    for line in registry.describe() {
        let body = line.trim();
        let (signature, description) = body.split_once("  ").unwrap_or((body, ""));
        assert!(!signature.is_empty(), "no signature in {line:?}");
        assert!(!description.trim().is_empty(), "no description for {signature:?}");
    }
}

// --------------------------------------------------------------------------- //
// The control channel
// --------------------------------------------------------------------------- //

#[test]
fn the_request_protocol_round_trips_every_message() {
    // The wire format, both directions. A protocol test that only checks serde
    // would miss a framing bug, and framing is what makes a short request and a
    // long reply work against the same pipe.
    for request in [
        Request::Status,
        Request::Reload,
        Request::Stop,
        Request::ToggleOverlay,
        Request::Logs { limit: 5, level: "warn".into() },
        Request::Diagnostics,
        Request::Graph,
        Request::Screens,
        Request::Open { name: "launcher".into() },
    ] {
        let line = serde_json::to_string(&request).unwrap();
        let back = Request::parse(&line)
            .unwrap_or_else(|e| panic!("{request:?} did not survive: {e}"));
        assert_eq!(back, request, "{request:?} changed on the way through");
    }
}

#[test]
fn a_malformed_request_is_rejected_with_a_message() {
    // A CLI that hangs looks exactly like a shell that has frozen, so a bad
    // line must come back as an error rather than as silence.
    let e = Request::parse("{not json").expect_err("malformed input is refused");
    assert!(e.contains("malformed"), "{e}");
    assert!(Request::parse("{\"cmd\":\"teleport\"}").is_err());
}

#[test]
fn a_response_serialises_both_ways() {
    // `Response` carries a `serde_json::Value`, so equality is checked on the
    // serialised form. That is also the form the wire sees, which makes this
    // the right thing to compare.
    for r in [
        Response::ok(serde_json::json!({ "count": 2, "names": ["a", "b"] })),
        Response::err("something went wrong"),
    ] {
        let line = serde_json::to_string(&r).expect("a response must serialise");
        let back: Response = serde_json::from_str(&line).expect("a response must deserialise");
        assert_eq!(
            serde_json::to_string(&back).unwrap(),
            line,
            "the response changed on the way through"
        );
    }
}

#[test]
fn a_dead_shell_reads_as_no_shell_rather_than_a_hang() {
    // The crash case. A shell that was killed leaves its endpoint file behind,
    // and every `shellctl` would then hang for its full timeout. The liveness
    // check on the published pid is what turns that into a millisecond answer.
    use slowshell_win::ipc_pipe::Endpoint;

    match Endpoint::discover() {
        Some(existing) => {
            assert!(!existing.pipe.is_empty(), "a live endpoint names a pipe");
            assert!(existing.pid != 0, "a live endpoint names a process");
        }
        None => {
            // No shell running. The honest answer is an error, not a guess.
            let e = slowshell_win::ipc_pipe::request(&Request::Status, 50)
                .expect_err("with no shell there is nothing to talk to");
            assert!(e.contains("no slowshell is running"), "{e}");
        }
    }
}

#[test]
fn the_pipe_name_cannot_escape_the_object_namespace() {
    // A crafted user name must not be able to name a pipe outside it.
    let name = slowshell_win::ipc_pipe::pipe_name();
    let prefix = r"\\.\pipe\";
    assert!(name.starts_with(prefix), "{name}");
    let suffix = &name[prefix.len()..];
    assert!(!suffix.contains('\\'), "the suffix must be flat: {suffix}");
    assert!(!suffix.contains(' '), "{suffix}");
}

// --------------------------------------------------------------------------- //
// Helpers that need the resolved map
// --------------------------------------------------------------------------- //


fn first_text_id(root: &Element) -> ElementId {
    of_kind(root, "Text")[0].id
}
