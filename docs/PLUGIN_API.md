# Plugin API

**There is no plugin loader.** This document describes the extension surface
that exists, why the loader is not built, and what building one would require.

This is here rather than omitted because "no plugins" reads as an oversight, and
it is not one — there is a design decision behind it, and the design is sound.

---

## What exists

### 1. The action registry

`slowshell_core::Registry` is the extension point. An action is a named
function with a documented signature that a config may call:

```rust
registry.register(
    "system.lock",
    "Lock the workstation.",
    &[],
    |_| { /* … */ Ok(Value::str("locked")) },
);
```

Everything registered is:

- callable from any config, in any handler
- validated at **compile time**, so a typo is a diagnostic next to the line
- listed by `shellctl actions`
- listed by `shellctl status` on the *running* shell, which is the authoritative
  answer to "what can this config do?"

Adding a capability to Slowshell without touching the language, the parser, the
renderer or the compiler means adding one `register` call. That is the property
the registry exists to provide.

### 2. The provider interface

A provider is an `Object` — a read-only, dotted-path namespace:

```rust
providers.insert("battery".into(), my_battery_object());
```

Reading a path is what makes a property reactive: building a binding inside a
tracking scope records the read as a dependency, and the value is pushed in by
the provider's tick. A provider that pushes an unchanged value tells the shell
nothing happened, and no frame is drawn.

The contract a provider must honour:

| | |
|---|---|
| **Push, do not poll** | Call `reactor.set(path, value)` when something changes. Do not wake on a timer. `Reactor::set` returns `false` for an unchanged value, and that is the whole idle budget. |
| **Report the null case** | A provider that is not implemented should be registered as a stub and listed in `Pending`, so `doctor` names it and a config can guard on it. |
| **Register every path** | A path that resolves on one frame and not the next is worse than one that never resolves. |

The last point is not theoretical: `battery.present` reading `null` is fine, and
`battery.percentage` reading `87` on one frame and `null` on the next is a
widget that flickers for no reason.

### 3. The renderer

`Painter` is a concrete type in `slowshell-win::render`, and `RenderTarget` is a
trait with a blanket impl over anything that can be a Direct2D target. A new
drawing primitive is a method on `Painter`; a new *backend* is an impl of
`RenderTarget` plus a `Surface`.

---

## What does not exist

| | |
|---|---|
| No dynamic library loading | Nothing is `LoadLibrary`d. No DLL, no `.wasm`, no scripting host. |
| No plugin discovery | No directory is scanned for plugins. |
| No plugin lifecycle | No `onLoad`/`onUnload`, no dependency ordering, no per-plugin config. |
| No sandbox | There is nothing to sandbox, because there is nothing to load. |

---

## Why not

Three reasons, in the order they matter.

### The safety story would be dishonest

A shell plugin runs in the shell's process. A plugin that crashes takes the
desktop with it. Mitigating that means an out-of-process protocol, which means a
local IPC surface, which means a *much* larger attack surface for a shell that
has to run all day.

Slowshell's position is that a config file is a safe extension mechanism —
it cannot segfault the process — and a dynamic library is not, unless the loader
is out-of-process, which is a much larger project than it looks.

### Nobody can audit what they cannot see

`cargo build --release` produces two binaries. That is a property worth keeping.
A plugin loader would mean the shipped artifact is no longer the whole story, and
"what is this shell actually doing" becomes a question about every plugin on the
machine.

### The surface is not the bottleneck yet

The action registry already covers the thing people want plugins for: a custom
action that a config can call. The thing plugins are *not* yet needed for is new
widgets — and new widgets need the language, the layout pass and the renderer to
change together, which is a change to Slowshell, not an extension of it.

A plugin API for changing the widget set is a real design problem. It has not
been solved here, and guessing at it would be worse than not having it.

---

## What building one would take

For the record, and in the order it would have to be done.

### 1. An out-of-process protocol

A plugin is a separate process. It talks to the shell over the same named-pipe
transport `shellctl` uses — the mechanism is there and tested.

```
Shell.exe  ←── pipe ──→  plugin.exe
   │                      │
   │  "battery.percentage"  │  answers a query, or pushes a value
   │  "focus.toggle()"       │  calls an action the shell implements
   │  "widget.render()"      │  returns draw commands
```

A crashed plugin then costs a restart, not a desktop.

### 2. A draw-command protocol

This is the hard part, and the reason it is not done.

`Painter` draws immediately. A plugin on the other end of a pipe cannot. So the
protocol needs a retained command list — rectangles, round rectangles, text runs
with resolved metrics, images, clipping — that the shell replays into its own
`Painter`.

That is a real serialisation format, and it has to survive:

- **DPI.** The plugin has no idea what scale it is being drawn at. Either the
  shell sends logical units and the plugin measures with a font, which means
  shipping a text-measuring service too, or it sends device units and the
  plugin has to know the scale.
- **Fonts.** Text needs real DirectWrite metrics for the shell to lay it out. A
  plugin cannot measure text without either DirectWrite or a round trip per
  string, and a round trip per string at 60 Hz is not viable.
- **Versioning.** A protocol between a shell and a plugin it did not build has
  to be versioned, and an old plugin on a new shell has to degrade rather than
  corrupt.

The text problem is the one that decides the design, and it has not been
answered here.

### 3. Widgets, not just actions

Making a plugin able to define a *widget* — a new element type the compiler
knows about, the layout pass can size, and the painter can draw — means
extending three static tables that are currently plain `match` statements:
`props::is_known_type`, `compiler::kind_of`, and the paint walk.

Making those dynamic is feasible and would be a genuinely good design. It is also
a large change to code that is currently correct, and doing it speculatively
would be building a framework for a plugin nobody has written.

---

## What to do instead, today

The registry is the supported extension point. If you need a capability:

1. **If it can be a config action** — a bound key, a URL, a program, a file —
   it already is, or it is one `register` call.
2. **If it is a system value** — register a provider and push into the reactor.
3. **If it is a widget** — it needs a change to Slowshell, and an issue
   describing the widget is worth more than a plugin.

Contributions that extend the registry are welcome and small. See
[CONTRIBUTING.md](CONTRIBUTING.md).

---

## The honest version

Slowshell today is: a config language, a reactive system, a renderer, an action
registry, and a control channel. That is enough to build a bar, a dock, a
launcher and a dashboard, and all of those are in [`examples/`](../examples).

It is not enough to be a platform for other people's code, and it does not
pretend to be. The registry, the provider contract and the `RenderTarget` trait
are the shapes such a platform would take, and they are in place because the
things built on them were needed anyway.
