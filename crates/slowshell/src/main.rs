//! `Shell.exe` — the slowshell runtime process.
//!
//! Startup, the frame loop, and the wiring between the four layers. Everything
//! platform-specific lives in `slowshell-win`, everything widget-shaped in
//! `slowshell-ui`, and the language and reactivity in `slowshell-core`; this file
//! only decides the order things happen in.
//!
//! ## The frame loop
//!
//! ```text
//!   ┌─ pump Windows messages ──────────────┐
//!   │                                      │
//!   │   ┌─ publish system values ────┐     │
//!   │   │  (event driven + 1 Hz)    │     │
//!   │   └────────────┬──────────────┘     │
//!   │                ▼                    │
//!   │   ┌─ did the reactor invalidate? ─┐ │
//!   │   │   no → sleep until the next  │ │
//!   │   │        deadline (idle ~0%)    │ │
//!   │   │   yes ↓                      │ │
//!   │   │   layout → paint → present   │ │
//!   │   └───────────────────────────────┘ │
//!   └──────────────────────────────────────┘
//! ```
//!
//! The sleep in the "no" branch is what keeps idle CPU under a percent: a shell
//! that repaints on a timer regardless of whether anything changed is the most
//! common way a desktop shell ends up burning a core.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use slowshell_core::ipc::{Request, Response};
use slowshell_core::log::{self, Level};
use slowshell_core::{Color, Diag, Value};
use slowshell_runtime::Runtime;
use slowshell_ui::layout::ElementIdKey;
use slowshell_ui::style::{Position, Size, Style, Theme};
use slowshell_ui::{Element, Rect};
use slowshell_win::render::{Graphics, Painter, Surface, TextEngine};
use slowshell_win::window::{self, SurfaceHandle, SurfaceRole, WindowEvent};
use slowshell_win::{hit_test, Monitor, Monitors};

/// A panel that opens when the pointer enters it.
///
/// The dock-edge gesture: a thin strip is always on screen, so there is something
/// to hover, and hovering expands the panel to its declared width. A hidden panel
/// cannot do this, because a window nobody can see cannot be hovered.
///
/// The width is animated rather than snapped, and that is the whole feature. A
/// panel that jumps open reads as a different panel arriving; one that grows into
/// place reads as the same panel deciding to show itself.
struct Reveal {
    /// Collapsed width in physical pixels. Never zero: a window with no width
    /// cannot be hovered, so it could never open again.
    collapsed: u32,
    /// Expanded width in physical pixels, decided once when the surface was built.
    expanded: u32,
    /// The width currently on screen.
    ///
    /// Held rather than read back from the window, so "did this change?" is a
    /// comparison against a number instead of a `GetWindowRect`. That is what lets
    /// a tween produce a smooth float while the window only moves on whole pixels.
    width: u32,
    /// Where the width is now, and where it is going.
    tween: slowshell_core::ease::Tween,
    /// When to start closing, set when the pointer leaves.
    ///
    /// Without a delay, moving the pointer diagonally out of a panel that is still
    /// opening closes it mid-flight, and the far side of the user's own menu is
    /// unreachable. This is the fix for that, not a nicety.
    close_at: Option<Instant>,
    /// Seconds to wait before closing.
    delay: f32,
    ease: slowshell_core::ease::Ease,
}

impl Reveal {
    fn new(collapsed: u32, expanded: u32, delay: f32, ease: slowshell_core::ease::Ease) -> Reveal {
        Reveal {
            collapsed,
            expanded,
            width: collapsed,
            // Starts closed, and settled there, so the first frame draws the strip
            // rather than animating open on its own.
            tween: slowshell_core::ease::Tween::settled(collapsed as f32),
            close_at: None,
            delay,
            ease,
        }
    }

    fn with_duration(&mut self, seconds: f32) {
        self.tween.with_duration(seconds);
    }

    fn open_now(&mut self) {
        self.close_at = None;
        self.retarget(self.expanded as f32);
    }

    fn schedule_close(&mut self, now: Instant) {
        self.close_at = Some(now + Duration::from_secs_f32(self.delay));
    }

    fn retarget(&mut self, to: f32) {
        // Retargeting rather than restarting: pulling the pointer away and back
        // must continue from where the panel is, not snap to the strip and
        // re-expand. That snap is the whole difference between a menu that feels
        // alive and one that twitches.
        self.tween.retarget(to, self.ease);
    }

    /// Whether anything is still moving, so the frame loop knows to stay awake.
    fn is_animating(&self) -> bool {
        !self.tween.is_settled() || self.close_at.is_some()
    }

    /// Advance by `dt` seconds at wall-clock `now`, returning the new width when
    /// it changed and `None` when nothing did.
    ///
    /// `now` is passed in rather than read so the whole state machine is
    /// deterministic and testable without a clock, a window, or a pointer — which
    /// matters, because the thing that decides whether a hover menu opens cannot be
    /// tested by hovering it.
    fn advance(&mut self, dt: f32, now: Instant) -> Option<u32> {
        // The delay elapsing is what starts the close, not the leave itself.
        if self.close_at.is_some_and(|at| now >= at) {
            self.close_at = None;
            self.retarget(self.collapsed as f32);
        }
        if !self.tween.advance(dt) {
            return None;
        }
        // A tween produces a smooth float; the window moves in whole pixels. Both
        // halves are needed: rounding here keeps the tween continuous while the
        // window only resizes when a pixel really changed.
        let want = self.tween.value().round().max(1.0) as u32;
        if want == self.width {
            return None;
        }
        self.width = want;
        Some(want)
    }
}

/// One live surface: a window, its swap chain, and the panel tree it draws.
struct SurfaceState {
    handle: SurfaceHandle,
    surface: Surface,
    panel: Element,
    /// The panel's declared `name`, or empty for a bar.
    name: String,
    /// How the panel is anchored and whether it holds screen space.
    ///
    /// Kept so a reload can tell an ordinary edit from one that moves the panel
    /// or changes what it reserves. Comparing the compiled panel instead would
    /// mean re-deriving this every time, and would report "changed" for every
    /// edit that touches a child, making the bar flicker on each keystroke.
    anchor: PanelAnchor,
    /// Present when this panel opens on hover.
    reveal: Option<Reveal>,
    theme: Theme,
    /// The panel's element, kept so a rebuild can replace it wholesale.
    values: HashMap<ElementIdKey, slowshell_ui::Resolved>,
    /// Position and size in physical pixels, for a resize.
    bounds: (i32, i32, u32, u32),
    monitor_scale: f32,
    /// Set when the swap chain must be recreated before the next frame.
    needs_resize: bool,
    /// Set when the tree must be re-laid-out, e.g. after a rebuild.
    needs_layout: bool,
}

impl SurfaceState {
    /// The client size in logical pixels, which is the unit layout works in.
    ///
    /// `bounds` holds physical pixels because that is what Win32 reports, and
    /// the divisor is the monitor's scale rather than the DPI.
    fn logical_size(&self) -> (f32, f32) {
        let scale = if self.monitor_scale > 0.0 { self.monitor_scale } else { 1.0 };
        (self.bounds.2 as f32 / scale, self.bounds.3 as f32 / scale)
    }

    /// The DPI the surface is drawn at, which the text engine's cache keys on.
    fn surface_dpi(&self) -> f32 {
        96.0 * if self.monitor_scale > 0.0 { self.monitor_scale } else { 1.0 }
    }
}

struct App {
    runtime: Runtime,
    graphics: Rc<Graphics>,
    text: TextEngine,
    surfaces: Vec<SurfaceState>,
    monitors: Monitors,
    config_path: PathBuf,
    /// Files the last build read, watched for changes.
    watched: Vec<PathBuf>,
    /// Last modification time per file, so a touch that changes nothing is free.
    stamps: HashMap<PathBuf, std::time::SystemTime>,
    /// The next time the clock's displayed value could change.
    next_clock: Instant,
    /// Set when a config file changed on disk.
    reload_requested: bool,
    running: bool,
    /// Rotating counter for the log's frame statistics.
    frames: u64,
    last_fps_report: Instant,
    /// When the last frame was drawn, so animations step by real elapsed time.
    last_frame: Instant,
    /// Software rendering changes the frame budget, so it is reported once.
    reported_renderer: bool,
    /// The control pipe, absent only if the name was taken by something else.
    ipc: Option<slowshell_win::ipc_pipe::Server>,
    /// The current media session, watched on a thread of its own.
    ///
    /// Read once per tick and compared, so a track that is not changing costs one
    /// mutex read. The watcher never blocks the frame loop: it is the thing that
    /// waits on SMTC, not us.
    media: Arc<slowshell_win::MediaState>,
    /// Diagnostics from the last build, so `shellctl diagnostics` can report them.
    last_diagnostics: Vec<Diag>,
    /// Set by `shellctl overlay`.
    overlay: bool,
    /// When the process started, for `shellctl status`.
    started: Instant,
    /// Actions that need the frame loop, queued by their registry closures.
    deferred: Deferred,
}

impl App {
    fn new(config_path: PathBuf) -> std::io::Result<App> {
        let mut runtime = Runtime::new();
        runtime.seed_sources();
        let deferred: Deferred = Rc::new(RefCell::new(Vec::new()));
        register_actions(&mut runtime, deferred.clone());
        let graphics = Rc::new(Graphics::new().map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("Could not create the graphics device: {e}"),
            )
        })?);
        // The device is built after the presentation probe, so a rejected
        // configuration cannot leave the real device unusable.
        graphics.create().map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::Other, format!("graphics device: {e}"))
        })?;
        let text = TextEngine::new(graphics.write_factory());
        let monitors = Monitors::enumerate();
        // A shell that cannot be controlled still runs. Losing the pipe costs
        // `shellctl`, not the desktop, so a failure here is a warning.
        let ipc = match slowshell_win::ipc_pipe::Server::start() {
            Ok(s) => {
                slowshell_core::info!("control pipe: {}", s.name());
                Some(s)
            }
            Err(e) => {
                slowshell_core::warn!("shellctl will not work: {e}");
                None
            }
        };
        Ok(App {
            runtime,
            graphics,
            text,
            surfaces: Vec::new(),
            monitors,
            config_path,
            watched: Vec::new(),
            stamps: HashMap::new(),
            next_clock: Instant::now(),
            reload_requested: false,
            running: true,
            frames: 0,
            last_fps_report: Instant::now(),
            last_frame: Instant::now(),
            reported_renderer: false,
            ipc,
            media: slowshell_win::media::watch(),
            last_diagnostics: Vec::new(),
            overlay: false,
            started: Instant::now(),
            deferred,
        })
    }

    /// Load the config and create a surface per panel.
    fn build(&mut self) {
        slowshell_core::info!("building: {}", self.config_path.display());
        let outcome = self.runtime.build(&self.config_path);
        // Kept so `shellctl diagnostics` can report the current state rather than
        // whatever happened at launch.
        self.last_diagnostics = outcome.diagnostics.clone();
        for d in &outcome.diagnostics {
            match d.severity {
                slowshell_core::Severity::Error | slowshell_core::Severity::Fatal => {
                    slowshell_core::error!("{}", d.render().trim_end())
                }
                slowshell_core::Severity::Warning => {
                    slowshell_core::warn!("{}", d.render().trim_end())
                }
                _ => slowshell_core::debug!("{}", d.render().trim_end()),
            }
        }
        if outcome.compiled.is_none() {
            slowshell_core::error!("build failed: {}", outcome.summary());
            // The previous surfaces keep running, so a bad edit never blanks the
            // desktop. This is the whole point of hot reload.
            return;
        }
        let summary = outcome.summary();
        let compiled = outcome.compiled.expect("checked above");
        slowshell_core::info!("{summary}");
        self.watched = outcome.files.clone();
        self.stamps = self.watched.iter().map(|p| (p.clone(), stamp(p))).collect();

        // Rebuild the surfaces. Windows are recreated only when the count or the
        // geometry changes, so an ordinary edit does not make the bar flicker.
        //
        // Hidden panels are skipped: they are compiled, and `shell.open` can
        // build them on demand, but they get no window until something asks.
        // A launcher that is present but invisible from the first frame is not
        // hidden, it is broken.
        let panels: Vec<&Element> =
            compiled.panels.iter().filter(|p| !panel_is_hidden(p)).collect();
        if panels.is_empty() {
            slowshell_core::warn!("every panel in this config is hidden; nothing to show");
        }
        // Expand `wrap: true` into the panel plus its companion arms, so the rest
        // of the rebuild keeps treating surfaces as a flat list with no knowledge
        // of wrapping. Expansion happens here rather than in the compiler because
        // the arm geometry needs the monitor's work area, which the compiler does
        // not read and should not.
        let panels: Vec<Element> = self.expand_wrap(&panels, &compiled.theme);
        self.surfaces.retain(|s| {
            s.handle.is_valid() && s.bounds.0 != i32::MIN
        });
        // Drop surfaces beyond the new panel count.
        while self.surfaces.len() > panels.len() {
            if let Some(s) = self.surfaces.pop() {
                window::destroy(s.handle);
            }
        }
        for (i, panel) in panels.iter().enumerate() {
            if i < self.surfaces.len() {
                // A panel has to become a new surface — not be patched in place —
                // when anything that decides *where it sits* or *what it reserves*
                // changes: its declared name, the edge it is anchored to, the
                // display it is on, or whether it holds screen space.
                //
                // `exclusive` is the one that bites if it is left out. Patching in
                // place keeps the window, so it keeps its app bar registration:
                // turning `exclusive` off leaves the space reserved with nothing on
                // screen holding it, and turning it on never registers at all. Both
                // fail silently, and the first one holds a strip of the desktop
                // until the shell is killed.
                let existing = &self.surfaces[i];
                let anchored_changed = existing.name != panel_name(panel)
                    || existing.anchor != panel_anchor(panel);
                if anchored_changed {
                    let s = self.surfaces.remove(i);
                    // Releasing the old window first also releases its app bar, so
                    // the space comes back before the new one claims any.
                    window::destroy(s.handle);
                    match self.create_surface(panel, &compiled.theme) {
                        Ok(s) => self.surfaces.insert(i, s),
                        Err(e) => slowshell_core::error!("could not create surface {i}: {e}"),
                    }
                    continue;
                }
                self.surfaces[i].panel = panel.clone();
                self.surfaces[i].theme = compiled.theme.clone();
                self.surfaces[i].needs_layout = true;
                self.surfaces[i].needs_resize = true;
                continue;
            }
            match self.create_surface(panel, &compiled.theme) {
                Ok(s) => self.surfaces.push(s),
                Err(e) => slowshell_core::error!("could not create surface {i}: {e}"),
            }
        }
        // Surfaces may have been created or dropped, so the IPC thread needs the
        // new window list before the frame loop blocks on a stale one.
        self.sync_windows();
    }

    /// Expand `wrap: true` into the panel plus a thin arm on each other edge.
    ///
    /// `wrap` is one line in a config and four windows on screen. That is not an
    /// implementation detail leaking out: a single full-screen window would be the
    /// obvious way to draw a frame around the desktop, and it cannot work here,
    /// because the presentation path rejects per-pixel alpha and the window would
    /// be an opaque rectangle over everything. Four edge-anchored windows produce
    /// the same picture, each reserves its own strip through the normal app bar
    /// path, and each is destroyed by the same reload logic as any other panel.
    ///
    /// The arms carry explicit pixel bounds rather than a `position`, because they
    /// are not anchored to an edge — each is the remainder between two edges. The
    /// bar occupies one edge and the arms fill the other three without overlapping
    /// it, so the four together make a closed frame and the work area is inset on
    /// all four sides.
    fn expand_wrap(&self, panels: &[&Element], theme: &Theme) -> Vec<Element> {
        let mut out: Vec<Element> = Vec::with_capacity(panels.len());
        for panel in panels {
            let (position, screen, wrap, wrap_size) = match &panel.kind {
                slowshell_ui::ElementKind::Panel {
                    position,
                    screen,
                    wrap,
                    wrap_size,
                    ..
                } => (*position, screen.clone(), *wrap, *wrap_size),
                _ => (Position::Top, String::new(), false, None),
            };
            // A floating panel has no edge to continue from, and a hidden one has
            // no window to continue around.
            if wrap && position != Position::Floating && !panel_is_hidden(panel) {
                let monitor = self
                    .monitors
                    .get(&screen)
                    .cloned()
                    .unwrap_or_else(|| self.monitors.primary().clone());
                out.extend(wrap_arms(panel, &monitor, wrap_size, theme));
                slowshell_core::info!(
                    "wrap: {position:?} on {} framed with {} arms",
                    monitor.id,
                    3
                );
            }
            out.push((*panel).clone());
        }
        out
    }

    /// Default thickness of a `wrap` arm, in logical pixels.
/// Work out where a panel goes and create its window.
    fn create_surface(&mut self, panel: &Element, theme: &Theme) -> std::io::Result<SurfaceState> {
        let (position, screen, name, hidden, backdrop, exclusive) = match &panel.kind {
            slowshell_ui::ElementKind::Panel {
                position, screen, name, hidden, backdrop, exclusive, ..
            } => (*position, screen.clone(), name.clone(), *hidden, *backdrop, *exclusive),
            _ => (
                Position::Top,
                "primary".to_string(),
                String::new(),
                false,
                slowshell_win::Backdrop::None,
                false,
            ),
        };
        let monitor = self
            .monitors
            .get(&screen)
            .cloned()
            .unwrap_or_else(|| self.monitors.primary().clone());
        let scale = monitor.scale;

        // Size the panel from its content, in logical pixels, then convert.
        // Real font metrics, because this is what decides a floating panel's
        // width and a vertical panel's height.
        let (content_w, content_h) = {
            let mut measurer = slowshell_ui::layout::TextMeasurer::new(&mut self.text, 96.0 * scale);
            slowshell_ui::panel_intrinsic(panel, monitor.logical_width(), &mut measurer)
        };
        let (declared_w, declared_h) = declared_size(panel);
        let horizontal = !position.is_vertical();

        // An exclusive bar is measured against the **work area**, not the screen.
        //
        // The work area is what Windows reports as usable once every app bar —
        // the real taskbar included — has taken its strip. Anchoring to it is
        // what makes two bars stack instead of overlap: with Explorer's taskbar
        // at the bottom, a bottom bar placed against the screen edge is drawn
        // straight through it.
        //
        // The monitor list is re-read here rather than reused, because the whole
        // point is to react to space something else has already claimed, and the
        // cached list predates every reservation this shell has made. Two
        // exclusive bars on one edge therefore land one above the other.
        let placement_monitor = if exclusive {
            Monitors::enumerate()
                .get(&screen)
                .cloned()
                .unwrap_or_else(|| monitor.clone())
        } else {
            monitor.clone()
        };

        // An edge-anchored panel spans the display unless the config says otherwise:
        // a top bar that does not reach the screen edge is not a top bar.
        //
        // An *exclusive* one spans the work area instead, so a bar that shares an
        // edge with the real taskbar stops where the taskbar begins instead of
        // being painted through it.
        let span_w = if exclusive {
            placement_monitor.work_width as f32 / placement_monitor.scale
        } else {
            monitor.width as f32
        };

        // A revealing side panel spans the display the way a bar spans its width.
        // Without this it sizes to its content, which makes the thing you hover a
        // short stub in the middle of the edge instead of a strip you can find.
        let reveals = matches!(&panel.kind,
            slowshell_ui::ElementKind::Panel { reveal, .. } if *reveal);
        let tall = reveals && position.is_vertical();
        let span_h = placement_monitor.work_height as f32 / placement_monitor.scale;

        let lw = match declared_w {
            Some(v) => v,
            None if horizontal && position != Position::Floating => span_w,
            None => content_w,
        };
        let lh = match declared_h {
            Some(v) => v,
            None if tall => span_h,
            None if horizontal && position != Position::Floating => content_h.max(24.0),
            None => content_h,
        };

        let w = monitor.physical(lw).max(1) as u32;
        let h = monitor.physical(lh).max(1) as u32;
        // The width the panel will have when open, kept before the reveal narrows
        // it below. Reading it from `w` afterwards gives the collapsed width, and
        // the panel then expands to exactly what it started at — which looks like
        // hover does nothing at all.
        let open_w = w;

        // A revealing panel starts as a thin strip on its edge. The window is
        // created at that width rather than being created wide and hidden, because
        // a window with no width cannot be hovered — so a panel that opened from
        // nothing could never be reopened.
        let reveal_spec = match &panel.kind {
            slowshell_ui::ElementKind::Panel {
                reveal,
                reveal_size,
                reveal_duration,
                reveal_delay,
                reveal_ease,
                ..
            } => (*reveal, *reveal_size, *reveal_duration, *reveal_delay, *reveal_ease),
            _ => (false, 8.0, 0.18, 0.26, slowshell_core::ease::Ease::OutCubic),
        };
        // Only a side edge can reveal this way. On `top` or `bottom` the panel's
        // width is the display's, and collapsing it would leave a full-width gap
        // in the middle of the desktop rather than a strip at the edge.
        let revealable = reveal_spec.0 && position.is_vertical();
        let collapsed_w = if revealable {
            monitor.physical(reveal_spec.1.max(1.0)).max(2) as u32
        } else {
            w
        };
        let collapsed_w = collapsed_w.min(w);

        // A wrap arm arrives with its rectangle already worked out: it is the
        // space *between* two edges rather than an edge strip, so no combination
        // of `position` and `exclusive` describes where it goes. Anything without
        // explicit bounds is placed the normal way.
        let explicit = match &panel.kind {
            slowshell_ui::ElementKind::Panel { bounds, .. } => *bounds,
            _ => None,
        };
        let (x, y, w, h) = match explicit {
            Some((bx, by, bw, bh)) => (bx, by, bw, bh),
            None => {
                // Placed at its full width so the expanded edge is right, then
                // narrowed to the strip below. The reveal grows it back towards
                // the width it was placed for.
                let (x, y) = if exclusive {
                    position_exclusive(position, &placement_monitor, w, h)
                } else {
                    position_of(position, &monitor, w, h)
                };
                if revealable {
                    // A left edge grows rightwards, so the strip stays at `x`. A
                    // right edge grows leftwards, so its strip moves to the edge.
                    let x = if position == Position::Right {
                        x + (w - collapsed_w) as i32
                    } else {
                        x
                    };
                    (x, y, collapsed_w, h)
                } else {
                    (x, y, w, h)
                }
            }
        };
        // An arm is decoration. It has no children and therefore no hit regions,
        // so a click on it has to fall through to whatever is underneath rather
        // than landing on a window that swallows it.
        let arm = explicit.is_some();
        slowshell_core::info!(
            "panel {position:?} on {} -> {x},{y} {w}x{h} @ {scale}{}{}",
            monitor.id,
            if arm { " (wrap arm)" } else { "" },
            if revealable {
                format!(" (reveals from {collapsed_w}px)")
            } else {
                String::new()
            }
        );

        let handle = window::create_surface(SurfaceRole::Panel, x, y, w, h, "Slowshell")?;
        // The window must be visible before a render target is attached to it:
        // DXGI rejects a chain whose window has never been shown, and a window
        // render target presents nothing, both with a bare error code.
        if !hidden {
            window::show(handle.hwnd);
        }

        // A revealing panel must be told where the pointer is even while collapsed
        // and empty, or it is transparent to the pointer and can never be opened.
        if revealable {
            window::set_hover_capture(handle.hwnd, true);
        }

        // `exclusive: true` reserves the panel's strip of screen, so maximised
        // windows stop at it instead of covering it.
        //
        // Registered immediately after the window is shown and *before* the
        // graphics device is touched. Order matters here: a window registered as
        // an app bar is tracked by the OS from that moment, and doing this after
        // the swap chain exists makes the reservation silently do nothing —
        // `ABM_SETPOS` still returns success, and the work area never changes,
        // which is the most expensive possible way for a feature to be broken.
        //
        // A floating panel is an overlay, not a bar, so it reserves nothing.
        if exclusive {
            let edge = match position {
                Position::Top => slowshell_win::Edge::Top,
                Position::Bottom => slowshell_win::Edge::Bottom,
                Position::Left => slowshell_win::Edge::Left,
                Position::Right => slowshell_win::Edge::Right,
                Position::Floating => slowshell_win::Edge::None,
            };
            if window::register_appbar(handle.hwnd, edge) {
                window::reserve_appbar(handle.hwnd);
                slowshell_core::info!("exclusive: reserving {w}x{h} on the {edge:?} edge");
            }
        }

        let surface = self.graphics.create_surface(handle.hwnd, w, h).map_err(|e| {
            window::destroy(handle);
            std::io::Error::new(std::io::ErrorKind::Other, format!("render target: {e}"))
        })?;
        window::set_always_on_top(handle.hwnd, true);
        // The DWM material is only meaningful on a path that presents per-pixel
        // alpha: on a window render target it would draw the blur over a solid
        // bar, which looks like a rendering fault rather than a material.
        let backdrop = if surface.honours_alpha() { backdrop } else { slowshell_win::Backdrop::None };
        slowshell_win::backdrop::apply(handle.hwnd, backdrop, theme.dark);

        let values = slowshell_ui::resolve_values(panel, &self.runtime.reactor);
        slowshell_core::info!(
            "surface: {}{} (alpha {:?})",
            surface.presentation().label(),
            if hidden { ", hidden" } else { "" },
            surface.alpha_mode()
        );
        Ok(SurfaceState {
            handle,
            surface,
            panel: panel.clone(),
            name,
            anchor: panel_anchor(panel),
            reveal: if revealable {
                let expanded = match explicit {
                    Some((_, _, bw, _)) => bw,
                    None => open_w.max(collapsed_w),
                };
                slowshell_core::debug!("reveal: {expanded}px wide when open, {collapsed_w}px closed");
                let mut r = Reveal::new(collapsed_w, expanded, reveal_spec.3, reveal_spec.4);
                // Applied here rather than read per frame, so the configured
                // duration is used and cannot quietly stop being one.
                r.with_duration(reveal_spec.2);
                Some(r)
            } else {
                None
            },
            theme: theme.clone(),
            values,
            bounds: (x, y, w, h),
            monitor_scale: scale,
            needs_resize: false,
            needs_layout: true,
        })
    }

    /// Open or close a revealing panel, or do nothing if it is not one.
    fn set_reveal_open(&mut self, hwnd: isize, open: bool) {
        let Some(s) = self.surfaces.iter_mut().find(|s| s.handle.id() == hwnd) else {
            return;
        };
        let Some(reveal) = s.reveal.as_mut() else {
            return;
        };
        if open {
            reveal.open_now();
        } else {
            reveal.schedule_close(Instant::now());
        }
        s.needs_layout = true;
    }

    /// Advance every running reveal by `dt` seconds, and report whether anything
    /// still moving, so the frame loop knows to stay awake.
    ///
    /// A settled reveal costs one branch per surface and the loop goes straight
    /// back to blocking in the kernel, which is the only reason animations can
    /// exist without spending the idle budget.
    fn step_reveals(&mut self, dt: f32) -> bool {
        let now = Instant::now();
        let mut moving = false;
        for s in &mut self.surfaces {
            let Some(reveal) = s.reveal.as_mut() else {
                continue;
            };
            if reveal.is_animating() {
                moving = true;
            }
            if let Some(want) = reveal.advance(dt, now) {
                s.bounds.2 = want;
                let (x, y, _, h) = s.bounds;
                window::set_bounds(s.handle.hwnd, x, y, want, h);
                s.needs_resize = true;
                s.needs_layout = true;
            }
        }
        moving
    }

    /// Publish the values the UI reads, and report whether anything changed.
    fn publish_system(&mut self) -> bool {
        let mut changed = false;

        // Media first, and only when the watcher says something moved. `take`
        // consumes the change flag, so the common case — a track that is not
        // changing — is one branch and no string work at all. This is the whole
        // reason the watcher owns a thread: SMTC is a COM call with a latency
        // budget measured in seconds, and the frame loop has a budget measured
        // in milliseconds.
        if let Some(m) = self.media.take() {
            let playing = m.is_playing();
            changed |= self.runtime.reactor.set("media.title", Value::str(m.title));
            changed |= self.runtime.reactor.set("media.artist", Value::str(m.artist));
            changed |= self.runtime.reactor.set("media.status", Value::str(m.status));
            changed |= self.runtime.reactor.set("media.app", Value::str(m.app));
            changed |= self.runtime.reactor.set("media.playing", Value::Bool(playing));
        }

        let now = Instant::now();
        if now >= self.next_clock {
            // `GetLocalTime` is the authoritative local clock, so the timestamp the
            // UI receives round-trips to the wall time the user sees.
            let (y, mo, d, h, mi, s) = slowshell_win::local_civil();
            let local = slowshell_ui::civil_to_unix(y, mo, d, h, mi, s);
            let stamp = format!("{h:02}:{mi:02}");
            let date = format!("{d:02}/{mo:02}/{y:04}");
            // `Reactor::set` is a no-op when the value is unchanged, so a second
            // in which nothing changed costs nothing downstream.
            changed |= self.runtime.reactor.set("clock.unix", Value::Int(local));
            changed |= self.runtime.reactor.set("clock.time", Value::str(stamp));
            changed |= self.runtime.reactor.set("clock.date", Value::str(date));
            // Wake for the next wall-clock second.
            let until = (1000u64 - (s as u64 * 1000 % 1000)).max(50);
            self.next_clock = now + Duration::from_millis(until);

            // The foreground window, on the same one-second cadence. Not on a
            // separate timer and not per frame: a window title changes when the
            // user changes windows, which is a human-speed event, and reading it
            // every frame would be the most expensive thing in the shell for the
            // least benefit.
            //
            // `reactor.set` no-ops when the value is unchanged, so a title that has
            // been sitting there for an hour costs one comparison.
            let title = slowshell_win::platform::active_window_title();
            changed |= self
                .runtime
                .reactor
                .set("windows.active.title", Value::str(title));
        }

        // A stale derived value is recomputed lazily by the next layout pass, so
        // the notification is consumed here. Left set, it would report "changed"
        // for the rest of the session and the shell would redraw as fast as the
        // machine allows — which is precisely what the idle budget exists to
        // prevent, and it is invisible: the bar looks identical either way.
        if self.runtime.reactor.has_pending() {
            self.runtime.reactor.take_pending();
            for s in &mut self.surfaces {
                s.needs_layout = true;
            }
            changed = true;
        }
        changed
    }

    /// Lay out, paint and present one surface.
    fn draw(&mut self, i: usize) {
        let dpi = self.surfaces[i].surface_dpi();
        if self.surfaces[i].needs_resize {
            if self.surfaces[i].surface.resize().is_ok() {
                self.surfaces[i].needs_resize = false;
            }
        }
        // Reactive values are re-resolved every frame. The reactor is
        // pull-based, so this is a walk of the tree with no dependency
        // bookkeeping — the cost is proportional to the number of elements, not
        // to the size of the graph. The layout pass is gated on the result,
        // because a value that did not change cannot move a box, and a clock that
        // did change has to.
        let values = slowshell_ui::resolve_values(&self.surfaces[i].panel, &self.runtime.reactor);
        if values != self.surfaces[i].values || self.surfaces[i].needs_layout {
            let (lw, lh) = self.surfaces[i].logical_size();
            let panel = &self.surfaces[i].panel;
            let theme = &self.surfaces[i].theme;
            // Real font metrics, not the pre-surface approximation: a label
            // measured a pixel narrow comes out trimmed on screen.
            let mut measurer = slowshell_ui::layout::TextMeasurer::new(&mut self.text, dpi);
            let bounds = Rect::new(0.0, 0.0, lw, lh);
            slowshell_ui::layout::layout(panel, bounds, theme, &mut measurer, &values);
            slowshell_core::debug!("layout of surface {i}:\n{}", panel.debug_tree());
            self.surfaces[i].values = values;
            self.surfaces[i].needs_layout = false;
        }

        let s = &mut self.surfaces[i];
        // When the presentation path cannot carry per-pixel alpha, the frame is
        // cleared to an opaque backdrop and every translucent colour is
        // flattened against the same one, so a bar still looks like a bar rather
        // than a black rectangle.
        let backdrop = if s.surface.honours_alpha() {
            None
        } else {
            s.theme.get("background").map(|c| c.with_opacity(1.0))
        };
        s.surface.begin(dpi, backdrop);
        let mut painter = Painter::new(s.surface.target(), &mut self.text, dpi);
        if let Some(bg) = backdrop {
            painter.set_composite_backdrop(bg);
        }
        slowshell_ui::paint(&mut painter, &s.panel, &s.theme, &s.values, &Style::default());
        let output = slowshell_ui::take_output();
        slowshell_core::debug!(
            "surface {i}: {} drawn, {} skipped, {} hit regions",
            output.drawn,
            output.skipped,
            output.regions.len()
        );
        // Publish the interactive regions so `WM_NCHITTEST` can pass clicks
        // through the empty parts of the bar.
        hit_test::set_regions(output.regions);
        let balanced = painter.is_balanced();
        let stats = painter.stats();
        drop(painter);
        s.surface.present();
        if !balanced {
            slowshell_core::warn!("surface {i} left a layer or clip unbalanced");
        }
        if stats.is_empty() {
            slowshell_core::warn!("surface {i} drew nothing this frame");
        } else if stats.texts_dropped > 0 {
            slowshell_core::warn!(
                "surface {i} dropped {} of {} text runs",
                stats.texts_dropped,
                stats.texts
            );
        }
    }

    fn frame(&mut self) {
        for i in 0..self.surfaces.len() {
            self.draw(i);
        }
        self.frames += 1;
        if self.last_fps_report.elapsed() >= Duration::from_secs(5) {
            let fps = self.frames as f32 / self.last_fps_report.elapsed().as_secs_f32();
            slowshell_core::info!("{fps:.1} fps, {} surfaces", self.surfaces.len());
            self.frames = 0;
            self.last_fps_report = Instant::now();
        }
    }

    fn handle_event(&mut self, hwnd: isize, ev: WindowEvent) {
        // A tick every frame would drown the log; anything else is rare enough
        // to be worth a line when something is not behaving.
        if !matches!(ev, WindowEvent::Tick) {
            slowshell_core::debug!("event: {}", ev.render());
        }
        match ev {
            WindowEvent::Tick => {
                // A `Tick` is the shell's "something about the display may have
                // changed" signal, so it is where a reserved bar re-negotiates its
                // space. Idempotent, so the common case costs one rect read.
                window::reserve_appbar_by_id(hwnd);
            }
            WindowEvent::Resized { width, height, scale } => {
                if let Some(s) = self.surfaces.iter_mut().find(|s| s.handle.id() == hwnd) {
                    s.bounds.2 = width;
                    s.bounds.3 = height;
                    s.monitor_scale = scale;
                    s.needs_resize = true;
                    s.needs_layout = true;
                }
                // The bar just got taller or shorter, so the space it holds
                // against the edge is stale. Re-reserving is what keeps
                // maximised windows from sliding under the new height.
                window::reserve_appbar_by_id(hwnd);
            }
            WindowEvent::DpiChanged { scale } => {
                if let Some(s) = self.surfaces.iter_mut().find(|s| s.handle.id() == hwnd) {
                    s.monitor_scale = scale;
                    s.needs_layout = true;
                }
                window::reserve_appbar_by_id(hwnd);
            }
            WindowEvent::CloseRequested => self.running = false,
            WindowEvent::Pointer(slowshell_win::InputEvent::Moved { .. }) => {
                self.set_reveal_open(hwnd, true)
            }
            WindowEvent::Pointer(slowshell_win::InputEvent::Left) => {
                // Leaving does not close immediately: the delay exists so a pointer
                // crossing the panel diagonally does not shut it on the way to the
                // far side of the user's own menu.
                self.set_reveal_open(hwnd, false)
            }
            WindowEvent::Pointer(slowshell_win::InputEvent::Pressed { x, y, button, .. }) => {
                if button == 0 {
                    if let Some(id) = hit_test::region_at(x, y) {
                        slowshell_core::debug!("click on element {id} at {x},{y}");
                        self.dispatch(id);
                    }
                }
            }
            _ => {}
        }
    }

    /// Run the handler bound to an element.
    ///
    /// The registry is consulted for real: an action that cannot be found is a
    /// warning with the reason, not a shrug. Handler targets are also checked at
    /// build time, so reaching here with a bad name means a handler was written
    /// after the last build, which is worth saying out loud.
    fn dispatch(&mut self, id: u32) {
        let Some(path) = self.surfaces.iter().find_map(|s| {
            let e = s.panel.find(slowshell_ui::ElementId(id))?;
            let handlers = e.handlers.borrow();
            handlers
                .iter()
                .find(|(name, _)| name == "onClick")
                .map(|(_, h)| h.clone())
        }) else {
            return;
        };
        let segments: Vec<String> = path.action.split('.').map(str::to_string).collect();
        slowshell_core::info!("running `{}`", path.action);
        match self.runtime.actions.call(&segments, &path.args) {
            Ok(v) => slowshell_core::debug!("`{}` -> {}", path.action, v.to_string_lossy()),
            Err(e) => slowshell_core::warn!("`{}`: {e}", path.action),
        }
        self.run_deferred();
    }

    /// Carry out whatever the action queue asked for.
    ///
    /// Drained here rather than inside the action, because an action that
    /// rebuilt a surface while the paint pass was walking it would be a use
    /// after free rather than a feature.
    fn run_deferred(&mut self) {
        let queued: Vec<(String, Vec<Value>)> = self.deferred.borrow_mut().drain(..).collect();
        for (path, args) in queued {
            let arg0 = args.first().map(|v| v.to_string_lossy()).unwrap_or_default();
            match path.as_str() {
                "shell.reload" => self.reload_requested = true,
                "shell.overlay" => {
                    self.overlay = !self.overlay;
                    for s in &mut self.surfaces {
                        s.needs_layout = true;
                    }
                }
                "shell.stop" => self.running = false,
                "shell.open" => {
                    if let Err(e) = self.open_surface(&arg0) {
                        slowshell_core::warn!("`shell.open({arg0})`: {e}");
                    }
                }
                other => slowshell_core::warn!("`{other}` was queued but nothing can carry it out"),
            }
        }
    }

    /// Whether a named surface could be opened, before anything is queued.
    fn can_open(&self, name: &str) -> bool {
        !name.is_empty()
            && (self.surfaces.iter().any(|s| s.name == name)
                || self.runtime.hidden_panel(name).is_some())
    }

    /// Show a named extra surface, if the config declares one.
    ///
    /// A launcher is a second `Panel` the config keeps hidden until asked for,
    /// not a special case in the renderer: one more panel in the config is one
    /// more window, and the renderer does not learn a new concept for it.
    fn open_surface(&mut self, name: &str) -> Result<(), String> {
        if let Some(index) = self.surfaces.iter().position(|s| s.name == name) {
            // Already open: bring it forward rather than stacking a second copy.
            let state = &mut self.surfaces[index];
            window::show(state.handle.hwnd);
            window::set_always_on_top(state.handle.hwnd, true);
            state.needs_layout = true;
            return Ok(());
        }
        let Some(panel) = self.runtime.hidden_panel(name) else {
            return Err(format!(
                "no panel named `{name}` is declared. Add `name: \"{name}\" hidden: true` to a Panel."
            ));
        };
        let mut state =
            self.create_surface(&panel, &self.runtime.theme()).map_err(|e| e.to_string())?;
        // `create_surface` honours the panel's `hidden: true` and leaves the
        // window unshown, which is right for a panel nobody has asked for and
        // wrong for this one. The caller asking for it *is* the ask.
        window::show(state.handle.hwnd);
        state.needs_layout = true;
        self.surfaces.push(state);
        // Surfaces changed, so the IPC thread needs the new window list.
        self.sync_windows();
        slowshell_core::info!("opened surface `{name}`");
        Ok(())
    }

    /// Answer one `shellctl` request.
    fn handle_request(&mut self, req: Request) -> Response {
        match req {
            Request::Status => Response::ok(serde_json::json!({
                "version": slowshell_core::VERSION,
                "pid": std::process::id(),
                "config": self.config_path.display().to_string(),
                "panels": self.surfaces.len(),
                "elements": self.surfaces.iter().map(|s| s.panel.count()).sum::<usize>(),
                "adapter": self.graphics.adapter_name(),
                "presentation": self.graphics.presentation().label(),
                "uptimeSeconds": (self.started.elapsed().as_millis() / 1000) as u64,
                "watching": self.watched.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
                "overlay": self.overlay,
                "actions": self.runtime.actions.paths(),
            })),
            Request::Reload => {
                self.reload_requested = true;
                Response::ok(serde_json::json!({ "reloading": true }))
            }
            Request::Stop => {
                self.running = false;
                Response::ok(serde_json::json!({ "stopping": true }))
            }
            Request::ToggleOverlay => {
                self.overlay = !self.overlay;
                for s in &mut self.surfaces {
                    s.needs_layout = true;
                }
                Response::ok(serde_json::json!({ "overlay": self.overlay }))
            }
            Request::Logs { limit, level } => {
                let wanted = Level::parse(&level).unwrap_or(Level::Info);
                let records: Vec<serde_json::Value> = slowshell_core::log::recent(limit, wanted)
                    .into_iter()
                    .map(|r| {
                        serde_json::json!({
                            "level": r.level.label(),
                            "target": r.target,
                            "message": r.message,
                            "tsMs": r.ts_ms,
                        })
                    })
                    .collect();
                Response::ok(serde_json::json!({ "records": records }))
            }
            Request::Diagnostics => {
                let diags: Vec<serde_json::Value> = self
                    .last_diagnostics
                    .iter()
                    .map(|d| {
                        serde_json::json!({
                            "severity": format!("{:?}", d.severity).to_lowercase(),
                            "kind": format!("{:?}", d.kind),
                            "message": d.message,
                            "file": d.span.as_ref().map(|s| s.file.to_string()),
                            "line": d.span.as_ref().map(|s| s.start.line),
                            "column": d.span.as_ref().map(|s| s.start.col),
                            "hints": d.hints,
                            "notes": d.notes,
                        })
                    })
                    .collect();
                Response::ok(serde_json::json!({ "count": diags.len(), "diagnostics": diags }))
            }
            Request::Graph => {
                let stats = self.runtime.reactor.stats();
                Response::ok(serde_json::json!({
                    "nodes": stats.nodes,
                    "sources": stats.sources,
                    "derived": stats.derived,
                    "edges": stats.edges,
                    "recomputes": stats.recomputes,
                    "pending": stats.pending,
                }))
            }
            Request::Screens => {
                // Enumerated *here*, not read from the cache the panels were
                // placed with.
                //
                // The work area is the one field here that changes without the
                // display changing: registering an exclusive bar resizes it, and
                // so does the user toggling the real taskbar. Reporting the
                // placement-time snapshot instead makes `shellctl screens` deny
                // that the shell's own bar is holding any space, which is the
                // worst possible answer to the question the command exists to
                // answer. A monitor query is cheap and answers are not cached
                // across it.
                let screens: Vec<serde_json::Value> = Monitors::enumerate()
                    .list
                    .iter()
                    .map(|m| {
                        serde_json::json!({
                            "id": m.id,
                            "primary": m.primary,
                            "x": m.x, "y": m.y,
                            "width": m.width, "height": m.height,
                            "workX": m.work_x, "workY": m.work_y,
                            "workWidth": m.work_width, "workHeight": m.work_height,
                            "scale": m.scale,
                            "refreshHz": m.refresh_hz,
                            "rotation": m.rotation,
                        })
                    })
                    .collect();
                // What this shell itself has reserved, which is exact. The OS's
                // own bar list is a lower bound, so the authoritative answer to
                // "is space reserved" is the work area above.
                let mine: Vec<serde_json::Value> = self
                    .surfaces
                    .iter()
                    .filter_map(|s| {
                        window::appbar_bounds(s.handle.hwnd).map(|(l, t, r, b)| {
                            // An unnamed panel is still a panel the user wrote, so
                            // it gets a label from its role rather than an empty
                            // one — "panel" against "launcher" is the difference
                            // between which window is holding the space.
                            let label = if s.name.is_empty() {
                                format!(
                                    "{:?}",
                                    window::role_of(s.handle.hwnd)
                                        .unwrap_or(slowshell_win::SurfaceRole::Panel)
                                )
                                .to_lowercase()
                            } else {
                                s.name.clone()
                            };
                            // `{:?}` on an `Option<Edge>` would print `Some(Top)`,
                            // which reads like a type rather than a setting.
                            let edge = window::appbar_rect(s.handle.hwnd)
                                .map(|e| e.name())
                                .unwrap_or("none");
                            serde_json::json!({
                                "panel": label,
                                "edge": edge,
                                "x": l, "y": t, "width": r - l, "height": b - t,
                                // Not auto-hide: nothing slides the window. It
                                // says the user asked for a bar that gets out of
                                // the way and the shell has not implemented it,
                                // which is worth saying rather than leaving them
                                // to wonder.
                                "autoHideRequested": window::appbar_auto_hidden(s.handle.hwnd),
                            })
                        })
                    })
                    .collect();
                Response::ok(serde_json::json!({ "screens": screens, "reserved": mine }))
            }
            Request::Open { name } => {
                // Checked here rather than only in the action, because this reply
                // is what the user reads. An action that queues work and reports
                // success would make `shellctl open nope` claim it opened a
                // panel that does not exist, and the failure would only ever
                // appear in a log nobody is reading.
                if !self.can_open(&name) {
                    return Response::err(format!(
                        "no panel named `{name}` is declared. Add `name: \"{name}\" hidden: true` \
                         to a Panel, or check the name."
                    ));
                }
                // `shell.open` takes the name as an argument, so the path is
                // `shell.open` and `name` rides along. Building the path as
                // `shell.open.launcher` would look for an action that does not
                // exist, which the registry would correctly refuse.
                let segments = vec!["shell".to_string(), "open".to_string()];
                match self.runtime.actions.call(&segments, &[Value::str(&name)]) {
                    Ok(_) => Response::ok(serde_json::json!({ "opened": name })),
                    Err(e) => Response::err(e),
                }
            }
        }
    }

    /// Drain any waiting control requests. Called once per loop turn.
    fn serve_ipc(&mut self) {
        let Some(ipc) = &self.ipc else { return };
        // A client is not going to send five requests, so one per turn is plenty
        // and keeps the loop's latency unchanged.
        let Some(mut pending) = ipc.try_recv() else { return };
        let request = pending.take();
        let name = request.name();
        slowshell_core::debug!("shellctl asked for `{name}`");
        let stopping = matches!(request, Request::Stop);
        let response = self.handle_request(request);
        // `Request::Open` queues through the same path as a click, so the
        // surface is actually built before the client is told it worked.
        self.run_deferred();
        pending.answer(response);
        if stopping {
            // The reply is written by the IPC thread, not here, and the process
            // is about to exit. Without this the thread can be killed mid-write
            // and the user is told their `shellctl stop` failed when it worked.
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// Cheap change detection: stat each watched file and compare.
    fn poll_files(&mut self) {
        for path in &self.watched {
            let now = stamp(path);
            if let Some(then) = self.stamps.get(path) {
                if *then != now {
                    self.reload_requested = true;
                    return;
                }
            }
        }
    }

    fn run(&mut self) {
        if !self.reported_renderer {
            self.reported_renderer = true;
            slowshell_core::info!(
                "renderer: {}{}",
                self.graphics.adapter_name(),
                if self.graphics.software() { " (software rasteriser)" } else { "" }
            );
        }
        // Tell the IPC thread which windows exist, so a `shellctl` request can
        // post a message and wake the loop out of its kernel wait.
        self.sync_windows();
        while self.running {
            let mut events: Vec<(isize, WindowEvent)> = Vec::new();
            window::pump(|hwnd, ev| events.push((hwnd, ev)));
            for (hwnd, ev) in events {
                self.handle_event(hwnd, ev);
            }
            self.serve_ipc();
            if !self.running {
                break;
            }
            self.poll_files();
            if self.reload_requested {
                self.reload_requested = false;
                slowshell_core::info!("config changed, reloading");
                self.build();
                continue;
            }
            let changed = self.publish_system();
            // Animations get their own clock, stepped here rather than inside
            // `publish_system` so a system value changing and a panel moving are
            // never confused for each other.
            let now = Instant::now();
            let dt = now
                .saturating_duration_since(self.last_frame)
                .as_secs_f32()
                .min(0.1);
            self.last_frame = now;
            let animating = self.step_reveals(dt);
            if changed || animating {
                self.frame();
                continue;
            }
            // Nothing to do. Block in the kernel until a message arrives or the
            // clock's displayed value could change.
            //
            // This used to be a `sleep` with a 50 ms ceiling so a `shellctl`
            // request would be answered within a frame or two. That ceiling was
            // the entire idle cost: twenty wake-ups a second, each one a message
            // pump and a handful of stats, on a desktop where nothing is
            // happening. Now the only wake-ups are real ones — a posted message
            // from the IPC thread, a user event, or the next clock tick — so
            // idle is zero rather than small.
            let now = Instant::now();
            let until = self.next_clock.saturating_duration_since(now);
            let until = until.clamp(Duration::from_millis(4), Duration::from_millis(1_000));
            window::wait(until.as_millis() as u32, |_, _| {});
        }
        for s in self.surfaces.drain(..) {
            window::destroy(s.handle);
        }
    }

    /// Publish the current window list to the IPC thread.
    fn sync_windows(&self) {
        if let Some(ipc) = &self.ipc {
            let hwnds: Vec<isize> = self.surfaces.iter().map(|s| s.handle.hwnd.0 as isize).collect();
            ipc.set_windows(&hwnds);
        }
    }
}

/// Everything about a panel that decides where it sits and what it reserves.
///
/// Part of the surface's identity for hot reload: if any of these change, the
/// window has to be recreated rather than patched, because the window's own
/// position, its app bar registration, and the space it holds were all decided
/// from them when it was created.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PanelAnchor {
    position: Position,
    screen: String,
    exclusive: bool,
}

/// Read a panel's anchoring, defaulting the way the compiler does.
fn panel_anchor(panel: &Element) -> PanelAnchor {
    match &panel.kind {
        slowshell_ui::ElementKind::Panel { position, screen, exclusive, .. } => PanelAnchor {
            position: *position,
            screen: screen.clone(),
            exclusive: *exclusive,
        },
        _ => PanelAnchor {
            position: Position::Top,
            screen: "primary".to_string(),
            exclusive: false,
        },
    }
}

/// Default thickness of a `wrap` arm, in logical pixels.
///
/// Thin on purpose. The frame's job is to show that the desktop is inset, and a
/// thick border eats into the work area the user is trying to get back.
const WRAP_ARM: f32 = 6.0;

/// The three companion panels that continue `panel`'s background around the rest
/// of the display.
///
/// Pure geometry, deliberately: it takes a `Monitor` and returns elements, with
/// no window or API involved, so the interesting part — do the four rectangles
/// tile the work area without overlapping or leaving a gap — is testable.
///
/// The arms are built against the *current* work area, which already excludes the
/// real taskbar and any other bar. So a wrapped bar frames what is actually
/// usable, and does not paint over the taskbar.
fn wrap_arms(
    panel: &Element,
    monitor: &Monitor,
    wrap_size: Option<f32>,
    theme: &Theme,
) -> Vec<Element> {
    let (position, screen) = match &panel.kind {
        slowshell_ui::ElementKind::Panel { position, screen, .. } => {
            (*position, screen.clone())
        }
        // A floating panel has no edge to continue from.
        _ => return Vec::new(),
    };
    if position == Position::Floating {
        return Vec::new();
    }
    let t = monitor.physical(wrap_size.unwrap_or(WRAP_ARM).max(1.0)).max(1);

    let (wx, wy) = (monitor.work_x, monitor.work_y);
    let (ww, wh) = (monitor.work_width as i32, monitor.work_height as i32);
    let (bar_w, bar_h) = declared_physical_size(panel, monitor);

    // Work in terms of the axis the bar runs *along* and the axis it *eats*.
    //
    // A top bar runs along x and eats y. Everything else follows from that, so
    // there is one case to reason about rather than four — and the previous
    // version had four, and got the right-hand arm wrong in one of them.
    let horizontal = matches!(position, Position::Top | Position::Bottom);
    let (along, cross) = if horizontal { (ww, wh) } else { (wh, ww) };
    let bar_eats = if horizontal { bar_h } else { bar_w };

    // Where the bar's own strip sits on the cross axis, and what is left over.
    // The opposite arm takes the far `t` of the remainder; the two side arms take
    // the near `t` of what is between. Between them they tile the frame, and
    // neither touches the bar.
    let (bar_lo, bar_hi) = match position {
        Position::Top | Position::Left => (0, bar_eats),
        _ => (cross - bar_eats, cross),
    };
    // The band the side arms live in: from the bar's inner edge to the opposite
    // arm's outer edge. Empty when the bar plus two arms would not fit, which is
    // a very small work area rather than a geometry error.
    let band_lo = if matches!(position, Position::Top | Position::Left) {
        bar_hi
    } else {
        t
    };
    let band_hi = if matches!(position, Position::Top | Position::Left) {
        cross - t
    } else {
        cross - bar_eats - t
    };
    let band = band_hi - band_lo;

    let mut arms: Vec<(Position, i32, i32, i32, i32)> = Vec::new();

    let opposite = match position {
        Position::Top => Position::Bottom,
        Position::Bottom => Position::Top,
        Position::Left => Position::Right,
        _ => Position::Left,
    };
    // The arm opposite the bar spans the whole run, because nothing interrupts
    // it. It has to be built on the axis the bar runs along: for a top bar that
    // is a bottom strip, but for a left bar it is a *right* strip. Sharing one
    // shape for both is how the right-hand edge of a side bar ends up drawn along
    // the bottom instead — which the per-edge test catches.
    let opp = if horizontal {
        if matches!(opposite, Position::Top) {
            (wx, wy, ww.max(1), t)
        } else {
            (wx, wy + wh - t, ww.max(1), t)
        }
    } else if matches!(opposite, Position::Left) {
        (wx, wy, t, wh.max(1))
    } else {
        (wx + ww - t, wy, t, wh.max(1))
    };
    arms.push((opposite, opp.0, opp.1, opp.2, opp.3));

    // The two side arms run between the bar and the opposite arm, so the four
    // rectangles meet corner to corner with no overlap and no gap.
    if band > 0 {
        for edge in [Position::Left, Position::Right] {
            let (x, y, w, h) = if horizontal {
                let x = if edge == Position::Left { wx } else { wx + ww - t };
                (x, wy + band_lo, t, band)
            } else {
                let y = if edge == Position::Left { wy } else { wy + wh - t };
                (wx + band_lo, y, band, t)
            };
            arms.push((edge, x, y, w, h));
        }
    }
    let _ = (along, bar_lo, bar_hi);

    arms.into_iter()
        .map(|(edge, x, y, w, h)| {
            wrap_arm_element(panel, &screen, edge, (x, y, w.max(1) as u32, h.max(1) as u32), theme)
        })
        .collect()
}

/// Build the element for one wrap arm.
///
/// It inherits the panel's identity fields so the shell's own reload logic treats
/// it like any other surface, but carries explicit bounds and no children — there
/// is nothing to click on an arm, and nothing to lay out.
fn wrap_arm_element(
    panel: &Element,
    screen: &str,
    edge: Position,
    bounds: (i32, i32, u32, u32),
    theme: &Theme,
) -> Element {
    let (name, hidden, backdrop) = match &panel.kind {
        slowshell_ui::ElementKind::Panel { name, hidden, backdrop, .. } => {
            (name.clone(), *hidden, *backdrop)
        }
        _ => (String::new(), false, slowshell_win::Backdrop::None),
    };
    // The arm paints the same background as the bar it continues, taken from the
    // theme rather than from the panel's own `background` property: the arm is
    // generated after compilation, so it never sees the panel's props, and a
    // mismatch would show as a seam along the edge.
    let mut style = Style::default();
    if let Some(bg) = theme.get("background") {
        style.background = slowshell_ui::ColorRef::literal(bg);
    }
    // Square against the bezel. A rounded corner on an arm shows a notch in the
    // one place the frame has to meet the display edge cleanly, and the bar's own
    // radius is applied where its content actually is.
    style.radius = 0.0;
    style.padding = slowshell_ui::Edges::ZERO;
    let el = Element::new(
        slowshell_ui::ElementId(u32::MAX),
        slowshell_ui::ElementKind::Panel {
            position: edge,
            screen: screen.to_string(),
            // An arm reserves its strip like any other bar, which is what insets
            // the work area on that edge.
            exclusive: true,
            wrap: false,
            wrap_size: None,
            bounds: Some(bounds),
            // An arm is decoration with nothing in it, so it never reveals.
            reveal: false,
            reveal_size: 8.0,
            reveal_duration: 0.18,
            reveal_delay: 0.26,
            reveal_ease: slowshell_core::ease::Ease::OutCubic,
            name,
            hidden,
            backdrop,
        },
        style,
        slowshell_core::Span::default(),
    );
    el
}

/// A panel's declared size in physical pixels, falling back to the minimum the
/// shell gives an edge-anchored panel with no declared height.
fn declared_physical_size(panel: &Element, monitor: &Monitor) -> (i32, i32) {
    let (dw, dh) = declared_size(panel);
    let w = match dw {
        Some(v) => monitor.physical(v),
        None => monitor.width as i32,
    };
    let h = match dh {
        Some(v) => monitor.physical(v),
        None => monitor.physical(24.0),
    };
    (w.max(1), h.max(1))
}

/// Whether a panel asked to stay out of the way until something opens it.
fn panel_is_hidden(panel: &Element) -> bool {
    matches!(&panel.kind, slowshell_ui::ElementKind::Panel { hidden: true, .. })
}

/// The `name` a panel declared, or the empty string.
fn panel_name(panel: &Element) -> String {
    match &panel.kind {
        slowshell_ui::ElementKind::Panel { name, .. } => name.clone(),
        _ => String::new(),
    }
}

fn stamp(p: &PathBuf) -> std::time::SystemTime {
    std::fs::metadata(p).and_then(|m| m.modified()).unwrap_or(std::time::SystemTime::UNIX_EPOCH)
}

/// Actions that only the frame loop can carry out, queued by a registry closure.
///
/// The alternative is a registry that can reach into the running shell, which
/// would make every action untestable without a desktop. A closure pushes here
/// instead, returns to the caller, and the frame loop does the work at a point
/// where rebuilding a surface or tearing one down is safe.
pub type Deferred = Rc<RefCell<Vec<(String, Vec<Value>)>>>;

/// Build a runtime that knows every action, without a window.
///
/// `--check` and `--doctor` both need this, and it is the same code the shell
/// runs, so a handler that passes `--check` is a handler the shell can resolve.
/// A separate, smaller list here would make `--check` report a false error on
/// every `onClick`, which is exactly the "the tool lies to me" failure a
/// diagnostic tool must not have.
fn runtime_with_actions() -> (Runtime, Deferred) {
    let mut runtime = Runtime::new();
    runtime.seed_sources();
    let deferred: Deferred = Rc::new(RefCell::new(Vec::new()));
    register_actions(&mut runtime, deferred.clone());
    (runtime, deferred)
}

/// What a config may call from `onClick`.
///
/// Split by ownership rather than by convenience. Anything that only needs
/// Windows lives in `slowshell_win::platform`; anything that only needs the
/// runtime is here. Neither reaches into the [`App`], which is what keeps the
/// action list testable without a display.
fn register_actions(runtime: &mut Runtime, deferred: Deferred) {
    let mut a = std::mem::take(&mut runtime.actions);

    // -- shell control -------------------------------------------------------
    // These queue rather than act: the frame loop is the only place that may
    // rebuild or tear down a surface.
    let queue = deferred.clone();
    a.register("shell.reload", "Re-read the config and rebuild the scene.", &[], move |_| {
        queue.borrow_mut().push(("shell.reload".into(), Vec::new()));
        Ok(Value::str("reloading"))
    });
    let queue = deferred.clone();
    a.register(
        "shell.overlay",
        "Show or hide the developer error overlay.",
        &[],
        move |_| {
            queue.borrow_mut().push(("shell.overlay".into(), Vec::new()));
            Ok(Value::str("toggled"))
        },
    );
    let queue = deferred.clone();
    a.register(
        "shell.open",
        "Open a named surface, such as the launcher.",
        &["name"],
        move |args| {
            queue.borrow_mut().push(("shell.open".into(), args.to_vec()));
            Ok(args[0].clone())
        },
    );
    let queue = deferred.clone();
    a.register("shell.stop", "Quit the shell.", &[], move |_| {
        queue.borrow_mut().push(("shell.stop".into(), Vec::new()));
        Ok(Value::str("stopping"))
    });

    a.register(
        "clipboard.set",
        "Copy text to the clipboard.",
        &["text"],
        |args| {
            slowshell_win::clipboard::set(&args[0].to_string_lossy())
                .map(|_| Value::str("copied"))
        },
    );

    a.register(
        "clipboard.get",
        "Read the clipboard.",
        &[],
        |_| Ok(slowshell_win::clipboard::get().map(Value::str).unwrap_or(Value::Null)),
    );

    a.register(
        "notify",
        "Show a transient notification.",
        &["title", "body"],
        |args| {
            slowshell_win::notify::toast(&args[0].to_string_lossy(), &args[1].to_string_lossy())
                .map(|_| Value::str("shown"))
        },
    );

    a.register(
        "launch",
        "Start a program or open a file, as if typed in Run.",
        &["target"],
        |args| {
            slowshell_win::launch::open(&args[0].to_string_lossy())
                .map(|_| Value::str("launched"))
        },
    );

    a.register(
        "exec.run",
        "Run a program and wait for it to exit.",
        &["program", "args"],
        |args| {
            let program = args[0].to_string_lossy();
            let rest = match &args[1] {
                Value::List(items) => {
                    items.iter().map(|v| v.to_string_lossy()).collect::<Vec<_>>()
                }
                Value::Null => Vec::new(),
                other => vec![other.to_string_lossy()],
            };
            slowshell_win::launch::run(&program, &rest)
                .map(|code| Value::Int(code as i64))
        },
    );

    slowshell_win::platform::register(&mut a);

    runtime.actions = a;
}

/// An explicit `width`/`height` on the panel, in logical pixels.
fn declared_size(panel: &Element) -> (Option<f32>, Option<f32>) {
    let w = match panel.style.width {
        Size::Fixed(v) => Some(v),
        _ => None,
    };
    let h = match panel.style.height {
        Size::Fixed(v) => Some(v),
        _ => None,
    };
    (w, h)
}

/// Where a non-exclusive panel goes for a given edge.
fn position_of(position: Position, m: &Monitor, w: u32, h: u32) -> (i32, i32) {
    match position {
        Position::Top => (m.x, m.y),
        Position::Bottom => (m.x, m.y + m.height as i32 - h as i32),
        Position::Left => (m.x, m.y),
        Position::Right => (m.x + m.width as i32 - w as i32, m.y),
        // A floating panel has no anchor yet; the centre is the least surprising
        // default until `anchorX`/`anchorY` are implemented.
        Position::Floating => (m.x + (m.width as i32 - w as i32) / 2, m.y + (m.height as i32 - h as i32) / 2),
    }
}

/// Where an exclusive panel sits: against the work-area edge on that side.
fn position_exclusive(position: Position, m: &Monitor, w: u32, h: u32) -> (i32, i32) {
    match position {
        Position::Top => (m.work_x, m.work_y),
        Position::Bottom => (m.work_x, m.work_y + m.work_height as i32 - h as i32),
        Position::Left => (m.work_x, m.work_y),
        Position::Right => (m.work_x + m.work_width as i32 - w as i32, m.work_y),
        // A floating panel reserves nothing, so there is no edge to hug.
        Position::Floating => position_of(position, m, w, h),
    }
}

/// Write the built-in starter config somewhere, and say where.
///
/// This exists because the starter config is only written when the file is
/// *absent*, so it is a snapshot of whatever the defaults were on the day you
/// first ran the shell. New defaults — `exclusive: true`, for one — therefore
/// never reach an existing config, and the honest way to see them is to write
/// the current ones out and compare.
///
/// It never touches the live config and never overwrites without `--force`,
/// because that file is the user's work and losing it to a convenience flag
/// would be unforgivable.
fn write_starter(args: &[String]) -> i32 {
    let force = args.iter().any(|a| a == "--force");
    let live = slowshell_core::paths::shell_config();
    // Default to a sibling of the live config rather than the live config
    // itself, so a stray flag cannot overwrite someone's shell.
    let target = args
        .iter()
        .skip_while(|a| *a != "--starter")
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| live.with_file_name("shell.starter.config"));

    if target == live {
        eprintln!(
            "Refusing to overwrite your live config at {}.\n\
             Pass a different path:  Shell.exe --starter other.config",
            live.display()
        );
        return 1;
    }
    if target.exists() && !force {
        eprintln!(
            "{} already exists. Nothing written.\n\
             Compare it against your config by hand, or pass --force to replace it.",
            target.display()
        );
        return 1;
    }
    if let Some(parent) = target.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!("Could not create {}: {e}", parent.display());
            return 1;
        }
    }
    if let Err(e) = std::fs::write(&target, slowshell_runtime::DEFAULT_CONFIG) {
        eprintln!("Could not write {}: {e}", target.display());
        return 1;
    }
    println!("Wrote the current starter config to {}", target.display());
    println!("Your live config is untouched: {}", live.display());
    if live.exists() {
        println!();
        println!("To use it:  Shell.exe {}", target.display());
    }
    0
}

fn print_help() {
    println!(
        "Slowshell {} — a programmable desktop shell for Windows

USAGE:
    Shell.exe [config]

    config   Path to shell.config. Defaults to %APPDATA%\\Slowshell\\shell.config

OPTIONS:
    --version    Print the version and exit
    --doctor     Print an environment report and exit
    --check      Load and compile the config, print any problems, and exit
    --starter [path]
                Write the built-in starter config to `path` (default:
                shell.starter.config beside your live config) and exit. It never
                touches your live config. Use it to see the current defaults,
                which do not change an existing config.
    --force      With --starter, replace the file if it already exists
    --help       Print this help

Run `shellctl reload` to apply config changes, or just save the file.

A Panel with `exclusive: true` takes its height out of the usable desktop, so
maximized windows stop below it instead of covering it. That is on in the
starter config. Check the result with `shellctl screens`.",
        slowshell_core::VERSION
    );
}

/// Collect environment facts for `--doctor` and for `shellctl doctor`.
fn doctor(args: &[String]) -> i32 {
    let mons = Monitors::enumerate();
    println!("Slowshell {}", slowshell_core::VERSION);
    println!();
    println!("Displays");
    for m in &mons.list {
        println!(
            "  {} {:<9} {}x{} @ {:.0}x ({} Hz) origin {},{}",
            m.id,
            if m.primary { "primary" } else { "" },
            m.width,
            m.height,
            m.scale,
            m.refresh_hz,
            m.x,
            m.y
        );
        // The work area is what applications are told they may use, so a
        // reserved bar shows up here as a shorter or offset rectangle. This is
        // the number to check when a maximized window does not respect the bar.
        println!(
            "  {:<18} work area {},{} {}x{}",
            "", m.work_x, m.work_y, m.work_width, m.work_height
        );
    }
    // Every app bar this process can see. A lower bound: Windows does not
    // reliably disclose other processes' bars, and Explorer's own taskbar may
    // not appear at all. The per-monitor work area above is the authoritative
    // answer to "is space reserved".
    let bars = slowshell_win::appbar::describe_reserved();
    if bars.is_empty() {
        println!("  app bars     none visible to this process");
    } else {
        for b in bars {
            println!("  app bar      {b}");
        }
    }
    println!();
    println!("Graphics");
    match Graphics::new().and_then(|g| g.create().map(|()| g)) {
        Ok(g) => {
            println!("  adapter      {}", g.adapter_name());
            println!("  renderer     {}", if g.software() { "software (WARP)" } else { "hardware" });
            // The presentation path is the single most useful thing to know about
            // a machine: it decides whether panels are translucent.
            println!("  presentation {}", g.presentation().label());
            if !g.presentation().honours_alpha() {
                println!("  alpha        discarded at present; translucent colours are pre-composited");
            }
        }
        Err(e) => println!("  device       unavailable: {e}"),
    }
    println!("  backdrop  {}", if slowshell_win::backdrop::supported() { "available" } else { "not available on this build" });
    println!();
    println!("Config");
    // A path may be given so the report covers the config being worked on rather
    // than the installed one. `Shell.exe --doctor examples\topbar.config` is the
    // question "why does *this* file not work", and answering about a different
    // file would be worse than not answering.
    let path = config_path_from(&args);
    println!("  path      {}", path.display());
    println!("  exists    {}", path.exists());
    if let Err(e) = slowshell_core::paths::ensure_dirs() {
        println!("  writable   no: {e}");
    } else {
        println!("  writable   yes");
    }
    let (mut rt, _deferred) = runtime_with_actions();
    let outcome = rt.build(&path);
    println!("  build      {}", outcome.summary());
    if !outcome.diagnostics.is_empty() {
        println!();
        for d in &outcome.diagnostics {
            println!("{}", d.render());
            println!();
        }
    }
    // Every provider a config can read, and an honest note about the ones that
    // answer with null. A shell that quietly reads 0 for the battery is worse
    // than one that says the battery provider is not live yet.
    println!();
    println!("Providers");
    let live = ["clock", "screens", "notifications"];
    for name in rt.shell_host.provider_names() {
        let state = if live.contains(&name) {
            "live"
        } else if rt.shell_host.stub_providers().contains(&name) {
            "reads as null"
        } else {
            "live"
        };
        println!("  {:<18} {state}", name);
    }
    for note in rt.shell_host.pending_notes() {
        println!("  note: {note}");
    }
    0
}

/// The first non-flag argument, or the installed config.
///
/// Shared by `--doctor` and `--check` so both agree on what file is being
/// talked about. Flags are skipped, so the path may come before or after them.
fn config_path_from(args: &[String]) -> PathBuf {
    args.iter()
        .find(|a| !a.starts_with('-'))
        .map(PathBuf::from)
        .unwrap_or_else(slowshell_core::paths::shell_config)
}

/// Compile the config and report, without starting the UI.
fn check(args: &[String]) -> i32 {
    let path = config_path_from(args);
    if !path.exists() {
        eprintln!("No config at {}. Run `Shell.exe` once to create it.", path.display());
        return 1;
    }
    let (mut rt, _deferred) = runtime_with_actions();
    let outcome = rt.build(&path);
    println!("{}", outcome.summary());
    for d in &outcome.diagnostics {
        println!();
        println!("{}", d.render());
    }
    if outcome.has_errors() {
        1
    } else {
        0
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return;
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("slowshell {}", slowshell_core::VERSION);
        return;
    }
    if args.iter().any(|a| a == "--doctor") {
        std::process::exit(doctor(&args));
    }
    if args.iter().any(|a| a == "--check") {
        std::process::exit(check(&args));
    }
    if args.iter().any(|a| a == "--starter") {
        std::process::exit(write_starter(&args));
    }

    let config_path = args
        .first()
        .map(PathBuf::from)
        .unwrap_or_else(slowshell_core::paths::shell_config);

    // A first run writes a working config so the shell is never a blank screen.
    // It is written where the user pointed, which is what makes
    // `Shell.exe path\to\other.config` do the obvious thing.
    if !config_path.exists() {
        let _ = slowshell_core::paths::ensure_dirs();
        match slowshell_runtime::ensure_default_config_at(&config_path) {
            Ok((p, true)) => println!("Wrote a starter config to {}", p.display()),
            Ok((_, false)) => {}
            Err(e) => {
                eprintln!("Could not create {}: {e}", config_path.display());
                std::process::exit(1);
            }
        }
    }

    log::set_level(
        std::env::var("SLOWSHELL_LOG")
            .ok()
            .and_then(|v| Level::parse(&v))
            .unwrap_or(Level::Info),
    );

    // A panic in a window procedure would take the shell with it, so the top
    // level catches and reports instead.
    let result = std::panic::catch_unwind(move || {
        slowshell_win::init_process();
        let mut app = match App::new(config_path) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("Slowshell could not start: {e}");
                std::process::exit(2);
            }
        };
        app.build();
        if app.surfaces.is_empty() {
            eprintln!("No surfaces were created; the shell has nothing to draw.");
            std::process::exit(3);
        }
        app.run();
    });

    if result.is_err() {
        slowshell_core::error!("the shell panicked; see the message above");
        std::process::exit(4);
    }
}

// Silence the unused-import warning for types used only in signatures above.
#[allow(dead_code)]
type Unused = (Rc<()>, RefCell<u8>, Color);

#[cfg(test)]
mod tests {
    use super::*;

    /// A panel element, with only the panel properties under test set.
    fn panel(position: &str, screen: &str, exclusive: bool) -> Element {
        Element::new(
            slowshell_ui::ElementId(0),
            slowshell_ui::ElementKind::Panel {
                position: match position {
                    "bottom" => Position::Bottom,
                    "left" => Position::Left,
                    "right" => Position::Right,
                    "floating" => Position::Floating,
                    _ => Position::Top,
                },
                screen: screen.to_string(),
                exclusive,
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
            Style::default(),
            slowshell_core::Span::default(),
        )
    }

    /// A wrapped panel of a declared height.
    ///
    /// The height goes in `style.height` rather than a property, because that is
    /// where `declared_size` reads it from — the same place the compiler puts it.
    fn wrapped(position: &str, height: f32) -> Element {
        let mut p = panel(position, "primary", true);
        p.style.height = Size::Fixed(height);
        match &mut p.kind {
            slowshell_ui::ElementKind::Panel { wrap, .. } => *wrap = true,
            _ => unreachable!("a panel"),
        }
        p
    }

    #[test]
    fn toggling_exclusive_changes_the_panels_identity() {
        // This is the regression that matters. Hot reload patches a surface in
        // place unless its identity changed, so missing `exclusive` from the
        // comparison meant turning it off left the window registered as an app
        // bar — holding a strip of the desktop that nothing on screen was using,
        // with no way to release it short of killing the shell.
        let on = panel("top", "primary", true);
        let off = panel("top", "primary", false);
        assert_ne!(panel_anchor(&on), panel_anchor(&off));
    }

    #[test]
    fn moving_a_panel_changes_its_identity() {
        assert_ne!(
            panel_anchor(&panel("top", "primary", true)),
            panel_anchor(&panel("bottom", "primary", true))
        );
        assert_ne!(
            panel_anchor(&panel("top", "primary", true)),
            panel_anchor(&panel("top", "1#2", true))
        );
    }

    #[test]
    fn an_ordinary_edit_keeps_the_panels_identity() {
        // The opposite requirement, and the reason the comparison is not simply
        // "recreate every surface": editing the bar's text must not destroy and
        // recreate its window, or the bar flickers on every keystroke.
        let a = panel("top", "primary", true);
        let b = panel("top", "primary", true);
        assert_eq!(panel_anchor(&a), panel_anchor(&b));
    }

    #[test]
    fn an_exclusive_bar_is_placed_inside_the_work_area() {
        // A bar placed against the screen edge is drawn through the real taskbar
        // when they share one. The work area already excludes every other bar, so
        // anchoring to it is what makes them stack.
        let m = Monitor {
            name: r"\\.\DISPLAY1".to_string(),
            id: "primary".to_string(),
            primary: true,
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            scale: 1.0,
            refresh_hz: 60,
            rotation: 0,
            work_x: 0,
            work_y: 0,
            work_width: 1920,
            work_height: 1032,
        };
        // A 40px bar at the bottom sits above the 48px taskbar, not on it.
        assert_eq!(position_exclusive(Position::Bottom, &m, 1920, 40), (0, 992));
        // And spans the work area, so it does not run under a side taskbar.
        assert_eq!(position_exclusive(Position::Top, &m, 1920, 48), (0, 0));
        // A floating panel reserves nothing and hugs no edge.
        assert_eq!(
            position_exclusive(Position::Floating, &m, 100, 40),
            (910, 520)
        );
        // A non-exclusive panel is unaffected: it keeps the screen-edge anchor,
        // because it reserves nothing and so has nothing to stay clear of.
        assert_eq!(position_of(Position::Bottom, &m, 1920, 40), (0, 1040));
    }

    #[test]
    fn an_exclusive_bar_never_lands_outside_the_display() {
        // A work area larger than the display would place the bar off screen.
        let m = Monitor {
            name: r"\\.\DISPLAY1".to_string(),
            id: "primary".to_string(),
            primary: true,
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            scale: 1.0,
            refresh_hz: 60,
            rotation: 0,
            work_x: 0,
            work_y: 0,
            work_width: 1920,
            work_height: 1080,
        };
        let (_, y) = position_exclusive(Position::Bottom, &m, 1920, 40);
        assert!(y >= 0 && y + 40 <= 1080, "bar at y={y} is off screen");
    }

    // -- wrap ----------------------------------------------------------------

    fn monitor_with_work_area(work_height: u32) -> Monitor {
        Monitor {
            name: r"\\.\DISPLAY1".to_string(),
            id: "primary".to_string(),
            primary: true,
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            scale: 1.0,
            refresh_hz: 60,
            rotation: 0,
            work_x: 0,
            work_y: 0,
            work_width: 1920,
            work_height,
        }
    }

    /// The bounds of each generated arm, keyed by the edge it sits on.
    fn arm_bounds(arms: &[Element]) -> Vec<(Position, (i32, i32, u32, u32))> {
        arms.iter()
            .filter_map(|e| match &e.kind {
                slowshell_ui::ElementKind::Panel {
                    position,
                    bounds: Some(b),
                    ..
                } => Some((*position, *b)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn wrap_makes_three_arms_not_four() {
        // Four would mean the bar is being duplicated, which would put a second
        // copy of the content on another edge.
        let m = monitor_with_work_area(1032);
        let arms = arm_bounds(&wrap_arms(&wrapped("top", 44.0), &m, None, &Theme::default()));
        assert_eq!(arms.len(), 3, "expected exactly three arms, got {arms:?}");
    }

    #[test]
    fn the_arms_tile_the_frame_without_gaps_or_overlaps() {
        // The whole point. Four rectangles that overlap show as a seam; four that
        // leave a gap show as a stripe of desktop inside the frame. Both look
        // like a rendering bug rather than a geometry one, so this is asserted
        // rather than eyeballed.
        let m = monitor_with_work_area(1032);
        let bar = wrapped("top", 44.0);
        let (_, bh) = declared_physical_size(&bar, &m);
        let mut rects: Vec<(i32, i32, u32, u32)> = vec![(0, 0, m.work_width, bh as u32)];
        rects.extend(
            arm_bounds(&wrap_arms(&bar, &m, None, &Theme::default()))
                .into_iter()
                .map(|(_, b)| b),
        );

        // Every rectangle inside the work area.
        for (x, y, w, h) in &rects {
            assert!(
                *x >= m.work_x
                    && *y >= m.work_y
                    && x + *w as i32 <= m.work_x + m.work_width as i32
                    && y + *h as i32 <= m.work_y + m.work_height as i32,
                "{x},{y} {w}x{h} escapes the work area"
            );
        }
        // No two overlap. Arms are 6px on a 1920x1032 area, so a mistake shows up
        // as a clearly negative overlap rather than a rounding crumb.
        for (i, a) in rects.iter().enumerate() {
            for b in rects.iter().skip(i + 1) {
                let overlap_x = (a.0 + a.2 as i32).min(b.0 + b.2 as i32) - a.0.max(b.0);
                let overlap_y = (a.1 + a.3 as i32).min(b.1 + b.3 as i32) - a.1.max(b.1);
                assert!(
                    overlap_x <= 0 || overlap_y <= 0,
                    "{a:?} overlaps {b:?} by {overlap_x}x{overlap_y}"
                );
            }
        }
        // And it reaches the far edges: the rightmost and bottommost rectangle
        // end exactly at the work area's edge, so there is no unframed strip.
        let rightmost = rects.iter().map(|r| r.0 + r.2 as i32).max().unwrap_or(0);
        assert_eq!(rightmost, m.work_x + m.work_width as i32, "right edge uncovered");
        let bottommost = rects.iter().map(|r| r.1 + r.3 as i32).max().unwrap_or(0);
        assert_eq!(bottommost, m.work_y + m.work_height as i32, "bottom edge uncovered");
        assert!(
            rects.iter().any(|r| r.0 > m.work_x),
            "nothing inset from the left"
        );
    }

    #[test]
    fn wrap_on_every_edge_puts_the_opposite_arm_on_the_other_side() {
        // The bug this guards against is using the bar's thickness for the wrong
        // axis, which puts a bottom arm at the top or a left arm at y=0.
        let m = monitor_with_work_area(1032);
        for (edge, expect) in [
            ("top", Position::Bottom),
            ("bottom", Position::Top),
            ("left", Position::Right),
            ("right", Position::Left),
        ] {
            let arms = arm_bounds(&wrap_arms(&wrapped(edge, 40.0), &m, None, &Theme::default()));
            let opposite = arms
                .iter()
                .find(|(p, _)| *p == expect)
                .unwrap_or_else(|| panic!("{edge}: no arm on {expect:?}, got {arms:?}"));
            match expect {
                Position::Bottom => assert_eq!(opposite.1 .1, 1032 - 6, "{edge}: bottom arm misplaced"),
                Position::Top => assert_eq!(opposite.1 .1, 0, "{edge}: top arm misplaced"),
                Position::Left => assert_eq!(opposite.1 .0, 0, "{edge}: left arm misplaced"),
                Position::Right => assert_eq!(opposite.1 .0, 1920 - 6, "{edge}: right arm misplaced"),
                Position::Floating => {}
            }
        }
    }

    #[test]
    fn a_floating_panel_has_nothing_to_wrap_around() {
        let m = monitor_with_work_area(1032);
        assert!(
            wrap_arms(&wrapped("floating", 40.0), &m, None, &Theme::default()).is_empty(),
            "a floating panel reserves nothing, so it has no frame"
        );
    }

    #[test]
    fn a_work_area_too_thin_for_arms_keeps_them_inside_it() {
        // A strip only a few pixels tall cannot hold a bar plus arms. Anything
        // still emitted must stay inside the work area rather than spilling onto
        // the display edge, which is the visible failure.
        let m = monitor_with_work_area(10);
        let arms = arm_bounds(&wrap_arms(&wrapped("top", 40.0), &m, None, &Theme::default()));
        for (edge, (x, y, w, h)) in &arms {
            assert!(
                *x >= 0 && *y >= 0 && x + *w as i32 <= 1920 && y + *h as i32 <= 10,
                "{edge:?} arm {x},{y} {w}x{h} is not inside a 10px work area"
            );
        }
    }

    // -- reveal --------------------------------------------------------------

    use slowshell_core::ease::Ease;

    fn reveal() -> Reveal {
        let mut r = Reveal::new(8, 210, 0.26, Ease::OutCubic);
        r.with_duration(0.18);
        r
    }

    /// Run a reveal forward by `secs`, returning every width it reported.
    fn run(r: &mut Reveal, secs: f32) -> Vec<u32> {
        let steps = (secs / 0.016) as usize;
        let mut seen = Vec::new();
        for _ in 0..steps {
            if let Some(w) = r.advance(0.016, Instant::now()) {
                seen.push(w);
            }
        }
        seen
    }

    #[test]
    fn a_reveal_starts_collapsed_and_says_so() {
        // A panel that animates open on its own is a panel that never lets the
        // user look at the desktop. It must start at the strip and be settled.
        let mut r = reveal();
        assert_eq!(r.width, 8);
        assert!(!r.is_animating(), "a fresh reveal must not be moving");
        assert_eq!(r.advance(0.016, Instant::now()), None);
    }

    #[test]
    fn opening_grows_the_panel_monotonically_to_its_full_width() {
        let mut r = reveal();
        r.open_now();
        assert!(r.is_animating());
        let seen = run(&mut r, 0.3);
        assert!(!seen.is_empty(), "opening produced no widths at all");
        // Monotonic, and never wider than the panel is meant to get.
        for w in seen.windows(2) {
            assert!(w[1] >= w[0], "the panel shrank while opening: {seen:?}");
        }
        assert_eq!(*seen.last().unwrap(), 210, "did not reach full width");
        assert!(seen.iter().all(|&w| w <= 210), "overshot its own width: {seen:?}");
    }

    #[test]
    fn a_settled_reveal_reports_no_more_movement() {
        // What lets the frame loop go back to sleep. If this is wrong the shell
        // repaints at 60 Hz forever, which is the idle budget spent on nothing.
        let mut r = reveal();
        r.open_now();
        run(&mut r, 0.4);
        assert_eq!(r.width, 210);
        assert!(!r.is_animating(), "still moving after arriving");
        assert_eq!(r.advance(0.016, Instant::now()), None);
    }

    #[test]
    fn leaving_does_not_close_until_the_delay_has_elapsed() {
        // The reason `revealDelay` exists. Without it, a pointer crossing a panel
        // diagonally shuts it mid-flight and the far side is unreachable.
        let mut r = reveal();
        r.open_now();
        run(&mut r, 0.3);
        assert_eq!(r.width, 210);

        let t0 = Instant::now();
        r.schedule_close(t0);
        // Well past the 260ms delay, but the caller is only stepping 16ms at a
        // time, so nothing happens until the deadline actually passes.
        for _ in 0..5 {
            assert_eq!(
                r.advance(0.016, t0 + Duration::from_millis(80)),
                None,
                "closed before the delay had elapsed"
            );
        }
        // Now past it.
        let mut closed = false;
        for _ in 0..40 {
            if r.advance(0.016, t0 + Duration::from_millis(300 + 16 * 40)).is_some() {
                closed = true;
            }
        }
        assert!(closed, "never started closing after the delay");
        run(&mut r, 0.3);
        assert_eq!(r.width, 8, "did not return to the collapsed strip");
    }

    #[test]
    fn re_entering_cancels_a_pending_close() {
        // The twitch this prevents: pulling the pointer away and straight back
        // must not let the close start and then reverse.
        let mut r = reveal();
        r.open_now();
        run(&mut r, 0.3);
        let t0 = Instant::now();
        r.schedule_close(t0);
        r.open_now();
        assert!(r.close_at.is_none(), "re-entering did not cancel the close");
        // And it stays open.
        for _ in 0..40 {
            r.advance(0.016, t0 + Duration::from_secs(2));
        }
        assert_eq!(r.width, 210, "a cancelled close still closed the panel");
    }

    #[test]
    fn reversing_mid_flight_continues_from_where_it_was() {
        // The panel must not snap back to the strip and re-expand. Asserted on the
        // width, which is what the user sees.
        let mut r = reveal();
        r.open_now();
        run(&mut r, 0.06);
        let mid = r.width;
        assert!(mid > 8 && mid < 210, "not actually mid-flight: {mid}");
        r.schedule_close(Instant::now());
        r.advance(0.016, Instant::now() + Duration::from_secs(1));
        let first = r.advance(0.016, Instant::now() + Duration::from_secs(1));
        if let Some(w) = first {
            assert!(
                w <= mid + 2,
                "snapped backwards from {mid} to {w} instead of continuing"
            );
        }
    }

    #[test]
    fn a_reveal_never_reports_a_width_of_zero() {
        // A zero-width window cannot be hovered, so the panel could never be
        // opened again — a one-way trip to a permanently invisible window.
        let mut r = reveal();
        r.open_now();
        let t0 = Instant::now();
        for i in 0..120 {
            if let Some(w) = r.advance(0.016, t0) {
                assert!(w >= 1, "step {i} produced width {w}");
            }
            r.schedule_close(t0);
        }
    }

    #[test]
    fn a_collapsed_reveal_is_never_wider_than_its_expanded_width() {
        // The bug that made `reveal` look like it did nothing: the expanded width
        // was read from a variable the reveal had already overwritten, so the
        // panel expanded to exactly what it started at.
        let r = reveal();
        assert!(
            r.expanded > r.collapsed,
            "expanded ({}) must be wider than collapsed ({})",
            r.expanded,
            r.collapsed
        );
    }
}
