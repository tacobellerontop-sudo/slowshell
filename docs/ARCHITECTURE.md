# Architecture

This document explains how Slowshell is put together, and — more usefully —
*why each piece is where it is*. Every "why" here is a decision that was
actually made, with the alternative that was rejected.

## The shape of the problem

A shell has four jobs, and they pull in opposite directions:

1. **Be fast when nothing is happening.** Idle CPU must be ~0.
2. **Be correct when something is.** A bar that lies about the battery is worse
   than a bar that says nothing.
3. **Never break.** A typo in a config file must not take the desktop with it.
4. **Be readable.** Someone has to be able to change this in a year.

The naive design — evaluate every expression every frame, poll every system
value on a timer — satisfies none of 1, 3, or 4. That single sentence
determines most of what follows.

## Crates

```
                    ┌──────────────────┐
                    │  Shell.exe       │   window loop, frame loop, surfaces
                    └────────┬─────────┘
              ┌──────────────┼──────────────┐
              ▼              ▼              ▼
      ┌──────────────┐ ┌──────────┐ ┌────────────────┐
      │ slowshell-   │ │ slowshell│ │ slowshell-ui   │
      │ runtime      │ │  -win    │ │                │
      │              │ │          │ │ tree, layout,  │
      │ loader,      │ │ Win32,   │ │ paint, style   │
      │ compiler,    │ │ D2D,     │ │                │
      │ providers,   │ │ DWrite,  │ └────────┬───────┘
      │ actions      │ │ pipes    │          │
      └──────┬───────┘ └──────────┘          │
             │                                │
             └──────────────┬─────────────────┘
                            ▼
                    ┌──────────────────┐
                    │ slowshell-core   │   language, reactivity, diagnostics
                    │                  │   logging, IPC protocol, actions
                    └──────────────────┘
                            ▲
                            │
                    ┌───────┴────────┐
                    │  shellctl.exe  │   argument parsing, pretty output
                    └────────────────┘
```

The dependency arrows are the whole design. `slowshell-core` knows nothing about
Windows. Everything above it may.

### `slowshell-core` — the language

The lexer, parser, evaluator, reactive graph, diagnostics, logging ring, the
IPC protocol, and the action registry. **Zero Windows APIs.** Not "few" — zero.
It compiles and runs on any platform, which is what makes the language
testable: 78 of the tests in this workspace need no desktop at all.

This crate is the reason a config file can be validated by a tool
(`Shell.exe --check`) without starting a shell, and the reason the `did you
mean` machinery can be tested exhaustively without a display attached.

### `slowshell-win` — the platform

Everything that touches Windows: windows and messages, monitors and DPI,
Direct2D and DirectWrite, the swap chain, DWM backdrops, hit testing, the
keyboard, the clipboard, launching processes, and the named-pipe transport for
`shellctl`.

Each module is a thin, honest wrapper. Where a wrapper has to choose, it says so
in the module comment and names the API it chose instead. `clipboard.rs`
documents why it retries; `platform.rs` documents why the volume control
injects media keys rather than opening an audio endpoint.

### `slowshell-ui` — the tree, the layout, the pixels

The retained element tree, the style system with its inheritance rules, layout,
and the paint walk that turns a tree into draw calls.

It depends on `slowshell-win` for `Rect`, `TextStyle` and the `Painter` trait,
but it makes no Win32 decisions of its own. Swapping the renderer means
implementing `Painter`; nothing here knows what a window is.

### `slowshell-runtime` — the wiring

Loads a config (including `include`), compiles it into element trees, owns the
reactive graph and the system providers, and validates handler targets against
the action registry. It is the only crate that knows the whole pipeline.

### `Shell.exe` and `shellctl.exe`

The shell owns the window loop, the frame loop, and one `SurfaceState` per live
panel. `shellctl` is only argument parsing and formatting: it speaks the
protocol in `slowshell-core::ipc` over the pipe in `slowshell-win::ipc_pipe`, and
prints what comes back.

## The reactive model

This is the part worth understanding, because everything about performance
follows from it.

A naive reactive system is a graph with bookkeeping. Every frame it walks the
graph, checks every node's dependencies, recomputes what looks dirty, and
invalidates what it read. That bookkeeping *is* the cost, and it scales with the
size of your config whether or not anything changed.

Slowshell inverts it: **push-invalidate, lazy-pull**.

```
   provider tick                    frame
   ─────────────                    ─────
   reactor.set("clock.time", "14:32")
        │
        ├─ marks the derived nodes that read clock.time as pending
        │  (and only those — the dependency edges were recorded once,
        │   at compile time)
        ▼
   returns false if the value is identical to what was already there
        │
        └─ the frame loop only draws when something actually changed
```

Three properties fall out of this, and they are the entire performance story:

1. **A frame does zero dependency bookkeeping.** Dependencies are recorded when
   the config is compiled, by building each binding inside a tracking scope. The
   frame loop never revisits them.
2. **`Reactor::set` returns a `bool`.** A provider that pushes an unchanged
   value tells the shell "nothing to do", and the shell skips the frame. The
   clock ticking the same second twice costs nothing.
3. **Only what is read is computed.** A widget that never mentions
   `battery.percentage` is never recomputed when the battery changes, no matter
   how many widgets there are.

The dependency tracker is a *stack*, not a single slot. A single slot is enough
if a binding can only read one value; an expression like
`battery.percentage + " of " + network.ssid` reads two, and both edges must be
recorded or the widget silently stops updating when the second one changes. That
was a real bug, found by a test that reads pixels.

### From a config line to a frame

```
Panel { Text { text: battery.percentage + "%" } }
  │
  │  loader:   parse, resolve include, one Document
  ▼
  │  compiler: bind_dynamic sees `text` is not a literal
  │            → build the expression inside tracked()
  │            → the tracker records that it read battery.percentage
  │            → reactor.derived("Text#1/text", …)
  ▼
  Element { dyn_props: { text: Some(node) } }
  │
  │  every frame: resolve_values walks the tree
  │    → reactor.get(node) → cached unless pending
  ▼
  Resolved { text: "87%" }
  │
  ▼
  layout → paint → present
```

## Two presentation paths

The renderer has one drawing API and two ways of getting a surface to draw into.

`RenderTarget` (in `render/target.rs`) is a trait with a blanket impl over
anything that can be a Direct2D target. The `Painter` is written against the
trait, so it does not know or care which path it is on.

- **Flip-model swap chain** (`Presentation::Dxgi`). Preferred. Independent
  presentation, correct per-pixel alpha, and DWM backdrops work.
- **`ID2D1HwndRenderTarget`** (`Presentation::Hwnd`). The fallback. Presents
  opaquely, and is what a driver that rejects every alpha-aware swap chain gets.

The probe that decides between them costs 3.2 seconds on the development
machine, so the verdict is cached on disk, keyed on the adapter's LUID and PCI
ids. `SLOWSHELL_REPROBE=1` forces a fresh probe. The cache is a real design
decision, not an optimisation: a shell that spends three seconds probing at
every launch has already failed the startup budget.

A swap chain also has to be revalidated: `D2DERR_RECREATE_TARGET` means the
device was lost, and the shell rebuilds the window target and carries on.

## The frame loop

```rust
while self.running {
    window::pump(/* … */);          // drain Win32 messages
    for event in events { … }
    self.serve_ipc();                // answer one shellctl request
    self.poll_files();               // stat watched files, compare mtimes
    if self.reload_requested { self.build(); continue; }

    if self.publish_system() {       // push values, report if any changed
        self.frame();
        continue;
    }

    // Nothing changed. Block in the kernel until a message arrives or the
    // clock's displayed value could change.
    let until = self.next_clock.saturating_duration_since(now)
        .clamp(Duration::from_millis(4), Duration::from_millis(1_000));
    window::wait(until.as_millis() as u32, |_, _| {});
}
```

There is no timer in the sense of a periodic tick that always does the same
thing. The loop blocks in `MsgWaitForMultipleObjectsEx` — in the kernel — until
something actually happens: a user event, a `shellctl` request (the IPC thread
*posts a message* to wake the loop), or the next moment the displayed clock
could change. When a provider pushes a change, `publish_system` returns `true`
and the loop draws immediately.

`publish_system` is what makes idle nearly free: it asks the reactor whether
anything actually changed, and the answer is `false` on a still desktop.

This replaced an earlier design that slept with a 50 ms ceiling so `shellctl`
would be answered promptly. That ceiling was the whole idle cost — twenty
wake-ups a second on a desktop where nothing was happening. Waking on a posted
message instead of on a timer is what took idle CPU from 0.6% to 0.26%.


## Error boundaries

A shell that crashes on a bad config is a shell nobody trusts twice. Three
mechanisms, at three levels:

| Level | What happens |
|---|---|
| Parse | The previous scene keeps running. The error is reported with file, line, column, and a `did you mean`. |
| Element | The element becomes a visible `Broken` placeholder in a tinted box. Its siblings are unaffected. |
| Frame | `catch_unwind` at the top of `main` reports the panic and exits cleanly, rather than letting a dialog appear over the desktop. |

The element-level one is a deliberate design choice, not a patch. A config with
one bad widget in a twelve-widget bar should show eleven working widgets and one
obvious marker, not nine working widgets and a silent gap.

The renderer also reports what it could not draw: `DrawStats` counts draws,
skips and dropped text runs, and the shell warns on an empty frame. Ten of the
bugs fixed during development were found by reading pixels this way, not by
reading code.

## Style inheritance

Only **typography and colour** cascade from parent to child
(`Style::inheritable()`). Padding, margin, background and border do not.

Inheriting everything looks reasonable and is wrong: a parent with
`padding: 12` would then hand that padding to every child, and the layout pass
— which already insets for the parent's padding — would count it twice. Every
child would be inset by 24 instead of 12. This was a real bug, found by
measuring rectangles in a test.

Relatedly, an element's **rect is its content box**: the slot minus its own
margin minus its own padding. Layout insets; paint must not inset again.

## Actions

Config *reads* the system through the host and *writes* through the action
registry (`slowshell-core::actions`). Keeping them separate is deliberate.

Reading a path is allowed from inside a reactive expression, so a bare write
hidden behind the same accessor would mean any future `screens.width = …` typo
silently became a write to a system object during layout. Actions are always a
*call*, never a bare name, which makes the destructive half of the language
visible at the call site.

The registry lives in the language crate, not the runtime, because it is a
property of the language: what a config may name, and what happens when it names
something that does not exist. `check_handlers` runs at build time, so
`onClick: launhcer.toggle()` is a diagnostic next to the line that contains it
rather than a shrug when the user discovers the button does nothing.

An empty registry means "no actions loaded yet", not "nothing exists" — so the
runtime skips the check rather than reporting every handler as unknown. The
shell registers the real list, which is why `Shell.exe --check` is trustworthy
about handlers.

Actions that must rebuild or tear down a surface do not do it. They push onto a
deferred queue and the frame loop drains it, because an action that rebuilt a
surface while the paint pass was walking it would be a use-after-free rather than
a feature.

## `shellctl` and the pipe

`ConnectNamedPipe` blocks until a client arrives. On the UI thread that would
freeze the bar for as long as nobody runs `shellctl`, which is essentially
always. So one thread accepts connections and hands requests over a channel, and
the frame loop drains it once per turn — the same place it already drains
window messages.

Two details that took real work:

**The pipe name carries the pid**, so two shells on one desktop do not share a
pipe and steal each other's requests. But a `shellctl` does not know the shell's
pid, so the shell publishes its endpoint to a small file and the CLI reads it.
That solves the crash case for free: a file left behind by a killed shell names
a pid that is no longer running, which is exactly the check needed to say "no
slowshell is running" rather than hanging.

**The next pipe instance is created before the current one is torn down.**
Otherwise there is a window in which the name has no live instance, a client's
`CreateFileW` succeeds, and its read fails with `ERROR_BROKEN_PIPE` — which
reads as "the shell crashed" when it did not.

## Hot reload

`poll_files` stats each file the last build read and compares modification
times. A touch that changes nothing costs one `stat` and nothing else. A real
change calls `build()`.

`build()` replaces the element trees but keeps the windows, so an ordinary edit
does not make the bar flicker. Windows are recreated only when the panel count
or a panel's declared name changes.

A build that fails keeps the previous surfaces. That is the entire point: a
half-typed line never blanks the desktop.

## Things that are deliberately not here

**A plugin loader.** There is no dynamic code loading, and the action registry
is the extension surface that exists. See [PLUGIN_API.md](PLUGIN_API.md) for
what a loader would need and why it is not built yet.

**Keyboard focus and text input.** A `Panel` takes clicks through
`WM_NCHITTEST` hit regions, which works. It does not take focus, so there is no
`onKey` and no text field. This is the single biggest gap between Slowshell and
a real launcher, and it is a large piece of work, not a detail.

**Animation.** The frame loop is event-driven, which is right for a bar and
means there is no animation clock to drive a transition. Animations are the
obvious next subsystem, and they are the reason a frame budget would need to
become a thing the shell measures rather than a thing it happens to have.
